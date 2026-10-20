use crate::{
    operation::MediaServerOperation, redaction::redact_media_server_text, MediaServerEpisode, MediaServerError,
    MediaServerErrorKind, MediaServerImageRef, MediaServerLibrary, MediaServerLibraryRef, MediaServerMovie,
    MediaServerPage, MediaServerPageRequest, MediaServerSeason, MediaServerSeries, MediaServerStatus,
    MediaServerStreamRef, MediaServerStreamResponse,
};
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue},
    Method, RequestBuilder,
};

#[allow(async_fn_in_trait)]
pub trait MediaServerCatalogClient: Send + Sync {
    async fn discover(&self) -> Result<MediaServerStatus, MediaServerError>;

    async fn list_libraries(&self) -> Result<Vec<MediaServerLibrary>, MediaServerError>;

    async fn list_movies(
        &self,
        library: &MediaServerLibraryRef,
        page: MediaServerPageRequest,
    ) -> Result<MediaServerPage<MediaServerMovie>, MediaServerError>;

    async fn list_series(
        &self,
        library: &MediaServerLibraryRef,
        page: MediaServerPageRequest,
    ) -> Result<MediaServerPage<MediaServerSeries>, MediaServerError>;

    async fn list_seasons(
        &self,
        library: &MediaServerLibraryRef,
        page: MediaServerPageRequest,
    ) -> Result<MediaServerPage<MediaServerSeason>, MediaServerError>;

    async fn list_episodes(
        &self,
        library: &MediaServerLibraryRef,
        page: MediaServerPageRequest,
    ) -> Result<MediaServerPage<MediaServerEpisode>, MediaServerError>;

    async fn open_stream(
        &self,
        stream_ref: &MediaServerStreamRef,
        range: Option<&str>,
    ) -> Result<MediaServerStreamResponse, MediaServerError>;

    async fn open_image(&self, image_ref: &MediaServerImageRef) -> Result<MediaServerStreamResponse, MediaServerError>;
}

#[derive(Clone)]
pub struct MediaServerHttpClient {
    client: reqwest::Client,
}

impl MediaServerHttpClient {
    pub fn new(client: reqwest::Client) -> Self { Self { client } }

    pub fn inner(&self) -> &reqwest::Client { &self.client }

    pub fn request(&self, method: Method, url: &str) -> MediaServerHttpRequestBuilder {
        MediaServerHttpRequestBuilder {
            safe_url: redact_media_server_text(url),
            builder: self.client.request(method, url),
            not_found_kind: MediaServerErrorKind::MediaServerItemNotFound,
            fallback_kind: MediaServerErrorKind::MediaServerStreamOpenFailed,
        }
    }

    pub(crate) fn request_for(
        &self,
        method: Method,
        url: &str,
        operation: MediaServerOperation,
    ) -> MediaServerHttpRequestBuilder {
        self.request(method, url).error_kinds(operation.not_found_kind(), operation.fallback_kind())
    }
}

pub struct MediaServerHttpRequestBuilder {
    safe_url: String,
    builder: RequestBuilder,
    not_found_kind: MediaServerErrorKind,
    fallback_kind: MediaServerErrorKind,
}

impl MediaServerHttpRequestBuilder {
    pub fn safe_url(&self) -> &str { &self.safe_url }

    pub fn header(mut self, key: HeaderName, value: HeaderValue) -> Self {
        self.builder = self.builder.header(key, value);
        self
    }

    pub fn headers(mut self, headers: HeaderMap) -> Self {
        self.builder = self.builder.headers(headers);
        self
    }

    pub fn json<T: serde::Serialize + ?Sized>(mut self, body: &T) -> Self {
        self.builder = self.builder.json(body);
        self
    }

    pub fn error_kinds(mut self, not_found_kind: MediaServerErrorKind, fallback_kind: MediaServerErrorKind) -> Self {
        self.not_found_kind = not_found_kind;
        self.fallback_kind = fallback_kind;
        self
    }

    pub fn discovery_errors(self) -> Self {
        self.error_kinds(MediaServerErrorKind::MediaServerUnavailable, MediaServerErrorKind::MediaServerDiscoveryFailed)
    }

    pub fn catalog_errors(self) -> Self {
        self.error_kinds(
            MediaServerErrorKind::MediaServerLibraryUnavailable,
            MediaServerErrorKind::MediaServerCatalogDecodeFailed,
        )
    }

    pub fn playback_errors(self) -> Self {
        self.error_kinds(
            MediaServerErrorKind::MediaServerItemNotFound,
            MediaServerErrorKind::MediaServerStreamOpenFailed,
        )
    }

    pub async fn send(self) -> Result<reqwest::Response, MediaServerError> {
        let detail = format!("request {} failed", self.safe_url);
        self.send_with_error_detail(detail).await
    }

    pub async fn send_with_error_detail(
        self,
        detail: impl Into<String>,
    ) -> Result<reqwest::Response, MediaServerError> {
        let detail = detail.into();
        self.builder.send().await.map_err(|err| {
            MediaServerError::from_reqwest_error_with_fallback(&err, self.not_found_kind, self.fallback_kind)
                .detail(&detail)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_server_http_request_builder_keeps_safe_url_redacted() {
        let client = MediaServerHttpClient::new(reqwest::Client::new());
        let request = client.request(Method::GET, "https://media.example.invalid/video?api_key=secret");

        assert!(!request.safe_url().contains("secret"));
        assert!(request.safe_url().contains("api_key=***"));
    }

    #[test]
    fn media_server_http_request_builder_can_select_catalog_error_context() {
        let client = MediaServerHttpClient::new(reqwest::Client::new());
        let request = client.request(Method::GET, "https://media.example.invalid/libraries").catalog_errors();

        assert_eq!(request.not_found_kind, MediaServerErrorKind::MediaServerLibraryUnavailable);
        assert_eq!(request.fallback_kind, MediaServerErrorKind::MediaServerCatalogDecodeFailed);
    }

    #[test]
    fn request_for_selects_error_kinds_per_operation() {
        let client = MediaServerHttpClient::new(reqwest::Client::new());
        for operation in
            [MediaServerOperation::Discovery, MediaServerOperation::Catalog, MediaServerOperation::Playback]
        {
            let request = client.request_for(Method::GET, "https://media.example.invalid/x", operation);

            assert_eq!(request.not_found_kind, operation.not_found_kind());
            assert_eq!(request.fallback_kind, operation.fallback_kind());
        }
    }

    #[test]
    fn json_keeps_safe_url_redacted() {
        let client = MediaServerHttpClient::new(reqwest::Client::new());
        let request = client
            .request(Method::POST, "https://media.example.invalid/login?api_key=secret")
            .json(&serde_json::json!({"Username": "u"}));

        assert!(!request.safe_url().contains("secret"));
        assert!(request.safe_url().contains("api_key=***"));
    }

    #[tokio::test]
    async fn json_sends_serialized_body_with_content_type() {
        use crate::test_server::{start_test_server, test_response};
        use http_body_util::BodyExt;
        use reqwest::StatusCode;

        let (base_url, server) = start_test_server(|req| async move {
            let content_type = req
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(ToOwned::to_owned);
            let body = req.into_body().collect().await.map(http_body_util::Collected::to_bytes).unwrap_or_default();
            let ok =
                content_type.as_deref() == Some("application/json") && body.as_ref() == br#"{"Username":"u","Pw":"p"}"#;
            Ok(test_response(if ok { StatusCode::OK } else { StatusCode::BAD_REQUEST }, b""))
        })
        .await;
        let client = MediaServerHttpClient::new(reqwest::Client::new());

        let response = client
            .request_for(Method::POST, &format!("{base_url}/login"), MediaServerOperation::Discovery)
            .json(&serde_json::json!({"Username": "u", "Pw": "p"}))
            .send()
            .await
            .expect("request is sent");

        assert_eq!(response.status(), StatusCode::OK);
        server.abort();
    }
}
