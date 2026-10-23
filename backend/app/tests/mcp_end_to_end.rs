//! Real Tuliprox process + a local M3U origin. The MCP endpoint is driven over TCP exactly like a remote client does.
use serde_json::{json, Value};
use std::{
    fmt::Write as _,
    process::Stdio,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

const ADMIN: (&str, &str) = ("mcp-admin", "admin-password-1");
const VIEWER: (&str, &str) = ("mcp-viewer", "viewer-password-1");
const INPUT_USERNAME: &str = "mcp-user-secret";
const INPUT_PASSWORD: &str = "mcp-pass-secret";
const READ_ONLY_MCP: &str = "enabled: true";
const READ_WRITE_MCP: &str = "enabled: true\n  read_only: false";

// Every scenario starts its own process; serialize them like the other process tests.
static SCENARIO: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Fixture {
    directory: tempfile::TempDir,
    base: String,
    process: tokio::process::Child,
    origin: tokio::task::JoinHandle<()>,
    origin_hits: Arc<AtomicUsize>,
    client: reqwest::Client,
    _scenario: tokio::sync::MutexGuard<'static, ()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.process.start_kill();
        self.origin.abort();
    }
}

#[derive(Default)]
struct FixtureBuilder {
    mcp: Option<&'static str>,
    web_users: Vec<(&'static str, &'static str, &'static str)>,
    groups: &'static str,
}

impl FixtureBuilder {
    /// Body of the `mcp:` block, indented by two spaces after the first line.
    fn mcp(mut self, body: &'static str) -> Self {
        self.mcp = Some(body);
        self
    }

    /// Enables web auth with the given `(username, password, groups)` users and `web-groups.txt` content.
    fn web_auth(mut self, users: &[(&'static str, &'static str, &'static str)], groups: &'static str) -> Self {
        self.web_users = users.to_vec();
        self.groups = groups;
        self
    }

    async fn start(self) -> Fixture {
        let scenario = SCENARIO.lock().await;
        let (origin, origin_base, origin_hits) = start_origin().await;
        let reserved = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = reserved.local_addr().unwrap().port();
        drop(reserved);
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        for name in ["web", "storage", "backup"] {
            std::fs::create_dir(root.join(name)).unwrap();
        }

        let auth = if self.web_users.is_empty() {
            String::new()
        } else {
            let mut users = String::new();
            for (name, password, groups) in &self.web_users {
                let hash = tuliprox_auth::generate_password_from_input(password).unwrap();
                writeln!(users, "{name}:{hash}:{groups}").unwrap();
            }
            std::fs::write(root.join("web-users.txt"), users).unwrap();
            std::fs::write(root.join("web-groups.txt"), self.groups).unwrap();
            format!("  auth:\n    enabled: true\n    issuer: tuliprox\n    secret: {}\n    userfile: web-users.txt\n    groupfile: web-groups.txt\n", "0123456789abcdef".repeat(4))
        };
        let mcp = self.mcp.map_or_else(String::new, |body| format!("mcp:\n  {body}\n"));
        let config = format!(
            "api:\n  host: 127.0.0.1\n  port: {port}\n  web_root: '{}'\nworking_dir: '{}'\nbackup_dir: '{}'\nweb_ui:\n  enabled: true\n{auth}log:\n  log_level: debug\n{mcp}",
            root.join("web").display(),
            root.join("storage").display(),
            root.join("backup").display(),
        );
        let sources = format!(
            "inputs:\n  - name: mcp-origin\n    type: m3u\n    url: {origin_base}/catalog.m3u\n    username: {INPUT_USERNAME}\n    password: {INPUT_PASSWORD}\nsources:\n  - inputs: [mcp-origin]\n    targets:\n      - name: mcp-target\n        output:\n          - type: m3u\n"
        );
        let users = format!(
            "server:\n  - name: local\n    protocol: http\n    host: 127.0.0.1\n    port: '{port}'\n    timezone: UTC\n    message: MCP test\nuser:\n  - target: mcp-target\n    credentials:\n      - username: viewer\n        password: test-password\n        proxy: reverse\n        server: local\n        status: Active\n"
        );
        for (name, body) in [("config.yml", config), ("source.yml", sources), ("api-proxy.yml", users)] {
            std::fs::write(root.join(name), body).unwrap();
        }

        let log = std::fs::File::create(root.join("sut.log")).unwrap();
        let process = tokio::process::Command::new(env!("CARGO_BIN_EXE_tuliprox"))
            .args(["-s", "-c", "config.yml", "-i", "source.yml", "-a", "api-proxy.yml"])
            .current_dir(root)
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut fixture = Fixture {
            directory,
            base: format!("http://127.0.0.1:{port}"),
            process,
            origin,
            origin_hits,
            client: reqwest::Client::builder().timeout(Duration::from_secs(10)).build().unwrap(),
            _scenario: scenario,
        };
        fixture.wait_ready().await;
        fixture
    }
}

/// Serves one M3U playlist and counts the playlist downloads.
async fn start_origin() -> (tokio::task::JoinHandle<()>, String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let catalog = format!("#EXTM3U\n#EXTINF:-1 tvg-id=\"one\" group-title=\"Test\",Channel One\n{base}/live/1.ts\n");
    let hits = Arc::new(AtomicUsize::new(0));
    let counted = hits.clone();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            let (catalog, counted) = (catalog.clone(), counted.clone());
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut chunk = [0_u8; 1024];
                while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(read) => head.extend_from_slice(&chunk[..read]),
                    }
                }
                if head.starts_with(b"GET /catalog.m3u") {
                    counted.fetch_add(1, Ordering::AcqRel);
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: audio/mpegurl\r\nConnection: close\r\n\r\n{catalog}",
                    catalog.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    (task, base, hits)
}

impl Fixture {
    fn builder() -> FixtureBuilder { FixtureBuilder::default() }

    fn log(&self) -> String { std::fs::read_to_string(self.directory.path().join("sut.log")).unwrap_or_default() }

    async fn wait_ready(&mut self) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            assert!(self.process.try_wait().unwrap().is_none(), "SUT exited: {}", self.log());
            if let Ok(response) = self.client.get(format!("{}/healthcheck", self.base)).send().await {
                if response.status().is_success() {
                    return;
                }
            }
            assert!(tokio::time::Instant::now() < deadline, "SUT not ready: {}", self.log());
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn login(&self, (username, password): (&str, &str)) -> String {
        let response = self
            .client
            .post(format!("{}/auth/token", self.base))
            .json(&json!({ "username": username, "password": password }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "login failed: {}", self.log());
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["username"], username);
        body["token"].as_str().expect("token").to_owned()
    }

    /// Raw `POST /mcp`, for status and header assertions.
    async fn post_mcp(&self, token: Option<&str>, body: &Value) -> reqwest::Response {
        let mut request = self.client.post(format!("{}/mcp", self.base)).json(body);
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        request.send().await.unwrap()
    }

    /// JSON-RPC request answered with HTTP 200; returns the reply envelope.
    async fn rpc(&self, token: &str, method: &str, params: Value) -> Value {
        let response =
            self.post_mcp(Some(token), &json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params })).await;
        assert_eq!(response.status(), 200, "{method}: {}", self.log());
        let reply: Value = response.json().await.unwrap();
        assert_eq!(reply["jsonrpc"], "2.0");
        assert_eq!(reply["id"], 1);
        reply
    }

    /// `tools/call` that reaches the tool; returns `(isError, text)`.
    async fn call(&self, token: &str, tool: &str, arguments: Value) -> (bool, String) {
        let reply = self.rpc(token, "tools/call", json!({ "name": tool, "arguments": arguments })).await;
        let result = &reply["result"];
        assert!(result.is_object(), "{tool} did not reach the tool: {reply}");
        (result["isError"].as_bool().expect("isError"), result["content"][0]["text"].as_str().expect("text").to_owned())
    }

    async fn tool_names(&self, token: &str) -> Vec<String> {
        let reply = self.rpc(token, "tools/list", json!({})).await;
        reply["result"]["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .map(|tool| tool["name"].as_str().expect("tool name").to_owned())
            .collect()
    }
}

fn assert_bearer_challenge(response: &reqwest::Response) {
    assert_eq!(response.status(), 401);
    let challenge = response.headers().get("www-authenticate").expect("WWW-Authenticate").to_str().unwrap();
    assert!(challenge.starts_with("Bearer"), "challenge: {challenge}");
}

fn rpc_request(id: u64, method: &str) -> Value { json!({ "jsonrpc": "2.0", "id": id, "method": method }) }

fn admin_users() -> [(&'static str, &'static str, &'static str); 1] { [(ADMIN.0, ADMIN.1, "admin")] }

#[tokio::test]
async fn endpoint_is_not_mounted_without_an_mcp_block() {
    let fixture = Fixture::builder().start().await;
    let response = fixture.post_mcp(None, &rpc_request(1, "initialize")).await;
    let status = response.status();
    let body = response.text().await.unwrap();
    let reply = serde_json::from_str::<Value>(&body).ok();
    assert!(
        reply.as_ref().is_none_or(|reply| reply.get("jsonrpc").is_none()),
        "unexpected JSON-RPC reply ({status}): {body}"
    );
    assert!(!fixture.log().contains("MCP endpoint"), "MCP must not be announced: {}", fixture.log());
}

#[tokio::test]
async fn read_only_session_with_login_and_secret_redaction() {
    let fixture = Fixture::builder().mcp(READ_ONLY_MCP).web_auth(&admin_users(), "").start().await;

    for method in ["initialize", "tools/list"] {
        let response = fixture.post_mcp(None, &rpc_request(1, method)).await;
        assert_bearer_challenge(&response);
    }
    let response = fixture.post_mcp(Some("not-a-token"), &rpc_request(1, "tools/list")).await;
    assert_bearer_challenge(&response);
    let wrong = fixture
        .client
        .post(format!("{}/auth/token", fixture.base))
        .json(&json!({ "username": ADMIN.0, "password": "wrong-password" }))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);

    let token = fixture.login(ADMIN).await;
    let init = fixture
        .rpc(&token, "initialize", json!({ "protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {} }))
        .await;
    assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(init["result"]["serverInfo"]["name"], "tuliprox");
    assert!(init["result"]["serverInfo"]["version"].is_string());

    let tools = fixture.tool_names(&token).await;
    assert!(tools.contains(&"get_status".to_owned()), "tools: {tools:?}");
    assert!(!tools.contains(&"update_playlists".to_owned()), "tools: {tools:?}");

    let denied = fixture.rpc(&token, "tools/call", json!({ "name": "update_playlists", "arguments": {} })).await;
    assert_eq!(denied["error"]["code"], -32602, "{denied}");
    assert!(denied.get("result").is_none());
    assert_eq!(fixture.origin_hits.load(Ordering::Acquire), 0, "read-only mode must not start an update");

    let (is_error, text) = fixture.call(&token, "get_status", json!({})).await;
    assert!(!is_error, "get_status: {text}");
    let status: Value = serde_json::from_str(&text).expect("status is JSON");
    assert_eq!(status["status"], "ok", "{text}");

    let (is_error, config) = fixture.call(&token, "get_config", json!({})).await;
    assert!(!is_error, "get_config: {config}");
    assert!(serde_json::from_str::<Value>(&config).is_ok(), "get_config is JSON: {config}");
    assert!(config.contains("mcp-origin"), "the input must still be listed: {config}");
    assert!(!config.contains(INPUT_USERNAME), "username leaked: {config}");
    assert!(!config.contains(INPUT_PASSWORD), "password leaked: {config}");
    assert!(config.contains("********"), "redaction marker missing: {config}");
}

#[tokio::test]
async fn write_tool_is_forbidden_for_a_group_without_playlist_write() {
    let fixture = Fixture::builder()
        .mcp(READ_WRITE_MCP)
        .web_auth(&[(VIEWER.0, VIEWER.1, "viewer")], "viewer:system.read,playlist.read\n")
        .start()
        .await;
    let token = fixture.login(VIEWER).await;

    let tools = fixture.tool_names(&token).await;
    assert!(tools.contains(&"update_playlists".to_owned()), "tools: {tools:?}");

    let (is_error, text) = fixture.call(&token, "update_playlists", json!({})).await;
    assert!(is_error, "expected an error result: {text}");
    assert!(text.starts_with("HTTP 403"), "text: {text}");
    assert_eq!(fixture.origin_hits.load(Ordering::Acquire), 0, "a forbidden call must not start an update");

    let (is_error, text) = fixture.call(&token, "get_status", json!({})).await;
    assert!(!is_error, "read permission still works: {text}");
}

#[tokio::test]
async fn admin_triggers_a_playlist_update_and_polls_its_status() {
    let fixture = Fixture::builder().mcp(READ_WRITE_MCP).web_auth(&admin_users(), "").start().await;
    let token = fixture.login(ADMIN).await;
    assert!(fixture.tool_names(&token).await.contains(&"update_playlists".to_owned()));
    assert_eq!(fixture.origin_hits.load(Ordering::Acquire), 0, "update_on_boot is off");

    let (is_error, text) = fixture.call(&token, "update_playlists", json!({})).await;
    assert!(!is_error, "update_playlists: {text}");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let finished = loop {
        let (is_error, text) = fixture.call(&token, "get_playlist_update_status", json!({})).await;
        assert!(!is_error, "get_playlist_update_status: {text}");
        let status: Value = serde_json::from_str(&text).expect("status is JSON");
        let input = &status["inputs"][0];
        if input["last_input_update"].is_object() {
            break input["last_input_update"].clone();
        }
        assert!(tokio::time::Instant::now() < deadline, "update did not finish: {status}\n{}", fixture.log());
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert_eq!(finished["state"], "Success", "{finished}\n{}", fixture.log());
    assert!(finished["timestamp"].as_u64().is_some_and(|timestamp| timestamp > 0), "{finished}");
    assert!(fixture.origin_hits.load(Ordering::Acquire) >= 1, "the origin was never requested");
}

#[tokio::test]
async fn transport_rules_for_notifications_methods_and_origin() {
    let fixture = Fixture::builder()
        .mcp("enabled: true\n  allowed_origins: ['https://assistant.example/']")
        .web_auth(&admin_users(), "")
        .start()
        .await;
    let token = fixture.login(ADMIN).await;

    let notification = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
    let response = fixture.post_mcp(Some(&token), &notification).await;
    assert_eq!(response.status(), 202);
    assert_eq!(response.text().await.unwrap(), "");

    let response = fixture.client.get(format!("{}/mcp", fixture.base)).bearer_auth(&token).send().await.unwrap();
    assert_eq!(response.status(), 405);
    assert_eq!(response.headers().get("allow").expect("Allow").to_str().unwrap(), "POST");

    let ping = rpc_request(7, "ping");
    let post_with_origin = |origin: String| {
        fixture.client.post(format!("{}/mcp", fixture.base)).bearer_auth(&token).header("Origin", origin).json(&ping)
    };
    let response = post_with_origin("http://evil.example".to_owned()).send().await.unwrap();
    assert_eq!(response.status(), 403);
    assert_eq!(response.json::<Value>().await.unwrap()["error"], "Origin not allowed");

    let host = fixture.base.clone();
    for origin in [host, "https://assistant.example".to_owned()] {
        let response = post_with_origin(origin.clone()).send().await.unwrap();
        assert_eq!(response.status(), 200, "origin {origin}");
        let reply: Value = response.json().await.unwrap();
        assert_eq!(reply["id"], 7);
        assert!(reply["result"].is_object(), "origin {origin}: {reply}");
    }
}
