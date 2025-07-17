use crate::model::{TraktApiConfig, TraktListConfig, TraktListItem};
use shared::error::TuliproxError;
use reqwest::header::{HeaderMap, HeaderValue};
use std::sync::Arc;
use log::{debug, info};
use shared::error::{info_err, TuliproxErrorKind};
use shared::utils::trim_last_slash;
use super::errors::{handle_trakt_api_error};

pub struct TraktClient {
    client: Arc<reqwest::Client>,
    api_config: TraktApiConfig,
    // Pre-computed headers to avoid recreating them each time
    headers: HeaderMap,
}

impl TraktClient {
    pub fn new(client: Arc<reqwest::Client>, api_config: TraktApiConfig) -> Self {
        let headers = Self::create_headers(&api_config);
        Self {
            client,
            api_config,
            headers,
        }
    }

    fn create_headers(api_config: &TraktApiConfig) -> axum::http::HeaderMap {
        let mut headers = HeaderMap::new();

        headers.insert(reqwest::header::CONTENT_TYPE, HeaderValue::from_static(mime::APPLICATION_JSON.as_ref()));
        headers.insert("trakt-api-key", HeaderValue::from_str(api_config.key.as_str()).unwrap_or_else(|_| HeaderValue::from_static("")));
        headers.insert("trakt-api-version", HeaderValue::from_str(api_config.version.as_str()).unwrap_or_else(|_| HeaderValue::from_static("")));

        headers
    }

    fn build_list_url(&self, user: &str, list_slug: &str) -> String {
        format!("{}/users/{user}/lists/{list_slug}/items", trim_last_slash(&self.api_config.url))
    }

    pub async fn get_list_items(&self, list_config: &TraktListConfig) -> Result<Vec<TraktListItem>, TuliproxError> {
        debug!("Fetching Trakt list {}:{}", list_config.user, list_config.list_slug);

        let url = self.build_list_url(&list_config.user, &list_config.list_slug);

        let response = self.client
            .get(&url)
            .headers(self.headers.clone())
            .send()
            .await
            .map_err(|err| info_err!(format!("Failed to fetch Trakt list {url}: {err}")))?;

        if !response.status().is_success() {
            handle_trakt_api_error(response.status(), &list_config.user, &list_config.list_slug)?;
        }

        let response_text = response
            .text()
            .await
            .map_err(|error: reqwest::Error| info_err!(format!("Failed to read Trakt response: {error}")))?;

        let mut items: Vec<TraktListItem> = serde_json::from_str(&response_text)
            .map_err(|error: serde_json::Error| info_err!(format!("Failed to parse Trakt response: {error}")))?;
        items.iter_mut().for_each(TraktListItem::prepare);
        info!("Successfully fetched {} items from Trakt list {}:{}", items.len(), list_config.user, list_config.list_slug);

        Ok(items)
    }

} 