use crate::media_server::{
    redaction::redact_media_server_text, MediaServerEpisode, MediaServerError, MediaServerErrorKind, MediaServerImageRef,
    MediaServerLibrary, MediaServerLibraryRef, MediaServerMovie, MediaServerPage, MediaServerPageRequest,
    MediaServerResourceResponse, MediaServerSeason, MediaServerSeries, MediaServerStatus, MediaServerStreamRef,
    MediaServerStreamResponse,
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

    async fn open_image(&self, image_ref: &MediaServerImageRef) -> Result<MediaServerResourceResponse, MediaServerError>;
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
        self.error_kinds(MediaServerErrorKind::MediaServerItemNotFound, MediaServerErrorKind::MediaServerStreamOpenFailed)
    }

    pub async fn send(self) -> Result<reqwest::Response, MediaServerError> {
        self.builder.send().await.map_err(|err| {
            MediaServerError::from_reqwest_error_with_fallback(&err, self.not_found_kind, self.fallback_kind)
                .detail(format!("request {} failed", self.safe_url))
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
}
