use crate::tuliprox_error::{create_tuliprox_error_result, TuliproxError, TuliproxErrorKind};
use crate::model::{TraktApiConfig, TraktListConfig, TraktListItem, TraktListCache, TraktCacheMap};
use reqwest::header::{HeaderMap, HeaderValue};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use log::{debug, info, warn};
use regex::Regex;
use std::sync::LazyLock;

const CACHE_EXPIRY_HOURS: u64 = 6;

pub struct TraktClient {
    client: Arc<reqwest::Client>,
    api_config: TraktApiConfig,
    cache: TraktCacheMap,
}

impl TraktClient {
    pub fn new(client: Arc<reqwest::Client>, api_config: TraktApiConfig) -> Self {
        Self {
            client,
            api_config,
            cache: HashMap::new(),
        }
    }

    fn get_headers(&self) -> Result<HeaderMap, TuliproxError> {
        let mut headers = HeaderMap::new();
        
        headers.insert(
            "Content-Type",
            HeaderValue::from_static("application/json")
        );
        
        let api_key_header = HeaderValue::from_str(self.api_config.get_api_key())
            .map_err(|e| TuliproxError::new(TuliproxErrorKind::Notify, format!("Invalid Trakt API key: {}", e)))?;
        headers.insert("trakt-api-key", api_key_header);
        
        let api_version_header = HeaderValue::from_str(self.api_config.get_api_version())
            .map_err(|e| TuliproxError::new(TuliproxErrorKind::Notify, format!("Invalid API version: {}", e)))?;
        headers.insert("trakt-api-version", api_version_header);

        Ok(headers)
    }

    fn build_list_url(&self, user: &str, list_slug: &str) -> String {
        format!("{}/users/{}/lists/{}/items", self.api_config.get_base_url(), user, list_slug)
    }

    fn get_cache_key(&self, user: &str, list_slug: &str) -> String {
        format!("{}:{}", user, list_slug)
    }

    fn is_cache_valid(&self, cache_entry: &TraktListCache) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        
        let cache_age_hours = (now - cache_entry.last_updated) / 3600;
        cache_age_hours < CACHE_EXPIRY_HOURS
    }

    pub async fn get_list_items(&mut self, list_config: &TraktListConfig) -> Result<Vec<TraktListItem>, TuliproxError> {

        let cache_key = self.get_cache_key(&list_config.user, &list_config.list_slug);
        
        // Check cache first
        if let Some(cache_entry) = self.cache.get(&cache_key) {
            if self.is_cache_valid(cache_entry) {
                debug!("Using cached Trakt list for {}:{}", list_config.user, list_config.list_slug);
                return Ok(cache_entry.items.clone());
            }
        }

        info!("Fetching Trakt list {}:{}", list_config.user, list_config.list_slug);
        
        let url = self.build_list_url(&list_config.user, &list_config.list_slug);
        let headers = self.get_headers()?;
        
        let response = self.client
            .get(&url)
            .headers(headers)
            .send()
            .await
            .map_err(|e| TuliproxError::new(TuliproxErrorKind::Notify, format!("Failed to fetch Trakt list {}: {}", url, e)))?;

        if !response.status().is_success() {
            return match response.status().as_u16() {
                404 => create_tuliprox_error_result!(TuliproxErrorKind::Notify, "Trakt list not found: {}:{}", list_config.user, list_config.list_slug),
                401 => create_tuliprox_error_result!(TuliproxErrorKind::Notify, "Trakt API key is invalid or expired"),
                429 => create_tuliprox_error_result!(TuliproxErrorKind::Notify, "Trakt API rate limit exceeded"),
                _ => create_tuliprox_error_result!(TuliproxErrorKind::Notify, "Trakt API error {}: {}", response.status(), response.status().canonical_reason().unwrap_or("Unknown"))
            };
        }

        let etag = response.headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let response_text = response
            .text()
            .await
            .map_err(|e| TuliproxError::new(TuliproxErrorKind::Notify, format!("Failed to read Trakt response: {}", e)))?;

        let items: Vec<TraktListItem> = serde_json::from_str(&response_text)
            .map_err(|e| TuliproxError::new(TuliproxErrorKind::Notify, format!("Failed to parse Trakt response: {}", e)))?;

        info!("Successfully fetched {} items from Trakt list {}:{}", items.len(), list_config.user, list_config.list_slug);

        // Update cache
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let cache_entry = TraktListCache {
            user: list_config.user.clone(),
            list_slug: list_config.list_slug.clone(),
            items: items.clone(),
            last_updated: now,
            etag,
        };

        self.cache.insert(cache_key, cache_entry);

        Ok(items)
    }

    pub async fn get_all_lists(&mut self, list_configs: &[TraktListConfig]) -> Result<HashMap<String, Vec<TraktListItem>>, Vec<TuliproxError>> {
        let mut results = HashMap::new();
        let mut errors = Vec::new();

        for list_config in list_configs {
            match self.get_list_items(list_config).await {
                Ok(items) => {
                    let key = format!("{}:{}", list_config.user, list_config.list_slug);
                    results.insert(key, items);
                }
                Err(err) => {
                    warn!("Failed to fetch Trakt list {}:{}: {}", list_config.user, list_config.list_slug, err.message);
                    errors.push(err);
                }
            }
        }

        if results.is_empty() && !errors.is_empty() {
            Err(errors)
        } else {
            Ok(results)
        }
    }

    pub fn clear_cache(&mut self) {
        self.cache.clear();
        debug!("Trakt cache cleared");
    }

    pub fn get_cache_size(&self) -> usize {
        self.cache.len()
    }
}

// Helper struct for normalization configuration
#[derive(Debug, Clone)]
pub struct TraktNormalizeConfig {
    pub normalize_regex: Regex,
}

impl Default for TraktNormalizeConfig {
    fn default() -> Self {
        Self {
            // Regex to remove all non-alphanumeric characters (except spaces), similar to current logic
            normalize_regex: Regex::new(r"[^a-z0-9\s]").unwrap(), 
        }
    }
}

pub static DEFAULT_TRAKT_NORMALIZE_CONFIG: LazyLock<TraktNormalizeConfig> = LazyLock::new(TraktNormalizeConfig::default);

pub fn normalize_title_for_matching(title: &str, config: &TraktNormalizeConfig) -> String {
    use deunicode::deunicode;
    
    let normalized = deunicode(title.trim()).to_lowercase();
    
    let cleaned_name = config.normalize_regex.replace_all(&normalized, "");
    
    cleaned_name
        .split_whitespace()
        .collect::<Vec<&str>>()
        .join(" ")
        .trim()
        .to_string()
}

pub fn extract_year_from_title(title: &str) -> (String, Option<u32>) {
    let year_regex = regex::Regex::new(r"\(?(\d{4})\)?$").unwrap();
    
    if let Some(captures) = year_regex.captures(title) {
        if let Some(year_str) = captures.get(1) {
            if let Ok(year) = year_str.as_str().parse::<u32>() {
                if year >= 1900 && year <= 2100 {
                    let title_without_year = title
                        .replace(&format!("({})", year), "")
                        .replace(&format!(" {}", year), "")
                        .trim()
                        .to_string();
                    return (title_without_year, Some(year));
                }
            }
        }
    }
    
    (title.to_string(), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_title() {
        assert_eq!(normalize_title_for_matching("The Matrix", &DEFAULT_TRAKT_NORMALIZE_CONFIG), "the matrix");
        assert_eq!(normalize_title_for_matching("Spider-Man: No Way Home", &DEFAULT_TRAKT_NORMALIZE_CONFIG), "spider man no way home");
        assert_eq!(normalize_title_for_matching("Élite", &DEFAULT_TRAKT_NORMALIZE_CONFIG), "elite");
    }

    #[test]
    fn test_extract_year() {
        let (title, year) = extract_year_from_title("The Matrix (1999)");
        assert_eq!(title, "The Matrix");
        assert_eq!(year, Some(1999));

        let (title, year) = extract_year_from_title("Avengers Endgame 2019");
        assert_eq!(title, "Avengers Endgame");
        assert_eq!(year, Some(2019));

        let (title, year) = extract_year_from_title("Just a Title");
        assert_eq!(title, "Just a Title");
        assert_eq!(year, None);
    }
} 