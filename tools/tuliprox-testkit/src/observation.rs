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

    pub async fn stream_history(&self, session_id: u64) -> Result<Value, TestkitError> {
        let timestamp = i64::try_from(session_id >> 32)
            .map_err(|error| TestkitError::Protocol(format!("invalid stream history session timestamp: {error}")))?;
        let date = chrono::DateTime::from_timestamp(timestamp, 0)
            .ok_or_else(|| TestkitError::Protocol("invalid stream history session timestamp".to_owned()))?
            .format("%Y-%m-%d");
        self.get_json(&format!("stream-history?from={date}")).await
    }

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
