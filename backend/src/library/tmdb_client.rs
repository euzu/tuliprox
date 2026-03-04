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
use std::{
    collections::{HashSet, VecDeque},
    hash::Hash,
};
use tokio::time::{sleep, timeout, Duration, Instant};
use url::Url;

// TODO make this configurable in Library tmdb config
const TMDB_API_BASE_URL: &str = "https://api.themoviedb.org/3";
const MAX_RETRIES: u32 = 3;
const REQUEST_TIMEOUT_SECS: u64 = 30;
const MAX_FETCHED_CACHE_ENTRIES: usize = 10_000;
/// Minimum Jaro-Winkler score required to accept a TMDB search result.
const TMDB_MATCH_THRESHOLD: f64 = 0.9;

/// Bounded set with insertion-order (FIFO) eviction.
/// This is intentionally not LRU: `contains()` does not refresh recency.
#[derive(Debug)]
struct BoundedSet<T>
where
    T: Eq + Hash + Clone,
{
    entries: HashSet<T>,
    insertion_order: VecDeque<T>,
    capacity: usize,
}

impl<T> BoundedSet<T>
where
    T: Eq + Hash + Clone,
{
    fn new(capacity: usize) -> Self {
        Self { entries: HashSet::new(), insertion_order: VecDeque::new(), capacity: capacity.max(1) }
    }

    fn contains(&self, value: &T) -> bool { self.entries.contains(value) }

    /// Inserts `value` and evicts a single oldest entry when full.
    /// Returns `true` if an eviction happened.
    fn insert(&mut self, value: T) -> bool {
        if self.entries.contains(&value) {
            return false;
        }

        let evicted = self.evict_one_if_full();
        self.entries.insert(value.clone());
        self.insertion_order.push_back(value);
        evicted
    }

    fn evict_one_if_full(&mut self) -> bool {
        if self.entries.len() < self.capacity {
            return false;
        }

        while let Some(oldest) = self.insertion_order.pop_front() {
            if self.entries.remove(&oldest) {
                return true;
            }
        }

        if let Some(any_existing) = self.entries.iter().next().cloned() {
            self.entries.remove(&any_existing);
            return true;
        }

        false
    }
}

// TMDB API client with rate limiting
pub struct TmdbClient {
    api_key: String,
    client: reqwest::Client,
    rate_limit_ms: u64,
    storage: MetadataStorage,
    fetched_movie_metadata: tokio::sync::RwLock<BoundedSet<u32>>,
    fetched_series_metadata: tokio::sync::RwLock<BoundedSet<u32>>,
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
            fetched_movie_metadata: tokio::sync::RwLock::new(BoundedSet::new(MAX_FETCHED_CACHE_ENTRIES)),
            fetched_series_metadata: tokio::sync::RwLock::new(BoundedSet::new(MAX_FETCHED_CACHE_ENTRIES)),
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
        let request_timeout = Duration::from_secs(REQUEST_TIMEOUT_SECS);
        let mut attempt = 0;
        loop {
            attempt += 1;
            self.reserve_rate_limit_slot().await;
            let response = match timeout(request_timeout, self.client.get(url).send()).await {
                Ok(Ok(response)) => response,
                Ok(Err(err)) => {
                    if attempt < MAX_RETRIES {
                        warn!(
                            "TMDB request failed for {safe_url}: {err}, retrying {attempt}/{MAX_RETRIES}"
                        );
                        sleep(Duration::from_millis(500 * u64::from(attempt))).await;
                        continue;
                    }
                    return Err(format!("TMDB API request failed after {MAX_RETRIES} attempts: {err}"));
                }
                Err(_) => {
                    if attempt < MAX_RETRIES {
                        warn!(
                            "TMDB request timed out for {safe_url} (attempt {attempt}/{MAX_RETRIES}), retrying"
                        );
                        sleep(Duration::from_millis(500 * u64::from(attempt))).await;
                        continue;
                    }
                    return Err(format!(
                        "TMDB API request timed out after {MAX_RETRIES} attempts for {safe_url}"
                    ));
                }
            };

            let status = response.status();
            if status.is_success() {
                match timeout(request_timeout, response.bytes()).await {
                    Ok(Ok(content)) => return Ok(Some(content)),
                    Ok(Err(err)) => {
                        if attempt < MAX_RETRIES {
                            warn!(
                                "Failed to read TMDB response body for {safe_url}: {err}, retrying {attempt}/{MAX_RETRIES}"
                            );
                            sleep(Duration::from_millis(500 * u64::from(attempt))).await;
                            continue;
                        }
                        return Err(format!(
                            "Failed to read TMDB response body after {MAX_RETRIES} attempts for {safe_url}: {err}"
                        ));
                    }
                    Err(_) => {
                        if attempt < MAX_RETRIES {
                            warn!(
                                "Timed out reading TMDB response body for {safe_url} (attempt {attempt}/{MAX_RETRIES}), retrying"
                            );
                            sleep(Duration::from_millis(500 * u64::from(attempt))).await;
                            continue;
                        }
                        return Err(format!(
                            "Timed out reading TMDB response body after {MAX_RETRIES} attempts for {safe_url}"
                        ));
                    }
                }
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
                let message = format!("Failed to parse TMDB response from {safe_url}: {e}");
                warn!("{message}");
                Err(message)
            }
        }
    }

    async fn remember_movie_metadata(&self, movie_id: u32) {
        let mut fetched = self.fetched_movie_metadata.write().await;
        if fetched.insert(movie_id) {
            debug!("TMDB movie metadata cache reached limit ({MAX_FETCHED_CACHE_ENTRIES}), evicted one oldest entry.");
        }
    }

    async fn remember_series_metadata(&self, series_id: u32) {
        let mut fetched = self.fetched_series_metadata.write().await;
        if fetched.insert(series_id) {
            debug!("TMDB series metadata cache reached limit ({MAX_FETCHED_CACHE_ENTRIES}), evicted one oldest entry.");
        }
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

    fn build_movie_details_url(&self, movie_id: u32) -> Result<Url, String> {
        let mut url = Url::parse(&format!("{TMDB_API_BASE_URL}/movie/{movie_id}"))
            .map_err(|e| format!("Failed to parse URL for TMDB movie details: {e}"))?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("api_key", &self.api_key);
            q.append_pair("append_to_response", "credits,videos,external_ids");
        }
        Ok(url)
    }

    // Fetches detailed movie information
    async fn fetch_movie_details(&self, movie_id: u32) -> Result<Option<MediaMetadata>, String> {
        if movie_id == 0 {
            return Ok(None);
        }

        if self.fetched_movie_metadata.read().await.contains(&movie_id) {
            match self.storage.read_tmdb_movie_info(movie_id).await {
                Ok(content_bytes) => match serde_json::from_slice::<TmdbMovieDetails>(&content_bytes) {
                    Ok(details) => return Ok(Some(MediaMetadata::Movie(details.to_meta_data()))),
                    Err(err) => warn!("Failed to parse cached TMDB movie details for {movie_id}: {err}"),
                },
                Err(err) => warn!("Failed to read cached TMDB movie info for {movie_id}: {err}"),
            }
            // Fallthrough to fetch if cache read or parse fails
        }

        let url = self.build_movie_details_url(movie_id)?;

        let Some(content_bytes) = self.execute_raw_request(url.as_str()).await? else {
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

        if let Some(id) = tmdb_id {
            debug!("Searching TMDB for series: {title} [ID: {id}]");
        } else {
            debug!("Searching TMDB for series: {title}");
        }

        // Validate ID is not 0
        let valid_id = tmdb_id.filter(|&id| id > 0);

        if let Some(series_id) = valid_id {
            self.fetch_series_details(series_id).await
        } else {
            self.search_series_by_title(title, year).await
        }
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
            match self.storage.read_tmdb_series_info(series_id).await {
                Ok(content_bytes) => match serde_json::from_slice::<TmdbSeriesInfoDetails>(&content_bytes) {
                    Ok(details) => return Ok(Some(MediaMetadata::Series(details.to_meta_data()))),
                    Err(err) => warn!("Failed to parse cached TMDB series details for {series_id}: {err}"),
                },
                Err(err) => warn!("Failed to read cached TMDB series info for {series_id}: {err}"),
            }
            // Fallthrough to fetch if cache read or parse fails
        }

        // Fetch series info from TMDB API
        let mut url = Url::parse(&format!("{TMDB_API_BASE_URL}/tv/{series_id}"))
            .map_err(|e| format!("Failed to parse URL for TMDB series details: {e}"))?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("api_key", &self.api_key);
            q.append_pair("append_to_response", "credits,videos,external_ids");
        }

        let Some(series_content) = self.execute_raw_request(url.as_str()).await? else {
            warn!("TMDB Series ID {series_id} not found");
            return Ok(None);
        };

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

        self.remember_series_metadata(series_id).await;
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
        let url = match Url::parse(&format!("{TMDB_API_BASE_URL}/tv/{series_id}/season/{season}")) {
            Ok(mut u) => {
                {
                    let mut q = u.query_pairs_mut();
                    q.append_pair("api_key", &self.api_key);
                    q.append_pair("append_to_response", "credits");
                }
                u
            }
            Err(e) => {
                error!("Failed to parse TMDB season URL for {series_id} S{season}: {e}");
                return (None, None);
            }
        };

        match self.execute_raw_request(url.as_str()).await {
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

#[cfg(test)]
mod tests {
    const RESULT: &str = r#"{"adult":false,"backdrop_path":"/aM6E4DBP6588q3tEr9hz41ls80q.jpg","belongs_to_collection":null,"budget":13000000,"genres":[{"id":18,"name":"Drama"}],"homepage":"https://www.lionsgate.com/movies/the-perks-of-being-a-wallflower","id":84892,"imdb_id":"tt1659337","origin_country":["US"],"original_language":"en","original_title":"The Perks of Being a Wallflower","overview":"Pittsburgh, Pennsylvania, 1991. High school freshman Charlie is a wallflower, always watching life from the sidelines, until two senior students, Sam and her stepbrother Patrick, become his mentors, helping him discover the joys of friendship, music and love.","popularity":5.8295,"poster_path":"/aKCvdFFF5n80P2VdS7d8YBwbCjh.jpg","production_companies":[{"id":2130,"logo_path":"/g0lqeY2FvhzXcOI6z8RVXbORRUY.png","name":"Mr. Mudd","origin_country":"US"}],"production_countries":[{"iso_3166_1":"US","name":"United States of America"}],"release_date":"2012-09-20","revenue":33384127,"runtime":103,"spoken_languages":[{"english_name":"English","iso_639_1":"en","name":"English"}],"status":"Released","tagline":"We are infinite.","title":"The Perks of Being a Wallflower","video":false,"vote_average":7.803,"vote_count":11045,"credits":{"cast":[{"adult":false,"gender":2,"id":33235,"known_for_department":"Acting","name":"Logan Lerman","original_name":"Logan Lerman","popularity":2.7882,"profile_path":"/wEte1WtpzwNzhA9adib1rJGbTvb.jpg","cast_id":6,"character":"Charlie","credit_id":"52fe49169251416c910a23af","order":0},{"adult":false,"gender":1,"id":10990,"known_for_department":"Acting","name":"Emma Watson","original_name":"Emma Watson","popularity":8.9879,"profile_path":"/A14lLCZYDhfYdBa0fFRpwMDiwRN.jpg","cast_id":12,"character":"Sam","credit_id":"53173900c3a368136e0029f1","order":1},{"adult":false,"gender":3,"id":132157,"known_for_department":"Acting","name":"Ezra Miller","original_name":"Ezra Miller","popularity":3.2172,"profile_path":"/hLtxNK8eeWZkFSeaAASFWm15Qv0.jpg","cast_id":9,"character":"Patrick","credit_id":"52fe49169251416c910a23bb","order":2},{"adult":false,"gender":1,"id":52404,"known_for_department":"Acting","name":"Mae Whitman","original_name":"Mae Whitman","popularity":2.2888,"profile_path":"/x0DdzjoYN8K2PwjrnH3ogPYv2zo.jpg","cast_id":8,"character":"Mary Elizabeth","credit_id":"52fe49169251416c910a23b7","order":3},{"adult":false,"gender":1,"id":61114,"known_for_department":"Acting","name":"Kate Walsh","original_name":"Kate Walsh","popularity":2.0713,"profile_path":"/jRg4pmjvI2063YnQ6PJidclOc4L.jpg","cast_id":14,"character":"Mother","credit_id":"531739709251412ccd001076","order":4},{"adult":false,"gender":2,"id":32597,"known_for_department":"Acting","name":"Dylan McDermott","original_name":"Dylan McDermott","popularity":1.853,"profile_path":"/3i69RNLuL7KOD9pCmHQYruhcOdn.jpg","cast_id":32,"character":"Father","credit_id":"55992a9a9251413d96002e93","order":5},{"adult":false,"gender":1,"id":15091,"known_for_department":"Acting","name":"Melanie Lynskey","original_name":"Melanie Lynskey","popularity":2.084,"profile_path":"/kzrWI1sTgnA0H7TCIKzDOUtOW4n.jpg","cast_id":10,"character":"Aunt Helen","credit_id":"52fe49169251416c910a23bf","order":6},{"adult":false,"gender":1,"id":19961,"known_for_department":"Acting","name":"Nina Dobrev","original_name":"Nina Dobrev","popularity":3.3999,"profile_path":"/67A1s3I8k831MJ7VRLX59hBNdNt.jpg","cast_id":4,"character":"Candace","credit_id":"52fe49169251416c910a23a7","order":7},{"adult":false,"gender":2,"id":27104,"known_for_department":"Acting","name":"Johnny Simmons","original_name":"Johnny Simmons","popularity":1.0991,"profile_path":"/51LZiAAI3ZW4vabNOQnd3weYgUm.jpg","cast_id":13,"character":"Brad","credit_id":"5317393b9251415861001b5d","order":8},{"adult":false,"gender":1,"id":3234,"known_for_department":"Acting","name":"Joan Cusack","original_name":"Joan Cusack","popularity":2.3403,"profile_path":"/69cfjfZFjVxfu2QbngnXOkipcyn.jpg","cast_id":15,"character":"Dr. Burton","credit_id":"5317399f92514158a0001c3f","order":9},{"adult":false,"gender":2,"id":22226,"known_for_department":"Acting","name":"Paul Rudd","original_name":"Paul Rudd","popularity":2.8312,"profile_path":"/6jtwNOLKy0LdsRAKwZqgYMAfd5n.jpg","cast_id":7,"character":"Mr. Anderson","credit_id":"52fe49169251416c910a23b3","order":10},{"adult":false,"gender":2,"id":85139,"known_for_department":"Acting","name":"Nicholas Braun","original_name":"Nicholas Braun","popularity":1.5594,"profile_path":"/b2I6bZptuld3pjlVkYIy4DtMKGg.jpg","cast_id":18,"character":"Ponytail Derek","credit_id":"54984f1c9251417a810060e6","order":11},{"adult":false,"gender":2,"id":48463,"known_for_department":"Acting","name":"Reece Thompson","original_name":"Reece Thompson","popularity":0.5843,"profile_path":"/xA212pV7L7FcF6yxO24RGEOiHWb.jpg","cast_id":35,"character":"Craig","credit_id":"5905937492514169d0019096","order":12},{"adult":false,"gender":0,"id":1107313,"known_for_department":"Acting","name":"Patrick de Ledebur","original_name":"Patrick de Ledebur","popularity":0.5647,"profile_path":null,"cast_id":44,"character":"Senior Bully","credit_id":"5e718db88de0ae0013553923","order":13},{"adult":false,"gender":0,"id":227229,"known_for_department":"Acting","name":"Brian Balzerini","original_name":"Brian Balzerini","popularity":0.2854,"profile_path":null,"cast_id":45,"character":"Linebacker","credit_id":"5e718dee357c000016473a3a","order":14},{"adult":false,"gender":0,"id":2569643,"known_for_department":"Acting","name":"Tom Kruszewski","original_name":"Tom Kruszewski","popularity":0.1167,"profile_path":null,"cast_id":46,"character":"Nose Tackle","credit_id":"5e718e01b1f68d0014dbdf1b","order":15},{"adult":false,"gender":1,"id":936970,"known_for_department":"Acting","name":"Julia Garner","original_name":"Julia Garner","popularity":2.0298,"profile_path":"/ud1RXbvW70J89iqeic7no8olxvb.jpg","cast_id":17,"character":"Susan","credit_id":"5381f182c3a368737d00224e","order":16},{"adult":false,"gender":2,"id":11161,"known_for_department":"Costume & Make-Up","name":"Tom Savini","original_name":"Tom Savini","popularity":0.8798,"profile_path":"/zBYnzxzlAIEoanEU00bGYJmRS6k.jpg","cast_id":31,"character":"Mr. Callahan","credit_id":"559929fc9251413d96002e84","order":17},{"adult":false,"gender":0,"id":2569647,"known_for_department":"Acting","name":"Emily Marie Callaway","original_name":"Emily Marie Callaway","popularity":0.215,"profile_path":null,"cast_id":47,"character":"Mean Freshman Girl","credit_id":"5e718e808de0ae001a553d4a","order":18},{"adult":false,"gender":1,"id":1456334,"known_for_department":"Acting","name":"Chelsea Zhang","original_name":"Chelsea Zhang","popularity":0.2055,"profile_path":"/1bI5RT3LST8qDxkgBWH2M8AdZVW.jpg","cast_id":48,"character":"Shakespeare Girl","credit_id":"5e718ea9b1f68d0012dbb177","order":19},{"adult":false,"gender":0,"id":2569648,"known_for_department":"Acting","name":"Jesse Scheirer","original_name":"Jesse Scheirer","popularity":0.1367,"profile_path":null,"cast_id":49,"character":"Freshman Boy","credit_id":"5e718ec12f3b170014486f29","order":20},{"adult":false,"gender":0,"id":2569649,"known_for_department":"Acting","name":"Justine Nicole Schaefer","original_name":"Justine Nicole Schaefer","popularity":0.042,"profile_path":null,"cast_id":50,"character":"Twin Girl #1","credit_id":"5e718eeacabfe4001518b2a5","order":21},{"adult":false,"gender":0,"id":2569650,"known_for_department":"Acting","name":"Julie Marie Schaefer","original_name":"Julie Marie Schaefer","popularity":0.2808,"profile_path":null,"cast_id":51,"character":"Twin Girl #2","credit_id":"5e718efc357c00001347a50d","order":22},{"adult":false,"gender":0,"id":2569651,"known_for_department":"Acting","name":"Leo Miles Farmerie","original_name":"Leo Miles Farmerie","popularity":0.1415,"profile_path":null,"cast_id":52,"character":"7-Year-Old Charlie","credit_id":"5e718f54b1f68d0019dc2700","order":23},{"adult":false,"gender":0,"id":2569652,"known_for_department":"Acting","name":"Isabel Muschweck","original_name":"Isabel Muschweck","popularity":0.0984,"profile_path":null,"cast_id":53,"character":"9-Year-Old Candace","credit_id":"5e718f658de0ae0017556b09","order":24},{"adult":false,"gender":2,"id":1517251,"known_for_department":"Acting","name":"Adam Hagenbuch","original_name":"Adam Hagenbuch","popularity":1.2086,"profile_path":"/42CrwdFz2VuFHxzxUTIaHGnjltN.jpg","cast_id":34,"character":"Bob","credit_id":"5905936692514169c80195dc","order":25},{"adult":false,"gender":1,"id":1053419,"known_for_department":"Acting","name":"Erin Wilhelmi","original_name":"Erin Wilhelmi","popularity":1.1874,"profile_path":"/nj3nWlDsWbVIJiFeL8TB0VwYkDE.jpg","cast_id":16,"character":"Alice","credit_id":"531739c992514158a0001c42","order":26},{"adult":false,"gender":0,"id":2563015,"known_for_department":"Directing","name":"Jordan Paley","original_name":"Jordan Paley","popularity":0.4119,"profile_path":null,"cast_id":54,"character":"Rocky MC","credit_id":"5e71905bcabfe4001518bbad","order":27},{"adult":false,"gender":2,"id":987572,"known_for_department":"Acting","name":"Zane Holtz","original_name":"Zane Holtz","popularity":0.7137,"profile_path":"/z5sXU2zOR59A3kfJTgwPObgrPhf.jpg","cast_id":55,"character":"Chris","credit_id":"5e71906f357c00001947a831","order":28},{"adult":false,"gender":0,"id":2569659,"known_for_department":"Acting","name":"Timothy Breslin","original_name":"Timothy Breslin","popularity":0.0652,"profile_path":null,"cast_id":56,"character":"Policeman","credit_id":"5e719086357c00001947a865","order":29},{"adult":false,"gender":2,"id":1432326,"known_for_department":"Acting","name":"Mark McClain Wilson","original_name":"Mark McClain Wilson","popularity":0.1199,"profile_path":"/dvReWjPgr8Y8B6iZv9NiBDYuAJl.jpg","cast_id":57,"character":"Emergency Room Policeman","credit_id":"5e71909cf9aa470013cfdacf","order":30},{"adult":false,"gender":2,"id":1129400,"known_for_department":"Acting","name":"Atticus Cain","original_name":"Atticus Cain","popularity":0.7613,"profile_path":"/zTMpCXOND4zxca2pOEncm2UfmNO.jpg","cast_id":58,"character":"Emergency Room Doctor","credit_id":"5e7190b2b1f68d0012dbc391","order":31},{"adult":false,"gender":1,"id":120356,"known_for_department":"Acting","name":"Stacy Chbosky","original_name":"Stacy Chbosky","popularity":0.4934,"profile_path":"/m4A1RZc8GelxBufn9Ehqv7VcKZh.jpg","cast_id":59,"character":"Young Mom","credit_id":"5e7190c18de0ae0017556cfc","order":32},{"adult":false,"gender":2,"id":207069,"known_for_department":"Acting","name":"Dihlon McManne","original_name":"Dihlon McManne","popularity":0.6182,"profile_path":null,"cast_id":60,"character":"Priest","credit_id":"5e7190d12f3b17001148ab92","order":33},{"adult":false,"gender":0,"id":1537118,"known_for_department":"Acting","name":"Laurie Klatscher","original_name":"Laurie Klatscher","popularity":0.5801,"profile_path":null,"cast_id":61,"character":"School Principal","credit_id":"5e7190ec8de0ae0013553c93","order":34},{"adult":false,"gender":2,"id":1231874,"known_for_department":"Acting","name":"Landon Pigg","original_name":"Landon Pigg","popularity":0.4364,"profile_path":"/705aTqQe4FMswVAcIPOZZQ6JNnT.jpg","cast_id":62,"character":"Peter","credit_id":"5e7190fe357c00001347a7d5","order":35},{"adult":false,"gender":1,"id":1555395,"known_for_department":"Acting","name":"Jennifer Enskat","original_name":"Jennifer Enskat","popularity":0.7623,"profile_path":"/aibiuaBXq9W65QgEUz0ZcJHjfKW.jpg","cast_id":63,"character":"Sam's Mom","credit_id":"5e719113f9aa470015cf85d8","order":36},{"adult":false,"gender":2,"id":1605510,"known_for_department":"Acting","name":"William L. Thomas","original_name":"William L. Thomas","popularity":0.1091,"profile_path":"/rNhe83bSwTvHtYHTMufUkmunBDK.jpg","cast_id":64,"character":"Patrick's Dad","credit_id":"5e7191292f3b17001148ac67","order":37},{"adult":false,"gender":1,"id":1205752,"known_for_department":"Acting","name":"Morgan Wolk","original_name":"Morgan Wolk","popularity":1.6609,"profile_path":"/pdQiuPTihAm6NERSDcqaxjQ1ULs.jpg","cast_id":65,"character":"Candace's Friend","credit_id":"5e71914bcabfe4001518bcfb","order":38},{"adult":false,"gender":2,"id":1543874,"known_for_department":"Acting","name":"Joe Fishel","original_name":"Joe Fishel","popularity":0.9006,"profile_path":"/Aapim9EAy3vY8eeS4n4tnF4iHsY.jpg","cast_id":108,"character":"Father of Twin Girls (uncredited)","credit_id":"65d4cf0eb9a0bd0186684338","order":39}],"crew":[{"adult":false,"gender":2,"id":19311,"known_for_department":"Writing","name":"Stephen Chbosky","original_name":"Stephen Chbosky","popularity":0.6439,"profile_path":"/9PdTBjn8dJqfn3ygKvsCdl9G06J.jpg","credit_id":"52fe49169251416c910a2397","department":"Directing","job":"Director"},{"adult":false,"gender":2,"id":19311,"known_for_department":"Writing","name":"Stephen Chbosky","original_name":"Stephen Chbosky","popularity":0.6439,"profile_path":"/9PdTBjn8dJqfn3ygKvsCdl9G06J.jpg","credit_id":"52fe49169251416c910a23a3","department":"Writing","job":"Screenplay"},{"adult":false,"gender":1,"id":1329415,"known_for_department":"Crew","name":"Samantha MacIvor","original_name":"Samantha MacIvor","popularity":0.565,"profile_path":"/ztH9qxR7mknj8Rs2MCNhayA7n5l.jpg","credit_id":"64ee96e9caa50800c885cda5","department":"Crew","job":"Stunts"},{"adult":false,"gender":2,"id":6949,"known_for_department":"Acting","name":"John Malkovich","original_name":"John Malkovich","popularity":2.8206,"profile_path":"/7GoOdGNc4ra1L0F5nJTkmIB37iu.jpg","credit_id":"531738bcc3a36813a60028a2","department":"Production","job":"Producer"},{"adult":false,"gender":2,"id":19016,"known_for_department":"Sound","name":"Michael Brook","original_name":"Michael Brook","popularity":0.217,"profile_path":"/PbHAuMBPkxtgyEmV6d69BTj2fZ.jpg","credit_id":"553108b7c3a368412100215a","department":"Sound","job":"Original Music Composer"},{"adult":false,"gender":1,"id":15350,"known_for_department":"Editing","name":"Mary Jo Markey","original_name":"Mary Jo Markey","popularity":0.5209,"profile_path":"/lfGOI3S9JModCwIgqsYBB3kP2NV.jpg","credit_id":"55310807c3a3680a94001eb8","department":"Editing","job":"Editor"},{"adult":false,"gender":2,"id":8846,"known_for_department":"Camera","name":"Andrew Dunn","original_name":"Andrew Dunn","popularity":0.7751,"profile_path":"/j7ShQfHmJtz0Pw2tMpYEvSKkqGL.jpg","credit_id":"5531084ec3a3684112002224","department":"Camera","job":"Director of Photography"},{"adult":false,"gender":1,"id":52445,"known_for_department":"Production","name":"Lianne Halfon","original_name":"Lianne Halfon","popularity":0.2749,"profile_path":null,"credit_id":"553109edc3a3680f420017f8","department":"Production","job":"Producer"},{"adult":false,"gender":2,"id":19311,"known_for_department":"Writing","name":"Stephen Chbosky","original_name":"Stephen Chbosky","popularity":0.6439,"profile_path":"/9PdTBjn8dJqfn3ygKvsCdl9G06J.jpg","credit_id":"55310a53c3a36841120022b7","department":"Production","job":"Executive Producer"},{"adult":false,"gender":1,"id":1193617,"known_for_department":"Art","name":"Inbal Weinberg","original_name":"Inbal Weinberg","popularity":1.6401,"profile_path":"/6vXjNLJJLjCnkd1IEMjuH92zVFT.jpg","credit_id":"553109d1c3a3680a94001f3b","department":"Art","job":"Production Design"},{"adult":false,"gender":2,"id":52897,"known_for_department":"Production","name":"Russell Smith","original_name":"Russell Smith","popularity":0.1128,"profile_path":null,"credit_id":"55310a1bc3a3680f42001807","department":"Production","job":"Producer"},{"adult":false,"gender":2,"id":82132,"known_for_department":"Production","name":"James Powers","original_name":"James Powers","popularity":0.4909,"profile_path":null,"credit_id":"55310f8b9251410675000001","department":"Production","job":"Executive Producer"},{"adult":false,"gender":1,"id":1015922,"known_for_department":"Directing","name":"Diane Hassinger Newman","original_name":"Diane Hassinger Newman","popularity":0.5791,"profile_path":null,"credit_id":"5532163cc3a36848ca000ea9","department":"Directing","job":"Script Supervisor"},{"adult":false,"gender":1,"id":39123,"known_for_department":"Production","name":"Venus Kanani","original_name":"Venus Kanani","popularity":0.6783,"profile_path":"/lXCI0CoU6FvJqKcHN32Yq6ilbcP.jpg","credit_id":"553215e4c3a368222a00202f","department":"Production","job":"Casting"},{"adult":false,"gender":1,"id":5914,"known_for_department":"Production","name":"Mary Vernieu","original_name":"Mary Vernieu","popularity":0.9105,"profile_path":"/z37Cmn0MJdWCSC8ydkyoseiYUYk.jpg","credit_id":"5532158c92514163100017f2","department":"Production","job":"Casting"},{"adult":false,"gender":1,"id":1521494,"known_for_department":"Costume & Make-Up","name":"Diane Collins","original_name":"Diane Collins","popularity":0.4266,"profile_path":null,"credit_id":"637b10c5336e010082e206c1","department":"Costume & Make-Up","job":"Costume Supervisor"},{"adult":false,"gender":0,"id":2773778,"known_for_department":"Art","name":"Aaron Streiner","original_name":"Aaron Streiner","popularity":0.1377,"profile_path":null,"credit_id":"637b11372cde980075ad58af","department":"Art","job":"Set Dresser"},{"adult":false,"gender":2,"id":1432038,"known_for_department":"Visual Effects","name":"Phillip Hoffman","original_name":"Phillip Hoffman","popularity":0.4458,"profile_path":null,"credit_id":"637b10e82cde9800cc15e789","department":"Visual Effects","job":"Visual Effects Producer"},{"adult":false,"gender":0,"id":1395032,"known_for_department":"Costume & Make-Up","name":"Amanda Jenkins","original_name":"Amanda Jenkins","popularity":0.2001,"profile_path":null,"credit_id":"637b10b45b2f4700d58382a6","department":"Costume & Make-Up","job":"Set Costumer"},{"adult":false,"gender":2,"id":1316448,"known_for_department":"Art","name":"Thomas F. Kelly","original_name":"Thomas F. Kelly","popularity":0.2431,"profile_path":null,"credit_id":"637b1111156cc7009435f2a0","department":"Art","job":"Set Dresser"},{"adult":false,"gender":1,"id":2053858,"known_for_department":"Costume & Make-Up","name":"Melanie Marie Evans","original_name":"Melanie Marie Evans","popularity":0.3429,"profile_path":null,"credit_id":"637b10cf156cc7009435f27c","department":"Costume & Make-Up","job":"Set Costumer"},{"adult":false,"gender":0,"id":2773759,"known_for_department":"Art","name":"Eugene Doyle","original_name":"Eugene Doyle","popularity":0.2853,"profile_path":null,"credit_id":"637b11065b2f47009ba9f7a0","department":"Art","job":"Set Dresser"},{"adult":false,"gender":1,"id":1145972,"known_for_department":"Art","name":"Merissa Lombardo","original_name":"Merissa Lombardo","popularity":0.4407,"profile_path":null,"credit_id":"5e7192d52f3b170017488727","department":"Art","job":"Set Decoration"},{"adult":false,"gender":2,"id":1371064,"known_for_department":"Sound","name":"Gregg Barbanell","original_name":"Gregg Barbanell","popularity":0.4921,"profile_path":null,"credit_id":"5e7194b9f9aa470019cfb740","department":"Sound","job":"Foley Artist"},{"adult":false,"gender":0,"id":1557612,"known_for_department":"Sound","name":"Jeffree Bloomer","original_name":"Jeffree Bloomer","popularity":0.2458,"profile_path":null,"credit_id":"5e7192318de0ae001a554255","department":"Sound","job":"Sound Mixer"},{"adult":false,"gender":0,"id":1636644,"known_for_department":"Art","name":"Christina Myal","original_name":"Christina Myal","popularity":0.1342,"profile_path":null,"credit_id":"5e7192eccabfe4001318eafe","department":"Art","job":"Graphic Designer"},{"adult":false,"gender":0,"id":2034507,"known_for_department":"Costume & Make-Up","name":"Patty Bell","original_name":"Patty Bell","popularity":0.1535,"profile_path":null,"credit_id":"5e71931ecabfe4001118e767","department":"Costume & Make-Up","job":"Key Makeup Artist"},{"adult":false,"gender":1,"id":1535770,"known_for_department":"Production","name":"Natalie Angel","original_name":"Natalie Angel","popularity":0.3398,"profile_path":null,"credit_id":"5e719379b1f68d0019dc2c66","department":"Production","job":"Production Coordinator"},{"adult":false,"gender":0,"id":1544667,"known_for_department":"Costume & Make-Up","name":"Nancy Keslar","original_name":"Nancy Keslar","popularity":0.2894,"profile_path":null,"credit_id":"5e719348cabfe4001518c232","department":"Costume & Make-Up","job":"Key Hair Stylist"},{"adult":false,"gender":2,"id":82132,"known_for_department":"Production","name":"James Powers","original_name":"James Powers","popularity":0.4909,"profile_path":null,"credit_id":"5e718d2ef9aa470019cfa511","department":"Production","job":"Unit Production Manager"},{"adult":false,"gender":0,"id":2441835,"known_for_department":"Directing","name":"Susan Ransom-Coyle","original_name":"Susan Ransom-Coyle","popularity":0.3107,"profile_path":null,"credit_id":"5e718d668de0ae001a553b8b","department":"Directing","job":"Second Assistant Director"},{"adult":false,"gender":2,"id":162522,"known_for_department":"Crew","name":"Blaise Corrigan","original_name":"Blaise Corrigan","popularity":0.4892,"profile_path":"/ycsMR3gck2D8jJT6QEtzyrjUmC6.jpg","credit_id":"5e7191902f3b17001948f8df","department":"Crew","job":"Stunt Coordinator"},{"adult":false,"gender":2,"id":1319968,"known_for_department":"Crew","name":"Eric Bergman","original_name":"Eric Bergman","popularity":1.0646,"profile_path":null,"credit_id":"5e7193cacabfe4001318ec99","department":"Crew","job":"Post Production Supervisor"},{"adult":false,"gender":0,"id":1191813,"known_for_department":"Sound","name":"Noel Vought","original_name":"Noel Vought","popularity":0.5387,"profile_path":null,"credit_id":"5e71947ef9aa470015cf8c87","department":"Sound","job":"Foley Artist"},{"adult":false,"gender":2,"id":1406389,"known_for_department":"Sound","name":"Bruce Tanis","original_name":"Bruce Tanis","popularity":1.1468,"profile_path":null,"credit_id":"5e71946a8de0ae001a5548c6","department":"Sound","job":"Sound Editor"},{"adult":false,"gender":2,"id":1608764,"known_for_department":"Lighting","name":"Patrick Murray","original_name":"Patrick Murray","popularity":0.5066,"profile_path":null,"credit_id":"5e719254b1f68d0019dc2a99","department":"Lighting","job":"Gaffer"},{"adult":false,"gender":1,"id":1538148,"known_for_department":"Sound","name":"Alexandra Patsavas","original_name":"Alexandra Patsavas","popularity":0.5455,"profile_path":"/R9rHK6zVup8IXTO6gNgQ1JZ3nR.jpg","credit_id":"5e718c8e357c0000164738a1","department":"Sound","job":"Music Supervisor"},{"adult":false,"gender":2,"id":1391389,"known_for_department":"Camera","name":"John Bramley","original_name":"John Bramley","popularity":0.6375,"profile_path":null,"credit_id":"5e7191f9f9aa470015cf87c7","department":"Camera","job":"Still Photographer"},{"adult":false,"gender":0,"id":1545922,"known_for_department":"Sound","name":"Trevor Metz","original_name":"Trevor Metz","popularity":0.4125,"profile_path":null,"credit_id":"5e7194588de0ae001a55489f","department":"Sound","job":"Sound Editor"},{"adult":false,"gender":0,"id":2569663,"known_for_department":"Production","name":"Chris Gary","original_name":"Chris Gary","popularity":0.7371,"profile_path":"/pLhe9DdNytnega9dqLEKIcNkw1t.jpg","credit_id":"5e7191c8cabfe4001518bf30","department":"Production","job":"Associate Producer"},{"adult":false,"gender":2,"id":1564580,"known_for_department":"Sound","name":"Anthony Cargioli","original_name":"Anthony Cargioli","popularity":0.2092,"profile_path":null,"credit_id":"5e7192408de0ae001a554267","department":"Sound","job":"Boom Operator"},{"adult":false,"gender":0,"id":2569667,"known_for_department":"Sound","name":"Richard Dawn","original_name":"Richard Dawn","popularity":0.1517,"profile_path":null,"credit_id":"5e719446f9aa470017cfe307","department":"Sound","job":"Sound Editor"},{"adult":false,"gender":0,"id":1897204,"known_for_department":"Art","name":"Pete Dancy","original_name":"Pete Dancy","popularity":0.3924,"profile_path":null,"credit_id":"5e71928c2f3b17001748864e","department":"Art","job":"Property Master"},{"adult":false,"gender":2,"id":1533589,"known_for_department":"Visual Effects","name":"Russell Tyrrell","original_name":"Russell Tyrrell","popularity":0.0983,"profile_path":null,"credit_id":"5e7193a4f9aa470019cfb5cd","department":"Crew","job":"Special Effects Coordinator"},{"adult":false,"gender":2,"id":1367667,"known_for_department":"Sound","name":"Perry Robertson","original_name":"Perry Robertson","popularity":0.6849,"profile_path":null,"credit_id":"5e7193e7357c00001947ad06","department":"Sound","job":"Supervising Sound Editor"},{"adult":false,"gender":2,"id":1423757,"known_for_department":"Sound","name":"Scott Sanders","original_name":"Scott Sanders","popularity":0.4203,"profile_path":null,"credit_id":"5e7193fd8de0ae001a55480d","department":"Sound","job":"Supervising Sound Editor"},{"adult":false,"gender":2,"id":4190,"known_for_department":"Costume & Make-Up","name":"David C. Robinson","original_name":"David C. Robinson","popularity":0.4954,"profile_path":null,"credit_id":"5e718ca98de0ae001755663b","department":"Costume & Make-Up","job":"Costume Designer"},{"adult":false,"gender":0,"id":1208355,"known_for_department":"Art","name":"Gregory A. Weimerskirch","original_name":"Gregory A. Weimerskirch","popularity":0.4659,"profile_path":null,"credit_id":"5e7192b7f9aa470015cf8927","department":"Art","job":"Art Direction"},{"adult":false,"gender":1,"id":1328146,"known_for_department":"Costume & Make-Up","name":"Evelyne Noraz","original_name":"Evelyne Noraz","popularity":0.4285,"profile_path":"/vWlfd0cO6zotDVKxFxSJQTFb8yP.jpg","credit_id":"5e71930dcabfe4001118e747","department":"Costume & Make-Up","job":"Makeup Department Head"},{"adult":false,"gender":0,"id":1852975,"known_for_department":"Production","name":"Gillian Brown","original_name":"Gillian Brown","popularity":0.3992,"profile_path":null,"credit_id":"5e7191b1cabfe4001518befb","department":"Production","job":"Co-Producer"},{"adult":false,"gender":2,"id":40119,"known_for_department":"Camera","name":"Keith Seymour","original_name":"Keith Seymour","popularity":0.1726,"profile_path":null,"credit_id":"5e71926f357c00001347aacb","department":"Camera","job":"Key Grip"},{"adult":false,"gender":1,"id":1406893,"known_for_department":"Costume & Make-Up","name":"Suzy Mazzarese-Allison","original_name":"Suzy Mazzarese-Allison","popularity":0.2112,"profile_path":null,"credit_id":"5e719338cabfe4001318ebbc","department":"Costume & Make-Up","job":"Hair Department Head"},{"adult":false,"gender":2,"id":1010751,"known_for_department":"Sound","name":"Joe Barnett","original_name":"Joe Barnett","popularity":0.6252,"profile_path":"/sA1hottTzB3uCz3jtYTURZkE0gM.jpg","credit_id":"5e719410357c000011475a2b","department":"Sound","job":"Sound Re-Recording Mixer"},{"adult":false,"gender":0,"id":1825167,"known_for_department":"Directing","name":"Chip Signore","original_name":"Chip Signore","popularity":0.6752,"profile_path":null,"credit_id":"5e718d49f9aa470019cfa54c","department":"Directing","job":"First Assistant Director"},{"adult":false,"gender":2,"id":1931337,"known_for_department":"Production","name":"Shawn Boyachek","original_name":"Shawn Boyachek","popularity":0.1506,"profile_path":null,"credit_id":"5e71929fb1f68d0012dbcadf","department":"Production","job":"Location Manager"},{"adult":false,"gender":0,"id":1432039,"known_for_department":"Visual Effects","name":"Adam Avitabile","original_name":"Adam Avitabile","popularity":0.2606,"profile_path":null,"credit_id":"5e7194ecf9aa470015cf8db6","department":"Visual Effects","job":"Visual Effects Supervisor"},{"adult":false,"gender":0,"id":1417005,"known_for_department":"Production","name":"Janice F. Sperling","original_name":"Janice F. Sperling","popularity":0.1941,"profile_path":null,"credit_id":"5e71935ecabfe4001518c260","department":"Production","job":"Production Supervisor"},{"adult":false,"gender":0,"id":2569665,"known_for_department":"Production","name":"Ava Dellaira","original_name":"Ava Dellaira","popularity":0.103,"profile_path":null,"credit_id":"5e7191e1cabfe4001318e916","department":"Production","job":"Associate Producer"},{"adult":false,"gender":0,"id":1402111,"known_for_department":"Sound","name":"Marshall Garlington","original_name":"Marshall Garlington","popularity":0.2226,"profile_path":null,"credit_id":"5e7194258de0ae0013553f92","department":"Sound","job":"Sound Re-Recording Mixer"},{"adult":false,"gender":2,"id":1114134,"known_for_department":"Acting","name":"Andy Partridge","original_name":"Andy Partridge","popularity":0.4647,"profile_path":null,"credit_id":"68d0685cca7c17934aac831a","department":"Sound","job":"Songs"},{"adult":false,"gender":2,"id":19311,"known_for_department":"Writing","name":"Stephen Chbosky","original_name":"Stephen Chbosky","popularity":0.6439,"profile_path":"/9PdTBjn8dJqfn3ygKvsCdl9G06J.jpg","credit_id":"698736166c26679a2ff80912","department":"Writing","job":"Book"}]},"videos":{"results":[{"iso_639_1":"en","iso_3166_1":"US","name":"'Charlie Takes One Last Ride' Scene | The Perks of Being a Wallflower","key":"jyneTS1B854","site":"YouTube","size":1080,"type":"Clip","official":true,"published_at":"2024-09-11T16:00:48.000Z","id":"66e1cc6704f4dd348c45624c"},{"iso_639_1":"en","iso_3166_1":"US","name":"'We Accept the Love We Think We Deserve' Scene | The Perks of Being a Wallflower","key":"AgUDpwAhwWg","site":"YouTube","size":1080,"type":"Clip","official":true,"published_at":"2024-09-10T16:00:10.000Z","id":"66e11c79c3ff8f970708144c"},{"iso_639_1":"en","iso_3166_1":"US","name":"'Patrick & Charlie's Deep Conversation' Scene | The Perks of Being a Wallflower","key":"dbgf-5kPhEY","site":"YouTube","size":1080,"type":"Clip","official":true,"published_at":"2024-09-09T16:00:30.000Z","id":"66e11c714f3a71968054e156"},{"iso_639_1":"en","iso_3166_1":"US","name":"'Charlie Kisses the Prettiest Girl in the Room' Scene | The Perks of Being a Wallflower","key":"GD6uSOrq7iY","site":"YouTube","size":1080,"type":"Clip","official":true,"published_at":"2024-09-08T16:00:10.000Z","id":"66e11c6a16773d46c95f00d7"},{"iso_639_1":"en","iso_3166_1":"US","name":"'Sam & Charlie Perform at The Rocky Horror Picture Show' Scene | The Perks of Being A Wallflower","key":"vt7sUtJAK2c","site":"YouTube","size":1080,"type":"Clip","official":true,"published_at":"2024-09-07T16:01:01.000Z","id":"66e11c612c98375fa1051569"},{"iso_639_1":"en","iso_3166_1":"US","name":"'Sam Helps Charlie Through a Bad Trip' Scene | The Perks of Being a Wallflower","key":"ebhHsFO4mts","site":"YouTube","size":1080,"type":"Clip","official":true,"published_at":"2024-09-06T16:00:42.000Z","id":"66e11c4d79ea57072f8fd1b3"},{"iso_639_1":"en","iso_3166_1":"US","name":"'Charlie & Sam's First Kiss' Scene | The Perks of Being a Wallflower","key":"sehE3hKxwoM","site":"YouTube","size":1080,"type":"Clip","official":true,"published_at":"2024-09-05T16:00:49.000Z","id":"66e11c4300000000004c9022"},{"iso_639_1":"en","iso_3166_1":"US","name":"'Driving Through the Tunnel' Scene | The Perks of Being a Wallflower","key":"avqZ2UMbc7Q","site":"YouTube","size":1080,"type":"Clip","official":true,"published_at":"2024-09-04T16:00:03.000Z","id":"66e11c354d6a14993435c3d2"},{"iso_639_1":"en","iso_3166_1":"US","name":"'The Homecoming Dance' Scene | The Perks of Being a Wallflower","key":"Y307eLrOcec","site":"YouTube","size":1080,"type":"Clip","official":true,"published_at":"2024-09-03T16:00:24.000Z","id":"66e11c2e000000000022a8f7"},{"iso_639_1":"en","iso_3166_1":"US","name":"'Charlie Meets Sam at the Football Game' Scene | The Perks of Being a Wallflower","key":"GyI_LYPoZAU","site":"YouTube","size":1080,"type":"Clip","official":true,"published_at":"2024-09-02T16:00:21.000Z","id":"66e11c2830b2e5c7af8fcfcf"},{"iso_639_1":"en","iso_3166_1":"US","name":"The First 10 Minutes of The Perks of Being a Wallflower (2012)","key":"qypWtUeaD40","site":"YouTube","size":1080,"type":"Clip","official":true,"published_at":"2024-09-01T16:00:51.000Z","id":"66e11c2179ea57072f8fd1a5"},{"iso_639_1":"en","iso_3166_1":"US","name":"DVD/BD Trailer","key":"x0nTfbg24Qs","site":"YouTube","size":720,"type":"Trailer","official":true,"published_at":"2013-01-04T19:56:58.000Z","id":"66e11b8b00000000004c9289"},{"iso_639_1":"en","iso_3166_1":"US","name":"The Perks of Love -- Perks of Being a Wallflower 2012","key":"sSlGwcqGO3g","site":"YouTube","size":480,"type":"Teaser","official":true,"published_at":"2012-11-21T03:06:25.000Z","id":"66e11bc6000000000022a834"},{"iso_639_1":"en","iso_3166_1":"US","name":"The Perks of Being a Wallflower - Stephen Chbosky Q&A","key":"gyvUnM3hj_I","site":"YouTube","size":1080,"type":"Featurette","official":true,"published_at":"2012-10-12T18:34:39.000Z","id":"66e11d795142e627648b2bf9"},{"iso_639_1":"en","iso_3166_1":"US","name":"THE PERKS OF BEING A WALLFLOWER - TV Spot \"Master Review\"","key":"eZkoZPEUZG0","site":"YouTube","size":1080,"type":"Teaser","official":true,"published_at":"2012-10-11T01:00:40.000Z","id":"66e11d5e00000000004c9357"},{"iso_639_1":"en","iso_3166_1":"US","name":"The Perks of Being A Wallflower (2012) Official Roundtable \"First Impressions\"","key":"iDjo_jnr4Xs","site":"YouTube","size":1080,"type":"Featurette","official":true,"published_at":"2012-10-05T20:38:01.000Z","id":"66e11d88d7270e37d3afea19"},{"iso_639_1":"en","iso_3166_1":"US","name":"Academy Conversations: The Perks of Being a Wallflower","key":"RffYip5VWbQ","site":"YouTube","size":720,"type":"Featurette","official":true,"published_at":"2012-10-02T22:37:08.000Z","id":"66e11da2f370a2eda254e222"},{"iso_639_1":"en","iso_3166_1":"US","name":"THE PERKS OF BEING A WALLFLOWER - TV Spot \"Review\"","key":"jpM4WApoy78","site":"YouTube","size":1080,"type":"Teaser","official":true,"published_at":"2012-09-20T21:56:18.000Z","id":"66e11cc413d104edad36a725"},{"iso_639_1":"en","iso_3166_1":"US","name":"The Perks Of Being A Wallflower (2012) Official BTS \"Cast & Filmmaker Chat\"","key":"LrUnp17yVM8","site":"YouTube","size":1080,"type":"Behind the Scenes","official":true,"published_at":"2012-06-22T23:24:18.000Z","id":"66e11ccd5a7474fa8c8fcfb9"},{"iso_639_1":"en","iso_3166_1":"US","name":"Trailer","key":"QE7CGX1d6LU","published_at":"2012-06-04T19:11:51.000Z","site":"YouTube","size":1080,"type":"Trailer","official":true,"id":"533ec6d1c3a368544800796d"}]},"external_ids":{"imdb_id":"tt1659337","wikidata_id":"Q675468","facebook_id":"WallflowerMovie","instagram_id":"perksmovie","twitter_id":"WallflowerMovie"}}"#;

}
