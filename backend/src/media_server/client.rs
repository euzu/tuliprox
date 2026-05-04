use crate::media_server::{
    redaction::redact_media_server_text, MediaServerEpisode, MediaServerImageRef, MediaServerLibrary, MediaServerLibraryRef, MediaServerError,
    MediaServerMovie, MediaServerPage, MediaServerPageRequest, MediaServerResourceResponse, MediaServerStatus, MediaServerStreamRef,
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
        }
    }
}

pub struct MediaServerHttpRequestBuilder {
    safe_url: String,
    builder: RequestBuilder,
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

    pub async fn send(self) -> Result<reqwest::Response, MediaServerError> {
        self.builder
            .send()
            .await
            .map_err(|err| MediaServerError::from_reqwest_error(&err).detail(format!("request {} failed", self.safe_url)))
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
        assert!(request.safe_url().contains("api_key=<redacted>"));
    }
}
