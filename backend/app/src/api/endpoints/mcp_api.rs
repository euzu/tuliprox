//! Model Context Protocol endpoint (`POST {web_ui_path}/mcp`).
//!
//! Stateless JSON-RPC 2.0 over plain `application/json` responses. A
//! `tools/call` is dispatched in-process into the `/api/v1` router with the
//! caller's `Authorization` header, so the existing permission layers decide
//! what the caller may do; nothing is re-implemented here.

use super::{
    mcp_redact::{redact_json_body, REDACT_BODY_LIMIT},
    mcp_tools::{find_tool, McpTool, McpToolKind, McpToolMethod, MCP_TOOLS},
};
use crate::{
    api::{auth_middleware::validator_authenticated, endpoints::v1_api::API_V1_PATH, model::AppState},
    model::McpConfig,
    VERSION,
};
use axum::{
    body::{Body, Bytes},
    http::{header, HeaderMap, Request, StatusCode},
    response::{IntoResponse, Response},
    Extension, Router,
};
use http_body_util::BodyExt;
use log::{info, warn};
use serde_json::{json, Map, Value};
use shared::utils::concat_path_leading_slash;
use std::{fmt::Write as _, sync::Arc};
use tower::ServiceExt;

const MCP_PATH: &str = "mcp";
const SUPPORTED_PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

/// `InitializeResult.instructions`: a short usage guide shown to the client.
const MCP_INSTRUCTIONS: &str = "\
Tuliprox is an IPTV playlist proxy and processor. These tools inspect its configuration, streams and stored playlists.
Playlist workflow: call list_playlist_sources first (targets and inputs are addressed by NAME), then query_playlist_items \
for one target or input and one cluster (live, video, series). Reading is always safe: only stored data is read and no \
provider is contacted.
Paging: results carry total_matched (all matches), returned and has_more. Request the next page with \
offset = offset + returned while has_more is true. limit is clamped (items 500). Narrow with search, group, fields.
Secrets: provider URLs and credentials are never returned and any secret-like value is redacted.
Debug EPG: query_playlist_items with epg_status=unmatched (or missing_id) finds items whose EPG id is not in the EPG \
database; query_target_epg_channels with issue=unreferenced, expired, no_programmes, not_current or no_title finds broken \
EPG channels; get_target_epg_channel shows the programmes of one channel. An empty EPG result with a `note` means no \
EPG data exists, not that everything is fine.
The server is read-only by default; update tools are listed only when write access is enabled.";

const PARSE_ERROR: i32 = -32700;
const INVALID_REQUEST: i32 = -32600;
const METHOD_NOT_FOUND: i32 = -32601;
const INVALID_PARAMS: i32 = -32602;

struct McpState {
    /// The `/api/v1` router with its state applied.
    router: Router,
    api_prefix: String,
    read_only: bool,
    allowed_origins: Vec<String>,
    max_response_bytes: usize,
}

/// Web auth disabled means no permission layer protects the write routes, so
/// the MCP server never exposes them in that mode.
const fn effective_read_only(configured_read_only: bool, web_auth_enabled: bool) -> bool {
    configured_read_only || !web_auth_enabled
}

/// Builds the MCP router when `mcp.enabled` is set. `v1_router` is the same
/// router that serves `/api/v1`, so a tool call is checked exactly like the
/// REST call it stands for.
pub fn mcp_api_register_if_enabled<S>(
    v1_router: &Router<Arc<AppState>>,
    app_state: &Arc<AppState>,
    web_auth_enabled: bool,
    web_ui_path: &str,
) -> Option<Router<S>>
where
    S: Clone + Send + Sync + 'static,
{
    let config = app_state.app_config.config.load();
    let mcp = config.mcp.as_ref().filter(|mcp| mcp.enabled)?;
    let read_only = effective_read_only(mcp.read_only, web_auth_enabled);
    if !web_auth_enabled {
        warn!(
            "MCP is enabled but web auth is disabled: every client that can reach {} is unauthenticated; write tools are forced off",
            concat_path_leading_slash(web_ui_path, MCP_PATH)
        );
    }
    let state =
        Arc::new(McpState::new(v1_router.clone().with_state(Arc::clone(app_state)), mcp, read_only, web_ui_path));
    info!(
        "MCP endpoint: POST {} ({})",
        concat_path_leading_slash(web_ui_path, MCP_PATH),
        if read_only { "read-only" } else { "read/write" }
    );
    Some(mcp_api_register(state, app_state, web_ui_path))
}

impl McpState {
    fn new(router: Router, config: &McpConfig, read_only: bool, web_ui_path: &str) -> Self {
        Self {
            router,
            api_prefix: concat_path_leading_slash(web_ui_path, API_V1_PATH),
            read_only,
            allowed_origins: config.allowed_origins.clone(),
            max_response_bytes: config.max_response_bytes,
        }
    }
}

/// The route sits behind the same authentication layer as the other
/// authenticated `/api/v1` routes, so a missing, invalid or expired token is a
/// transport-level `401` with `WWW-Authenticate: Bearer` for every JSON-RPC
/// method. The layer is a no-op while web auth is disabled.
fn mcp_api_register<S>(state: Arc<McpState>, app_state: &Arc<AppState>, web_ui_path: &str) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .route(&concat_path_leading_slash(web_ui_path, MCP_PATH), axum::routing::post(mcp_post))
        .layer(axum::middleware::from_fn_with_state(Arc::clone(app_state), validator_authenticated))
        .layer(Extension(state))
}

async fn mcp_post(Extension(state): Extension<Arc<McpState>>, headers: HeaderMap, body: Bytes) -> Response {
    if !origin_allowed(&headers, &state.allowed_origins) {
        return (StatusCode::FORBIDDEN, axum::Json(json!({ "error": "Origin not allowed" }))).into_response();
    }
    match handle_message(&state, &headers, &body).await {
        Some(reply) => axum::Json(reply).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

/// DNS-rebinding guard required by the Streamable HTTP transport: a request
/// that carries an `Origin` must come from the host it was sent to or from an
/// explicitly allowed origin.
///
/// A reverse proxy that rewrites the `Host` header makes a legitimate browser
/// origin differ from the `Host` this server sees; list such origins in
/// `mcp.allowed_origins`.
fn origin_allowed(headers: &HeaderMap, allowed_origins: &[String]) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    let origin = origin.trim().trim_end_matches('/');
    if allowed_origins.iter().any(|allowed| allowed.eq_ignore_ascii_case(origin)) {
        return true;
    }
    let origin_authority = origin.split_once("://").map(|(_, authority)| authority).filter(|a| !a.is_empty());
    let host = headers.get(header::HOST).and_then(|host| host.to_str().ok()).map(str::trim);
    matches!((origin_authority, host), (Some(authority), Some(host)) if authority.eq_ignore_ascii_case(host))
}

fn negotiate_protocol_version(requested: Option<&str>) -> &'static str {
    requested
        .and_then(|requested| SUPPORTED_PROTOCOL_VERSIONS.iter().find(|supported| **supported == requested))
        .copied()
        .unwrap_or(SUPPORTED_PROTOCOL_VERSIONS[0])
}

fn rpc_result(id: &Value, result: Value) -> Value {
    let mut reply = json!({ "jsonrpc": "2.0", "id": id });
    reply["result"] = result;
    reply
}

fn rpc_error(id: &Value, code: i32, message: impl Into<String>) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message.into() } })
}

/// `None` means "notification, answer 202 without a body".
async fn handle_message(state: &McpState, headers: &HeaderMap, body: &[u8]) -> Option<Value> {
    let message: Value = match serde_json::from_slice(body) {
        Ok(message) => message,
        Err(err) => return Some(rpc_error(&Value::Null, PARSE_ERROR, format!("Parse error: {err}"))),
    };
    let Value::Object(message) = message else {
        let reason = if message.is_array() { "Batch requests are not supported" } else { "Invalid request" };
        return Some(rpc_error(&Value::Null, INVALID_REQUEST, reason));
    };
    let id = match message.get("id") {
        None => None,
        Some(id @ (Value::String(_) | Value::Number(_))) => Some(id),
        Some(_) => return Some(rpc_error(&Value::Null, INVALID_REQUEST, "id must be a string or a number")),
    };
    let reply_id = id.unwrap_or(&Value::Null);
    if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Some(rpc_error(reply_id, INVALID_REQUEST, "jsonrpc must be \"2.0\""));
    }
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return Some(rpc_error(reply_id, INVALID_REQUEST, "method is required"));
    };
    // A message without an id is a notification; it never gets a JSON-RPC reply.
    let id = id?;
    let params = message.get("params");

    Some(match method {
        "initialize" => rpc_result(id, initialize_result(params)),
        "ping" => rpc_result(id, json!({})),
        "tools/list" => rpc_result(id, json!({ "tools": tools_list(state.read_only) })),
        "tools/call" => match call_tool(state, headers, params).await {
            Ok(result) => rpc_result(id, result),
            Err(message) => rpc_error(id, INVALID_PARAMS, message),
        },
        other => rpc_error(id, METHOD_NOT_FOUND, format!("Method not found: {other}")),
    })
}

fn initialize_result(params: Option<&Value>) -> Value {
    let requested = params.and_then(|params| params.get("protocolVersion")).and_then(Value::as_str);
    json!({
        "protocolVersion": negotiate_protocol_version(requested),
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "tuliprox", "version": VERSION },
        "instructions": MCP_INSTRUCTIONS,
    })
}

fn visible_tools(read_only: bool) -> impl Iterator<Item = &'static McpTool> {
    MCP_TOOLS.iter().filter(move |tool| !read_only || tool.kind == McpToolKind::Read)
}

fn tools_list(read_only: bool) -> Vec<Value> {
    visible_tools(read_only)
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "inputSchema": tool.schema(),
                "annotations": { "readOnlyHint": tool.kind == McpToolKind::Read },
            })
        })
        .collect()
}

/// `Err` is a JSON-RPC protocol error (-32602); everything the REST route
/// answers, failures included, is an `isError` tool result instead.
async fn call_tool(state: &McpState, headers: &HeaderMap, params: Option<&Value>) -> Result<Value, String> {
    let params = params.and_then(Value::as_object).ok_or("params must be an object")?;
    let name = params.get("name").and_then(Value::as_str).ok_or("params.name is required")?;
    let tool = find_tool(name).ok_or_else(|| format!("Unknown tool: {name}"))?;
    if state.read_only && tool.kind == McpToolKind::Write {
        return Err(format!("Tool '{name}' is not available in read-only mode"));
    }
    let arguments = match params.get("arguments") {
        None | Some(Value::Null) => None,
        Some(Value::Object(arguments)) => Some(arguments),
        Some(_) => return Ok(tool_result(true, "arguments must be an object")),
    };
    let tool_request = match build_tool_request(tool, arguments, &state.api_prefix) {
        Ok(tool_request) => tool_request,
        Err(message) => return Ok(tool_result(true, &message)),
    };

    let mut request = Request::builder()
        .method(tool.method.as_http())
        .uri(&tool_request.uri)
        .header(header::ACCEPT, "application/json");
    if let Some(authorization) = headers.get(header::AUTHORIZATION) {
        request = request.header(header::AUTHORIZATION, authorization);
    }
    let body = match tool_request.body {
        Some(body) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(body)
        }
        None => Body::empty(),
    };
    let Ok(request) = request.body(body) else {
        return Ok(tool_result(true, "Failed to build request"));
    };

    let response = match state.router.clone().oneshot(request).await {
        Ok(response) => response,
        Err(never) => match never {},
    };
    let status = response.status();
    let text = if !tool.redact_secrets {
        let bytes = read_capped(response.into_body(), state.max_response_bytes).await;
        body_to_text(&bytes, state.max_response_bytes)
    } else if status.is_success() {
        // Redaction needs the whole document; truncation applies to the redacted text.
        let bytes = read_capped(response.into_body(), REDACT_BODY_LIMIT).await;
        body_to_text(redact_json_body(&bytes).as_bytes(), state.max_response_bytes)
    } else {
        // An error body of a redacted tool is not forwarded; the status is enough.
        String::new()
    };
    Ok(if status.is_success() {
        tool_result(false, &text)
    } else if text.is_empty() {
        tool_result(true, &format!("HTTP {status}"))
    } else {
        tool_result(true, &format!("HTTP {status}: {text}"))
    })
}

fn tool_result(is_error: bool, text: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

/// Reads at most `max + 1` bytes: the extra byte tells the caller the body
/// was cut without buffering an unbounded response.
async fn read_capped(mut body: Body, max: usize) -> Vec<u8> {
    let mut collected = Vec::new();
    while let Some(Ok(frame)) = body.frame().await {
        let Some(data) = frame.data_ref() else {
            continue;
        };
        collected.extend_from_slice(data);
        if collected.len() > max {
            collected.truncate(max + 1);
            break;
        }
    }
    collected
}

fn body_to_text(bytes: &[u8], max: usize) -> String {
    let truncated = bytes.len() > max;
    let slice = &bytes[..bytes.len().min(max)];
    let mut text = match std::str::from_utf8(slice) {
        Ok(text) => text.to_owned(),
        // The cut may split a multi-byte character; drop the partial tail.
        Err(err) if truncated && err.error_len().is_none() => {
            String::from_utf8_lossy(&slice[..err.valid_up_to()]).into_owned()
        }
        Err(_) => String::from_utf8_lossy(slice).into_owned(),
    };
    if truncated {
        let _ = write!(text, "\n[truncated: response exceeded {max} bytes]");
    }
    text
}

struct ToolRequest {
    uri: String,
    body: Option<Vec<u8>>,
}

fn build_tool_request(
    tool: &McpTool,
    arguments: Option<&Map<String, Value>>,
    api_prefix: &str,
) -> Result<ToolRequest, String> {
    let empty = Map::new();
    let arguments = arguments.unwrap_or(&empty);
    let schema = tool.schema();
    let properties = schema.get("properties").and_then(Value::as_object);
    if let Some(unknown) = arguments.keys().find(|key| !properties.is_some_and(|props| props.contains_key(*key))) {
        return Err(format!("Unknown argument '{unknown}'"));
    }
    let required = schema.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str);
    for field in required {
        if arguments.get(field).is_none_or(Value::is_null) {
            return Err(format!("Missing required argument '{field}'"));
        }
    }

    let (path, used) = expand_path(tool.path, arguments)?;
    let rest = arguments.iter().filter(|(key, value)| !used.contains(&key.as_str()) && !value.is_null());

    match tool.method {
        McpToolMethod::Get => {
            let mut query = url::form_urlencoded::Serializer::new(String::new());
            for (key, value) in rest {
                match value {
                    Value::Array(items) => {
                        for item in items {
                            query.append_pair(key, &scalar_to_string(key, item)?);
                        }
                    }
                    other => {
                        query.append_pair(key, &scalar_to_string(key, other)?);
                    }
                }
            }
            let query = query.finish();
            let uri =
                if query.is_empty() { format!("{api_prefix}{path}") } else { format!("{api_prefix}{path}?{query}") };
            Ok(ToolRequest { uri, body: None })
        }
        McpToolMethod::Post => {
            let body: Map<String, Value> = rest.map(|(key, value)| (key.clone(), value.clone())).collect();
            let body = serde_json::to_vec(&body).map_err(|err| format!("Failed to encode arguments: {err}"))?;
            Ok(ToolRequest { uri: format!("{api_prefix}{path}"), body: Some(body) })
        }
    }
}

fn scalar_to_string(key: &str, value: &Value) -> Result<String, String> {
    match value {
        Value::String(text) => Ok(text.clone()),
        Value::Number(number) => Ok(number.to_string()),
        Value::Bool(flag) => Ok(flag.to_string()),
        _ => Err(format!("Argument '{key}' must be a string, number or boolean")),
    }
}

/// Fills the `{param}` placeholders of a route template from the arguments and
/// returns the names it consumed. Each value becomes exactly one path segment.
fn expand_path<'a>(template: &'a str, arguments: &Map<String, Value>) -> Result<(String, Vec<&'a str>), String> {
    let mut path = String::with_capacity(template.len());
    let mut used = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        let (literal, tail) = rest.split_at(start);
        path.push_str(literal);
        let Some((name, after)) = tail[1..].split_once('}') else {
            return Err(format!("Invalid route template '{template}'"));
        };
        let value = arguments
            .get(name)
            .filter(|value| !value.is_null())
            .ok_or_else(|| format!("Missing required argument '{name}'"))?;
        path.push_str(&encode_path_segment(&scalar_to_string(name, value)?));
        used.push(name);
        rest = after;
    }
    path.push_str(rest);
    Ok((path, used))
}

/// Percent-encodes everything that is not unreserved so a value can never
/// introduce a separator, a query or a fragment. `.` and `..` are encoded as
/// well, otherwise they would be dot segments.
fn encode_path_segment(value: &str) -> String {
    if value == "." || value == ".." {
        return value.replace('.', "%2E");
    }
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect::<String>().replace('+', "%20")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::{
            endpoints::{
                v1_api::v1_api_register,
                v1_api_playlist::{
                    epg_inspect::{EpgIssue, EpgOutput},
                    items::{EpgStatus, ItemField, ItemSearchField},
                },
            },
            model::create_test_app_state,
        },
        auth::create_jwt_web_user,
        model::{Config, WebAuthConfig, WebUiUser},
    };
    use shared::model::{Permission, WebAuthConfigDto, WebUiConfigDto};
    use tower::ServiceExt;

    fn args(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            other => panic!("not an object: {other}"),
        }
    }

    fn tool(name: &str) -> &'static McpTool { find_tool(name).expect("tool") }

    fn config_with_mcp(web_auth: bool, mcp: shared::model::McpConfigDto) -> Config {
        let web_ui = WebUiConfigDto {
            auth: web_auth.then(|| WebAuthConfigDto {
                enabled: true,
                issuer: "test".to_string(),
                secret: "test-secret".to_string(),
                ..WebAuthConfigDto::default()
            }),
            ..WebUiConfigDto::default()
        };
        let mut config =
            Config { web_ui: Some((&web_ui).into()), mcp: Some(McpConfig::from(mcp)), ..Config::default() };
        if let Some(auth) = config.web_ui.as_mut().and_then(|web_ui| web_ui.auth.as_mut()) {
            auth.t_users = Some(vec![WebUiUser {
                username: "alice".into(),
                password_hash: "password-hash".into(),
                groups: vec!["admin".into()],
            }]);
        }
        config
    }

    fn enabled(read_only: bool) -> shared::model::McpConfigDto {
        shared::model::McpConfigDto { enabled: true, read_only, ..Default::default() }
    }

    fn token(config: &Config, permissions: Permission) -> String {
        let web_auth = config.web_ui.as_ref().and_then(|web_ui| web_ui.auth.as_ref()).expect("web auth");
        create_jwt_web_user(
            web_auth,
            "alice",
            permissions.into(),
            WebAuthConfig::pwd_version_from_hash("password-hash"),
            shared::model::UserId::web("alice"),
        )
        .expect("token")
    }

    fn bearer(token: &str) -> String { format!("Bearer {token}") }

    fn mcp_router(config: Config) -> Router {
        let web_auth_enabled = config.web_ui.as_ref().and_then(|web_ui| web_ui.auth.as_ref()).is_some();
        let app_state = create_test_app_state(config);
        let v1 = v1_api_register(web_auth_enabled, &app_state, "");
        mcp_api_register_if_enabled::<()>(&v1, &app_state, web_auth_enabled, "").expect("mcp enabled").with_state(())
    }

    async fn post(router: &Router, body: &str, extra: &[(&str, &str)]) -> (StatusCode, Value) {
        let mut request = Request::builder().method("POST").uri("/mcp").header("content-type", "application/json");
        for (name, value) in extra {
            request = request.header(*name, *value);
        }
        let response = router
            .clone()
            .oneshot(request.body(Body::from(body.to_owned())).expect("request"))
            .await
            .expect("response");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("body");
        let value = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).expect("json body") };
        (status, value)
    }

    fn rpc(id: u32, method: &str, params: Value) -> String {
        let mut message = json!({ "jsonrpc": "2.0", "id": id, "method": method });
        message["params"] = params;
        message.to_string()
    }

    fn call(name: &str, arguments: Value) -> String {
        let mut params = json!({ "name": name });
        params["arguments"] = arguments;
        rpc(1, "tools/call", params)
    }

    fn tool_text(reply: &Value) -> &str { reply["result"]["content"][0]["text"].as_str().expect("tool text") }

    // ---- protocol helpers -------------------------------------------------

    #[test]
    fn protocol_version_is_echoed_when_supported_and_falls_back_otherwise() {
        for version in SUPPORTED_PROTOCOL_VERSIONS {
            assert_eq!(negotiate_protocol_version(Some(version)), version);
        }
        assert_eq!(negotiate_protocol_version(Some("1999-01-01")), "2025-06-18");
        assert_eq!(negotiate_protocol_version(None), "2025-06-18");
    }

    #[test]
    fn origin_must_match_host_or_allow_list() {
        let headers = |origin: Option<&str>, host: Option<&str>| {
            let mut headers = HeaderMap::new();
            if let Some(origin) = origin {
                headers.insert(header::ORIGIN, origin.parse().unwrap());
            }
            if let Some(host) = host {
                headers.insert(header::HOST, host.parse().unwrap());
            }
            headers
        };
        let allowed = vec!["https://claude.ai".to_string()];

        assert!(origin_allowed(&headers(None, Some("localhost:8901")), &[]));
        assert!(origin_allowed(&headers(Some("http://localhost:8901"), Some("localhost:8901")), &[]));
        assert!(origin_allowed(&headers(Some("http://LOCALHOST:8901/"), Some("localhost:8901")), &[]));
        assert!(!origin_allowed(&headers(Some("http://evil.example"), Some("localhost:8901")), &[]));
        assert!(!origin_allowed(&headers(Some("http://localhost:9999"), Some("localhost:8901")), &[]));
        assert!(!origin_allowed(&headers(Some("null"), Some("localhost:8901")), &[]));
        assert!(!origin_allowed(&headers(Some("http://localhost:8901"), None), &[]));
        assert!(origin_allowed(&headers(Some("https://claude.ai"), Some("localhost:8901")), &allowed));
        assert!(!origin_allowed(&headers(Some("https://other.ai"), Some("localhost:8901")), &allowed));
    }

    #[test]
    fn web_auth_disabled_forces_read_only() {
        assert!(effective_read_only(true, true));
        assert!(effective_read_only(false, false));
        assert!(effective_read_only(true, false));
        assert!(!effective_read_only(false, true));
    }

    #[test]
    fn read_only_hides_write_tools() {
        assert!(visible_tools(true).all(|tool| tool.kind == McpToolKind::Read));
        assert!(visible_tools(false).any(|tool| tool.kind == McpToolKind::Write));
        let names: Vec<_> = tools_list(true).iter().map(|tool| tool["name"].as_str().unwrap().to_string()).collect();
        assert!(names.contains(&"get_status".to_string()));
        assert!(!names.contains(&"update_playlists".to_string()));
    }

    #[test]
    fn catalog_is_consistent() {
        let mut names = std::collections::HashSet::new();
        for tool in MCP_TOOLS {
            assert!(names.insert(tool.name), "duplicate tool {}", tool.name);
            let schema: Value = serde_json::from_str(tool.input_schema).expect("schema is JSON");
            assert_eq!(schema["type"], "object", "{}", tool.name);
            let properties = schema["properties"].as_object().expect("properties");
            for required in schema.get("required").and_then(Value::as_array).into_iter().flatten() {
                assert!(properties.contains_key(required.as_str().unwrap()), "{} requires undeclared field", tool.name);
            }
            let mut rest = tool.path;
            while let Some((_, tail)) = rest.split_once('{') {
                let (name, after) = tail.split_once('}').expect("closed placeholder");
                assert!(properties.contains_key(name), "{} path placeholder {name} undeclared", tool.name);
                rest = after;
            }
            assert!(tool.path.starts_with('/'));
            assert_ne!(tool.description, "");
        }
    }

    // ---- request building -------------------------------------------------

    #[test]
    fn path_values_are_percent_encoded() {
        let tool = tool("get_qos_snapshot");
        for (value, encoded) in [
            ("abc", "abc"),
            ("a b", "a%20b"),
            ("a+b", "a%2Bb"),
            ("../etc", "..%2Fetc"),
            ("..", "%2E%2E"),
            ("x?y=1#z", "x%3Fy%3D1%23z"),
            ("ü", "%C3%BC"),
        ] {
            let request =
                build_tool_request(tool, Some(&args(json!({ "stream_identity_key": value }))), "/api/v1").unwrap();
            assert_eq!(request.uri, format!("/api/v1/qos-snapshots/{encoded}"), "value {value}");
            assert!(request.body.is_none());
        }
    }

    #[test]
    fn get_arguments_become_an_encoded_query() {
        let request = build_tool_request(
            tool("query_stream_history"),
            Some(&args(json!({ "from": "2026-01-01", "page": 2, "search": "a&b=c", "search_mode": null }))),
            "/ui/api/v1",
        )
        .unwrap();
        assert_eq!(request.uri, "/ui/api/v1/stream-history?from=2026-01-01&page=2&search=a%26b%3Dc");
    }

    #[test]
    fn search_field_array_becomes_repeated_query_keys() {
        let request = build_tool_request(
            tool("query_stream_history"),
            Some(&args(json!({ "search": "x", "search_field": ["title", "group"] }))),
            "/api/v1",
        )
        .unwrap();
        assert_eq!(request.uri, "/api/v1/stream-history?search=x&search_field=title&search_field=group");
    }

    #[test]
    fn search_field_schema_lists_every_searchable_record_field() {
        use strum::IntoEnumIterator;

        let schema = tool("query_stream_history").schema();
        let listed: Vec<String> = schema["properties"]["search_field"]["items"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|value| value.as_str().map(str::to_owned))
            .collect();
        let expected: Vec<String> =
            shared::model::StreamHistorySearchField::iter().map(|field| field.as_ref().to_owned()).collect();
        assert_eq!(listed, expected);
    }

    const PLAYLIST_TOOLS: [&str; 4] =
        ["list_playlist_sources", "query_playlist_items", "query_target_epg_channels", "get_target_epg_channel"];

    /// Enum values of a schema property, looking through `items` for array properties.
    fn schema_enum(tool_name: &str, property: &str) -> Vec<String> {
        let schema = tool(tool_name).schema();
        let property = &schema["properties"][property];
        let values = property.get("enum").or_else(|| property["items"].get("enum")).expect("enum");
        values.as_array().unwrap().iter().map(|value| value.as_str().unwrap().to_owned()).collect()
    }

    #[test]
    fn playlist_tools_are_listed_read_only_with_redaction() {
        let names: Vec<_> = tools_list(true).iter().map(|tool| tool["name"].as_str().unwrap().to_string()).collect();
        for name in PLAYLIST_TOOLS {
            assert!(names.contains(&name.to_string()), "{name} missing in read-only tools/list");
            let tool = tool(name);
            assert_eq!(tool.kind, McpToolKind::Read, "{name}");
            assert_eq!(tool.method, McpToolMethod::Get, "{name}");
            assert!(tool.redact_secrets, "{name}");
        }
    }

    #[test]
    fn every_schema_is_strict_with_unique_names_and_enums() {
        for tool in MCP_TOOLS {
            let schema: Value = serde_json::from_str(tool.input_schema).expect("schema is JSON");
            assert_eq!(schema["type"], "object", "{}", tool.name);
            assert_eq!(schema["additionalProperties"], false, "{}", tool.name);
            let properties = schema["properties"].as_object().expect("properties");
            for name in properties.keys() {
                // a duplicated key would silently collapse during JSON parsing
                let declaration = format!("\"{name}\":{{");
                assert_eq!(tool.input_schema.matches(&declaration).count(), 1, "{} declares {name} twice", tool.name);
            }
            let required: Vec<&str> = schema
                .get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let unique: std::collections::HashSet<_> = required.iter().collect();
            assert_eq!(unique.len(), required.len(), "{} has duplicate required entries", tool.name);
            for property in properties.values() {
                let enums = property.get("enum").into_iter().chain(property["items"].get("enum"));
                for list in enums.filter_map(Value::as_array) {
                    let unique: std::collections::HashSet<_> = list.iter().map(Value::to_string).collect();
                    assert_eq!(unique.len(), list.len(), "{} has a duplicate enum value in {list:?}", tool.name);
                }
            }
        }
    }

    #[test]
    fn playlist_tool_schemas_document_every_property() {
        for name in PLAYLIST_TOOLS {
            let tool = tool(name);
            assert!(tool.description.len() > 80, "{name} description too short");
            let schema = tool.schema();
            for (property, definition) in schema["properties"].as_object().unwrap() {
                let description = definition["description"].as_str().unwrap_or_default();
                assert!(!description.is_empty(), "{name}.{property} has no description");
            }
            let properties = &schema["properties"];
            if !properties["limit"].is_null() {
                let maximum = if name == "get_target_epg_channel" { 1000 } else { 500 };
                assert_eq!(properties["limit"]["minimum"], 1, "{name}.limit");
                assert_eq!(properties["limit"]["maximum"], maximum, "{name}.limit");
            }
            if !properties["offset"].is_null() {
                assert_eq!(properties["offset"]["minimum"], 0, "{name}.offset");
            }
        }
    }

    #[test]
    fn playlist_tool_schemas_expose_the_contract_parameters() {
        let properties = |name: &str| -> Vec<String> {
            tool(name).schema()["properties"].as_object().unwrap().keys().cloned().collect::<Vec<_>>()
        };
        let sorted = |mut names: Vec<String>| {
            names.sort();
            names
        };
        let expected = |names: &[&str]| sorted(names.iter().map(|name| (*name).to_owned()).collect());
        assert_eq!(properties("list_playlist_sources").len(), 0);
        assert_eq!(
            sorted(properties("query_playlist_items")),
            expected(&[
                "target",
                "input",
                "cluster",
                "offset",
                "limit",
                "search",
                "search_field",
                "group",
                "fields",
                "epg_status"
            ])
        );
        assert_eq!(
            sorted(properties("query_target_epg_channels")),
            expected(&["target", "output", "offset", "limit", "search", "issue"])
        );
        assert_eq!(
            sorted(properties("get_target_epg_channel")),
            expected(&["target", "output", "channel_id", "from", "to", "limit"])
        );
        assert_eq!(tool("query_playlist_items").schema()["required"], json!(["cluster"]));
        assert_eq!(tool("query_target_epg_channels").schema()["required"], json!(["target"]));
        assert_eq!(tool("get_target_epg_channel").schema()["required"], json!(["target", "channel_id"]));
    }

    #[test]
    fn playlist_tool_enums_list_the_contract_values() {
        assert_eq!(schema_enum("query_playlist_items", "cluster"), ["live", "video", "series"]);
        assert_eq!(schema_enum("query_playlist_items", "search_field"), ItemSearchField::NAMES);
        assert_eq!(schema_enum("query_playlist_items", "fields"), ItemField::NAMES);
        assert_eq!(schema_enum("query_playlist_items", "epg_status"), EpgStatus::NAMES);
        assert_eq!(schema_enum("query_target_epg_channels", "issue"), EpgIssue::NAMES);
        for name in ["query_target_epg_channels", "get_target_epg_channel"] {
            assert_eq!(schema_enum(name, "output"), EpgOutput::NAMES);
        }
        let fields = schema_enum("query_playlist_items", "fields");
        let description =
            tool("query_playlist_items").schema()["properties"]["fields"]["description"].as_str().unwrap().to_owned();
        for field in &fields {
            assert!(description.contains(&format!("{field} = ")), "fields description lacks a meaning for {field}");
        }
        assert!(!fields.iter().any(|field| field == "url"), "url must never be selectable");
    }

    #[test]
    fn playlist_array_arguments_become_repeated_query_keys() {
        let request = build_tool_request(
            tool("query_playlist_items"),
            Some(&args(json!({
                "target": "my target",
                "cluster": "live",
                "limit": 10,
                "fields": ["name", "epg_status"],
                "epg_status": ["unmatched", "missing_id"],
            }))),
            "/api/v1",
        )
        .unwrap();
        assert_eq!(
            request.uri,
            "/api/v1/playlist/items?target=my+target&cluster=live&limit=10&fields=name&fields=epg_status&epg_status=unmatched&epg_status=missing_id"
        );
        let err = build_tool_request(tool("query_playlist_items"), Some(&args(json!({ "target": "t" }))), "/api/v1")
            .err()
            .unwrap();
        assert!(err.contains("cluster"), "{err}");
        let err = build_tool_request(
            tool("query_playlist_items"),
            Some(&args(json!({ "cluster": "live", "url": "x" }))),
            "/api/v1",
        )
        .err()
        .unwrap();
        assert!(err.contains("url"), "{err}");
    }

    #[test]
    fn no_arguments_means_no_query() {
        let request = build_tool_request(tool("get_status"), None, "/api/v1").unwrap();
        assert_eq!(request.uri, "/api/v1/status");
    }

    #[test]
    fn post_arguments_become_the_json_body() {
        let request =
            build_tool_request(tool("update_playlists"), Some(&args(json!({ "targets": ["a", "b"] }))), "/api/v1")
                .unwrap();
        assert_eq!(request.uri, "/api/v1/playlist/update");
        let body: Value = serde_json::from_slice(&request.body.unwrap()).unwrap();
        assert_eq!(body, json!({ "targets": ["a", "b"] }));

        let empty = build_tool_request(tool("update_playlists"), None, "/api/v1").unwrap();
        assert_eq!(empty.body.unwrap(), b"{}");
    }

    #[test]
    fn invalid_arguments_name_the_field() {
        let err = build_tool_request(tool("get_qos_snapshot"), None, "/api/v1").err().unwrap();
        assert!(err.contains("stream_identity_key"), "{err}");
        let err = build_tool_request(tool("get_status"), Some(&args(json!({ "extra": 1 }))), "/api/v1").err().unwrap();
        assert!(err.contains("extra"), "{err}");
        let err =
            build_tool_request(tool("query_stream_history"), Some(&args(json!({ "search": { "a": 1 } }))), "/api/v1")
                .err()
                .unwrap();
        assert!(err.contains("search"), "{err}");
    }

    #[test]
    fn body_is_truncated_on_a_char_boundary_with_a_marker() {
        assert_eq!(body_to_text(b"hello", 5), "hello");
        let text = body_to_text("aäb".as_bytes(), 2);
        assert!(text.starts_with("a\n[truncated: response exceeded 2 bytes]"), "{text}");
        let text = body_to_text(&[b'x'; 11], 10);
        assert!(text.starts_with("xxxxxxxxxx\n[truncated"), "{text}");
    }

    #[tokio::test]
    async fn read_capped_stops_after_one_byte_over_the_limit() {
        let bytes = read_capped(Body::from(vec![b'a'; 100]), 10).await;
        assert_eq!(bytes.len(), 11);
        let bytes = read_capped(Body::from(vec![b'a'; 10]), 10).await;
        assert_eq!(bytes.len(), 10);
    }

    // ---- endpoint ---------------------------------------------------------

    #[tokio::test]
    async fn disabled_or_absent_mcp_config_mounts_nothing() {
        for mcp in [None, Some(shared::model::McpConfigDto::default())] {
            let config = Config { mcp: mcp.as_ref().map(Into::into), ..Config::default() };
            let app_state = create_test_app_state(config);
            let v1 = v1_api_register(false, &app_state, "");
            assert!(mcp_api_register_if_enabled::<Arc<AppState>>(&v1, &app_state, false, "").is_none());
        }
    }

    #[tokio::test]
    async fn initialize_negotiates_the_protocol_version() {
        let router = mcp_router(config_with_mcp(false, enabled(true)));
        let (status, reply) =
            post(&router, &rpc(1, "initialize", json!({ "protocolVersion": "2025-03-26" })), &[]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(reply["jsonrpc"], "2.0");
        assert_eq!(reply["id"], 1);
        assert_eq!(reply["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(reply["result"]["serverInfo"]["name"], "tuliprox");
        assert!(reply["result"]["capabilities"]["tools"].is_object());

        let (_, reply) = post(&router, &rpc(2, "initialize", json!({ "protocolVersion": "9999" })), &[]).await;
        assert_eq!(reply["result"]["protocolVersion"], "2025-06-18");
    }

    #[tokio::test]
    async fn initialize_carries_usage_instructions() {
        let router = mcp_router(config_with_mcp(false, enabled(true)));
        let (_, reply) = post(&router, &rpc(1, "initialize", json!({})), &[]).await;
        let instructions = reply["result"]["instructions"].as_str().expect("instructions");
        assert_eq!(instructions, MCP_INSTRUCTIONS);
        assert_ne!(instructions.trim(), "");
        assert!(instructions.len() <= 1300, "instructions are too long: {}", instructions.len());
    }

    #[test]
    fn instructions_only_name_existing_tools() {
        for name in PLAYLIST_TOOLS {
            assert!(MCP_INSTRUCTIONS.contains(name), "instructions do not mention {name}");
        }
        for word in MCP_INSTRUCTIONS.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
            let looks_like_tool = ["list_", "query_", "get_", "update_"].iter().any(|prefix| word.starts_with(prefix));
            if looks_like_tool && !word.ends_with('_') {
                assert!(find_tool(word).is_some(), "instructions name unknown tool {word}");
            }
        }
    }

    #[tokio::test]
    async fn ping_and_notifications() {
        let router = mcp_router(config_with_mcp(false, enabled(true)));
        let (status, reply) = post(&router, &rpc(7, "ping", json!({})), &[]).await;
        assert_eq!((status, &reply["result"]), (StatusCode::OK, &json!({})));

        let (status, reply) = post(&router, r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#, &[]).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(reply, Value::Null);
    }

    #[tokio::test]
    async fn malformed_messages_answer_json_rpc_errors() {
        let router = mcp_router(config_with_mcp(false, enabled(true)));
        let code = |reply: &Value| reply["error"]["code"].as_i64().unwrap();

        let (status, reply) = post(&router, "{not json", &[]).await;
        assert_eq!((status, code(&reply)), (StatusCode::OK, i64::from(PARSE_ERROR)));
        assert_eq!(reply["id"], Value::Null);

        let (_, reply) = post(&router, r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#, &[]).await;
        assert_eq!(code(&reply), i64::from(INVALID_REQUEST));

        let (_, reply) = post(&router, r#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#, &[]).await;
        assert_eq!(code(&reply), i64::from(INVALID_REQUEST));
        assert_eq!(reply["id"], 1);

        let (_, reply) = post(&router, r#"{"jsonrpc":"2.0","id":1}"#, &[]).await;
        assert_eq!(code(&reply), i64::from(INVALID_REQUEST));

        let (_, reply) = post(&router, r#"{"jsonrpc":"2.0","id":{"a":1},"method":"ping"}"#, &[]).await;
        assert_eq!(code(&reply), i64::from(INVALID_REQUEST));

        let (_, reply) = post(&router, &rpc(3, "resources/list", json!({})), &[]).await;
        assert_eq!(code(&reply), i64::from(METHOD_NOT_FOUND));
        assert_eq!(reply["id"], 3);
    }

    #[tokio::test]
    async fn get_and_delete_are_not_allowed() {
        let router = mcp_router(config_with_mcp(false, enabled(true)));
        for method in ["GET", "DELETE"] {
            let response = router
                .clone()
                .oneshot(Request::builder().method(method).uri("/mcp").body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED, "{method}");
            assert_eq!(response.headers().get(header::ALLOW).expect("Allow header"), "POST", "{method}");
        }
    }

    #[tokio::test]
    async fn foreign_origin_is_forbidden() {
        let router = mcp_router(config_with_mcp(false, enabled(true)));
        let body = rpc(1, "ping", json!({}));
        let (status, _) = post(&router, &body, &[("host", "localhost:8901"), ("origin", "http://evil.example")]).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) =
            post(&router, &body, &[("host", "localhost:8901"), ("origin", "http://localhost:8901")]).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn status_tool_returns_the_rest_json() {
        let router = mcp_router(config_with_mcp(false, enabled(true)));
        let (status, reply) = post(&router, &call("get_status", json!({})), &[]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(reply["result"]["isError"], false);
        let status_json: Value = serde_json::from_str(tool_text(&reply)).expect("status JSON");
        assert_eq!(status_json["status"], "ok");
    }

    #[tokio::test]
    async fn tools_call_rejects_unknown_tools_and_bad_params() {
        let router = mcp_router(config_with_mcp(false, enabled(true)));
        let (_, reply) = post(&router, &call("nope", json!({})), &[]).await;
        assert_eq!(reply["error"]["code"], i64::from(INVALID_PARAMS));
        let (_, reply) = post(&router, &rpc(1, "tools/call", json!({})), &[]).await;
        assert_eq!(reply["error"]["code"], i64::from(INVALID_PARAMS));
        let (_, reply) = post(&router, &rpc(1, "tools/call", json!("x")), &[]).await;
        assert_eq!(reply["error"]["code"], i64::from(INVALID_PARAMS));
    }

    #[tokio::test]
    async fn missing_required_argument_is_a_tool_error_naming_the_field() {
        let router = mcp_router(config_with_mcp(false, enabled(true)));
        let (_, reply) = post(&router, &call("get_qos_snapshot", json!({})), &[]).await;
        assert_eq!(reply["result"]["isError"], true);
        assert!(tool_text(&reply).contains("stream_identity_key"));
    }

    #[tokio::test]
    async fn rest_errors_are_tool_errors_with_the_status() {
        // QoS aggregation is off in the test config, so the REST route
        // answers 503; query parameters must still parse (not 400).
        let router = mcp_router(config_with_mcp(false, enabled(true)));
        for (name, arguments) in [
            ("list_qos_snapshots", json!({ "limit": 5, "provider_name": "p" })),
            ("query_stream_history", json!({ "page": 1, "page_size": 25, "search": "x", "from": "2026-01-01" })),
            ("get_stream_history_summary", json!({ "from": "2026-01-01", "provider_name": "p" })),
            ("get_qos_snapshot", json!({ "stream_identity_key": "a b" })),
        ] {
            let (_, reply) = post(&router, &call(name, arguments), &[]).await;
            assert_eq!(reply["result"]["isError"], true, "{name}");
            assert!(tool_text(&reply).starts_with("HTTP 503"), "{name}: {}", tool_text(&reply));
        }
    }

    #[tokio::test]
    async fn missing_or_invalid_token_is_a_transport_401_for_every_method() {
        let router = mcp_router(config_with_mcp(true, enabled(true)));
        let bodies = [
            rpc(1, "initialize", json!({ "protocolVersion": "2025-06-18" })),
            rpc(2, "ping", json!({})),
            rpc(3, "tools/list", json!({})),
            call("get_status", json!({})),
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.to_string(),
        ];
        for authorization in [None, Some("Bearer not-a-token"), Some("Basic abc")] {
            for body in &bodies {
                let mut request =
                    Request::builder().method("POST").uri("/mcp").header("content-type", "application/json");
                if let Some(authorization) = authorization {
                    request = request.header("authorization", authorization);
                }
                let response = router.clone().oneshot(request.body(Body::from(body.clone())).unwrap()).await.unwrap();
                assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{authorization:?} {body}");
                assert_eq!(response.headers().get(header::WWW_AUTHENTICATE).unwrap(), "Bearer");
            }
        }
    }

    #[tokio::test]
    async fn valid_token_reaches_every_method() {
        let config = config_with_mcp(true, enabled(true));
        let authorization = bearer(&token(&config, Permission::SystemRead));
        let router = mcp_router(config);
        let headers = [("authorization", authorization.as_str())];

        let (status, reply) = post(&router, &rpc(1, "initialize", json!({})), &headers).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(reply["result"]["serverInfo"]["name"], "tuliprox");
        let (status, reply) = post(&router, &rpc(2, "tools/list", json!({})), &headers).await;
        assert_eq!(status, StatusCode::OK);
        assert!(reply["result"]["tools"].is_array());
        let (status, reply) = post(&router, &call("get_status", json!({})), &headers).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(reply["result"]["isError"], false, "{}", tool_text(&reply));
        assert!(tool_text(&reply).contains("\"status\""));
    }

    #[tokio::test]
    async fn valid_token_without_rest_permission_is_a_403_tool_error() {
        let config = config_with_mcp(true, enabled(true));
        let authorization = bearer(&token(&config, Permission::ConfigRead));
        let router = mcp_router(config);

        let (status, reply) =
            post(&router, &call("get_status", json!({})), &[("authorization", authorization.as_str())]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(reply["result"]["isError"], true);
        assert!(tool_text(&reply).contains("403"), "{}", tool_text(&reply));
    }

    #[tokio::test]
    async fn read_only_hides_and_rejects_write_tools() {
        let config = config_with_mcp(true, enabled(true));
        let authorization = bearer(&token(&config, Permission::SystemRead));
        let router = mcp_router(config);
        let headers = [("authorization", authorization.as_str())];
        let (_, reply) = post(&router, &rpc(1, "tools/list", json!({})), &headers).await;
        let names: Vec<_> =
            reply["result"]["tools"].as_array().unwrap().iter().map(|tool| tool["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"get_status"));
        assert!(!names.contains(&"update_playlists"));
        assert!(!names.contains(&"update_geoip"));

        let (_, reply) = post(&router, &call("update_playlists", json!({})), &headers).await;
        assert_eq!(reply["error"]["code"], i64::from(INVALID_PARAMS));
    }

    #[tokio::test]
    async fn web_auth_disabled_forces_read_only_even_when_configured_writable() {
        let router = mcp_router(config_with_mcp(false, enabled(false)));
        let (_, reply) = post(&router, &rpc(1, "tools/list", json!({})), &[]).await;
        let names: Vec<_> =
            reply["result"]["tools"].as_array().unwrap().iter().map(|tool| tool["name"].as_str().unwrap()).collect();
        assert!(!names.contains(&"update_playlists"));
        let (_, reply) = post(&router, &call("update_geoip", json!({})), &[]).await;
        assert_eq!(reply["error"]["code"], i64::from(INVALID_PARAMS));
    }

    #[tokio::test]
    async fn writable_server_exposes_write_tools_behind_rest_permissions() {
        let config = config_with_mcp(true, enabled(false));
        let authorization = bearer(&token(&config, Permission::SystemRead));
        let router = mcp_router(config);
        let headers = [("authorization", authorization.as_str())];

        let (_, reply) = post(&router, &rpc(1, "tools/list", json!({})), &headers).await;
        let names: Vec<_> =
            reply["result"]["tools"].as_array().unwrap().iter().map(|tool| tool["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"update_playlists"));
        assert!(names.contains(&"update_geoip"));

        let (_, reply) = post(&router, &call("update_geoip", json!({})), &headers).await;
        assert_eq!(reply["result"]["isError"], true);
        assert!(tool_text(&reply).starts_with("HTTP 403"), "{}", tool_text(&reply));
    }

    #[tokio::test]
    async fn oversized_results_are_truncated_with_a_marker() {
        let mcp = shared::model::McpConfigDto { max_response_bytes: 16, ..enabled(true) };
        let router = mcp_router(config_with_mcp(false, mcp));
        let (_, reply) = post(&router, &call("get_status", json!({})), &[]).await;
        assert_eq!(reply["result"]["isError"], false);
        assert!(tool_text(&reply).contains("[truncated: response exceeded 16 bytes]"), "{}", tool_text(&reply));
    }

    #[tokio::test]
    async fn web_ui_path_prefixes_the_endpoint_and_the_dispatched_routes() {
        let config = config_with_mcp(false, enabled(true));
        let app_state = create_test_app_state(config);
        let v1 = v1_api_register(false, &app_state, "ui");
        let router = mcp_api_register_if_enabled::<()>(&v1, &app_state, false, "ui").unwrap().with_state(());
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/ui/mcp")
                    .body(Body::from(call("get_status", json!({}))))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let reply: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(reply["result"]["isError"], false, "{reply}");
    }

    // ---- result redaction -------------------------------------------------

    fn stub_state(config_body: &'static str, config_status: StatusCode) -> McpState {
        let secret_body =
            r#"{"sources":{"inputs":[{"url":"http://h/get.php?username=u&password=pw","password":"pw"}]}}"#;
        let router = Router::new()
            .route("/api/v1/config", axum::routing::get(move || async move { (config_status, config_body) }))
            .route("/api/v1/status", axum::routing::get(move || async move { secret_body }));
        McpState::new(router, &McpConfig::from(enabled(true)), true, "")
    }

    async fn call_stub(state: &McpState, name: &str) -> Value {
        handle_message(state, &HeaderMap::new(), call(name, json!({})).as_bytes()).await.expect("reply")
    }

    #[tokio::test]
    async fn get_config_result_is_redacted() {
        let body = r#"{"config":{"web_ui":{"auth":{"secret":"s3cret"}}},"sources":{"inputs":[{"url":"http://u:p4ss@h/get.php?username=bob&password=pw","password":"pw"}]},"api_proxy":{"user":[{"target":"t","credentials":[{"username":"carol","password":"cp"}]}]}}"#;
        let reply = call_stub(&stub_state(body, StatusCode::OK), "get_config").await;
        assert_eq!(reply["result"]["isError"], false);
        let text = tool_text(&reply);
        for leaked in ["s3cret", "p4ss", "bob", "pw", "carol", "cp\""] {
            assert!(!text.contains(leaked), "{leaked} leaked: {text}");
        }
        let parsed: Value = serde_json::from_str(text).expect("redacted JSON");
        assert_eq!(parsed["api_proxy"]["user"], json!([{ "target": "t", "credentials": 1 }]));
    }

    #[tokio::test]
    async fn get_config_withholds_non_json_and_error_bodies() {
        let reply = call_stub(&stub_state("password=hunter2", StatusCode::OK), "get_config").await;
        assert_eq!(tool_text(&reply), super::super::mcp_redact::REDACTION_FAILED);

        let reply = call_stub(&stub_state("password=hunter2", StatusCode::INTERNAL_SERVER_ERROR), "get_config").await;
        assert_eq!(reply["result"]["isError"], true);
        assert_eq!(tool_text(&reply), "HTTP 500 Internal Server Error");
    }

    #[tokio::test]
    async fn redaction_applies_to_config_and_playlist_tools_only() {
        assert_eq!(
            MCP_TOOLS.iter().filter(|tool| tool.redact_secrets).map(|tool| tool.name).collect::<Vec<_>>(),
            [
                "get_config",
                "list_playlist_sources",
                "query_playlist_items",
                "query_target_epg_channels",
                "get_target_epg_channel"
            ]
        );
        let reply = call_stub(&stub_state("{}", StatusCode::OK), "get_status").await;
        assert!(tool_text(&reply).contains("\"password\":\"pw\""), "{}", tool_text(&reply));
    }
}
