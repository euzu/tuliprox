use crate::library::{
    metadata::MediaMetadata,
    tmdb::{
        TmdbMovieDetails, TmdbSearchResponse, TmdbSeriesInfoDetails, TmdbSeriesInfoSeasonDetails, TmdbTvSearchResponse,
    },
    MetadataStorage,
};
use log::{debug, error, warn};
use serde::de::DeserializeOwned;
use serde_json::Value;
use shared::utils::sanitize_sensitive_info;
use std::collections::HashSet;
use tokio::time::{sleep, Duration, Instant};
use url::Url;

// TODO make this configurable in Library tmdb config
const TMDB_API_BASE_URL: &str = "https://api.themoviedb.org/3";
const MAX_RETRIES: u32 = 3;
const MAX_FETCHED_CACHE_ENTRIES: usize = 10_000;
/// Minimum Jaro-Winkler score required to accept a TMDB search result.
const TMDB_MATCH_THRESHOLD: f64 = 0.9;

// TMDB API client with rate limiting
pub struct TmdbClient {
    api_key: String,
    client: reqwest::Client,
    rate_limit_ms: u64,
    storage: MetadataStorage,
    fetched_movie_metadata: tokio::sync::RwLock<HashSet<u32>>,
    fetched_series_metadata: tokio::sync::RwLock<HashSet<u32>>,
    fetched_series_key: tokio::sync::RwLock<HashSet<String>>,
    next_request_slot: tokio::sync::Mutex<Instant>,
}

impl TmdbClient {
    // Creates a new TMDB client
    pub fn new(api_key: String, rate_limit_ms: u64, client: reqwest::Client, storage: MetadataStorage) -> Self {
        Self {
            api_key,
            client,
            rate_limit_ms,
            storage,
            fetched_movie_metadata: tokio::sync::RwLock::new(HashSet::new()),
            fetched_series_metadata: tokio::sync::RwLock::new(HashSet::new()),
            fetched_series_key: tokio::sync::RwLock::new(HashSet::new()),
            next_request_slot: tokio::sync::Mutex::new(Instant::now()),
        }
    }

    async fn reserve_rate_limit_slot(&self) {
        if self.rate_limit_ms == 0 {
            return;
        }
        let interval = Duration::from_millis(self.rate_limit_ms);
        let mut next_slot = self.next_request_slot.lock().await;
        let now = Instant::now();
        let scheduled_at = if *next_slot > now { *next_slot } else { now };
        *next_slot = scheduled_at + interval;
        drop(next_slot);

        let now_after = Instant::now();
        if scheduled_at > now_after {
            sleep(scheduled_at - now_after).await;
        }
    }

    /// Executes a TMDB request and returns raw response bytes.
    /// Applies shared rate-limiting and retry logic.
    async fn execute_raw_request(&self, url: &str) -> Result<Option<bytes::Bytes>, String> {
        let safe_url = sanitize_sensitive_info(url);
        let mut attempt = 0;
        loop {
            attempt += 1;
            self.reserve_rate_limit_slot().await;
            match self.client.get(url).send().await {
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() {
                        return response
                            .bytes()
                            .await
                            .map(Some)
                            .map_err(|err| format!("Failed to read TMDB response body: {err}"));
                    }

                    if status == reqwest::StatusCode::NOT_FOUND {
                        return Ok(None);
                    }

                    let retryable = status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS;
                    if retryable && attempt < MAX_RETRIES {
                        warn!("TMDB API error ({status}) for {safe_url}, retrying {attempt}/{MAX_RETRIES}");
                        sleep(Duration::from_millis(500 * u64::from(attempt))).await;
                        continue;
                    }

                    error!("TMDB API error ({status}) for {safe_url}");
                    return Err(format!("TMDB API error: {status}"));
                }
                Err(e) => {
                    if attempt < MAX_RETRIES {
                        warn!("TMDB request failed for {safe_url}: {e}, retrying {attempt}/{MAX_RETRIES}");
                        sleep(Duration::from_millis(500 * u64::from(attempt))).await;
                    } else {
                        return Err(format!("TMDB API request failed after {MAX_RETRIES} attempts: {e}"));
                    }
                }
            }
        }
    }

    /// Centralized request execution with retry logic, rate limiting, and error handling.
    async fn execute_request<T: DeserializeOwned>(&self, url: String) -> Result<Option<T>, String> {
        let safe_url = sanitize_sensitive_info(&url);
        let Some(content) = self.execute_raw_request(&url).await? else {
            return Ok(None);
        };

        match serde_json::from_slice::<T>(&content) {
            Ok(data) => Ok(Some(data)),
            Err(e) => {
                warn!("Failed to parse TMDB response from {safe_url}: {e}");
                Err(e.to_string())
            }
        }
    }

    async fn remember_movie_metadata(&self, movie_id: u32) {
        let mut fetched = self.fetched_movie_metadata.write().await;
        if fetched.len() >= MAX_FETCHED_CACHE_ENTRIES {
            debug!("TMDB movie metadata cache reached limit ({MAX_FETCHED_CACHE_ENTRIES}), clearing.");
            fetched.clear();
        }
        fetched.insert(movie_id);
    }

    async fn remember_series_metadata(&self, series_id: u32) {
        let mut fetched = self.fetched_series_metadata.write().await;
        if fetched.len() >= MAX_FETCHED_CACHE_ENTRIES {
            debug!("TMDB series metadata cache reached limit ({MAX_FETCHED_CACHE_ENTRIES}), clearing.");
            fetched.clear();
        }
        fetched.insert(series_id);
    }

    async fn remember_series_key(&self, key: String) {
        let mut fetched = self.fetched_series_key.write().await;
        if fetched.len() >= MAX_FETCHED_CACHE_ENTRIES {
            debug!("TMDB series key cache reached limit ({MAX_FETCHED_CACHE_ENTRIES}), clearing.");
            fetched.clear();
        }
        fetched.insert(key);
    }

    // Searches for a movie by title and optional year
    pub async fn search_movie(
        &self,
        tmdb_id: Option<u32>,
        title: &str,
        year: Option<u32>,
    ) -> Result<Option<MediaMetadata>, String> {
        let year_display = year.map_or_else(|| "None".to_string(), |y| y.to_string());

        // Validate the ID: Treat Some(0) the same as None
        let valid_id = tmdb_id.filter(|&id| id > 0);

        if let Some(id) = valid_id {
            debug!("TMDB search movie by ID: {title} ({year_display}) [ID: {id}]");
            return self.fetch_movie_details(id).await;
        }

        debug!("TMDB search movie by title: {title} ({year_display})");

        // Build search URL
        let url = self.build_movie_search_url(title, year)?;

        // Execute Search
        let search_result: Option<TmdbSearchResponse> = self.execute_request(url.to_string()).await?;

        if let Some(search) = search_result {
            let query_lower = title.to_lowercase();
            let candidates: Vec<(u32, &str, &str)> = search
                .results
                .iter()
                .filter(|m| m.id > 0)
                .map(|m| (m.id, m.title.as_str(), m.original_title.as_str()))
                .collect();

            if let Some((score, movie_id)) = Self::best_match_by_title(&query_lower, &candidates) {
                debug!("TMDB movie best match for '{title}': ID {movie_id} (score {score:.2})");
                self.fetch_movie_details(movie_id).await
            } else {
                debug!("TMDB movie search for '{title}': no result met threshold {TMDB_MATCH_THRESHOLD:.2}");
                Ok(None)
            }
        } else {
            Ok(None)
        }
    }

    fn build_movie_search_url(&self, title: &str, year: Option<u32>) -> Result<Url, String> {
        let mut url = Url::parse(&format!("{TMDB_API_BASE_URL}/search/movie"))
            .map_err(|e| format!("Failed to parse URL for TMDB movie search: {e}"))?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("api_key", &self.api_key);
            q.append_pair("query", title);
            if let Some(y) = year {
                q.append_pair("year", &y.to_string());
            }
        }
        Ok(url)
    }

    // Fetches detailed movie information
    async fn fetch_movie_details(&self, movie_id: u32) -> Result<Option<MediaMetadata>, String> {
        if movie_id == 0 {
            return Ok(None);
        }

        if self.fetched_movie_metadata.read().await.contains(&movie_id) {
            return Ok(None);
        }

        let url = format!(
            "{TMDB_API_BASE_URL}/movie/{movie_id}?api_key={}&append_to_response=credits,videos,external_ids",
            self.api_key
        );

        let Some(content_bytes) = self.execute_raw_request(&url).await? else {
            warn!("TMDB Movie ID {movie_id} not found");
            return Ok(None);
        };

        if let Err(err) = self.storage.store_tmdb_movie_info(movie_id, &content_bytes).await {
            warn!("Failed to cache TMDB movie info: {err}");
        }

        let details: TmdbMovieDetails = serde_json::from_slice(content_bytes.as_ref())
            .map_err(|err| format!("Failed to parse TMDB movie details: {err}"))?;
        self.remember_movie_metadata(movie_id).await;

        Ok(Some(MediaMetadata::Movie(details.to_meta_data())))
    }

    // Searches for a TV series by title and optional year
    pub async fn search_series(
        &self,
        tmdb_id: Option<u32>,
        title: &str,
        year: Option<u32>,
    ) -> Result<Option<MediaMetadata>, String> {
        let key = format!("{title}-{tmdb_id:?}-{year:?}");

        if let Some(id) = tmdb_id {
            debug!("Searching TMDB for series: {title} [ID: {id}]");
        } else {
            debug!("Searching TMDB for series: {title}");
        }

        if self.fetched_series_key.read().await.contains(&key) {
            return Ok(None);
        }

        // Validate ID is not 0
        let valid_id = tmdb_id.filter(|&id| id > 0);

        let result = if let Some(series_id) = valid_id {
            self.fetch_series_details(series_id).await
        } else {
            self.search_series_by_title(title, year).await
        };

        if result.as_ref().is_ok_and(Option::is_some) {
            self.remember_series_key(key).await;
        }

        result
    }

    async fn search_series_by_title(&self, title: &str, year: Option<u32>) -> Result<Option<MediaMetadata>, String> {
        let mut url = Url::parse(&format!("{TMDB_API_BASE_URL}/search/tv"))
            .map_err(|e| format!("Failed to parse TMDB search URL: {e}"))?;

        {
            let mut q = url.query_pairs_mut();
            q.append_pair("api_key", &self.api_key);
            q.append_pair("query", title);
            if let Some(y) = year {
                q.append_pair("first_air_date_year", &y.to_string());
            }
        }

        debug!("TMDB search series by title: {title}");

        let search_result: Option<TmdbTvSearchResponse> = self.execute_request(url.to_string()).await?;

        if let Some(search) = search_result {
            let query_lower = title.to_lowercase();
            let candidates: Vec<(u32, &str, &str)> = search
                .results
                .iter()
                .filter(|s| s.id > 0)
                .map(|s| (s.id, s.name.as_str(), s.original_name.as_str()))
                .collect();

            if let Some((score, series_id)) = Self::best_match_by_title(&query_lower, &candidates) {
                debug!("TMDB series best match for '{title}': ID {series_id} (score {score:.2})");
                self.fetch_series_details(series_id).await
            } else {
                debug!("TMDB series search for '{title}': no result met threshold {TMDB_MATCH_THRESHOLD:.2}");
                Ok(None)
            }
        } else {
            Ok(None)
        }
    }

    /// Selects the best matching candidate by Jaro-Winkler similarity.
    ///
    /// Compares `query` (already lowercased) against both the primary title and
    /// the original title of every candidate, takes the maximum score, and returns
    /// `Some((score, id))` only when the best score is >= `TMDB_MATCH_THRESHOLD`.
    fn best_match_by_title(query: &str, candidates: &[(u32, &str, &str)]) -> Option<(f64, u32)> {
        candidates
            .iter()
            .map(|&(id, title, original_title)| {
                let score = strsim::jaro_winkler(query, &title.to_lowercase())
                    .max(strsim::jaro_winkler(query, &original_title.to_lowercase()));
                (score, id)
            })
            .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
            .filter(|&(score, _)| score >= TMDB_MATCH_THRESHOLD)
    }

    // Fetches detailed TV series information
    pub async fn fetch_series_details(&self, series_id: u32) -> Result<Option<MediaMetadata>, String> {
        // Skip if metadata already fetched
        if self.fetched_series_metadata.read().await.contains(&series_id) {
            return Ok(None);
        }

        // Fetch series info from TMDB API
        let url = format!(
            "{TMDB_API_BASE_URL}/tv/{series_id}?api_key={}&append_to_response=credits,videos,external_ids",
            self.api_key
        );

        let Some(series_content) = self.execute_raw_request(&url).await? else {
            warn!("TMDB Series ID {series_id} not found");
            return Ok(None);
        };

        // Mark series as fetched
        self.remember_series_metadata(series_id).await;

        // Deserialize TMDB series info into struct
        let mut series: TmdbSeriesInfoDetails = serde_json::from_slice(series_content.as_ref())
            .map_err(|e| format!("Failed to parse TMDB series details: {e}"))?;

        // Determine number of seasons
        let season_count = Self::detect_season_count(&series);
        let mut stored_content = series_content.to_vec();

        if season_count > 0 {
            // Fetch season details
            let season_infos = self.fetch_seasons(series_id, season_count).await;
            if !season_infos.is_empty() {
                // Deserialize a raw JSON map to update dynamically
                let mut raw_series: serde_json::Map<String, serde_json::Value> =
                    serde_json::from_slice(&series_content)
                        .map_err(|e| format!("Failed to parse raw series JSON: {e}"))?;

                if let Some(series_seasons) = series.seasons.as_mut() {
                    for series_season in series_seasons {
                        let season_no = series_season.season_number;
                        for (season_details, raw_season_details_content) in &season_infos {
                            if season_details.season_number == season_no {
                                // Update struct with episodes, networks, credits
                                series_season.episodes = Some(season_details.episodes.clone());
                                series_season.networks = Some(season_details.networks.clone());
                                series_season.credits.clone_from(&season_details.credits);

                                // Update raw JSON
                                if let Ok(raw_season_details_json) =
                                    serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(
                                        raw_season_details_content.as_ref(),
                                    )
                                {
                                    if let Some(Value::Array(series_season_list)) = raw_series.get_mut("seasons") {
                                        for series_season_item in series_season_list {
                                            if let Value::Object(season_item_obj) = series_season_item {
                                                if let Some(Value::Number(no)) = season_item_obj.get("season_number") {
                                                    if no.as_u64().and_then(|n| u32::try_from(n).ok())
                                                        == Some(season_no)
                                                    {
                                                        season_item_obj.insert(
                                                            "episodes".to_string(),
                                                            raw_season_details_json
                                                                .get("episodes")
                                                                .cloned()
                                                                .unwrap_or(Value::Null),
                                                        );
                                                        season_item_obj.insert(
                                                            "networks".to_string(),
                                                            raw_season_details_json
                                                                .get("networks")
                                                                .cloned()
                                                                .unwrap_or(Value::Null),
                                                        );
                                                        season_item_obj.insert(
                                                            "credits".to_string(),
                                                            raw_season_details_json
                                                                .get("credits")
                                                                .cloned()
                                                                .unwrap_or(Value::Null),
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // Serialize updated raw JSON and store
                stored_content = serde_json::to_vec(&raw_series)
                    .map_err(|e| format!("Failed to serialize updated raw TMDB series JSON: {e}"))?;
            }
        }

        if let Err(err) = self.storage.store_tmdb_series_info(series_id, &stored_content).await {
            warn!("Failed to cache raw TMDB series info: {err}");
        }

        Ok(Some(MediaMetadata::Series(series.to_meta_data())))
    }

    fn detect_season_count(series: &TmdbSeriesInfoDetails) -> u32 {
        if series.number_of_seasons > 0 {
            series.number_of_seasons
        } else {
            series.seasons.as_ref().and_then(|s| u32::try_from(s.len()).ok()).unwrap_or(0)
        }
    }
    async fn fetch_seasons(&self, series_id: u32, seasons: u32) -> Vec<(TmdbSeriesInfoSeasonDetails, bytes::Bytes)> {
        let mut result = Vec::new();
        for season in 1..=seasons {
            if let (Some(info), Some(content)) = self.fetch_single_season(series_id, season).await {
                result.push((info, content));
            }
        }
        result
    }

    async fn fetch_single_season(
        &self,
        series_id: u32,
        season: u32,
    ) -> (Option<TmdbSeriesInfoSeasonDetails>, Option<bytes::Bytes>) {
        let url = format!(
            "{TMDB_API_BASE_URL}/tv/{series_id}/season/{season}?api_key={}&append_to_response=credits",
            self.api_key
        );

        match self.execute_raw_request(&url).await {
            Ok(Some(bytes)) => match serde_json::from_slice::<TmdbSeriesInfoSeasonDetails>(bytes.as_ref()) {
                Ok(details) => (Some(details), Some(bytes)),
                Err(e) => {
                    error!("Failed to parse season details for {series_id} S{season}: {e}");
                    (None, None)
                }
            },
            Ok(None) => {
                debug!("No TMDB season details found for series {series_id} season {season}");
                (None, None)
            }
            Err(err) => {
                warn!("TMDB season request failed for series {series_id} season {season}: {err}");
                (None, None)
            }
        }
    }
}
