use crate::remote_media::{
    redaction::redact_remote_text, RemoteEpisode, RemoteImageRef, RemoteLibrary, RemoteLibraryRef, RemoteMediaError,
    RemoteMovie, RemotePage, RemotePageRequest, RemoteResourceResponse, RemoteServerStatus, RemoteStreamRef,
    RemoteStreamResponse,
};
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue},
    Method, RequestBuilder,
};

#[allow(async_fn_in_trait)]
pub trait RemoteMediaCatalogClient: Send + Sync {
    async fn discover(&self) -> Result<RemoteServerStatus, RemoteMediaError>;

    async fn list_libraries(&self) -> Result<Vec<RemoteLibrary>, RemoteMediaError>;

    async fn list_movies(
        &self,
        library: &RemoteLibraryRef,
        page: RemotePageRequest,
    ) -> Result<RemotePage<RemoteMovie>, RemoteMediaError>;

    async fn list_episodes(
        &self,
        library: &RemoteLibraryRef,
        page: RemotePageRequest,
    ) -> Result<RemotePage<RemoteEpisode>, RemoteMediaError>;

    async fn open_stream(
        &self,
        stream_ref: &RemoteStreamRef,
        range: Option<&str>,
    ) -> Result<RemoteStreamResponse, RemoteMediaError>;

    async fn open_image(&self, image_ref: &RemoteImageRef) -> Result<RemoteResourceResponse, RemoteMediaError>;
}

#[derive(Clone)]
pub struct RemoteHttpClient {
    client: reqwest::Client,
}

impl RemoteHttpClient {
    pub fn new(client: reqwest::Client) -> Self { Self { client } }

    pub fn inner(&self) -> &reqwest::Client { &self.client }

    pub fn request(&self, method: Method, url: &str) -> RemoteHttpRequestBuilder {
        RemoteHttpRequestBuilder {
            safe_url: redact_remote_text(url),
            builder: self.client.request(method, url),
        }
    }
}

pub struct RemoteHttpRequestBuilder {
    safe_url: String,
    builder: RequestBuilder,
}

impl RemoteHttpRequestBuilder {
    pub fn safe_url(&self) -> &str { &self.safe_url }

    pub fn header(mut self, key: HeaderName, value: HeaderValue) -> Self {
        self.builder = self.builder.header(key, value);
        self
    }

    pub fn headers(mut self, headers: HeaderMap) -> Self {
        self.builder = self.builder.headers(headers);
        self
    }

    pub async fn send(self) -> Result<reqwest::Response, RemoteMediaError> {
        self.builder
            .send()
            .await
            .map_err(|err| RemoteMediaError::from_reqwest_error(&err).detail(format!("request {} failed", self.safe_url)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_http_request_builder_keeps_safe_url_redacted() {
        let client = RemoteHttpClient::new(reqwest::Client::new());
        let request = client.request(Method::GET, "https://media.example.invalid/video?api_key=secret");

        assert!(!request.safe_url().contains("secret"));
        assert!(request.safe_url().contains("api_key=<redacted>"));
    }
}
