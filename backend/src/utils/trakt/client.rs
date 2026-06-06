use super::errors::handle_trakt_api_error;
use crate::model::{TraktApiConfig, TraktChartConfig, TraktListConfig, TraktListItem, TraktMovie, TraktShow};
use log::{debug, info};
use reqwest::header::{HeaderMap, HeaderValue};
use serde::Deserialize;
use shared::{error::TuliproxError, utils::{trim_last_slash, DEFAULT_USER_AGENT, TRAKT_API_KEY}};

const TRAKT_PAGE_LIMIT: u32 = 100;
const TRAKT_MAX_PAGES: u32 = 100;

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

    fn build_chart_url(&self, chart_config: &TraktChartConfig) -> String {
        format!("{}/{}/{}", trim_last_slash(&self.api_config.url), chart_config.kind, chart_config.chart)
    }

    pub async fn get_chart_items(&self, chart_config: &TraktChartConfig) -> Result<Vec<TraktListItem>, TuliproxError> {
        let id_label = format!("{}:{}", chart_config.kind, chart_config.chart);
        self.paginate_items("chart", id_label, |page| async move {
            self.get_chart_items_page(chart_config, page).await
        })
        .await
    }

    pub async fn get_list_items(&self, list_config: &TraktListConfig) -> Result<Vec<TraktListItem>, TuliproxError> {
        let id_label = format!("{}:{}", list_config.user, list_config.list_slug);
        self.paginate_items("list", id_label, |page| async move {
            self.get_list_items_page(list_config, page).await
        })
        .await
    }

    /// Shared body of `get_chart_items` and `get_list_items`.
    /// Walks Trakt's paginated response one page at a time via `fetch_page`,
    /// logging per-page progress with `kind_label` and `id_label` for context.
    async fn paginate_items<F, Fut>(
        &self,
        kind_label: &'static str,
        id_label: String,
        mut fetch_page: F,
    ) -> Result<Vec<TraktListItem>, TuliproxError>
    where
        F: FnMut(u32) -> Fut,
        Fut: std::future::Future<Output = Result<TraktListItemsPage, TuliproxError>>,
    {
        debug!("Fetching Trakt {kind_label} {id_label}");

        let mut page = 1;
        let mut items = Vec::new();
        loop {
            let mut page_items = fetch_page(page).await?;
            let page_count = page_items.page_count;
            let item_count = page_items.item_count;
            debug!(
                "Fetched Trakt {kind_label} {id_label} page {page}/{page_count} with {} items",
                page_items.items.len()
            );
            let is_last_page = page >= page_count || page >= TRAKT_MAX_PAGES || page_items.items.is_empty();
            items.append(&mut page_items.items);
            if is_last_page {
                if page >= TRAKT_MAX_PAGES && page < page_count {
                    debug!(
                        "Stopped Trakt {kind_label} {id_label} after {TRAKT_MAX_PAGES} pages; reported page count was {page_count}"
                    );
                }
                info!(
                    "Successfully fetched {} items from Trakt {kind_label} {id_label}{}",
                    items.len(),
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
        let request_url = format!("{url}?page={page}&limit={TRAKT_PAGE_LIMIT}");
        let (response_text, page_count, item_count) = self
            .fetch_trakt_page(request_url, "list", (&list_config.user, &list_config.list_slug), page)
            .await?;
        let mut items: Vec<TraktListItem> = serde_json::from_str(&response_text)
            .map_err(|error: serde_json::Error| TuliproxError::Config(format!("Failed to parse Trakt response: {error}")))?;
        items.iter_mut().for_each(TraktListItem::prepare);

        Ok(TraktListItemsPage { items, page_count, item_count })
    }

    async fn get_chart_items_page(
        &self,
        chart_config: &TraktChartConfig,
        page: u32,
    ) -> Result<TraktListItemsPage, TuliproxError> {
        let url = self.build_chart_url(chart_config);
        let request_url = format!("{url}?page={page}&limit={TRAKT_PAGE_LIMIT}");
        let (response_text, page_count, item_count) = self
            .fetch_trakt_page(
                request_url,
                "chart",
                ("charts", &format!("{}:{}", chart_config.kind, chart_config.chart)),
                page,
            )
            .await?;
        let items = parse_chart_items(&response_text, chart_config, page)
            .map_err(|error| TuliproxError::Config(format!("Failed to parse Trakt chart response: {error}")))?;

        Ok(TraktListItemsPage { items, page_count, item_count })
    }

    /// Shared body of `get_list_items_page` / `get_chart_items_page`.
    /// Issues the GET, validates the status, parses the pagination headers, and
    /// returns the raw response body. Per-type item parsing stays with the caller.
    async fn fetch_trakt_page(
        &self,
        request_url: String,
        error_label: &str,
        error_id: (&str, &str),
        page: u32,
    ) -> Result<(String, u32, Option<u32>), TuliproxError> {
        let response = self
            .client
            .get(&request_url)
            .headers(self.headers.clone())
            .send()
            .await
            .map_err(|err| TuliproxError::Config(format!("Failed to fetch Trakt {error_label} {request_url}: {err}")))?;

        if !response.status().is_success() {
            handle_trakt_api_error(response.status(), error_id.0, error_id.1)?;
        }

        let page_count = parse_trakt_pagination_header(response.headers(), "x-pagination-page-count").unwrap_or(page);
        let item_count = parse_trakt_pagination_header(response.headers(), "x-pagination-item-count");
        let response_text = response
            .text()
            .await
            .map_err(|error: reqwest::Error| TuliproxError::Config(format!("Failed to read Trakt response: {error}")))?;

        Ok((response_text, page_count, item_count))
    }
}

struct TraktListItemsPage {
    items: Vec<TraktListItem>,
    page_count: u32,
    item_count: Option<u32>,
}

fn parse_chart_items(
    response_text: &str,
    chart_config: &TraktChartConfig,
    page: u32,
) -> Result<Vec<TraktListItem>, serde_json::Error> {
    let rank_base = page.saturating_sub(1).saturating_mul(TRAKT_PAGE_LIMIT);
    match (chart_config.kind, chart_config.chart) {
        (shared::model::TraktChartKind::Movies, shared::model::TraktChartType::Popular) => {
            let items = serde_json::from_str::<Vec<TraktMovie>>(response_text)?;
            Ok(items
                .into_iter()
                .enumerate()
                .map(|(index, movie)| TraktListItem::from_movie_chart(movie, chart_rank(rank_base, index)))
                .collect())
        }
        (shared::model::TraktChartKind::Movies, shared::model::TraktChartType::Trending) => {
            let items = serde_json::from_str::<Vec<TraktTrendingMovieItem>>(response_text)?;
            Ok(items
                .into_iter()
                .enumerate()
                .map(|(index, item)| TraktListItem::from_movie_chart(item.movie, chart_rank(rank_base, index)))
                .collect())
        }
        (shared::model::TraktChartKind::Shows, shared::model::TraktChartType::Popular) => {
            let items = serde_json::from_str::<Vec<TraktShow>>(response_text)?;
            Ok(items
                .into_iter()
                .enumerate()
                .map(|(index, show)| TraktListItem::from_show_chart(show, chart_rank(rank_base, index)))
                .collect())
        }
        (shared::model::TraktChartKind::Shows, shared::model::TraktChartType::Trending) => {
            let items = serde_json::from_str::<Vec<TraktTrendingShowItem>>(response_text)?;
            Ok(items
                .into_iter()
                .enumerate()
                .map(|(index, item)| TraktListItem::from_show_chart(item.show, chart_rank(rank_base, index)))
                .collect())
        }
    }
}

fn chart_rank(rank_base: u32, index: usize) -> u32 {
    rank_base.saturating_add(u32::try_from(index).unwrap_or(u32::MAX)).saturating_add(1)
}

#[derive(Deserialize)]
struct TraktTrendingMovieItem {
    movie: TraktMovie,
}

#[derive(Deserialize)]
struct TraktTrendingShowItem {
    show: TraktShow,
}

fn parse_trakt_pagination_header(headers: &HeaderMap, name: &'static str) -> Option<u32> {
    headers.get(name).and_then(|value| value.to_str().ok()).and_then(|value| value.parse::<u32>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::{TraktChartKind, TraktChartType, TraktContentType};
    use std::sync::{atomic::{AtomicUsize, Ordering}, Arc, Mutex};
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

    #[tokio::test]
    async fn get_chart_items_fetches_public_trending_movies() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let base_url = spawn_single_response_trakt_server(
            r#"[{"watchers":42,"movie":{"title":"Movie 1","year":2026,"ids":{"trakt":1,"slug":"movie-1","tvdb":null,"imdb":null,"tmdb":11,"tvrage":null}}}]"#,
            Arc::clone(&requests),
        )
        .await;
        let client = client(base_url);
        let chart_config = chart_config(TraktChartKind::Movies, TraktChartType::Trending);

        let items = client.get_chart_items(&chart_config).await.expect("trending movie chart should load");

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].rank, Some(1));
        assert_eq!(items[0].content_type, TraktContentType::Vod);
        assert_eq!(items[0].movie.as_ref().expect("movie").ids.tmdb, Some(11));
        assert!(requests.lock().expect("requests")[0].contains("GET /movies/trending?page=1&limit=100 "));
    }

    #[tokio::test]
    async fn get_chart_items_fetches_public_popular_shows() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let base_url = spawn_single_response_trakt_server(
            r#"[{"title":"Show 1","year":2026,"ids":{"trakt":2,"slug":"show-1","tvdb":null,"imdb":null,"tmdb":22,"tvrage":null}}]"#,
            Arc::clone(&requests),
        )
        .await;
        let client = client(base_url);
        let chart_config = chart_config(TraktChartKind::Shows, TraktChartType::Popular);

        let items = client.get_chart_items(&chart_config).await.expect("popular show chart should load");

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].rank, Some(1));
        assert_eq!(items[0].content_type, TraktContentType::Series);
        assert_eq!(items[0].show.as_ref().expect("show").ids.tmdb, Some(22));
        assert!(requests.lock().expect("requests")[0].contains("GET /shows/popular?page=1&limit=100 "));
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

    async fn spawn_single_response_trakt_server(body: &'static str, requests: Arc<Mutex<Vec<String>>>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else { return };
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
            requests.lock().expect("requests").push(String::from_utf8_lossy(&request_bytes).to_string());
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(), body
            );
            stream.write_all(response.as_bytes()).await.expect("write response");
        });
        format!("http://{addr}")
    }

    fn client(base_url: String) -> TraktClient {
        TraktClient::new(
            reqwest::Client::new(),
            TraktApiConfig {
                api_key: "test-key".to_string(),
                version: "2".to_string(),
                url: base_url,
                user_agent: "tuliprox-test".to_string(),
            },
        )
    }

    fn chart_config(kind: TraktChartKind, chart: TraktChartType) -> TraktChartConfig {
        TraktChartConfig {
            kind,
            chart,
            category_name: "category".to_string(),
            tmdb_only: false,
            fuzzy_match_threshold: 90,
        }
    }

    fn trakt_movie_json(page: u32) -> String {
        format!(
            r#"{{"id":{page},"rank":{page},"listed_at":"2026-01-01T00:00:00.000Z","type":"movie","movie":{{"title":"Movie {page}","year":2026,"ids":{{"trakt":{page},"slug":"movie-{page}","tvdb":null,"imdb":null,"tmdb":{page},"tvrage":null}}}}}}"#,
        )
    }
}
