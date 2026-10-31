//! stdio bridge for the MCP endpoint of a running instance.
//!
//! Reads newline-delimited JSON-RPC from a reader, POSTs every message to `<base>/mcp` and writes each
//! response as exactly one line to a writer. stdout carries protocol JSON only; diagnostics go to stderr.

use reqwest::{header::AUTHORIZATION, Client, StatusCode};
use serde_json::{json, Value};
use shared::model::{TokenResponse, UserCredential};
use std::{io, time::Duration};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

pub const ENV_MCP_USER: &str = "TULIPROX_MCP_USER";
pub const ENV_MCP_PASSWORD: &str = "TULIPROX_MCP_PASSWORD";

const CODE_UPSTREAM_FAILURE: i64 = -32000;
const CODE_UNAUTHORIZED: i64 = -32001;
const CODE_INTERNAL_ERROR: i64 = -32603;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_mins(5);

struct BridgeError {
    code: i64,
    message: String,
}

impl BridgeError {
    fn unauthorized() -> Self { Self { code: CODE_UNAUTHORIZED, message: "Unauthorized".to_string() } }

    fn upstream(message: String) -> Self { Self { code: CODE_UPSTREAM_FAILURE, message } }

    fn into_reply(self, id: &Value) -> String {
        json!({"jsonrpc": "2.0", "id": id, "error": {"code": self.code, "message": self.message}}).to_string()
    }
}

/// What to write for one request line.
enum Reply {
    /// One compact JSON document.
    Line(String),
    /// Accepted notification, nothing to write.
    None,
}

pub struct McpStdioBridge {
    client: Client,
    mcp_endpoint: String,
    token_endpoint: String,
    credentials: Option<UserCredential>,
    token: Option<String>,
}

impl McpStdioBridge {
    /// `base_url` is the full base of the instance including an optional `web_ui.path` prefix.
    pub fn new(base_url: &str, credentials: Option<UserCredential>) -> Result<Self, reqwest::Error> {
        let base = base_url.trim_end_matches('/');
        let client = Client::builder().connect_timeout(CONNECT_TIMEOUT).timeout(REQUEST_TIMEOUT).build()?;
        Ok(Self {
            client,
            mcp_endpoint: format!("{base}/mcp"),
            token_endpoint: format!("{base}/auth/token"),
            credentials,
            token: None,
        })
    }

    /// Credentials from `TULIPROX_MCP_USER` / `TULIPROX_MCP_PASSWORD`; `None` unless both are set and non-empty.
    pub fn credentials_from_env() -> Option<UserCredential> {
        let username = std::env::var(ENV_MCP_USER).ok().filter(|v| !v.is_empty());
        let password = std::env::var(ENV_MCP_PASSWORD).ok().filter(|v| !v.is_empty());
        match (username, password) {
            (Some(username), Some(password)) => Some(UserCredential { username, password }),
            (None, None) => None,
            _ => {
                eprintln!("{ENV_MCP_USER} and {ENV_MCP_PASSWORD} must both be set; continuing without credentials");
                None
            }
        }
    }

    /// Process lines until the reader reaches EOF. Only I/O failures on the reader or writer end the loop.
    pub async fn run<R, W>(&mut self, reader: R, mut writer: W) -> io::Result<()>
    where
        R: AsyncBufRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut lines = reader.lines();
        while let Some(line) = lines.next_line().await? {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Reply::Line(mut out) = self.handle_line(line).await {
                out.push('\n');
                writer.write_all(out.as_bytes()).await?;
                writer.flush().await?;
            }
        }
        Ok(())
    }

    async fn handle_line(&mut self, line: &str) -> Reply {
        match self.forward(line).await {
            Ok(reply) => reply,
            Err(err) => {
                eprintln!("mcp-stdio: {} ({})", err.message, err.code);
                let request = serde_json::from_str::<Value>(line).ok();
                if request.as_ref().is_some_and(is_notification) {
                    return Reply::None;
                }
                let id = request.and_then(|v| v.get("id").cloned()).unwrap_or(Value::Null);
                Reply::Line(err.into_reply(&id))
            }
        }
    }

    async fn forward(&mut self, line: &str) -> Result<Reply, BridgeError> {
        if self.credentials.is_some() && self.token.is_none() {
            self.login().await?;
        }
        let mut response = self.post(line).await?;
        if response.status() == StatusCode::UNAUTHORIZED {
            if self.credentials.is_none() {
                eprintln!("mcp-stdio: the server requires authentication; set {ENV_MCP_USER} and {ENV_MCP_PASSWORD}");
                return Err(BridgeError::unauthorized());
            }
            self.login().await?;
            response = self.post(line).await?;
            if response.status() == StatusCode::UNAUTHORIZED {
                return Err(BridgeError::unauthorized());
            }
        }
        Self::into_reply(response).await
    }

    async fn post(&self, body: &str) -> Result<reqwest::Response, BridgeError> {
        let mut request = self
            .client
            .post(&self.mcp_endpoint)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "application/json, text/event-stream")
            .body(body.to_string());
        if let Some(token) = &self.token {
            request = request.header(AUTHORIZATION, format!("Bearer {token}"));
        }
        request.send().await.map_err(|err| BridgeError::upstream(format!("request to MCP endpoint failed: {err}")))
    }

    async fn login(&mut self) -> Result<(), BridgeError> {
        self.token = None;
        let Some(credentials) = &self.credentials else {
            return Err(BridgeError::unauthorized());
        };
        let response = self
            .client
            .post(&self.token_endpoint)
            .json(credentials)
            .send()
            .await
            .map_err(|err| BridgeError::upstream(format!("login request failed: {err}")))?;
        let status = response.status();
        if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            eprintln!("mcp-stdio: login rejected with HTTP {status}");
            return Err(BridgeError::unauthorized());
        }
        if !status.is_success() {
            return Err(BridgeError::upstream(format!("login failed with HTTP {status}")));
        }
        let token = response
            .json::<TokenResponse>()
            .await
            .map_err(|err| BridgeError::upstream(format!("invalid login response: {err}")))?
            .token;
        self.token = Some(token);
        Ok(())
    }

    async fn into_reply(response: reqwest::Response) -> Result<Reply, BridgeError> {
        let status = response.status();
        if status == StatusCode::ACCEPTED {
            return Ok(Reply::None);
        }
        let body =
            response.text().await.map_err(|err| BridgeError::upstream(format!("cannot read MCP response: {err}")))?;
        if body.trim().is_empty() {
            return if status.is_success() {
                Ok(Reply::None)
            } else {
                Err(BridgeError { code: CODE_INTERNAL_ERROR, message: format!("HTTP {status}") })
            };
        }
        // Re-serialising guarantees a single line and filters non-JSON bodies (e.g. proxy error pages).
        match serde_json::from_str::<Value>(&body) {
            Ok(value) if value.is_object() => Ok(Reply::Line(value.to_string())),
            _ => Err(BridgeError { code: CODE_INTERNAL_ERROR, message: format!("HTTP {status}: non-JSON response") }),
        }
    }
}

fn is_notification(request: &Value) -> bool { request.get("method").is_some() && request.get("id").is_none() }

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::State,
        http::{HeaderMap, StatusCode as AxumStatus},
        response::IntoResponse,
        routing::post,
        Json, Router,
    };
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio::io::BufReader;

    const GOOD_PASSWORD: &str = "secret";

    #[derive(Default)]
    struct Stub {
        logins: AtomicUsize,
        mcp_calls: AtomicUsize,
        /// Every MCP call answers 401 regardless of the token.
        always_unauthorized: bool,
        /// Token issued on the next login; requests with any other token get 401.
        require_auth: bool,
        /// First token issued is already rejected, later ones are accepted.
        expire_first_token: bool,
    }

    async fn login_handler(State(stub): State<Arc<Stub>>, Json(req): Json<UserCredential>) -> impl IntoResponse {
        if req.username != "bob" || req.password != GOOD_PASSWORD {
            return AxumStatus::UNAUTHORIZED.into_response();
        }
        let n = stub.logins.fetch_add(1, Ordering::SeqCst) + 1;
        Json(json!({"token": format!("tok{n}"), "username": "bob"})).into_response()
    }

    async fn mcp_handler(State(stub): State<Arc<Stub>>, headers: HeaderMap, body: String) -> impl IntoResponse {
        stub.mcp_calls.fetch_add(1, Ordering::SeqCst);
        let bearer = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
        if stub.always_unauthorized {
            return AxumStatus::UNAUTHORIZED.into_response();
        }
        if stub.require_auth {
            let valid = if stub.expire_first_token { bearer == "Bearer tok2" } else { bearer == "Bearer tok1" };
            if !valid {
                return AxumStatus::UNAUTHORIZED.into_response();
            }
        }
        let Ok(request) = serde_json::from_str::<Value>(&body) else {
            return (AxumStatus::BAD_REQUEST, "not json").into_response();
        };
        if request.get("id").is_none() {
            return AxumStatus::ACCEPTED.into_response();
        }
        // Pretty-printed on purpose: the bridge must still emit a single line.
        let pretty = serde_json::to_string_pretty(&json!({
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": {"echo": request["method"], "auth": bearer}
        }))
        .unwrap_or_default();
        ([("content-type", "application/json")], pretty).into_response()
    }

    async fn spawn_stub(stub: Stub) -> (String, Arc<Stub>) {
        let stub = Arc::new(stub);
        let app = Router::new()
            .route("/mcp", post(mcp_handler))
            .route("/auth/token", post(login_handler))
            .with_state(Arc::clone(&stub));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), stub)
    }

    fn creds(password: &str) -> UserCredential {
        UserCredential { username: "bob".to_string(), password: password.to_string() }
    }

    async fn pump(bridge: &mut McpStdioBridge, input: &str) -> Vec<String> {
        let mut out: Vec<u8> = Vec::new();
        bridge.run(BufReader::new(input.as_bytes()), &mut out).await.unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.is_empty() || text.ends_with('\n'));
        text.lines().map(str::to_string).collect()
    }

    fn parse(line: &str) -> Value { serde_json::from_str(line).unwrap() }

    #[tokio::test]
    async fn passes_request_through_without_credentials() {
        let (url, stub) = spawn_stub(Stub::default()).await;
        let mut bridge = McpStdioBridge::new(&url, None).unwrap();
        let lines = pump(&mut bridge, "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n").await;
        assert_eq!(lines.len(), 1);
        let reply = parse(&lines[0]);
        assert_eq!(reply["id"], 1);
        assert_eq!(reply["result"]["echo"], "ping");
        assert_eq!(reply["result"]["auth"], "");
        assert_eq!(stub.logins.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn trailing_slash_in_base_url_is_ignored() {
        let (url, _stub) = spawn_stub(Stub::default()).await;
        let mut bridge = McpStdioBridge::new(&format!("{url}/"), None).unwrap();
        let lines = pump(&mut bridge, "{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"ping\"}\n").await;
        assert_eq!(parse(&lines[0])["id"], 7);
    }

    #[tokio::test]
    async fn accepted_notification_writes_nothing() {
        let (url, stub) = spawn_stub(Stub::default()).await;
        let mut bridge = McpStdioBridge::new(&url, None).unwrap();
        let lines = pump(&mut bridge, "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n").await;
        assert_eq!(lines, Vec::<String>::new());
        assert_eq!(stub.mcp_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn eof_terminates_and_blank_lines_are_skipped() {
        let (url, stub) = spawn_stub(Stub::default()).await;
        let mut bridge = McpStdioBridge::new(&url, None).unwrap();
        assert_eq!(pump(&mut bridge, "").await, Vec::<String>::new());
        assert_eq!(pump(&mut bridge, "\n  \n\r\n").await, Vec::<String>::new());
        assert_eq!(stub.mcp_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn every_message_is_exactly_one_line_in_order() {
        let (url, _stub) = spawn_stub(Stub::default()).await;
        let mut bridge = McpStdioBridge::new(&url, None).unwrap();
        let input = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"a\"}\n\
                     {\"jsonrpc\":\"2.0\",\"method\":\"notifications/x\"}\n\
                     {\"jsonrpc\":\"2.0\",\"id\":\"two\",\"method\":\"b\"}";
        let lines = pump(&mut bridge, input).await;
        assert_eq!(lines.len(), 2);
        assert_eq!(parse(&lines[0])["id"], 1);
        assert_eq!(parse(&lines[1])["id"], "two");
        assert!(lines.iter().all(|l| !l.contains('\n') && !l.contains('\r')));
    }

    #[tokio::test]
    async fn logs_in_lazily_once_and_sends_bearer_token() {
        let (url, stub) = spawn_stub(Stub { require_auth: true, ..Stub::default() }).await;
        let mut bridge = McpStdioBridge::new(&url, Some(creds(GOOD_PASSWORD))).unwrap();
        assert_eq!(stub.logins.load(Ordering::SeqCst), 0);
        let input =
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"a\"}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"b\"}\n";
        let lines = pump(&mut bridge, input).await;
        assert_eq!(lines.len(), 2);
        assert_eq!(parse(&lines[0])["result"]["auth"], "Bearer tok1");
        assert_eq!(parse(&lines[1])["result"]["auth"], "Bearer tok1");
        assert_eq!(stub.logins.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn relogs_in_once_after_401_and_retries() {
        let (url, stub) = spawn_stub(Stub { require_auth: true, expire_first_token: true, ..Stub::default() }).await;
        let mut bridge = McpStdioBridge::new(&url, Some(creds(GOOD_PASSWORD))).unwrap();
        let lines = pump(&mut bridge, "{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"a\"}\n").await;
        assert_eq!(lines.len(), 1);
        let reply = parse(&lines[0]);
        assert_eq!(reply["id"], 5);
        assert_eq!(reply["result"]["auth"], "Bearer tok2");
        assert_eq!(stub.logins.load(Ordering::SeqCst), 2);
        assert_eq!(stub.mcp_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn second_401_yields_unauthorized_error_with_echoed_id() {
        let (url, stub) = spawn_stub(Stub { always_unauthorized: true, ..Stub::default() }).await;
        let mut bridge = McpStdioBridge::new(&url, Some(creds(GOOD_PASSWORD))).unwrap();
        let lines = pump(&mut bridge, "{\"jsonrpc\":\"2.0\",\"id\":\"abc\",\"method\":\"a\"}\n").await;
        assert_eq!(lines.len(), 1);
        let reply = parse(&lines[0]);
        assert_eq!(reply["id"], "abc");
        assert_eq!(reply["error"]["code"], -32001);
        assert_eq!(reply["error"]["message"], "Unauthorized");
        // One retry only: initial login + one re-login, two MCP attempts.
        assert_eq!(stub.logins.load(Ordering::SeqCst), 2);
        assert_eq!(stub.mcp_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn unauthorized_without_credentials_replies_with_error() {
        let (url, stub) = spawn_stub(Stub { always_unauthorized: true, ..Stub::default() }).await;
        let mut bridge = McpStdioBridge::new(&url, None).unwrap();
        let lines = pump(&mut bridge, "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"a\"}\n").await;
        assert_eq!(parse(&lines[0])["error"]["code"], -32001);
        assert_eq!(stub.mcp_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn rejected_login_yields_unauthorized_error() {
        let (url, stub) = spawn_stub(Stub { require_auth: true, ..Stub::default() }).await;
        let mut bridge = McpStdioBridge::new(&url, Some(creds("wrong"))).unwrap();
        let lines = pump(&mut bridge, "{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"a\"}\n").await;
        let reply = parse(&lines[0]);
        assert_eq!(reply["id"], 9);
        assert_eq!(reply["error"]["code"], -32001);
        assert_eq!(stub.mcp_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn network_failure_yields_error_reply_and_loop_continues() {
        // Bind then drop to obtain a port nobody listens on.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let mut bridge = McpStdioBridge::new(&format!("http://{addr}"), None).unwrap();
        let input = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"a\"}\n\
                     {\"jsonrpc\":\"2.0\",\"method\":\"notifications/x\"}\n\
                     not json at all\n\
                     {\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"b\"}\n";
        let lines = pump(&mut bridge, input).await;
        // The notification gets no reply; the unparseable line gets an error with a null id.
        assert_eq!(lines.len(), 3);
        assert_eq!(parse(&lines[0])["id"], 1);
        assert_eq!(parse(&lines[0])["error"]["code"], -32000);
        assert_eq!(parse(&lines[1])["id"], Value::Null);
        assert_eq!(parse(&lines[2])["id"], 2);
    }

    #[tokio::test]
    async fn non_json_upstream_body_becomes_error_reply() {
        async fn html() -> impl IntoResponse { (AxumStatus::BAD_GATEWAY, "<html>bad gateway</html>") }
        let app = Router::new().route("/mcp", post(html));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut bridge = McpStdioBridge::new(&format!("http://{addr}"), None).unwrap();
        let lines = pump(&mut bridge, "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"a\"}\n").await;
        let reply = parse(&lines[0]);
        assert_eq!(reply["id"], 4);
        assert_eq!(reply["error"]["code"], -32603);
    }
}
