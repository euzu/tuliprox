use crate::model::{TraktApiConfig, TraktListConfig, TraktListItem};
use crate::tuliprox_error::TuliproxError;
use reqwest::header::{HeaderMap, HeaderValue};
use std::collections::HashMap;
use std::sync::Arc;
use log::{info, warn};

use super::errors::{handle_trakt_api_error, create_fetch_error, create_parse_error, create_read_error, create_header_error};

pub struct TraktClient {
    client: Arc<reqwest::Client>,
    api_config: TraktApiConfig,
    // Pre-computed headers to avoid recreating them each time
    headers: HeaderMap,
}

impl TraktClient {
    pub fn new(client: Arc<reqwest::Client>, api_config: TraktApiConfig) -> Result<Self, TuliproxError> {
        let headers = Self::create_headers(&api_config)?;
        Ok(Self {
            client,
            api_config,
            headers,
        })
    }

    fn create_headers(api_config: &TraktApiConfig) -> Result<HeaderMap, TuliproxError> {
        let mut headers = HeaderMap::new();
        
        headers.insert(
            "Content-Type",
            HeaderValue::from_static("application/json")
        );
        
        let api_key_header = HeaderValue::from_str(api_config.get_api_key())
            .map_err(|e| create_header_error("Trakt API key", e))?;
        headers.insert("trakt-api-key", api_key_header);
        
        let api_version_header = HeaderValue::from_str(api_config.get_api_version())
            .map_err(|e| create_header_error("API version", e))?;
        headers.insert("trakt-api-version", api_version_header);

        Ok(headers)
    }

    fn build_list_url(&self, user: &str, list_slug: &str) -> String {
        format!("{}/users/{}/lists/{}/items", self.api_config.get_base_url(), user, list_slug)
    }

    pub async fn get_list_items(&self, list_config: &TraktListConfig) -> Result<Vec<TraktListItem>, TuliproxError> {
        info!("Fetching Trakt list {}:{}", list_config.user, list_config.list_slug);
        
        let url = self.build_list_url(&list_config.user, &list_config.list_slug);
        
        let response = self.client
            .get(&url)
            .headers(self.headers.clone())
            .send()
            .await
            .map_err(|e| create_fetch_error(&url, e))?;

        if !response.status().is_success() {
            handle_trakt_api_error(response.status(), &list_config.user, &list_config.list_slug)?;
            unreachable!()
        }

        let response_text = response
            .text()
            .await
            .map_err(create_read_error)?;

        let items: Vec<TraktListItem> = serde_json::from_str(&response_text)
            .map_err(create_parse_error)?;

        info!("Successfully fetched {} items from Trakt list {}:{}", 
              items.len(), list_config.user, list_config.list_slug);

        Ok(items)
    }

    pub async fn get_all_lists(&self, list_configs: &[TraktListConfig]) -> Result<HashMap<String, Vec<TraktListItem>>, Vec<TuliproxError>> {
        let mut results = HashMap::new();
        let mut errors = Vec::new();

        for list_config in list_configs {
            let cache_key = format!("{}:{}", list_config.user, list_config.list_slug);
            
            match self.get_list_items(list_config).await {
                Ok(items) => {
                    results.insert(cache_key, items);
                }
                Err(err) => {
                    warn!("Failed to fetch Trakt list {}: {}", cache_key, err.message);
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
} 