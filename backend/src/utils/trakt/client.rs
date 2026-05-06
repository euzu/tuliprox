use super::errors::handle_trakt_api_error;
use crate::model::{TraktApiConfig, TraktListConfig, TraktListItem};
use log::{debug, info};
use reqwest::header::{HeaderMap, HeaderValue};
use shared::{error::TuliproxError, utils::{trim_last_slash, DEFAULT_USER_AGENT, TRAKT_API_KEY}};

const TRAKT_LIST_PAGE_LIMIT: u32 = 100;
const TRAKT_LIST_MAX_PAGES: u32 = 100;

pub struct TraktClient {
    client: reqwest::Client,
    api_config: TraktApiConfig,
    // Pre-computed headers to avoid recreating them each time
    headers: HeaderMap,
}

impl TraktClient {
    pub fn new(client: reqwest::Client, api_config: TraktApiConfig) -> Self {
        let headers = Self::create_headers(&api_config);
        Self {
            client,
            api_config,
            headers,
        }
    }

    fn create_headers(api_config: &TraktApiConfig) -> HeaderMap {
        let mut headers = HeaderMap::new();

        headers.insert(reqwest::header::CONTENT_TYPE, HeaderValue::from_static(mime::APPLICATION_JSON.as_ref()));
        headers.insert(reqwest::header::USER_AGENT, HeaderValue::from_str(api_config.user_agent.as_str()).unwrap_or_else(|_| HeaderValue::from_static(DEFAULT_USER_AGENT)));
        headers.insert("trakt-api-key", HeaderValue::from_str(api_config.api_key.as_str()).unwrap_or_else(|_| HeaderValue::from_static(TRAKT_API_KEY)));
        headers.insert("trakt-api-version", HeaderValue::from_str(api_config.version.as_str()).unwrap_or_else(|_| HeaderValue::from_static("2")));

        headers
    }

    fn build_list_url(&self, user: &str, list_slug: &str) -> String {
        format!("{}/users/{user}/lists/{list_slug}/items", trim_last_slash(&self.api_config.url))
    }

    pub async fn get_list_items(&self, list_config: &TraktListConfig) -> Result<Vec<TraktListItem>, TuliproxError> {
        debug!("Fetching Trakt list {}:{}", list_config.user, list_config.list_slug);

        let mut page = 1;
        let mut items = Vec::new();
        loop {
            let mut page_items = self.get_list_items_page(list_config, page).await?;
            let page_count = page_items.page_count;
            let item_count = page_items.item_count;
            debug!(
                "Fetched Trakt list {}:{} page {page}/{page_count} with {} items",
                list_config.user,
                list_config.list_slug,
                page_items.items.len()
            );
            let is_last_page = page >= page_count || page >= TRAKT_LIST_MAX_PAGES || page_items.items.is_empty();
            items.append(&mut page_items.items);
            if is_last_page {
                if page >= TRAKT_LIST_MAX_PAGES && page < page_count {
                    debug!(
                        "Stopped Trakt list {}:{} after {TRAKT_LIST_MAX_PAGES} pages; reported page count was {page_count}",
                        list_config.user, list_config.list_slug
                    );
                }
                info!(
                    "Successfully fetched {} items from Trakt list {}:{}{}",
                    items.len(),
                    list_config.user,
                    list_config.list_slug,
                    item_count.map(|count| format!(" (reported item count: {count})")).unwrap_or_default()
                );
                return Ok(items);
            }
            page += 1;
        }
    }

    async fn get_list_items_page(
        &self,
        list_config: &TraktListConfig,
        page: u32,
    ) -> Result<TraktListItemsPage, TuliproxError> {
        let url = self.build_list_url(&list_config.user, &list_config.list_slug);
        let request_url = format!("{url}?page={page}&limit={TRAKT_LIST_PAGE_LIMIT}");
        let response = self
            .client
            .get(&request_url)
            .headers(self.headers.clone())
            .send()
            .await
            .map_err(|err| TuliproxError::Config(format!("Failed to fetch Trakt list {url}: {err}")))?;

        if !response.status().is_success() {
            handle_trakt_api_error(response.status(), &list_config.user, &list_config.list_slug)?;
        }

        let page_count = parse_trakt_pagination_header(response.headers(), "x-pagination-page-count").unwrap_or(page);
        let item_count = parse_trakt_pagination_header(response.headers(), "x-pagination-item-count");
        let response_text = response
            .text()
            .await
            .map_err(|error: reqwest::Error| TuliproxError::Config(format!("Failed to read Trakt response: {error}")))?;

        let mut items: Vec<TraktListItem> = serde_json::from_str(&response_text)
            .map_err(|error: serde_json::Error| TuliproxError::Config(format!("Failed to parse Trakt response: {error}")))?;
        items.iter_mut().for_each(TraktListItem::prepare);

        Ok(TraktListItemsPage { items, page_count, item_count })
    }
}

struct TraktListItemsPage {
    items: Vec<TraktListItem>,
    page_count: u32,
    item_count: Option<u32>,
}

fn parse_trakt_pagination_header(headers: &HeaderMap, name: &'static str) -> Option<u32> {
    headers.get(name).and_then(|value| value.to_str().ok()).and_then(|value| value.parse::<u32>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::TraktContentType;
    use std::sync::{atomic::{AtomicUsize, Ordering}, Arc};
    use tokio::{io::{AsyncReadExt, AsyncWriteExt}, net::TcpListener};

    #[tokio::test]
    async fn get_list_items_follows_trakt_pagination_headers() {
        let requests = Arc::new(AtomicUsize::new(0));
        let base_url = spawn_paged_trakt_server(Arc::clone(&requests)).await;
        let client = TraktClient::new(
            reqwest::Client::new(),
            TraktApiConfig {
                api_key: "test-key".to_string(),
                version: "2".to_string(),
                url: base_url,
                user_agent: "tuliprox-test".to_string(),
            },
        );
        let list_config = TraktListConfig {
            user: "user".to_string(),
            list_slug: "list".to_string(),
            category_name: "category".to_string(),
            content_type: TraktContentType::Vod,
            tmdb_only: false,
            fuzzy_match_threshold: 90,
        };

        let items = client.get_list_items(&list_config).await.expect("paged list should load");

        assert_eq!(items.len(), 2);
        assert_eq!(items[0].content_type, TraktContentType::Vod);
        assert_eq!(items[1].content_type, TraktContentType::Vod);
        assert_eq!(requests.load(Ordering::SeqCst), 2);
    }

    async fn spawn_paged_trakt_server(requests: Arc<AtomicUsize>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else { break };
                let mut request_bytes = Vec::new();
                loop {
                    let mut buffer = [0; 1024];
                    let read = stream.read(&mut buffer).await.expect("read request");
                    if read == 0 {
                        break;
                    }
                    request_bytes.extend_from_slice(&buffer[..read]);
                    if request_bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8_lossy(&request_bytes);
                let page = if request.contains("page=2") { 2 } else { 1 };
                requests.fetch_add(1, Ordering::SeqCst);
                let body = format!("[{}]", trakt_movie_json(page));
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nx-pagination-page-count: 2\r\nx-pagination-item-count: 2\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(), body
                );
                stream.write_all(response.as_bytes()).await.expect("write response");
            }
        });
        format!("http://{addr}")
    }

    fn trakt_movie_json(page: u32) -> String {
        format!(
            r#"{{"id":{page},"rank":{page},"listed_at":"2026-01-01T00:00:00.000Z","type":"movie","movie":{{"title":"Movie {page}","year":2026,"ids":{{"trakt":{page},"slug":"movie-{page}","tvdb":null,"imdb":null,"tmdb":{page},"tvrage":null}}}}}}"#,
        )
    }
}
