use super::model::translate_page;
use crate::kernel::CuratedMediaReference;
use reqwest::{
    header::{HeaderValue, ACCEPT, AUTHORIZATION},
    Client, Url,
};
use shared::model::{TmdbTrendingKind, TmdbTrendingScope, TmdbTrendingTimeWindow};
use std::time::Duration;
use tuliprox_core::model::{TmdbCurationApiConfig, TmdbTrendingConfig};

const TMDB_ORIGIN: &str = "https://api.themoviedb.org/";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const BODY_LIMIT: usize = 1024 * 1024;

/// Deliberately contains no response text, URL or underlying error that could echo a credential.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum TmdbFailure {
    Configuration,
    Transport,
    Status(u16),
    Redirect,
    Body,
    BodyLimit,
    InvalidResponse,
}

pub(crate) struct TmdbClient {
    http: Client,
    authorization: HeaderValue,
    origin: Url,
    timeout: Duration,
    body_limit: usize,
}

impl TmdbClient {
    pub(crate) fn new(http: &Client, api: &TmdbCurationApiConfig) -> Result<Self, TmdbFailure> {
        let token = api.access_token.trim();
        if token.is_empty() {
            return Err(TmdbFailure::Configuration);
        }
        let mut authorization =
            HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| TmdbFailure::Configuration)?;
        authorization.set_sensitive(true);
        Ok(Self {
            http: http.clone(),
            authorization,
            origin: Url::parse(TMDB_ORIGIN).expect("fixed TMDB origin"),
            timeout: REQUEST_TIMEOUT,
            body_limit: BODY_LIMIT,
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(http: &Client, api: &TmdbCurationApiConfig, origin: &str) -> Result<Self, TmdbFailure> {
        let mut client = Self::new(http, api)?;
        client.origin = Url::parse(origin).expect("local fixture origin");
        Ok(client)
    }

    pub(crate) async fn trending(
        &self,
        selector: &TmdbTrendingConfig,
    ) -> Result<Vec<CuratedMediaReference>, TmdbFailure> {
        let kind = match selector.kind {
            TmdbTrendingKind::Movie => "movie",
            TmdbTrendingKind::Tv => "tv",
        };
        let window = match selector.time_window {
            TmdbTrendingTimeWindow::Day => "day",
            TmdbTrendingTimeWindow::Week => "week",
        };
        let TmdbTrendingScope::FirstPage = selector.scope;
        let mut url = self.origin.join(&format!("3/trending/{kind}/{window}")).expect("fixed trending path");
        url.query_pairs_mut().append_pair("language", "en-US");
        let mut response = self
            .http
            .get(url.clone())
            .header(AUTHORIZATION, self.authorization.clone())
            .header(ACCEPT, "application/json")
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|_| TmdbFailure::Transport)?;
        // Preserve both authority and requested scope after the configured client's redirect handling.
        if response.url() != &url {
            return Err(TmdbFailure::Redirect);
        }
        if !response.status().is_success() {
            return Err(TmdbFailure::Status(response.status().as_u16()));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| TmdbFailure::Body)? {
            if chunk.len() > self.body_limit.saturating_sub(body.len()) {
                return Err(TmdbFailure::BodyLimit);
            }
            body.extend_from_slice(&chunk);
        }
        translate_page(&body, selector.kind).map_err(|()| TmdbFailure::InvalidResponse)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{http_response, TestServer};

    const EMPTY: &str = r#"{"page":1,"total_pages":0,"total_results":0,"results":[]}"#;
    const TOKEN: &str = "private-test-token";

    fn selector(kind: TmdbTrendingKind, time_window: TmdbTrendingTimeWindow) -> TmdbTrendingConfig {
        TmdbTrendingConfig {
            kind,
            time_window,
            scope: TmdbTrendingScope::FirstPage,
            category_name: None,
            create_xtream_category: false,
        }
    }

    #[tokio::test]
    async fn tmdb_all_routes_use_sensitive_bearer_and_preserve_the_supplied_client() {
        for (kind, path) in [(TmdbTrendingKind::Movie, "movie"), (TmdbTrendingKind::Tv, "tv")] {
            for (window, period) in [(TmdbTrendingTimeWindow::Day, "day"), (TmdbTrendingTimeWindow::Week, "week")] {
                let server = TestServer::new(http_response(200, EMPTY)).await;
                let mut headers = reqwest::header::HeaderMap::new();
                headers.insert("x-transport-context", HeaderValue::from_static("retained"));
                let http = Client::builder().no_proxy().default_headers(headers).build().unwrap();
                let client = TmdbClient::for_test(
                    &http,
                    &TmdbCurationApiConfig { access_token: format!(" {TOKEN} ") },
                    &server.url,
                )
                .unwrap();
                assert!(client.authorization.is_sensitive());
                assert!(!format!("{:?}", client.authorization).contains(TOKEN));
                assert!(client.trending(&selector(kind, window)).await.unwrap().is_empty());
                let requests = server.requests.lock().unwrap();
                assert_eq!(requests.len(), 1);
                assert!(requests[0].starts_with(&format!("GET /3/trending/{path}/{period}?language=en-US HTTP/1.1")));
                let headers = requests[0].to_lowercase();
                assert!(headers.contains(&format!("authorization: bearer {TOKEN}")));
                assert!(headers.contains("x-transport-context: retained"));
                assert!(!requests[0].lines().next().unwrap().contains(TOKEN));
            }
        }
    }

    #[tokio::test]
    async fn tmdb_missing_or_invalid_token_never_makes_a_request() {
        let server = TestServer::new(http_response(200, EMPTY)).await;
        for token in ["", "  ", "bad\nheader"] {
            let error = TmdbClient::for_test(
                &Client::new(),
                &TmdbCurationApiConfig { access_token: token.to_string() },
                &server.url,
            )
            .err()
            .unwrap();
            assert_eq!(error, TmdbFailure::Configuration);
        }
        assert!(server.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn tmdb_status_and_payload_failures_are_not_empty_and_never_echo_the_body() {
        for (status, body, expected) in [
            (401, TOKEN, TmdbFailure::Status(401)),
            (403, TOKEN, TmdbFailure::Status(403)),
            (429, TOKEN, TmdbFailure::Status(429)),
            (500, TOKEN, TmdbFailure::Status(500)),
            (200, TOKEN, TmdbFailure::InvalidResponse),
        ] {
            let server = TestServer::new(http_response(status, body)).await;
            let client = TmdbClient::for_test(
                &Client::new(),
                &TmdbCurationApiConfig { access_token: TOKEN.to_string() },
                &server.url,
            )
            .unwrap();
            let error =
                client.trending(&selector(TmdbTrendingKind::Movie, TmdbTrendingTimeWindow::Week)).await.unwrap_err();
            assert_eq!(error, expected);
            assert!(!format!("{error:?}").contains(TOKEN));
        }
    }

    #[tokio::test]
    async fn tmdb_body_bound_and_interrupted_body_fail_without_truncating_into_success() {
        for response in [
            http_response(200, &"x".repeat(129)),
            format!(
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n81\r\n{}\r\n0\r\n\r\n",
                "x".repeat(129)
            ),
        ] {
            let server = TestServer::new(response).await;
            let mut client = TmdbClient::for_test(
                &Client::new(),
                &TmdbCurationApiConfig { access_token: TOKEN.to_string() },
                &server.url,
            )
            .unwrap();
            client.body_limit = 128;
            assert_eq!(
                client.trending(&selector(TmdbTrendingKind::Movie, TmdbTrendingTimeWindow::Week)).await.unwrap_err(),
                TmdbFailure::BodyLimit
            );
        }
        let server =
            TestServer::new(format!("HTTP/1.1 200 OK\r\nContent-Length: 10000\r\nConnection: close\r\n\r\n{EMPTY}"))
                .await;
        let client = TmdbClient::for_test(
            &Client::new(),
            &TmdbCurationApiConfig { access_token: TOKEN.to_string() },
            &server.url,
        )
        .unwrap();
        assert_eq!(
            client.trending(&selector(TmdbTrendingKind::Movie, TmdbTrendingTimeWindow::Week)).await.unwrap_err(),
            TmdbFailure::Body
        );
    }

    #[tokio::test]
    async fn tmdb_deadline_covers_headers_and_body() {
        for send_headers in [false, true] {
            let server = TestServer::delayed(http_response(200, EMPTY), Duration::from_secs(2), send_headers).await;
            let mut client = TmdbClient::for_test(
                &Client::new(),
                &TmdbCurationApiConfig { access_token: TOKEN.to_string() },
                &server.url,
            )
            .unwrap();
            client.timeout = Duration::from_millis(50);
            let error = tokio::time::timeout(
                Duration::from_secs(1),
                client.trending(&selector(TmdbTrendingKind::Movie, TmdbTrendingTimeWindow::Week)),
            )
            .await
            .unwrap()
            .unwrap_err();
            assert!(matches!(error, TmdbFailure::Transport | TmdbFailure::Body));
        }
    }

    #[tokio::test]
    async fn tmdb_same_origin_redirect_cannot_change_the_declared_feed_scope() {
        let server = TestServer::sequence(vec![
            "HTTP/1.1 302 Found\r\nLocation: /3/trending/movie/day?language=en-US\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
            http_response(200, EMPTY),
        ]).await;
        let client = TmdbClient::for_test(
            &Client::new(),
            &TmdbCurationApiConfig { access_token: TOKEN.to_string() },
            &server.url,
        )
        .unwrap();
        assert_eq!(
            client.trending(&selector(TmdbTrendingKind::Movie, TmdbTrendingTimeWindow::Week)).await.unwrap_err(),
            TmdbFailure::Redirect
        );
        assert_eq!(server.requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn tmdb_foreign_redirect_strips_authorization_and_is_not_authoritative() {
        let foreign = TestServer::new(http_response(200, EMPTY)).await;
        let redirect = TestServer::new(format!(
            "HTTP/1.1 302 Found\r\nLocation: {}foreign\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            foreign.url
        ))
        .await;
        let client = TmdbClient::for_test(
            &Client::new(),
            &TmdbCurationApiConfig { access_token: TOKEN.to_string() },
            &redirect.url,
        )
        .unwrap();
        assert_eq!(
            client.trending(&selector(TmdbTrendingKind::Movie, TmdbTrendingTimeWindow::Week)).await.unwrap_err(),
            TmdbFailure::Redirect
        );
        let requests = foreign.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(!requests[0].to_lowercase().contains("authorization:"));
        assert!(!requests[0].contains(TOKEN));
    }
}
