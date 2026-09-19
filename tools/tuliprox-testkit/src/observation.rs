use crate::TestkitError;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct RuntimeSnapshot {
    pub status: Value,
    pub streams: Value,
    pub history: Value,
}

#[derive(Clone)]
pub struct TuliproxObserver {
    base_url: String,
    client: reqwest::Client,
    credentials: Option<(String, String)>,
}

impl TuliproxObserver {
    #[must_use]
    pub fn new(base_url: impl Into<String>, credentials: Option<(String, String)>) -> Self {
        Self { base_url: base_url.into().trim_end_matches('/').to_owned(), client: reqwest::Client::new(), credentials }
    }

    pub async fn config(&self) -> Result<Value, TestkitError> { self.get_json("config").await }

    pub async fn status(&self) -> Result<Value, TestkitError> { self.get_json("status").await }

    pub async fn streams(&self) -> Result<Value, TestkitError> { self.get_json("streams").await }

    pub async fn stream_history(&self) -> Result<Value, TestkitError> { self.get_json("stream-history").await }

    pub async fn runtime_snapshot(&self) -> Result<RuntimeSnapshot, TestkitError> {
        let (status, streams) = tokio::join!(self.status(), self.streams());
        Ok(RuntimeSnapshot { status: status?, streams: streams?, history: Value::Null })
    }

    async fn get_json(&self, endpoint: &str) -> Result<Value, TestkitError> {
        let request = self.client.get(format!("{}/{}", self.base_url, endpoint));
        let request = match &self.credentials {
            Some((username, password)) => request.basic_auth(username, Some(password)),
            None => request,
        };
        let response = request.send().await?;
        if !response.status().is_success() {
            return Err(TestkitError::Protocol(format!("observer endpoint {endpoint} returned {}", response.status())));
        }
        response.json().await.map_err(TestkitError::Http)
    }
}
