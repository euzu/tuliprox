use crate::library::metadata::MediaMetadata;
use crate::library::tmdb::{TmdbMovieDetails, TmdbSearchResponse, TmdbSeriesInfoDetails, TmdbSeriesInfoSeasonDetails, TmdbTvSearchResponse};
use crate::library::MetadataStorage;
use log::{debug, error, warn};
use serde_json::Value;
use std::collections::HashSet;
use tokio::time::{sleep, Duration};
use url::Url;

// TODO make this configurable in Library tmdb config
const TMDB_API_BASE_URL: &str = "https://api.themoviedb.org/3";

// TMDB API client with rate limiting
pub struct TmdbClient {
    api_key: String,
    client: reqwest::Client,
    rate_limit_ms: u64,
    storage: MetadataStorage,
    fetched_movie_metadata: tokio::sync::RwLock<HashSet<u32>>,
    fetched_series_metadata: tokio::sync::RwLock<HashSet<u32>>,
    fetched_series_key: tokio::sync::RwLock<HashSet<String>>,
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
        }
    }

    // Searches for a movie by title and optional year
    pub async fn search_movie(&self, tmdb_id: Option<u32>, title: &str, year: Option<u32>) -> Result<Option<MediaMetadata>, String> {
        let year_display = year.map_or_else(|| "None".to_string(), |y| y.to_string());
        debug!("TMDB search movie: {title} ({year_display})");

        if let Some(movie_id) = tmdb_id {
            return self.fetch_movie_details(movie_id).await;
        }

        sleep(Duration::from_millis(self.rate_limit_ms)).await;

        let url = self.build_movie_search_url(title, year)?;
        let response = self.client.get(url).send().await.map_err(|e| format!("TMDB API request failed: {e}"))?;
        if !response.status().is_success() {
            error!("TMDB API error: {}", response.status());
            return Err(format!("TMDB API error: {}", response.status()));
        }

        let search = response.json::<TmdbSearchResponse>().await.map_err(|e| format!("Failed to parse TMDB search response: {e}"))?;

        if let Some(movie) = search.results.first() { self.fetch_movie_details(movie.id).await } else {
            debug!("No TMDB results for movie: {title}");
            Ok(None)
        }
    }

    fn build_movie_search_url(&self, title: &str, year: Option<u32>) -> Result<Url, String> {
        let mut url = Url::parse(&format!("{TMDB_API_BASE_URL}/search/movie")).map_err(|e| format!("Failed to parse URL for TMDB movie search: {e}"))?;
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
        if self.fetched_movie_metadata.read().await.contains(&movie_id) {
            return Ok(None);
        }

        sleep(Duration::from_millis(self.rate_limit_ms)).await;
        let url = format!("{TMDB_API_BASE_URL}/movie/{movie_id}?api_key={}&append_to_response=credits,videos,external_ids", self.api_key);
        let response = self.client.get(&url).send().await.map_err(|err| err.to_string())?;
        if !response.status().is_success() {
            warn!("TMDB API error fetching movie details: {}", response.status());
            return Err(format!("TMDB API error fetching movie details: {}", response.status()));
        }

        let content_bytes = response.bytes().await.map_err(|err| err.to_string())?;

        // Attempt to store the raw data, but don't fail the operation if storage fails
        if let Err(err) = self.storage.store_tmdb_movie_info(movie_id, &content_bytes).await {
            warn!("Failed to cache TMDB movie info: {err}");
        }

        let details: TmdbMovieDetails = serde_json::from_slice(&content_bytes).map_err(|err| err.to_string())?;
        self.fetched_movie_metadata.write().await.insert(movie_id);

        Ok(Some(MediaMetadata::Movie(details.to_meta_data())))
    }


    // Searches for a TV series by title and optional year
    pub async fn search_series(&self, tmdb_id: Option<u32>, title: &str, year: Option<u32>) -> Result<Option<MediaMetadata>, String> {
        debug!("Searching TMDB for series: {title}");

        let key = format!("{title}-{tmdb_id:?}-{year:?}");

        if self.fetched_series_key.read().await.contains(&key) {
            return Ok(None);
        }

        let result = if let Some(series_id) = tmdb_id {
            self.fetch_series_details(series_id).await
        } else {
            sleep(Duration::from_millis(self.rate_limit_ms)).await;
            self.search_series_by_title(title, year).await
        };

        if result.as_ref().is_ok_and(Option::is_some) {
            self.fetched_series_key.write().await.insert(key);
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

        debug!("TMDB search series: {title}");

        let response = self.client
            .get(url)
            .send()
            .await
            .map_err(|e| format!("TMDB API request failed: {e}"))?;

        if !response.status().is_success() {
            return Err(format!("TMDB API error: {}", response.status()));
        }

        let search = response
            .json::<TmdbTvSearchResponse>()
            .await
            .map_err(|e| format!("Failed to parse TMDB TV search response: {e}"))?;

        if let Some(series) = search.results.first() { self.fetch_series_details(series.id).await } else {
            debug!("No TMDB results for series: {title}");
            Ok(None)
        }
    }

    // Fetches detailed TV series information
    pub async fn fetch_series_details(&self, series_id: u32) -> Result<Option<MediaMetadata>, String> {
        // Skip if metadata already fetched
        if self.fetched_series_metadata.read().await.contains(&series_id) {
            return Ok(None);
        }

        // Apply rate limit
        sleep(Duration::from_millis(self.rate_limit_ms)).await;

        // Fetch series info from TMDB API
        let url = format!(
            "{TMDB_API_BASE_URL}/tv/{series_id}?api_key={}&append_to_response=credits,videos,external_ids",
            self.api_key
        );
        let response = self.client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("TMDB API request failed: {e}"))?;

        if !response.status().is_success() {
            error!("TMDB API error fetching series details: {}", response.status());
            return Err(format!("TMDB API error fetching series details: {}", response.status()));
        }

        // Read response bytes
        let series_content = response
            .bytes()
            .await
            .map_err(|e| format!("Failed to read TMDB response body: {e}"))?;

        // Mark series as fetched
        self.fetched_series_metadata.write().await.insert(series_id);

        // Deserialize TMDB series info into struct
        let mut series: TmdbSeriesInfoDetails = serde_json::from_slice(&series_content)
            .map_err(|e| format!("Failed to parse TMDB series details: {e}"))?;

        // Determine number of seasons
        let season_count = Self::detect_season_count(&series);

        if season_count > 0 {
            // Fetch season details
            let season_infos = self.fetch_seasons(series_id, season_count).await;
            if !season_infos.is_empty() {
                // Deserialize raw JSON map to update dynamically
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
                                if let Ok(raw_season_details_json) = serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(raw_season_details_content.as_ref()) {
                                    if let Some(Value::Array(series_season_list)) = raw_series.get_mut("seasons") {
                                        for series_season_item in series_season_list {
                                            if let Value::Object(season_item_obj) = series_season_item {
                                                if let Some(Value::Number(no)) = season_item_obj.get("season_number") {
                                                    if no.as_u64().and_then(|n| u32::try_from(n).ok()) == Some(season_no) {
                                                        season_item_obj.insert("episodes".to_string(), raw_season_details_json.get("episodes").cloned().unwrap_or(Value::Null));
                                                        season_item_obj.insert("networks".to_string(), raw_season_details_json.get("networks").cloned().unwrap_or(Value::Null));
                                                        season_item_obj.insert("credits".to_string(), raw_season_details_json.get("credits").cloned().unwrap_or(Value::Null));
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
                if let Ok(raw_series_bytes) = serde_json::to_vec(&raw_series) {
                    if let Err(err) = self.storage.store_tmdb_series_info(series_id, &raw_series_bytes).await {
                        warn!("Failed to cache raw TMDB series info: {err}");
                    }
                }

                // Return MediaMetadata struct
                return Ok(Some(MediaMetadata::Series(series.to_meta_data())));
            }
        }

        Ok(None)
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

    async fn fetch_single_season(&self, series_id: u32, season: u32) -> (Option<TmdbSeriesInfoSeasonDetails>, Option<bytes::Bytes>) {
        let url = format!(
            "{TMDB_API_BASE_URL}/tv/{series_id}/season/{season}?api_key={}&append_to_response=credits",
            self.api_key
        );

        let Ok(response) = self.client.get(&url).send().await else { return (None, None) };
        let Ok(bytes) = response.bytes().await else { return (None, None) };
        (match serde_json::from_slice::<TmdbSeriesInfoSeasonDetails>(&bytes) {
            Ok(details) => Some(details),
            Err(e) => {
                error!("Failed to parse series season details: tmdb-id: {series_id} season: {season} err: {e}");
                None
            }
        }, Some(bytes))
    }
}