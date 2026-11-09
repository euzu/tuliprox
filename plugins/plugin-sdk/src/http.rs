//! A blocking HTTP client over `wasi:http`, usable straight from a hook.
//!
//! On `wasm32` it is built on [`wstd`](https://docs.rs/wstd): the SDK's WIT is
//! `wasi:http@0.2.12`, which is exactly the `wasip2` 1.0 binding set `wstd`
//! 0.6 uses, `wstd` is maintained by the Bytecode Alliance, and
//! `wstd::runtime::block_on` turns its async client into a plain call that
//! works inside the synchronous exports the contract defines. (`waki` is
//! pinned to wit-bindgen 0.34 and the 2024 WASI snapshot, which would put a
//! second, older binding set in every plugin.) Everywhere else it goes to the
//! [mock host](crate::mock).
//!
//! The host enforces the network capability and its limits: `http`/`https`
//! only, 30 s per attempt, 16 MiB bodies, a per-plugin rate limit and circuit
//! breaker, and no redirect following (a 3xx is returned as is).
//!
//! `block_on` must not nest: do not call the client from inside another
//! `wstd` future.

use serde::{de::DeserializeOwned, Serialize};
use std::{fmt, time::Duration};

/// An HTTP method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Method {
    Get,
    Head,
    Post,
    Put,
    Patch,
    Delete,
}

impl Method {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Head => "HEAD",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
        }
    }
}

/// Why a request produced no usable response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The URL, a header or the method was invalid.
    InvalidRequest(String),
    /// Connection, TLS, timeout, or the host refused the call.
    Transport(String),
    /// [`Response::error_for_status`] on a non-2xx response.
    Status(u16),
    /// The body was not what the caller asked for (UTF-8, JSON).
    Decode(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(msg) => write!(f, "invalid http request: {msg}"),
            Self::Transport(msg) => write!(f, "http transport error: {msg}"),
            Self::Status(code) => write!(f, "http status {code}"),
            Self::Decode(msg) => write!(f, "http body: {msg}"),
        }
    }
}

impl std::error::Error for Error {}

/// So a hook can `?` a request straight into its `Result<_, String>`.
impl From<Error> for String {
    fn from(err: Error) -> Self { err.to_string() }
}

/// A request under construction. Consumed by [`Request::send`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    method: Method,
    url: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    timeout: Option<Duration>,
}

impl Request {
    #[must_use]
    pub fn new(method: Method, url: impl Into<String>) -> Self {
        Self { method, url: url.into(), headers: Vec::new(), body: Vec::new(), timeout: None }
    }
    #[must_use]
    pub fn get(url: impl Into<String>) -> Self { Self::new(Method::Get, url) }
    #[must_use]
    pub fn post(url: impl Into<String>) -> Self { Self::new(Method::Post, url) }
    #[must_use]
    pub fn put(url: impl Into<String>) -> Self { Self::new(Method::Put, url) }
    #[must_use]
    pub fn delete(url: impl Into<String>) -> Self { Self::new(Method::Delete, url) }

    /// Adds a header (repeatable).
    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// `Authorization: Bearer <token>`.
    #[must_use]
    pub fn bearer(self, token: &str) -> Self { self.header("authorization", format!("Bearer {token}")) }

    /// A raw body.
    #[must_use]
    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }

    /// A JSON body, with `content-type: application/json`.
    pub fn json<T: Serialize + ?Sized>(self, value: &T) -> Result<Self, Error> {
        let body = serde_json::to_vec(value).map_err(|err| Error::Decode(err.to_string()))?;
        Ok(self.header("content-type", "application/json").body(body))
    }

    /// Connect, first-byte and between-bytes timeout (each).
    #[must_use]
    pub const fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    #[must_use]
    pub const fn method(&self) -> Method { self.method }
    #[must_use]
    pub fn url(&self) -> &str { &self.url }
    #[must_use]
    pub fn headers(&self) -> &[(String, String)] { &self.headers }
    #[must_use]
    pub fn body_bytes(&self) -> &[u8] { &self.body }
    #[must_use]
    pub const fn timeout_value(&self) -> Option<Duration> { self.timeout }

    /// Sends the request and reads the whole body.
    pub fn send(self) -> Result<Response, Error> { transport(&self) }
}

/// A response with its body read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    #[must_use]
    pub const fn new(status: u16, headers: Vec<(String, String)>, body: Vec<u8>) -> Self {
        Self { status, headers, body }
    }

    #[must_use]
    pub const fn status(&self) -> u16 { self.status }

    #[must_use]
    pub const fn is_success(&self) -> bool { self.status >= 200 && self.status < 300 }

    /// The first header called `name` (case-insensitive).
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
    }

    #[must_use]
    pub fn headers(&self) -> &[(String, String)] { &self.headers }

    #[must_use]
    pub fn bytes(&self) -> &[u8] { &self.body }

    /// The body as UTF-8.
    pub fn text(&self) -> Result<String, Error> {
        String::from_utf8(self.body.clone()).map_err(|err| Error::Decode(err.to_string()))
    }

    /// The body as JSON.
    pub fn json<T: DeserializeOwned>(&self) -> Result<T, Error> {
        serde_json::from_slice(&self.body).map_err(|err| Error::Decode(err.to_string()))
    }

    /// `Err(Error::Status)` unless the status is 2xx.
    pub fn error_for_status(self) -> Result<Self, Error> {
        if self.is_success() {
            Ok(self)
        } else {
            Err(Error::Status(self.status))
        }
    }
}

/// Defaults (headers, timeout) applied to every request it builds.
#[derive(Debug, Clone, Default)]
pub struct Client {
    headers: Vec<(String, String)>,
    timeout: Option<Duration>,
}

impl Client {
    #[must_use]
    pub fn new() -> Self { Self::default() }

    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    #[must_use]
    pub const fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    #[must_use]
    pub fn request(&self, method: Method, url: impl Into<String>) -> Request {
        let mut request = Request::new(method, url);
        request.headers.clone_from(&self.headers);
        request.timeout = self.timeout;
        request
    }
    #[must_use]
    pub fn get(&self, url: impl Into<String>) -> Request { self.request(Method::Get, url) }
    #[must_use]
    pub fn post(&self, url: impl Into<String>) -> Request { self.request(Method::Post, url) }
    #[must_use]
    pub fn put(&self, url: impl Into<String>) -> Request { self.request(Method::Put, url) }
    #[must_use]
    pub fn delete(&self, url: impl Into<String>) -> Request { self.request(Method::Delete, url) }
}

/// `GET url`.
pub fn get(url: impl Into<String>) -> Result<Response, Error> { Request::get(url).send() }

/// `GET url` and parse the body as JSON, failing on a non-2xx status.
pub fn get_json<T: DeserializeOwned>(url: impl Into<String>) -> Result<T, Error> {
    Request::get(url).header("accept", "application/json").send()?.error_for_status()?.json()
}

#[cfg(not(target_arch = "wasm32"))]
fn transport(request: &Request) -> Result<Response, Error> { crate::mock::http_send(request) }

#[cfg(target_arch = "wasm32")]
fn transport(request: &Request) -> Result<Response, Error> {
    use wstd::http::{Body, Client, Method as WasiMethod, Request as WasiRequest};

    wstd::runtime::block_on(async {
        let method = WasiMethod::from_bytes(request.method.as_str().as_bytes())
            .map_err(|err| Error::InvalidRequest(err.to_string()))?;
        let mut builder = WasiRequest::builder().method(method).uri(request.url.as_str());
        for (name, value) in &request.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        let body = if request.body.is_empty() { Body::empty() } else { Body::from(request.body.clone()) };
        let wasi_request = builder.body(body).map_err(|err| Error::InvalidRequest(err.to_string()))?;

        let mut client = Client::new();
        if let Some(timeout) = request.timeout {
            client.set_connect_timeout(timeout);
            client.set_first_byte_timeout(timeout);
            client.set_between_bytes_timeout(timeout);
        }
        let response = client.send(wasi_request).await.map_err(|err| Error::Transport(err.to_string()))?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(name, value)| Some((name.as_str().to_string(), value.to_str().ok()?.to_string())))
            .collect();
        let mut body = response.into_body();
        let bytes = body.bytes_contents().await.map_err(|err| Error::Transport(err.to_string()))?;
        Ok(Response::new(status, headers, bytes.to_vec()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock;

    #[test]
    fn a_queued_response_is_returned_and_the_request_recorded() {
        mock::reset();
        mock::http_respond(Method::Get, "https://api.example/v1/teams", 200, r#"{"teams":["a","b"]}"#);
        let response = Client::new()
            .header("x-api-key", "k")
            .timeout(Duration::from_secs(5))
            .get("https://api.example/v1/teams")
            .bearer("t0ken")
            .send()
            .unwrap();
        assert!(response.is_success());
        let json: serde_json::Value = response.json().unwrap();
        assert_eq!(json["teams"][1], "b");

        let sent = mock::http_requests();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].method(), Method::Get);
        assert_eq!(sent[0].timeout_value(), Some(Duration::from_secs(5)));
        assert!(sent[0].headers().contains(&("x-api-key".to_string(), "k".to_string())));
        assert!(sent[0].headers().contains(&("authorization".to_string(), "Bearer t0ken".to_string())));
    }

    #[test]
    fn responses_are_first_in_first_out_and_json_bodies_are_sent() {
        mock::reset();
        mock::http_respond(Method::Post, "https://api.example/x", 500, "boom");
        mock::http_respond(Method::Post, "https://api.example/x", 201, "");
        let first = Request::post("https://api.example/x").json(&serde_json::json!({"a": 1})).unwrap().send().unwrap();
        assert_eq!(first.status(), 500);
        assert_eq!(first.clone().error_for_status(), Err(Error::Status(500)));
        assert_eq!(Request::post("https://api.example/x").send().unwrap().status(), 201);
        let sent = mock::http_requests();
        assert_eq!(sent[0].body_bytes(), br#"{"a":1}"#);
        assert!(sent[0].headers().iter().any(|(k, v)| k == "content-type" && v == "application/json"));
    }

    #[test]
    fn an_unqueued_request_and_a_queued_failure_are_transport_errors() {
        mock::reset();
        assert!(matches!(get("https://nowhere.example/"), Err(Error::Transport(_))));
        mock::http_fail(Method::Get, "https://down.example/", "connection refused");
        assert_eq!(get("https://down.example/"), Err(Error::Transport("connection refused".into())));
    }

    #[test]
    fn response_helpers() {
        let response = Response::new(404, vec![("Content-Type".into(), "text/plain".into())], b"nope".to_vec());
        assert_eq!(response.header("content-type"), Some("text/plain"));
        assert_eq!(response.text().unwrap(), "nope");
        assert!(matches!(response.json::<u8>(), Err(Error::Decode(_))));
        let as_hook_error: std::result::Result<Response, String> = (|| Ok(response.clone().error_for_status()?))();
        assert_eq!(as_hook_error, Err("http status 404".into()));
    }
}
