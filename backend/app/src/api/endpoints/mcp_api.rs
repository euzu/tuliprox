//! Model Context Protocol endpoint (`POST {web_ui_path}/mcp`).
//!
//! Stateless JSON-RPC 2.0 over plain `application/json` responses. A
//! `tools/call` is dispatched in-process into the `/api/v1` router with the
//! caller's `Authorization` header, so the existing permission layers decide
//! what the caller may do; nothing is re-implemented here.

use super::{
    mcp_redact::{redact_json_body, REDACT_BODY_LIMIT},
    mcp_tools::{
        plugin_tools, CallerContext, McpAudience, McpTool, McpToolKind, McpToolMethod, McpToolRef, OwnedMcpTool,
        ToolView,
    },
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
use shared::{
    model::{McpPlayerTools, PluginRpcErrorDto},
    utils::concat_path_leading_slash,
};
use std::{fmt::Write as _, sync::Arc};
use tower::ServiceExt;

const MCP_PATH: &str = "mcp";
const SUPPORTED_PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

macro_rules! mcp_shared_instructions {
    () => {
        "Paging: results carry total_matched (all matches), returned and has_more. Request the next page with \
offset = offset + returned while has_more is true. limit is clamped (items 500). Narrow with search, group, fields. \
skipped_records > 0 means unreadable stored records are missing from the listing.\n\
Secrets: provider URLs and credentials are never returned and any secret-like value is redacted."
    };
}

/// `InitializeResult.instructions`: a short usage guide shown to administrators and web clients.
const MCP_INSTRUCTIONS: &str = concat!(
    "Tuliprox is an IPTV playlist proxy and processor. These tools inspect its configuration, streams and stored playlists.\n",
    "Playlist workflow: call list_playlist_sources first (targets and inputs are addressed by NAME), then query_playlist_items \
for one target or input and one cluster (live, video, series). Reading is always safe: only stored data is read and no \
provider is contacted.\n",
    mcp_shared_instructions!(),
    "\nDebug EPG: query_playlist_items with epg_status=unmatched (or missing_id) finds items whose EPG id is not in the EPG \
database; query_target_epg_channels with issue=unreferenced, expired, no_programmes, not_current or no_title finds broken \
EPG channels; get_target_epg_channel shows the programmes of one channel. An empty EPG result with a `note` means no \
EPG data exists, not that everything is fine.\n",
    "Media servers (plex, emby, jellyfin, silo): list_media_server_libraries and get_media_server_status contact the \
server once; refresh_media_server_input only queues an update. query_playlist_items reads their stored catalog.\n",
    "The server is read-only by default; update tools are listed only when write access is enabled."
);

/// `InitializeResult.instructions`: usage guide shown to playlist API users.
const MCP_PLAYER_INSTRUCTIONS: &str = concat!(
    "Tuliprox is an IPTV playlist proxy and processor. These tools inspect and control your personal player streams, channels and recordings.\n",
    "Player workflow: discover capabilities with me_capabilities first, search channels with me_search_channels, \
and inspect programme guides with me_get_epg.\n",
    mcp_shared_instructions!(),
    "\nStreams: me_list_streams lists active playback streams; me_stop_stream disconnects an active stream. \
me_list_joinable lists shared live streams you can join without extra connections. \
Failover and sources: me_list_sources lists failover members, me_switch_source switches an active stream source, \
and me_split_stream splits a shared stream session.\n",
    "Recordings: me_list_recordings lists visible recordings and tasks; me_recording_conflicts previews capacity and scheduling conflicts. \
Schedule recordings with me_record_program, cancel with me_cancel_recording, or remove with me_delete_recording. \
Recurring rules: me_list_rules lists rules, me_create_rule schedules a weekly timeslot rule, and me_delete_rule deletes a rule.\n",
    "Write tools appear only when player_tools is set to write."
);

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
    /// Read live for the plugin switches (`mcp.plugin_tools`, `mcp.read_only`)
    /// and the plugin host, so a config change or an approval counts at call time.
    app_state: Arc<AppState>,
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
    let state = Arc::new(McpState::new(
        v1_router.clone().with_state(Arc::clone(app_state)),
        mcp,
        read_only,
        web_ui_path,
        Arc::clone(app_state),
    ));
    info!(
        "MCP endpoint: POST {} ({})",
        concat_path_leading_slash(web_ui_path, MCP_PATH),
        if read_only { "read-only" } else { "read/write" }
    );
    Some(mcp_api_register(state, app_state, web_ui_path))
}

impl McpState {
    fn new(router: Router, config: &McpConfig, read_only: bool, web_ui_path: &str, app_state: Arc<AppState>) -> Self {
        Self {
            router,
            api_prefix: concat_path_leading_slash(web_ui_path, API_V1_PATH),
            read_only,
            allowed_origins: config.allowed_origins.clone(),
            max_response_bytes: config.max_response_bytes,
            app_state,
        }
    }

    /// Read-only as of now: the startup decision, or `mcp.read_only` set since
    /// (a config without `mcp` counts as read-only). Never less strict than the
    /// startup decision.
    fn effective_read_only(&self) -> bool {
        self.read_only || self.app_state.app_config.config.load().mcp.as_ref().is_none_or(|mcp| mcp.read_only)
    }

    fn player_tools(&self) -> McpPlayerTools {
        self.app_state.app_config.config.load().mcp.as_ref().map_or(McpPlayerTools::Off, |mcp| mcp.player_tools)
    }

    /// The tools of this request. The plugin tools need all three switches:
    /// `mcp.plugin_tools` is read here, the export's `mcp: true` and the admin's
    /// approval by the plugin host, so each is re-checked on every list and call.
    ///
    /// Async because the plugin host reads `approvals.json` and the schema files
    /// after a registry change: that runs on the blocking pool.
    async fn tool_view(&self) -> ToolView {
        let plugin_tools_enabled =
            self.app_state.app_config.config.load().mcp.as_ref().is_some_and(|mcp| mcp.enabled && mcp.plugin_tools);
        if !plugin_tools_enabled {
            return ToolView::default();
        }
        let app_state = Arc::clone(&self.app_state);
        let plugin = tokio::task::spawn_blocking(move || plugin_tools(&app_state)).await.unwrap_or_default();
        ToolView::new(true, || plugin)
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

async fn mcp_post(
    Extension(state): Extension<Arc<McpState>>,
    fingerprint: crate::auth::Fingerprint,
    verified_claims: Option<Extension<crate::api::auth_middleware::VerifiedClaims>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !origin_allowed(&headers, &state.allowed_origins) {
        return (StatusCode::FORBIDDEN, axum::Json(json!({ "error": "Origin not allowed" }))).into_response();
    }
    let verified = verified_claims.as_ref().map(|Extension(c)| &c.0);
    match handle_message(&state, &headers, &body, verified, &fingerprint).await {
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
async fn handle_message(
    state: &McpState,
    headers: &HeaderMap,
    body: &[u8],
    verified_claims: Option<&shared::model::Claims>,
    fingerprint: &crate::auth::Fingerprint,
) -> Option<Value> {
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
        "initialize" => {
            let caller = resolve_caller(state, headers, verified_claims, fingerprint).await;
            rpc_result(id, initialize_result(params, &caller))
        }
        "ping" => rpc_result(id, json!({})),
        "tools/list" => {
            let caller = resolve_caller(state, headers, verified_claims, fingerprint).await;
            rpc_result(
                id,
                json!({
                    "tools": tools_list_for_caller(
                        &state.tool_view().await,
                        state.effective_read_only(),
                        state.player_tools(),
                        &caller,
                        &state.app_state,
                    )
                }),
            )
        }
        "tools/call" => match call_tool(state, headers, params, verified_claims, fingerprint).await {
            Ok(result) => rpc_result(id, result),
            Err(message) => rpc_error(id, INVALID_PARAMS, message),
        },
        other => rpc_error(id, METHOD_NOT_FOUND, format!("Method not found: {other}")),
    })
}

async fn resolve_caller(
    state: &McpState,
    headers: &HeaderMap,
    verified_claims: Option<&shared::model::Claims>,
    fingerprint: &crate::auth::Fingerprint,
) -> CallerContext {
    let enabled = state
        .app_state
        .app_config
        .config
        .load()
        .web_ui
        .as_ref()
        .and_then(|ui| ui.auth.as_ref())
        .is_some_and(|auth| auth.enabled);
    if !enabled {
        return CallerContext::NoAuth;
    }
    let claims = if let Some(claims) = verified_claims {
        claims.clone()
    } else {
        let Ok(crate::auth::AuthBearer(token)) = crate::auth::AuthBearer::from_headers(headers) else {
            return CallerContext::NoAuth;
        };
        let Ok(claims) = crate::api::auth_middleware::authenticate(&state.app_state, &token).await else {
            return CallerContext::NoAuth;
        };
        claims
    };
    if claims.subject_id.as_ref().is_some_and(shared::model::UserId::is_api) {
        if let Ok(ctx) = crate::api::auth_middleware::api_user_context(&state.app_state, claims, fingerprint) {
            return CallerContext::ApiUser {
                effective_permissions: ctx.claims.permissions,
                claims: ctx.claims,
                user: ctx.user,
                target: ctx.target,
            };
        }
        return CallerContext::Denied;
    }
    CallerContext::WebUser
}

fn initialize_result(params: Option<&Value>, caller: &CallerContext) -> Value {
    let requested = params.and_then(|params| params.get("protocolVersion")).and_then(Value::as_str);
    let instructions = match caller {
        CallerContext::ApiUser { .. } => MCP_PLAYER_INSTRUCTIONS,
        CallerContext::WebUser | CallerContext::NoAuth | CallerContext::Denied => MCP_INSTRUCTIONS,
    };
    json!({
        "protocolVersion": negotiate_protocol_version(requested),
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "tuliprox", "version": VERSION },
        "instructions": instructions,
    })
}

#[cfg(test)]
fn tools_list(view: &ToolView, read_only: bool) -> Vec<Value> {
    view.visible(read_only)
        .map(|tool| {
            let mut entry = json!({
                "name": tool.name(),
                "description": tool.description(),
                "inputSchema": tool.schema(),
                "annotations": tool_annotations(&tool),
            });
            if let Some(schema) = tool.output_schema() {
                entry["outputSchema"] = schema;
            }
            entry
        })
        .collect()
}

fn tools_list_for_caller(
    view: &ToolView,
    admin_read_only: bool,
    player_tools: McpPlayerTools,
    caller: &CallerContext,
    app_state: &AppState,
) -> Vec<Value> {
    view.visible_for_caller(caller, admin_read_only, player_tools, app_state)
        .map(|tool| {
            let mut entry = json!({
                "name": tool.name(),
                "description": tool.description(),
                "inputSchema": tool.schema(),
                "annotations": tool_annotations(&tool),
            });
            if let Some(schema) = tool.output_schema() {
                entry["outputSchema"] = schema;
            }
            entry
        })
        .collect()
}

/// MCP tool annotations. The spec defaults a non-read-only tool to destructive
/// and every tool to open-world, so both are stated where they are known: a
/// built-in tool only acts on tuliprox itself and none of them deletes data, so
/// it is not open-world. A plugin may call an external service, so it stays
/// open-world; whether its write tools are destructive comes from the export's
/// `destructive` flag, since only the plugin knows.
fn tool_annotations(tool: &McpToolRef<'_>) -> Value {
    match tool {
        McpToolRef::Builtin(builtin) if builtin.kind == McpToolKind::Read => {
            json!({ "readOnlyHint": true, "openWorldHint": false })
        }
        McpToolRef::Builtin(builtin) => {
            json!({ "readOnlyHint": false, "destructiveHint": builtin.destructive, "openWorldHint": false })
        }
        McpToolRef::Plugin(plugin) if plugin.kind == McpToolKind::Read => {
            json!({ "readOnlyHint": true, "openWorldHint": true })
        }
        McpToolRef::Plugin(plugin) => {
            json!({ "readOnlyHint": false, "destructiveHint": plugin.destructive, "openWorldHint": true })
        }
    }
}

/// `Err` is a JSON-RPC protocol error (-32602); everything the REST route
/// answers, failures included, is an `isError` tool result instead.
async fn call_tool(
    state: &McpState,
    headers: &HeaderMap,
    params: Option<&Value>,
    verified_claims: Option<&shared::model::Claims>,
    fingerprint: &crate::auth::Fingerprint,
) -> Result<Value, String> {
    call_tool_in(state, &state.tool_view().await, headers, params, verified_claims, fingerprint).await
}

/// Rejects arguments the tool schema does not declare, so every tool answers a typo the same way.
fn reject_unknown_args(schema: &Value, args: &Map<String, Value>) -> Result<(), String> {
    let properties = schema.get("properties").and_then(Value::as_object);
    if let Some(unknown) = args.keys().find(|key| !properties.is_some_and(|props| props.contains_key(*key))) {
        return Err(format!("Unknown argument '{unknown}'"));
    }
    Ok(())
}

/// [`call_tool`] over `view`, the tools resolved for this request.
#[allow(clippy::too_many_lines)]
async fn call_tool_in(
    state: &McpState,
    view: &ToolView,
    headers: &HeaderMap,
    params: Option<&Value>,
    verified_claims: Option<&shared::model::Claims>,
    fingerprint: &crate::auth::Fingerprint,
) -> Result<Value, String> {
    let params = params.and_then(Value::as_object).ok_or("params must be an object")?;
    let name = params.get("name").and_then(Value::as_str).ok_or("params.name is required")?;
    let caller = resolve_caller(state, headers, verified_claims, fingerprint).await;
    let tool = view
        .find_for_caller(name, &caller, state.effective_read_only(), state.player_tools(), &state.app_state)
        .ok_or_else(|| format!("Unknown tool: {name}"))?;
    if tool.audience() == McpAudience::Admin && state.effective_read_only() && tool.kind() == McpToolKind::Write {
        return Err(format!("Tool '{name}' is not available in read-only mode"));
    }
    let arguments = match params.get("arguments") {
        None | Some(Value::Null) => None,
        Some(Value::Object(arguments)) => {
            if matches!(tool, McpToolRef::Plugin(_)) && arguments.contains_key("input_draft") {
                return Ok(tool_result(true, "`input_draft` is not supported for MCP tools"));
            }
            Some(arguments)
        }
        Some(_) => return Ok(tool_result(true, "arguments must be an object")),
    };

    if tool.is_internal() {
        let CallerContext::ApiUser { claims, .. } = &caller else {
            return Ok(tool_result(true, "Unauthorized"));
        };
        let response =
            crate::api::endpoints::recording_api::recording_snapshot_response(&state.app_state, claims).await;
        let val = serde_json::to_value(&response).unwrap_or(Value::Null);
        let raw_text = serde_json::to_string(&val).unwrap_or_default();
        let max_bytes = state.max_response_bytes;
        let truncated = raw_text.len() > max_bytes;
        let text = body_to_text(raw_text.as_bytes(), max_bytes);
        let mut result = tool_result(false, &text);
        if !truncated && val.is_object() {
            result["structuredContent"] = val;
        }
        return Ok(result);
    }

    let empty = Map::new();
    let args = arguments.unwrap_or(&empty);
    let mut idempotency_key = None;
    let tool_request = if tool.name() == "me_record_program" {
        if let Err(err) = reject_unknown_args(&tool.schema(), args) {
            return Ok(tool_result(true, &err));
        }
        let CallerContext::ApiUser { target, claims, .. } = &caller else {
            return Ok(tool_result(true, "Unauthorized"));
        };
        let vid = match required_u64(args, "vid") {
            Ok(v) => v,
            Err(e) => return Ok(tool_result(true, &e)),
        };
        let program_start = match required_i64(args, "program_start") {
            Ok(v) => v,
            Err(e) => return Ok(tool_result(true, &e)),
        };
        let program_end = match required_i64(args, "program_end") {
            Ok(v) => v,
            Err(e) => return Ok(tool_result(true, &e)),
        };
        let title = match required_str(args, "title") {
            Ok(v) => v,
            Err(e) => return Ok(tool_result(true, &e)),
        };
        let visibility = args.get("visibility").and_then(Value::as_str).unwrap_or("private");
        let pre_roll_secs = args.get("pre_roll_secs").and_then(Value::as_u64);
        let post_roll_secs = args.get("post_roll_secs").and_then(Value::as_u64);

        let mut body = json!({
            "source": {
                "target_id": target.name,
                "virtual_id": vid.to_string(),
                "cluster": "Live",
                "input_name": ""
            },
            "program_title": title,
            "program_start": program_start,
            "program_end": program_end,
            "visibility": visibility,
        });
        if let Some(pre) = pre_roll_secs {
            body["pre_roll_secs"] = json!(pre);
        }
        if let Some(post) = post_roll_secs {
            body["post_roll_secs"] = json!(post);
        }

        let subject_str = claims.subject_id.as_ref().map_or_else(|| claims.username.clone(), ToString::to_string);
        let args_json = serde_json::to_string(args).unwrap_or_default();
        idempotency_key = Some(blake3::hash(format!("{subject_str}:{args_json}").as_bytes()).to_hex().to_string());

        let body_bytes = serde_json::to_vec(&body).map_err(|err| format!("Failed to encode arguments: {err}"))?;
        Ok(ToolRequest { uri: format!("{}{}", state.api_prefix, tool.path()), body: Some(body_bytes) })
    } else if tool.name() == "me_recording_conflicts" {
        if let Err(err) = reject_unknown_args(&tool.schema(), args) {
            return Ok(tool_result(true, &err));
        }
        let CallerContext::ApiUser { target, .. } = &caller else {
            return Ok(tool_result(true, "Unauthorized"));
        };
        let vid = match required_u64(args, "vid") {
            Ok(v) => v,
            Err(e) => return Ok(tool_result(true, &e)),
        };
        let program_start = match required_i64(args, "program_start") {
            Ok(v) => v,
            Err(e) => return Ok(tool_result(true, &e)),
        };
        let program_end = match required_i64(args, "program_end") {
            Ok(v) => v,
            Err(e) => return Ok(tool_result(true, &e)),
        };
        let pre_roll = args.get("pre_roll_secs").and_then(Value::as_u64).unwrap_or(0);
        let post_roll = args.get("post_roll_secs").and_then(Value::as_u64).unwrap_or(0);
        let priority = args.get("priority").and_then(Value::as_i64).unwrap_or(0);
        let padded_start = program_start.saturating_sub(pre_roll.cast_signed());
        let padded_end = program_end.saturating_add(post_roll.cast_signed());

        let body = json!({
            "source": {
                "target_name": target.name,
                "virtual_id": vid.to_string(),
                "input_name": ""
            },
            "candidate": {
                "padded_start": padded_start,
                "padded_end": padded_end,
                "pre_roll_secs": pre_roll,
                "post_roll_secs": post_roll,
                "priority": priority
            }
        });

        let body_bytes = serde_json::to_vec(&body).map_err(|err| format!("Failed to encode arguments: {err}"))?;
        Ok(ToolRequest { uri: format!("{}{}", state.api_prefix, tool.path()), body: Some(body_bytes) })
    } else if tool.name() == "me_create_rule" {
        if let Err(err) = reject_unknown_args(&tool.schema(), args) {
            return Ok(tool_result(true, &err));
        }
        let CallerContext::ApiUser { target, .. } = &caller else {
            return Ok(tool_result(true, "Unauthorized"));
        };
        let vid = match required_u64(args, "vid") {
            Ok(v) => v,
            Err(e) => return Ok(tool_result(true, &e)),
        };
        let weekday = match required_u64(args, "weekday") {
            Ok(v) => {
                if !(1..=7).contains(&v) {
                    return Ok(tool_result(true, "Argument 'weekday' must be between 1 and 7"));
                }
                v
            }
            Err(e) => return Ok(tool_result(true, &e)),
        };
        let local_start_time = match required_str(args, "local_start_time") {
            Ok(v) => v,
            Err(e) => return Ok(tool_result(true, &e)),
        };
        let duration_secs = match required_u64(args, "duration_secs") {
            Ok(v) => v,
            Err(e) => return Ok(tool_result(true, &e)),
        };
        let timezone = args.get("timezone").and_then(Value::as_str).unwrap_or("UTC");
        let visibility = args.get("visibility").and_then(Value::as_str);
        let pre_roll_secs = args.get("pre_roll_secs").and_then(Value::as_u64);
        let post_roll_secs = args.get("post_roll_secs").and_then(Value::as_u64);

        let mut body = json!({
            "target_id": target.name,
            "virtual_id": vid.to_string(),
            "input_name": "",
            "body": {
                "kind": "weekly_timeslot",
                "weekday": weekday,
                "local_start_time": local_start_time,
                "duration_secs": duration_secs,
                "timezone": timezone,
            }
        });
        if let Some(vis) = visibility {
            body["visibility"] = json!(vis);
        }
        if let Some(pre) = pre_roll_secs {
            body["pre_roll_secs"] = json!(pre);
        }
        if let Some(post) = post_roll_secs {
            body["post_roll_secs"] = json!(post);
        }

        let body_bytes = serde_json::to_vec(&body).map_err(|err| format!("Failed to encode arguments: {err}"))?;
        Ok(ToolRequest { uri: format!("{}{}", state.api_prefix, tool.path()), body: Some(body_bytes) })
    } else if tool.name() == "me_delete_rule" {
        let mut defaulted_args = arguments.cloned().unwrap_or_default();
        if !defaulted_args.contains_key("future") {
            defaulted_args.insert("future".to_string(), Value::String("retain".to_string()));
        }
        match tool {
            McpToolRef::Builtin(tool) => build_tool_request(tool, Some(&defaulted_args), &state.api_prefix),
            McpToolRef::Plugin(tool) => Ok(build_plugin_request(tool, Some(&defaulted_args), &state.api_prefix)),
        }
    } else {
        match tool {
            McpToolRef::Builtin(tool) => build_tool_request(tool, arguments, &state.api_prefix),
            McpToolRef::Plugin(tool) => Ok(build_plugin_request(tool, arguments, &state.api_prefix)),
        }
    };
    let tool_request = match tool_request {
        Ok(tool_request) => tool_request,
        Err(message) => return Ok(tool_result(true, &message)),
    };

    let mut request = Request::builder()
        .method(tool.method().as_http())
        .uri(&tool_request.uri)
        .header(header::ACCEPT, "application/json");
    if let Some(authorization) = headers.get(header::AUTHORIZATION) {
        request = request.header(header::AUTHORIZATION, authorization);
    }
    if let Some(idempotency) = idempotency_key {
        request = request.header("Idempotency-Key", idempotency);
    }
    let body = match tool_request.body {
        Some(body) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(body)
        }
        None => Body::empty(),
    };
    let Ok(mut request) = request.body(body) else {
        return Ok(tool_result(true, "Failed to build request"));
    };
    // The dispatched request has no socket of its own; the caller's fingerprint carries the real peer address
    // so the routes apply the same network-access rules as to the MCP request itself.
    request.extensions_mut().insert(fingerprint.clone());

    let response = match state.router.clone().oneshot(request).await {
        Ok(response) => response,
        Err(never) => match never {},
    };
    let status = response.status();
    // A plugin's answer is capped at the rpc result limit, whatever `max_response_bytes` is.
    let max_bytes = match tool {
        McpToolRef::Builtin(_) => state.max_response_bytes,
        McpToolRef::Plugin(_) => state.max_response_bytes.min(PLUGIN_RESULT_MAX_BYTES),
    };
    let retry_after = match tool {
        McpToolRef::Plugin(_) => retry_after_secs(response.headers()),
        McpToolRef::Builtin(_) => None,
    };
    let text = if tool.name() == "me_split_stream" && status.is_success() {
        let bytes = read_capped(response.into_body(), max_bytes).await;
        let raw_text = body_to_text(&bytes, max_bytes);
        redact_split_stream_json(&raw_text)
    } else if !tool.redact_secrets() {
        let bytes = read_capped(response.into_body(), max_bytes).await;
        body_to_text(&bytes, max_bytes)
    } else if status.is_success() {
        // Redaction needs the whole document; truncation applies to the redacted text.
        let bytes = read_capped(response.into_body(), REDACT_BODY_LIMIT).await;
        body_to_text(redact_json_body(&bytes).as_bytes(), max_bytes)
    } else if matches!(tool, McpToolRef::Plugin(_)) {
        // The rpc route's `PluginRpcErrorDto` (already sanitized and scrubbed of
        // the plugin's secrets) tells the model what to fix; any other body of a
        // plugin tool is not forwarded.
        let bytes = read_capped(response.into_body(), PLUGIN_ERROR_BODY_LIMIT).await;
        plugin_error_text(&bytes, max_bytes)
    } else {
        // An error body of a redacted tool is not forwarded; the status is enough.
        String::new()
    };
    let mut result = if status.is_success() {
        match output_schema_result(&tool, &text) {
            Ok(structured) => {
                let mut result = tool_result(false, &text);
                // A complete JSON object answer is also given as `structuredContent`;
                // a truncated answer carries a marker and no longer parses.
                if let Some(structured) = structured {
                    result["structuredContent"] = structured;
                }
                result
            }
            // The plugin's answer does not fit the output schema it declared:
            // the result is discarded, only the short reason reaches the model.
            Err(message) => tool_result(true, &message),
        }
    } else {
        let mut message = if text.is_empty() { format!("HTTP {status}") } else { format!("HTTP {status}: {text}") };
        if let Some(seconds) = retry_after {
            let _ = write!(message, " (retry after {seconds}s)");
        }
        tool_result(true, &message)
    };
    if let McpToolRef::Plugin(plugin_tool) = tool {
        result["_meta"] = json!({ "tuliprox/plugin": plugin_tool.plugin_id });
    }
    Ok(result)
}

/// The `structuredContent` [`call_tool_in`] may attach, or the short reason a
/// successful answer cannot be handed out.
///
/// A plugin tool that declares an `output_schema` is held to it: the answer must
/// be a JSON object (the spec's structured content), complete (not truncated),
/// and match the schema. Anything else is a tool error and the answer is
/// discarded. A tool without an `output_schema` keeps the additive behavior: a
/// complete JSON object is also structured content, everything else is text.
fn output_schema_result(tool: &McpToolRef<'_>, text: &str) -> Result<Option<Value>, String> {
    let output_schema = tool.output_schema();
    let declared = output_schema.is_some();
    if truncated_marker(text) {
        return if declared {
            Err(format!("The tool '{}' declares an outputSchema: its truncated answer was discarded", tool.name()))
        } else {
            Ok(None)
        };
    }
    let parsed = serde_json::from_str::<Value>(text);
    if !declared {
        return Ok(parsed.ok().filter(Value::is_object));
    }
    let value = parsed.map_err(|_| {
        format!(
            "The tool '{}' declares an outputSchema but its answer is not JSON; the answer was discarded",
            tool.name()
        )
    })?;
    if !value.is_object() {
        return Err(format!(
            "The tool '{}' declares an outputSchema but its answer is not a JSON object; the answer was discarded",
            tool.name()
        ));
    }
    let violations = tool.output_violations(&value);
    if violations.is_empty() {
        return Ok(Some(value));
    }
    let mut paths: Vec<&str> = violations.iter().map(String::as_str).take(5).collect();
    paths.sort_unstable();
    let more = if violations.len() > paths.len() { ", …" } else { "" };
    Err(format!(
        "The tool '{}' answered outside its outputSchema ({}{more}); the answer was discarded",
        tool.name(),
        paths.join(", ")
    ))
}

/// Whether `text` ends with the truncation marker [`body_to_text`] appends.
fn truncated_marker(text: &str) -> bool {
    text.rsplit_once("\n[truncated: response exceeded ").is_some_and(|(_, tail)| tail.ends_with(" bytes]"))
}

/// The rpc route's result limit (plan §7: plugin tool results are capped at it).
const PLUGIN_RESULT_MAX_BYTES: usize = 256 * 1024;

/// The most of an rpc error body that is read: the DTO is a message and a list of fields.
const PLUGIN_ERROR_BODY_LIMIT: usize = 64 * 1024;

/// `Retry-After` of a `429`, when it is whole seconds.
fn retry_after_secs(headers: &HeaderMap) -> Option<u64> {
    headers.get(header::RETRY_AFTER)?.to_str().ok()?.trim().parse().ok()
}

/// Redacts user password from reconnect URL in `me_split_stream` tool response.
fn redact_split_stream_json(text: &str) -> String {
    let Ok(mut val) = serde_json::from_str::<Value>(text) else {
        return text.to_string();
    };
    if let Some(url_val) = val.get_mut("url") {
        if let Some(url_str) = url_val.as_str() {
            let redacted = redact_xtream_live_password(url_str);
            *url_val = Value::String(redacted);
        }
    }
    serde_json::to_string(&val).unwrap_or_else(|_| text.to_string())
}

/// Replaces the password segment in `/live/{username}/{password}/{filename}` with `***`.
fn redact_xtream_live_password(url: &str) -> String {
    let (head, query) = match url.split_once('?') {
        Some((h, q)) => (h, Some(q)),
        None => (url, None),
    };
    let marker = "/live/";
    if let Some(idx) = head.find(marker) {
        let prefix = &head[..idx + marker.len()];
        let rest = &head[idx + marker.len()..];
        let mut parts = rest.splitn(3, '/');
        if let (Some(username), Some(_password), Some(filename)) = (parts.next(), parts.next(), parts.next()) {
            let mut out = format!("{prefix}{username}/***/{filename}");
            if let Some(q) = query {
                out.push('?');
                out.push_str(q);
            }
            return out;
        }
    }
    url.to_string()
}

/// The JSON of an rpc error body, redacted like a result; empty when the body is
/// not a `PluginRpcErrorDto` (the permission layers answer in other shapes).
fn plugin_error_text(body: &[u8], max_bytes: usize) -> String {
    let Ok(error) = serde_json::from_slice::<PluginRpcErrorDto>(body) else {
        return String::new();
    };
    let Ok(error) = serde_json::to_vec(&error) else {
        return String::new();
    };
    body_to_text(redact_json_body(&error).as_bytes(), max_bytes)
}

fn tool_result(is_error: bool, text: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

fn required_u64(args: &Map<String, Value>, key: &str) -> Result<u64, String> {
    args.get(key).and_then(Value::as_u64).ok_or_else(|| format!("Missing required argument '{key}'"))
}

fn required_i64(args: &Map<String, Value>, key: &str) -> Result<i64, String> {
    args.get(key).and_then(Value::as_i64).ok_or_else(|| format!("Missing required argument '{key}'"))
}

fn required_str<'a>(args: &'a Map<String, Value>, key: &str) -> Result<&'a str, String> {
    args.get(key).and_then(Value::as_str).ok_or_else(|| format!("Missing required argument '{key}'"))
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

/// A plugin tool's arguments are the rpc payload as they are: the route
/// validates them against the export's `input_schema`, so no argument is
/// mapped to a path or a query here.
fn build_plugin_request(tool: &OwnedMcpTool, arguments: Option<&Map<String, Value>>, api_prefix: &str) -> ToolRequest {
    let body = arguments.map_or_else(|| "{}".to_string(), |arguments| Value::Object(arguments.clone()).to_string());
    ToolRequest { uri: format!("{api_prefix}{}", tool.path), body: Some(body.into_bytes()) }
}

fn build_tool_request(
    tool: &McpTool,
    arguments: Option<&Map<String, Value>>,
    api_prefix: &str,
) -> Result<ToolRequest, String> {
    let empty = Map::new();
    let arguments = arguments.unwrap_or(&empty);
    let schema = tool.schema();
    reject_unknown_args(&schema, arguments)?;
    let required = schema.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str);
    for field in required {
        if arguments.get(field).is_none_or(Value::is_null) {
            return Err(format!("Missing required argument '{field}'"));
        }
    }

    let (path, used) = expand_path(tool.path, arguments)?;
    let rest = arguments.iter().filter(|(key, value)| !used.contains(&key.as_str()) && !value.is_null());

    match tool.method {
        McpToolMethod::Get | McpToolMethod::Delete => {
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
pub(super) fn encode_path_segment(value: &str) -> String {
    if value == "." || value == ".." {
        return value.replace('.', "%2E");
    }
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect::<String>().replace('+', "%20")
}

#[cfg(test)]
mod tests {
    use super::{
        super::mcp_tools::{find_tool, MCP_TOOLS},
        *,
    };
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
    use std::net::SocketAddr;
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

    fn token(config: &Config, permissions: impl Into<shared::model::PermissionSet>) -> String {
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

    /// Test routers get a loopback peer address, as a real serve loop would provide.
    fn mock_peer() -> axum::extract::connect_info::MockConnectInfo<SocketAddr> {
        axum::extract::connect_info::MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 12345)))
    }

    fn mcp_router(config: Config) -> Router {
        let web_auth_enabled = config.web_ui.as_ref().and_then(|web_ui| web_ui.auth.as_ref()).is_some();
        let app_state = create_test_app_state(config);
        // The MCP state keeps its own clone of the v1 router for tool dispatch, so both
        // routers need the peer address: the endpoint extracts it, the dispatched
        // request inherits it from the v1 router's layer.
        let v1 = v1_api_register(web_auth_enabled, &app_state, "");
        mcp_api_register_if_enabled::<()>(&v1, &app_state, web_auth_enabled, "")
            .expect("mcp enabled")
            .with_state(())
            .layer(mock_peer())
    }

    async fn post(router: &Router, body: &str, extra: &[(&str, &str)]) -> (StatusCode, Value) {
        let mut request = Request::builder().method("POST").uri("/mcp").header("content-type", "application/json");
        for (name, value) in extra {
            request = request.header(*name, *value);
        }
        let mut req = request.body(Body::from(body.to_owned())).expect("request");
        req.extensions_mut().insert(axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 12345))));
        let response = router.clone().oneshot(req).await.expect("response");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("body");
        let value = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).expect("json body") };
        (status, value)
    }

    fn test_fingerprint() -> crate::auth::Fingerprint {
        crate::auth::Fingerprint::from_headers_and_addr(&HeaderMap::new(), SocketAddr::from(([127, 0, 0, 1], 12345)))
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
        let view = ToolView::default();
        assert!(view.visible(true).all(|tool| tool.kind() == McpToolKind::Read));
        assert!(view.visible(false).any(|tool| tool.kind() == McpToolKind::Write));
        let names: Vec<_> =
            tools_list(&view, true).iter().map(|tool| tool["name"].as_str().unwrap().to_string()).collect();
        assert!(names.contains(&"get_status".to_string()));
        assert!(!names.contains(&"update_playlists".to_string()));
        assert!(!names.contains(&"update_imdb_ratings".to_string()));
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
            if !tool.is_internal {
                assert!(tool.path.starts_with('/'));
            }
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
        let names: Vec<_> = tools_list(&ToolView::default(), true)
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_string())
            .collect();
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
                // A duplicated key would silently collapse during JSON parsing. Only a
                // declaration that starts with the type is counted, so a nested object may
                // reuse a top-level name (an override patch carries its own `name` field).
                let declaration = format!("\"{name}\":{{\"type\"");
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

    // ---- override tools ---------------------------------------------------

    const OVERRIDE_TOOLS: [&str; 6] = [
        "get_overrides",
        "get_override_item",
        "list_override_orphans",
        "search_epg_ids",
        "save_overrides",
        "discard_overrides",
    ];

    #[test]
    fn override_tools_are_thin_adapters_over_their_rest_routes() {
        let expected = [
            ("get_overrides", McpToolKind::Read, McpToolMethod::Get, "/playlist/overrides"),
            ("get_override_item", McpToolKind::Read, McpToolMethod::Get, "/playlist/overrides/item"),
            ("list_override_orphans", McpToolKind::Read, McpToolMethod::Get, "/playlist/overrides/orphans"),
            ("search_epg_ids", McpToolKind::Read, McpToolMethod::Get, "/playlist/epg/search"),
            ("save_overrides", McpToolKind::Write, McpToolMethod::Post, "/playlist/overrides"),
            ("discard_overrides", McpToolKind::Write, McpToolMethod::Post, "/playlist/overrides/discard"),
        ];
        for (name, kind, method, path) in expected {
            let tool = tool(name);
            assert_eq!(tool.kind, kind, "{name}");
            assert_eq!(tool.method, method, "{name}");
            assert_eq!(tool.path, path, "{name}");
        }
        // The two writes are hidden as long as the server is read-only.
        let read_only: Vec<_> = tools_list(&ToolView::default(), true)
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_owned())
            .collect();
        let writable: Vec<_> = tools_list(&ToolView::default(), false)
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_owned())
            .collect();
        for name in ["get_overrides", "get_override_item", "list_override_orphans", "search_epg_ids"] {
            assert!(read_only.contains(&name.to_string()), "{name} is missing read-only");
        }
        for name in ["save_overrides", "discard_overrides"] {
            assert!(!read_only.contains(&name.to_string()), "{name} must not be listed read-only");
            assert!(writable.contains(&name.to_string()), "{name} is missing writable");
        }
    }

    /// Every object in a schema is closed, at any depth.
    fn assert_closed_objects(value: &Value, path: &str) {
        let Some(object) = value.as_object() else { return };
        if object.contains_key("properties") {
            assert_eq!(object.get("additionalProperties"), Some(&Value::Bool(false)), "{path} is not closed");
        }
        for (key, child) in object {
            assert_closed_objects(child, &format!("{path}.{key}"));
        }
    }

    #[test]
    fn override_schemas_are_strict_and_document_every_property() {
        for name in OVERRIDE_TOOLS {
            let tool = tool(name);
            assert!(tool.description.len() > 200, "{name} description too short");
            let schema: Value = serde_json::from_str(tool.input_schema).expect("schema is JSON");
            assert_eq!(schema["type"], "object", "{name}");
            assert_eq!(schema["additionalProperties"], false, "{name}");
            assert_closed_objects(&schema, name);
            for (property, definition) in schema["properties"].as_object().expect("properties") {
                let description = definition["description"].as_str().unwrap_or_default();
                assert!(!description.is_empty(), "{name}.{property} has no description");
            }
            for required in schema.get("required").and_then(Value::as_array).into_iter().flatten() {
                assert!(
                    schema["properties"].get(required.as_str().unwrap()).is_some(),
                    "{name} requires an undeclared field"
                );
            }
        }
        assert_eq!(tool("get_overrides").schema()["required"], json!(["scope", "name"]));
        assert_eq!(tool("list_override_orphans").schema()["required"], json!(["input"]));
        assert_eq!(tool("search_epg_ids").schema()["required"], json!(["q"]));
        assert_eq!(tool("get_override_item").schema()["required"], json!(["input", "kind", "id"]));
        assert_eq!(tool("save_overrides").schema()["required"], json!(["scope", "name", "base_revision", "patches"]));
        assert_eq!(tool("discard_overrides").schema()["required"], json!(["scope", "name", "base_revision"]));
        assert_eq!(schema_enum("save_overrides", "scope"), ["input", "target"]);
        assert_eq!(schema_enum("discard_overrides", "scope"), ["input", "target"]);
    }

    #[test]
    fn override_patch_schema_documents_the_tagged_op_format() {
        let schema = tool("save_overrides").schema();
        let item = &schema["properties"]["patches"]["items"];
        assert_eq!(item["additionalProperties"], false);
        assert_eq!(item["required"], json!(["kind", "id", "patch"]));
        assert_eq!(item["properties"]["kind"]["enum"], json!(["live", "vod", "series"]));
        let patch = &item["properties"]["patch"];
        assert_eq!(patch["additionalProperties"], false);
        let fields: Vec<&String> = patch["properties"].as_object().unwrap().keys().collect();
        assert!(fields.iter().any(|field| field.as_str() == "name"), "the patch schema has no name field");
        for field in [
            "audio_track",
            "cast",
            "chno",
            "director",
            "epg_channel_id",
            "genre",
            "group",
            "logo",
            "logo_small",
            "name",
            "parent_code",
            "plot",
            "rating",
            "rec",
            "release_date",
            "time_shift",
            "title",
            "tmdb",
        ] {
            let definition = &patch["properties"][field];
            assert_eq!(definition["properties"]["op"]["enum"], json!(["set", "clear", "reset"]), "{field}");
            assert_eq!(definition["required"], json!(["op"]), "{field}");
            assert_eq!(definition["additionalProperties"], false, "{field}");
            assert!(definition["properties"]["value"]["type"].is_string(), "{field} has no value type");
            assert!(
                definition["description"].as_str().unwrap_or_default().contains("Live items only")
                    || definition["description"].as_str().unwrap_or_default().contains("Video and series only")
                    || definition["description"].as_str().unwrap_or_default().contains("All item kinds"),
                "{field} does not say which item kinds it applies to"
            );
        }
        assert_eq!(patch["properties"]["remove_unknown"]["items"], json!({ "type": "string" }));
        let discard_item = &tool("discard_overrides").schema()["properties"]["items"]["items"];
        assert_eq!(discard_item["required"], json!(["kind", "id"]));
        assert_eq!(discard_item["additionalProperties"], false);
    }

    #[test]
    fn override_descriptions_name_the_follow_up_tools_and_the_409_rule() {
        let save = tool("save_overrides").description;
        for needle in ["base_revision", "get_overrides", "409", "update_playlists", "query_playlist_items", "op"] {
            assert!(save.contains(needle), "save_overrides description lacks {needle}");
        }
        for needle in ["set", "clear", "reset"] {
            assert!(save.contains(needle), "save_overrides description lacks the {needle} op");
        }
        let discard = tool("discard_overrides").description;
        for needle in ["base_revision", "get_overrides", "409", "update_playlists"] {
            assert!(discard.contains(needle), "discard_overrides description lacks {needle}");
        }
        assert!(tool("get_overrides").description.contains("revision"));
        assert!(tool("get_overrides").description.contains("query_playlist_items"));
        assert!(tool("get_override_item").description.contains("query_playlist_items"));
        assert!(tool("list_override_orphans").description.contains("discard_overrides"));
        assert!(tool("search_epg_ids").description.contains("epg_channel_id"));
    }

    #[test]
    fn override_arguments_become_the_documented_query_and_body() {
        let request = build_tool_request(
            tool("get_overrides"),
            Some(&args(json!({ "scope": "target", "name": "my target" }))),
            "/api/v1",
        )
        .unwrap();
        assert_eq!(request.uri, "/api/v1/playlist/overrides?scope=target&name=my+target");
        let request = build_tool_request(
            tool("get_override_item"),
            Some(&args(json!({ "input": "m3u-input", "kind": "vod", "id": "42", "target_id": 2 }))),
            "/api/v1",
        )
        .unwrap();
        assert_eq!(request.uri, "/api/v1/playlist/overrides/item?input=m3u-input&kind=vod&id=42&target_id=2");
        let request = build_tool_request(
            tool("search_epg_ids"),
            Some(&args(json!({ "q": "news hd", "input": "m3u-input", "limit": 5 }))),
            "/api/v1",
        )
        .unwrap();
        assert_eq!(request.uri, "/api/v1/playlist/epg/search?q=news+hd&input=m3u-input&limit=5");

        // Nested bodies survive the adapter unchanged: the REST handler sees the
        // tagged operations, not a stringified version of them.
        let patches = json!([{
            "kind": "live",
            "id": "p1",
            "patch": { "group": { "op": "set", "value": "News" }, "title": { "op": "clear" } }
        }]);
        let request = build_tool_request(
            tool("save_overrides"),
            Some(&args(json!({ "scope": "input", "name": "m3u-input", "base_revision": "r1", "patches": patches }))),
            "/api/v1",
        )
        .unwrap();
        assert_eq!(request.uri, "/api/v1/playlist/overrides");
        let body: Value = serde_json::from_slice(&request.body.expect("body")).unwrap();
        assert_eq!(body["scope"], "input");
        assert_eq!(body["base_revision"], "r1");
        assert_eq!(body["patches"][0]["patch"]["group"], json!({ "op": "set", "value": "News" }));
        assert_eq!(body["patches"][0]["patch"]["title"], json!({ "op": "clear" }));

        let err = build_tool_request(
            tool("save_overrides"),
            Some(&args(json!({ "scope": "input", "name": "n", "patches": [] }))),
            "/api/v1",
        )
        .err()
        .unwrap();
        assert!(err.contains("base_revision"), "{err}");
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
        assert!(instructions.len() <= 1500, "instructions are too long: {}", instructions.len());

        let env = setup_mcp_player_test_env(shared::model::McpPlayerTools::Write).await;
        let alice_headers = auth_headers(&env.alice_token);
        let reply_alice = handle_message(
            &env.state,
            &alice_headers,
            rpc(2, "initialize", json!({})).as_bytes(),
            None,
            &test_fingerprint(),
        )
        .await
        .unwrap();
        let player_instructions = reply_alice["result"]["instructions"].as_str().expect("player instructions");
        assert_eq!(player_instructions, MCP_PLAYER_INSTRUCTIONS);
        assert_ne!(player_instructions.trim(), "");
        assert!(player_instructions.len() <= 1500, "player instructions are too long: {}", player_instructions.len());
    }

    const PLAYER_INSTRUCTION_TOOLS: [&str; 17] = [
        "me_capabilities",
        "me_search_channels",
        "me_get_epg",
        "me_list_streams",
        "me_stop_stream",
        "me_list_sources",
        "me_switch_source",
        "me_split_stream",
        "me_list_joinable",
        "me_list_recordings",
        "me_record_program",
        "me_cancel_recording",
        "me_delete_recording",
        "me_recording_conflicts",
        "me_list_rules",
        "me_create_rule",
        "me_delete_rule",
    ];

    #[test]
    fn instructions_only_name_existing_tools() {
        for name in PLAYLIST_TOOLS {
            assert!(MCP_INSTRUCTIONS.contains(name), "admin instructions do not mention {name}");
        }
        for word in MCP_INSTRUCTIONS.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
            let looks_like_tool = ["list_", "query_", "get_", "update_"].iter().any(|prefix| word.starts_with(prefix));
            if looks_like_tool && !word.ends_with('_') {
                assert!(find_tool(word).is_some(), "admin instructions name unknown tool {word}");
            }
        }
        for name in PLAYER_INSTRUCTION_TOOLS {
            assert!(MCP_PLAYER_INSTRUCTIONS.contains(name), "player instructions do not mention {name}");
        }
        for word in MCP_PLAYER_INSTRUCTIONS.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
            let looks_like_tool =
                ["me_", "list_", "query_", "get_", "update_"].iter().any(|prefix| word.starts_with(prefix));
            if looks_like_tool && !word.ends_with('_') {
                assert!(find_tool(word).is_some(), "player instructions name unknown tool {word}");
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
        assert!(!names.contains(&"update_imdb_ratings"));

        let (_, reply) = post(&router, &call("update_playlists", json!({})), &headers).await;
        assert_eq!(reply["error"]["code"], i64::from(INVALID_PARAMS));
        let (_, reply) = post(&router, &call("update_imdb_ratings", json!({})), &headers).await;
        assert_eq!(reply["error"]["code"], i64::from(INVALID_PARAMS));
    }

    #[tokio::test]
    async fn web_auth_disabled_forces_read_only_even_when_configured_writable() {
        let router = mcp_router(config_with_mcp(false, enabled(false)));
        let (_, reply) = post(&router, &rpc(1, "tools/list", json!({})), &[]).await;
        let names: Vec<_> =
            reply["result"]["tools"].as_array().unwrap().iter().map(|tool| tool["name"].as_str().unwrap()).collect();
        assert!(!names.contains(&"update_playlists"));
        assert!(!names.contains(&"update_imdb_ratings"));
        let (_, reply) = post(&router, &call("update_geoip", json!({})), &[]).await;
        assert_eq!(reply["error"]["code"], i64::from(INVALID_PARAMS));
        let (_, reply) = post(&router, &call("update_imdb_ratings", json!({})), &[]).await;
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
        assert!(names.contains(&"update_imdb_ratings"));

        let (_, reply) = post(&router, &call("update_geoip", json!({})), &headers).await;
        assert_eq!(reply["result"]["isError"], true);
        assert!(tool_text(&reply).starts_with("HTTP 403"), "{}", tool_text(&reply));

        let (_, reply) = post(&router, &call("update_imdb_ratings", json!({})), &headers).await;
        assert_eq!(reply["result"]["isError"], true);
        assert!(tool_text(&reply).starts_with("HTTP 403"), "{}", tool_text(&reply));
    }

    // ---- override tools over the REST routes ------------------------------

    const OVERRIDE_INPUT: &str = "m3u-input";
    const OVERRIDE_TARGET: &str = "m3u-target";

    /// A playground with one input, one target and override files in a temp dir,
    /// so the override tools hit the real handlers of the `/api/v1` router.
    struct OverrideFixture {
        _dir: tempfile::TempDir,
        router: Router,
        reader: String,
        writer: String,
    }

    fn override_input() -> Arc<crate::model::ConfigInput> {
        use shared::utils::Internable;

        Arc::new(crate::model::ConfigInput {
            id: 1,
            name: OVERRIDE_INPUT.intern(),
            input_type: shared::model::InputType::M3u,
            url: "http://provider.example/get.php?username=user&password=pass".to_string(),
            username: Some("user".to_string()),
            password: Some("pass".to_string()),
            ..crate::model::ConfigInput::default()
        })
    }

    fn override_target() -> Arc<crate::model::ConfigTarget> {
        use shared::foundation::Filter;

        Arc::new(crate::model::ConfigTarget {
            managed_channels: Vec::new(),
            curation: None,
            id: 1,
            enabled: true,
            name: OVERRIDE_TARGET.to_string(),
            options: None,
            sort: None,
            filter: Filter::default().into(),
            output: vec![crate::model::TargetOutput::M3u(crate::model::M3uTargetOutput {
                filename: None,
                include_type_in_url: false,
                mask_redirect_url: false,
                filter: None,
            })],
            rename: None,
            mapping_ids: None,
            mapping: Arc::new(arc_swap::ArcSwapOption::new(None)),
            favourites: None,
            processing_order: shared::model::ProcessingOrder::default(),
            execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
            watch: None,
            use_memory_cache: false,
        })
    }

    /// `ConfigPaths` without any directory, as the default test state holds it.
    fn empty_paths() -> shared::model::ConfigPaths {
        shared::model::ConfigPaths {
            home_path: String::new(),
            config_path: String::new(),
            storage_path: String::new(),
            config_file_path: String::new(),
            sources_file_path: String::new(),
            mapping_file_path: None,
            mapping_files_used: None,
            template_file_path: None,
            template_files_used: None,
            failover_file_path: None,
            api_proxy_file_path: String::new(),
            custom_stream_response_path: None,
        }
    }

    fn override_fixture(mcp_read_only: bool) -> OverrideFixture {
        use shared::utils::Internable;

        let dir = tempfile::tempdir().expect("temp dir");
        let config_dir = dir.path().join("config");
        let storage_dir = dir.path().join("storage");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        std::fs::create_dir_all(&storage_dir).expect("storage dir");

        let mut config = config_with_mcp(true, enabled(mcp_read_only));
        config.storage_dir = storage_dir.to_string_lossy().into_owned();
        let reader = bearer(&token(&config, Permission::PlaylistRead));
        let writer = bearer(&token(&config, Permission::PlaylistRead | Permission::PlaylistWrite));

        let app_state = create_test_app_state(config);
        app_state.app_config.paths.store(Arc::new(shared::model::ConfigPaths {
            config_path: config_dir.to_string_lossy().into_owned(),
            storage_path: storage_dir.to_string_lossy().into_owned(),
            ..empty_paths()
        }));
        let inputs = vec![override_input()];
        app_state.app_config.sources.store(Arc::new(crate::model::SourcesConfig {
            group_lookup: shared::model::provider_saturation::build_group_lookup(&inputs),
            inputs,
            sources: vec![crate::model::ConfigSource {
                inputs: vec![OVERRIDE_INPUT.intern()],
                targets: vec![override_target()],
            }],
            ..Default::default()
        }));

        let v1 = v1_api_register(true, &app_state, "");
        let router = mcp_api_register_if_enabled::<()>(&v1, &app_state, true, "").expect("mcp enabled").with_state(());
        OverrideFixture { _dir: dir, router, reader, writer }
    }

    fn header_of(token: &str) -> [(&str, &str); 1] { [("authorization", token)] }

    /// Reads an override file through the tool and returns its parsed body.
    async fn read_overrides(fixture: &OverrideFixture, scope: &str, name: &str) -> Value {
        let headers = header_of(&fixture.reader);
        let arguments = json!({ "scope": scope, "name": name });
        let (_, reply) = post(&fixture.router, &call("get_overrides", arguments), &headers).await;
        assert_eq!(reply["result"]["isError"], false, "{}", tool_text(&reply));
        serde_json::from_str(tool_text(&reply)).expect("override file JSON")
    }

    /// Saves the given patches and returns the revision the handler answered.
    async fn save_overrides(fixture: &OverrideFixture, base: &str, patches: Value) -> Value {
        match save_overrides_result(fixture, base, patches).await {
            Ok(saved) => saved,
            Err(text) => panic!("save_overrides failed: {text}"),
        }
    }

    /// Saves the given patches and returns the revision or the tool error text.
    async fn save_overrides_result(fixture: &OverrideFixture, base: &str, patches: Value) -> Result<Value, String> {
        let headers = header_of(&fixture.writer);
        let arguments = json!({ "scope": "input", "name": OVERRIDE_INPUT, "base_revision": base, "patches": patches });
        let (_, reply) = post(&fixture.router, &call("save_overrides", arguments), &headers).await;
        let text = tool_text(&reply).to_owned();
        if reply["result"]["isError"] == Value::Bool(true) {
            return Err(text);
        }
        serde_json::from_str(&text).map_err(|err| format!("{err}: {text}"))
    }

    #[tokio::test]
    async fn save_overrides_round_trips_set_clear_and_reset_through_the_rest_handler() {
        let fixture = override_fixture(false);
        let base = read_overrides(&fixture, "input", OVERRIDE_INPUT).await;
        assert_eq!(base["entries"], json!([]), "a fresh input has no overrides");
        let base = base["revision"].as_str().expect("revision").to_owned();

        // set: the tagged op reaches the handler and is stored as the value
        let saved = save_overrides(
            &fixture,
            &base,
            json!([{ "kind": "live", "id": "p1", "patch": { "group": { "op": "set", "value": "News" } } }]),
        )
        .await;
        let revision = saved["revision"].as_str().expect("revision").to_owned();
        assert_ne!(revision, base, "a write changes the revision");

        let file = read_overrides(&fixture, "input", OVERRIDE_INPUT).await;
        assert_eq!(file["revision"], revision);
        assert_eq!(file["entries"][0]["id"], "p1");
        assert_eq!(file["entries"][0]["kind"], "live");
        assert_eq!(file["entries"][0]["fields"]["group"], "News");

        // clear: the field is stored as a null delta next to the set one
        let saved = save_overrides(
            &fixture,
            &revision,
            json!([{ "kind": "live", "id": "p1", "patch": { "chno": { "op": "clear" } } }]),
        )
        .await;
        let revision = saved["revision"].as_str().expect("revision").to_owned();
        let file = read_overrides(&fixture, "input", OVERRIDE_INPUT).await;
        assert_eq!(file["entries"][0]["fields"]["group"], "News");
        assert!(file["entries"][0]["fields"].as_object().unwrap().contains_key("chno"), "{file}");
        assert_eq!(file["entries"][0]["fields"]["chno"], Value::Null);
        // name, title and group can never be cleared; the handler rejects it as a field error
        let unsupported = save_overrides_result(
            &fixture,
            &revision,
            json!([{ "kind": "live", "id": "p1", "patch": { "title": { "op": "clear" } } }]),
        )
        .await
        .expect_err("a clear of title is a field error");
        assert!(unsupported.contains("cannot_clear"), "{unsupported}");

        // reset: both overrides are dropped and the entry disappears
        let saved = save_overrides(
            &fixture,
            &revision,
            json!([{ "kind": "live", "id": "p1", "patch": {
                "group": { "op": "reset" }, "chno": { "op": "reset" }
            } }]),
        )
        .await;
        assert!(saved["revision"].as_str().is_some_and(|revision| !revision.is_empty()));
        let file = read_overrides(&fixture, "input", OVERRIDE_INPUT).await;
        assert_eq!(file["entries"], json!([]), "a reset of every field removes the entry: {file}");
    }

    #[tokio::test]
    async fn save_overrides_reports_a_stale_revision_and_accepts_the_reread_one() {
        let fixture = override_fixture(false);
        let base = read_overrides(&fixture, "input", OVERRIDE_INPUT).await["revision"].as_str().unwrap().to_owned();
        let patches = json!([{ "kind": "live", "id": "p1", "patch": { "group": { "op": "set", "value": "A" } } }]);
        let saved = save_overrides(&fixture, &base, patches.clone()).await;
        assert!(saved["revision"].as_str().is_some());

        // the first revision is stale now: 409 with the revision to retry against
        let headers = header_of(&fixture.writer);
        let arguments = json!({ "scope": "input", "name": OVERRIDE_INPUT, "base_revision": base, "patches": patches });
        let (_, reply) = post(&fixture.router, &call("save_overrides", arguments), &headers).await;
        assert_eq!(reply["result"]["isError"], true);
        let text = tool_text(&reply);
        assert!(text.starts_with("HTTP 409"), "{text}");
        assert!(text.contains("current_revision"), "the 409 body carries the revision to retry with: {text}");

        let current = read_overrides(&fixture, "input", OVERRIDE_INPUT).await;
        let current = current["revision"].as_str().unwrap().to_owned();
        save_overrides(&fixture, &current, patches).await;
    }

    #[tokio::test]
    async fn discard_overrides_removes_a_stored_entry_through_the_rest_handler() {
        let fixture = override_fixture(false);
        let base = read_overrides(&fixture, "input", OVERRIDE_INPUT).await["revision"].as_str().unwrap().to_owned();
        let saved = save_overrides(
            &fixture,
            &base,
            json!([{ "kind": "live", "id": "p1", "patch": { "name": { "op": "set", "value": "Renamed" } } }]),
        )
        .await;
        let revision = saved["revision"].as_str().unwrap().to_owned();

        let headers = header_of(&fixture.writer);
        let arguments = json!({
            "scope": "input", "name": OVERRIDE_INPUT, "base_revision": revision,
            "items": [{ "kind": "live", "id": "p1" }]
        });
        let (_, reply) = post(&fixture.router, &call("discard_overrides", arguments), &headers).await;
        assert_eq!(reply["result"]["isError"], false, "{}", tool_text(&reply));
        let file = read_overrides(&fixture, "input", OVERRIDE_INPUT).await;
        assert_eq!(file["entries"], json!([]), "{file}");
    }

    #[tokio::test]
    async fn override_writes_are_refused_when_the_server_is_read_only() {
        let fixture = override_fixture(true);
        let headers = header_of(&fixture.writer);
        for name in ["save_overrides", "discard_overrides"] {
            let arguments = json!({ "scope": "input", "name": OVERRIDE_INPUT, "base_revision": "r" });
            let (_, reply) = post(&fixture.router, &call(name, arguments), &headers).await;
            assert_eq!(reply["error"]["code"], i64::from(INVALID_PARAMS), "{name}");
        }
        let (_, reply) = post(&fixture.router, &rpc(1, "tools/list", json!({})), &headers).await;
        let names: Vec<_> =
            reply["result"]["tools"].as_array().unwrap().iter().map(|tool| tool["name"].as_str().unwrap()).collect();
        assert!(!names.contains(&"save_overrides"), "{names:?}");
        assert!(!names.contains(&"discard_overrides"), "{names:?}");
        assert!(names.contains(&"get_overrides"), "{names:?}");
    }

    #[tokio::test]
    async fn override_writes_need_the_playlist_write_permission() {
        let fixture = override_fixture(false);
        let headers = header_of(&fixture.reader);
        let arguments = json!({
            "scope": "input", "name": OVERRIDE_INPUT, "base_revision": "r",
            "patches": [{ "kind": "live", "id": "p1", "patch": { "group": { "op": "set", "value": "A" } } }]
        });
        let (_, reply) = post(&fixture.router, &call("save_overrides", arguments), &headers).await;
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
        assert!(reply["result"].get("structuredContent").is_none(), "a truncated answer has no structured copy");
    }

    #[tokio::test]
    async fn a_json_object_result_is_also_structured_content() {
        let router = mcp_router(config_with_mcp(false, enabled(true)));
        let (_, reply) = post(&router, &call("get_status", json!({})), &[]).await;
        assert_eq!(reply["result"]["isError"], false);
        let text: Value = serde_json::from_str(tool_text(&reply)).expect("status is JSON");
        assert!(text.is_object());
        assert_eq!(reply["result"]["structuredContent"], text);
    }

    #[tokio::test]
    async fn web_ui_path_prefixes_the_endpoint_and_the_dispatched_routes() {
        let config = config_with_mcp(false, enabled(true));
        let app_state = create_test_app_state(config);
        let v1 = v1_api_register(false, &app_state, "ui");
        let router =
            mcp_api_register_if_enabled::<()>(&v1, &app_state, false, "ui").unwrap().with_state(()).layer(mock_peer());
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
        McpState::new(router, &McpConfig::from(enabled(true)), true, "", create_test_app_state(Config::default()))
    }

    async fn call_stub(state: &McpState, name: &str) -> Value {
        handle_message(state, &HeaderMap::new(), call(name, json!({})).as_bytes(), None, &test_fingerprint())
            .await
            .expect("reply")
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
                "get_target_epg_channel",
                "get_overrides",
                "get_override_item",
                "list_override_orphans"
            ]
        );
        let reply = call_stub(&stub_state("{}", StatusCode::OK), "get_status").await;
        assert!(tool_text(&reply).contains("\"password\":\"pw\""), "{}", tool_text(&reply));
    }

    // ---- plugin tools -----------------------------------------------------

    fn plugin_tool(name: &str, kind: McpToolKind) -> OwnedMcpTool {
        OwnedMcpTool {
            name: name.to_string(),
            description: "[plugin demo@1.0.0] Ping".to_string(),
            input_schema: json!({ "type": "object", "properties": { "league": { "type": "string" } } }),
            method: McpToolMethod::Post,
            path: "/plugins/demo/rpc/ping".to_string(),
            kind,
            redact_secrets: true,
            plugin_id: "demo".to_string(),
            destructive: false,
            output_schema: None,
            input_scoped: false,
        }
    }

    /// `plugin_tool` with an `output_schema`: `{"items": [{…}]}`.
    fn schema_tool(name: &str) -> OwnedMcpTool {
        let mut tool = plugin_tool(name, McpToolKind::Read);
        tool.output_schema = Some(json!({
            "type": "object",
            "required": ["items"],
            "additionalProperties": false,
            "properties": {
                "items": { "type": "array", "items": { "type": "object", "properties": { "id": { "type": "string" } } } },
                "next_cursor": { "type": ["string", "null"] }
            }
        }));
        tool
    }

    fn plugin_view(tools: Vec<OwnedMcpTool>) -> ToolView { ToolView::new(true, || tools) }

    fn plugin_state(router: Router, read_only: bool, max_response_bytes: usize) -> McpState {
        let dto = shared::model::McpConfigDto {
            enabled: true,
            read_only,
            plugin_tools: true,
            max_response_bytes,
            ..Default::default()
        };
        let config = Config { mcp: Some(McpConfig::from(dto.clone())), ..Config::default() };
        McpState::new(router, &McpConfig::from(dto), read_only, "", create_test_app_state(config))
    }

    fn rpc_router<H, T>(handler: H) -> Router
    where
        H: axum::handler::Handler<T, ()>,
        T: 'static,
    {
        Router::new().route("/api/v1/plugins/demo/rpc/ping", axum::routing::post(handler))
    }

    async fn call_plugin(state: &McpState, view: &ToolView, name: &str, arguments: Value) -> Result<Value, String> {
        let params = json!({ "name": name, "arguments": arguments });
        call_tool_in(state, view, &HeaderMap::new(), Some(&params), None, &test_fingerprint()).await
    }

    fn listed(view: &ToolView, read_only: bool) -> Vec<String> {
        tools_list(view, read_only).iter().map(|tool| tool["name"].as_str().unwrap().to_string()).collect()
    }

    fn result_text(reply: &Value) -> &str { reply["content"][0]["text"].as_str().expect("tool text") }

    #[cfg(feature = "plugins")]
    #[test]
    fn plugin_tools_are_derived_on_the_blocking_pool() {
        use crate::api::model::{counting_runtime, create_test_app_state_with_plugins, create_test_plugin_system};
        let dir = tempfile::tempdir().unwrap();
        let (runtime, threads) = counting_runtime();
        runtime.block_on(async {
            let dto = shared::model::McpConfigDto { enabled: true, plugin_tools: true, ..Default::default() };
            let config = Config { mcp: Some(McpConfig::from(dto.clone())), ..Config::default() };
            let app_state = create_test_app_state_with_plugins(config, Some(create_test_plugin_system(dir.path())));
            let state = McpState::new(rpc_router(|| async { "{}" }), &McpConfig::from(dto), true, "", app_state);
            let before = threads.load(std::sync::atomic::Ordering::SeqCst);
            assert!(listed(&state.tool_view().await, true).iter().all(|name| !name.starts_with("plugin_")));
            assert!(
                threads.load(std::sync::atomic::Ordering::SeqCst) > before,
                "approvals were read on the async thread"
            );
        });
    }

    #[test]
    fn plugin_tools_are_absent_while_the_global_switch_is_off() {
        let asked = std::cell::Cell::new(false);
        let view = ToolView::new(false, || {
            asked.set(true);
            vec![plugin_tool("plugin_demo_ping", McpToolKind::Read)]
        });
        assert!(!asked.get(), "the plugin host is not asked while mcp.plugin_tools is off");
        assert!(view.find("plugin_demo_ping").is_none());
        assert!(!listed(&view, false).contains(&"plugin_demo_ping".to_string()));
        let admin_tool_count = MCP_TOOLS.iter().filter(|t| t.audience == McpAudience::Admin).count();
        assert_eq!(listed(&view, false).len(), admin_tool_count);
    }

    #[tokio::test]
    async fn a_plugin_tool_that_is_not_listed_cannot_be_called() {
        let state = plugin_state(rpc_router(|| async { "{}" }), false, 262_144);
        let off = ToolView::new(false, || vec![plugin_tool("plugin_demo_ping", McpToolKind::Read)]);
        let err = call_plugin(&state, &off, "plugin_demo_ping", json!({})).await.unwrap_err();
        assert_eq!(err, "Unknown tool: plugin_demo_ping");
    }

    #[test]
    fn the_switch_on_lists_plugin_tools_beside_the_built_ins() {
        let view = plugin_view(vec![
            plugin_tool("plugin_demo_ping", McpToolKind::Read),
            plugin_tool("plugin_demo_reset", McpToolKind::Write),
        ]);
        let list = tools_list(&view, false);
        let ping = list.iter().find(|tool| tool["name"] == "plugin_demo_ping").expect("listed");
        assert_eq!(ping["description"], "[plugin demo@1.0.0] Ping");
        assert_eq!(ping["inputSchema"]["properties"]["league"]["type"], "string");
        assert_eq!(ping["annotations"], json!({ "readOnlyHint": true, "openWorldHint": true }));
        let reset = list.iter().find(|tool| tool["name"] == "plugin_demo_reset").expect("listed");
        assert_eq!(
            reset["annotations"],
            json!({ "readOnlyHint": false, "destructiveHint": false, "openWorldHint": true }),
            "a plugin write is not destructive unless its export says so"
        );
        let status = list.iter().find(|tool| tool["name"] == "get_status").expect("listed");
        assert_eq!(status["annotations"], json!({ "readOnlyHint": true, "openWorldHint": false }));
        let update = list.iter().find(|tool| tool["name"] == "update_playlists").expect("listed");
        assert_eq!(
            update["annotations"],
            json!({ "readOnlyHint": false, "destructiveHint": false, "openWorldHint": false })
        );
        assert!(listed(&view, false).contains(&"get_status".to_string()));
    }

    #[test]
    fn a_plugin_tool_never_shadows_a_built_in() {
        let mut shadow = plugin_tool("get_status", McpToolKind::Read);
        shadow.path = "/plugins/demo/rpc/get-status".to_string();
        let view = plugin_view(vec![shadow]);
        assert!(matches!(view.find("get_status"), Some(McpToolRef::Builtin(_))));
        assert_eq!(listed(&view, false).iter().filter(|name| *name == "get_status").count(), 1);
    }

    #[tokio::test]
    async fn a_write_plugin_tool_is_absent_and_rejected_while_read_only() {
        let view = plugin_view(vec![
            plugin_tool("plugin_demo_ping", McpToolKind::Read),
            plugin_tool("plugin_demo_reset", McpToolKind::Write),
        ]);
        let names = listed(&view, true);
        assert!(names.contains(&"plugin_demo_ping".to_string()));
        assert!(!names.contains(&"plugin_demo_reset".to_string()));

        let hit = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen = Arc::clone(&hit);
        let state = plugin_state(
            rpc_router(move || {
                let seen = Arc::clone(&seen);
                async move {
                    seen.store(true, std::sync::atomic::Ordering::SeqCst);
                    "{}"
                }
            }),
            true,
            262_144,
        );
        let err = call_plugin(&state, &view, "plugin_demo_reset", json!({})).await.unwrap_err();
        assert_eq!(err, "Tool 'plugin_demo_reset' is not available in read-only mode");
        assert!(!hit.load(std::sync::atomic::Ordering::SeqCst), "a rejected call never reaches the route");

        // The same tool is listed and callable once MCP is read/write.
        assert!(listed(&view, false).contains(&"plugin_demo_reset".to_string()));
        let state = plugin_state(rpc_router(|| async { "{}" }), false, 262_144);
        let reply = call_plugin(&state, &view, "plugin_demo_reset", json!({})).await.unwrap();
        assert_eq!(reply["isError"], false, "{reply}");
    }

    #[tokio::test]
    async fn read_only_set_after_startup_still_rejects_write_plugin_tools() {
        // Started read/write; the live config now says read-only.
        let dto =
            shared::model::McpConfigDto { enabled: true, read_only: true, plugin_tools: true, ..Default::default() };
        let config = Config { mcp: Some(McpConfig::from(dto.clone())), ..Config::default() };
        let state = McpState::new(
            rpc_router(|| async { "{}" }),
            &McpConfig::from(dto),
            false,
            "",
            create_test_app_state(config),
        );
        assert!(state.effective_read_only());
        let view = plugin_view(vec![plugin_tool("plugin_demo_reset", McpToolKind::Write)]);
        assert!(call_plugin(&state, &view, "plugin_demo_reset", json!({})).await.is_err());
    }

    #[tokio::test]
    async fn plugin_results_are_redacted_and_tagged_with_the_plugin() {
        let state = plugin_state(
            rpc_router(|| async { r#"{"api_key":"k-123","password":"hunter2","fixtures":[{"home":"A"}]}"# }),
            true,
            262_144,
        );
        let view = plugin_view(vec![plugin_tool("plugin_demo_ping", McpToolKind::Read)]);
        let reply = call_plugin(&state, &view, "plugin_demo_ping", json!({})).await.unwrap();
        assert_eq!(reply["isError"], false);
        let text = result_text(&reply);
        for leaked in ["k-123", "hunter2"] {
            assert!(!text.contains(leaked), "{leaked} leaked: {text}");
        }
        let parsed: Value = serde_json::from_str(text).expect("redacted JSON");
        assert_eq!(parsed["fixtures"][0]["home"], "A");
        assert_eq!(reply["_meta"], json!({ "tuliprox/plugin": "demo" }));
    }

    #[test]
    fn plugin_tool_annotations_follow_the_kind_and_the_destructive_flag() {
        let mut destructive = plugin_tool("plugin_demo_drop", McpToolKind::Write);
        destructive.destructive = true;
        let view = plugin_view(vec![
            plugin_tool("plugin_demo_ping", McpToolKind::Read),
            plugin_tool("plugin_demo_reset", McpToolKind::Write),
            destructive,
        ]);
        let list = tools_list(&view, false);
        let find = |name: &str| list.iter().find(|tool| tool["name"] == name).expect("listed").clone();

        // A plugin may call an external service: it keeps the spec's open world.
        assert_eq!(find("plugin_demo_ping")["annotations"], json!({ "readOnlyHint": true, "openWorldHint": true }));
        assert_eq!(
            find("plugin_demo_reset")["annotations"],
            json!({ "readOnlyHint": false, "destructiveHint": false, "openWorldHint": true })
        );
        assert_eq!(
            find("plugin_demo_drop")["annotations"],
            json!({ "readOnlyHint": false, "destructiveHint": true, "openWorldHint": true })
        );
        // The built-in annotations are unchanged, except destructive tools.
        assert_eq!(find("get_status")["annotations"], json!({ "readOnlyHint": true, "openWorldHint": false }));
        assert_eq!(
            find("update_playlists")["annotations"],
            json!({ "readOnlyHint": false, "destructiveHint": false, "openWorldHint": false })
        );
        assert_eq!(
            find("save_overrides")["annotations"],
            json!({ "readOnlyHint": false, "destructiveHint": true, "openWorldHint": false })
        );
        assert_eq!(
            find("discard_overrides")["annotations"],
            json!({ "readOnlyHint": false, "destructiveHint": true, "openWorldHint": false })
        );
    }

    #[test]
    fn an_output_schema_is_listed_as_output_schema_and_only_for_plugins() {
        let list = tools_list(&plugin_view(vec![schema_tool("plugin_demo_search")]), false);
        let search = list.iter().find(|tool| tool["name"] == "plugin_demo_search").expect("listed");
        assert_eq!(search["outputSchema"]["required"], json!(["items"]));
        assert_eq!(search["inputSchema"]["properties"]["league"]["type"], "string", "the input schema stays");
        for builtin in list.iter().filter(|tool| tool["name"] != "plugin_demo_search") {
            assert!(builtin.get("outputSchema").is_none(), "{builtin}");
        }
    }

    #[tokio::test]
    async fn a_result_that_matches_the_output_schema_is_structured_content() {
        let body = r#"{"items":[{"id":"a"}],"next_cursor":null}"#;
        let state = plugin_state(rpc_router(move || async move { body }), true, 262_144);
        let view = plugin_view(vec![schema_tool("plugin_demo_search")]);
        let reply = call_plugin(&state, &view, "plugin_demo_search", json!({})).await.unwrap();
        assert_eq!(reply["isError"], false, "{reply}");
        assert_eq!(reply["structuredContent"], json!({ "items": [{ "id": "a" }], "next_cursor": null }));
        assert_eq!(reply["_meta"], json!({ "tuliprox/plugin": "demo" }));
        assert!(result_text(&reply).contains(r#""items""#));
    }

    #[cfg(feature = "plugins")]
    #[tokio::test]
    async fn a_result_outside_the_output_schema_is_an_error_and_discarded() {
        let body = r#"{"items":"not a list","leak":"SECRET-VALUE"}"#;
        let state = plugin_state(rpc_router(move || async move { body }), true, 262_144);
        let view = plugin_view(vec![schema_tool("plugin_demo_search")]);
        let reply = call_plugin(&state, &view, "plugin_demo_search", json!({})).await.unwrap();
        assert_eq!(reply["isError"], true, "{reply}");
        assert!(reply.get("structuredContent").is_none(), "{reply}");
        let text = result_text(&reply);
        assert!(text.contains("outputSchema") && text.contains("items"), "{text}");
        assert!(!text.contains("SECRET-VALUE"), "the discarded answer does not reach the model: {text}");
    }

    #[tokio::test]
    async fn a_non_object_or_non_json_result_with_an_output_schema_is_an_error() {
        for body in ["[1,2]", r#""text""#, "42", "not json"] {
            let state = plugin_state(rpc_router(move || async move { body }), true, 262_144);
            let view = plugin_view(vec![schema_tool("plugin_demo_search")]);
            let reply = call_plugin(&state, &view, "plugin_demo_search", json!({})).await.unwrap();
            assert_eq!(reply["isError"], true, "{body}: {reply}");
            assert!(reply.get("structuredContent").is_none(), "{body}");
            assert!(result_text(&reply).contains("discarded"), "{body}: {}", result_text(&reply));
        }
    }

    #[tokio::test]
    async fn a_truncated_result_with_an_output_schema_is_an_error() {
        let body = format!(r#"{{"items":["{}"]}}"#, "a".repeat(200));
        let state = plugin_state(rpc_router(move || async move { body }), true, 32);
        let view = plugin_view(vec![schema_tool("plugin_demo_search")]);
        let reply = call_plugin(&state, &view, "plugin_demo_search", json!({})).await.unwrap();
        assert_eq!(reply["isError"], true, "{reply}");
        assert!(reply.get("structuredContent").is_none());
        assert!(result_text(&reply).contains("truncated"), "{}", result_text(&reply));
    }

    #[tokio::test]
    async fn without_an_output_schema_a_non_object_result_stays_text() {
        let state = plugin_state(rpc_router(|| async { "[1,2]" }), true, 262_144);
        let view = plugin_view(vec![plugin_tool("plugin_demo_ping", McpToolKind::Read)]);
        let reply = call_plugin(&state, &view, "plugin_demo_ping", json!({})).await.unwrap();
        assert_eq!(reply["isError"], false, "{reply}");
        assert!(reply.get("structuredContent").is_none(), "only a JSON object becomes structured content");
    }

    #[cfg(feature = "plugins")]
    #[test]
    fn a_plugin_tool_is_mapped_to_a_forced_redaction_post_of_the_rpc_route() {
        for (kind, mapped) in [
            (tuliprox_plugins::PluginMcpToolKind::Read, McpToolKind::Read),
            (tuliprox_plugins::PluginMcpToolKind::Write, McpToolKind::Write),
        ] {
            let tool = OwnedMcpTool::from(&tuliprox_plugins::PluginMcpTool {
                name: "plugin_demo_ping".to_string(),
                description: "[plugin demo@1.0.0]".to_string(),
                input_schema: json!({ "type": "object" }),
                plugin_id: "demo".to_string(),
                method: "ping-it".to_string(),
                kind,
                destructive: true,
                output_schema: Some(json!({ "type": "object" })),
                input_scoped: true,
            });
            assert!(tool.destructive && tool.input_scoped);
            assert_eq!(tool.output_schema, Some(json!({ "type": "object" })));
            assert!(tool.redact_secrets);
            assert_eq!(tool.method, McpToolMethod::Post);
            assert_eq!(tool.path, "/plugins/demo/rpc/ping-it");
            assert_eq!(tool.kind, mapped);
            assert_eq!(tool.plugin_id, "demo");
        }
    }

    #[tokio::test]
    async fn arguments_are_the_rpc_body_as_they_are() {
        let state = plugin_state(
            rpc_router(|body: Bytes| async move {
                let seen: Value = serde_json::from_slice(&body).expect("the body is JSON");
                axum::Json(json!({ "seen": seen }))
            }),
            true,
            262_144,
        );
        let view = plugin_view(vec![plugin_tool("plugin_demo_ping", McpToolKind::Read)]);
        // No path or query mapping: whatever the keys are, the body is the plugin's business.
        let arguments = json!({ "league": "epl", "limit": 5, "nested": { "a": [1, 2] }, "id": "x" });
        let reply = call_plugin(&state, &view, "plugin_demo_ping", arguments.clone()).await.unwrap();
        let parsed: Value = serde_json::from_str(result_text(&reply)).unwrap();
        assert_eq!(parsed["seen"], arguments);

        let reply = call_plugin(&state, &view, "plugin_demo_ping", Value::Null).await.unwrap();
        let parsed: Value = serde_json::from_str(result_text(&reply)).unwrap();
        assert_eq!(parsed["seen"], json!({}));
    }

    #[test]
    fn a_plugin_request_goes_to_the_rpc_route() {
        let tool = plugin_tool("plugin_demo_ping", McpToolKind::Read);
        let request = build_plugin_request(&tool, Some(&args(json!({ "league": "epl" }))), "/ui/api/v1");
        assert_eq!(request.uri, "/ui/api/v1/plugins/demo/rpc/ping");
        assert_eq!(serde_json::from_slice::<Value>(&request.body.unwrap()).unwrap(), json!({ "league": "epl" }));
    }

    #[tokio::test]
    async fn a_plugin_result_is_capped_at_256_kib_whatever_the_configured_limit() {
        let big = format!("\"{}\"", "a".repeat(300 * 1024));
        let state = plugin_state(
            rpc_router(move || {
                let big = big.clone();
                async move { big }
            }),
            true,
            10 * 1024 * 1024,
        );
        let view = plugin_view(vec![plugin_tool("plugin_demo_ping", McpToolKind::Read)]);
        let reply = call_plugin(&state, &view, "plugin_demo_ping", json!({})).await.unwrap();
        let text = result_text(&reply);
        assert!(text.contains("[truncated: response exceeded 262144 bytes]"));
        assert!(text.len() < PLUGIN_RESULT_MAX_BYTES + 100);
        assert_eq!(reply["_meta"]["tuliprox/plugin"], "demo");
    }

    #[tokio::test]
    async fn a_plugin_error_reaches_the_model_with_its_fields_and_the_retry_hint() {
        let router = Router::new()
            .route(
                "/api/v1/plugins/demo/rpc/ping",
                axum::routing::post(|| async {
                    (
                        StatusCode::UNPROCESSABLE_ENTITY,
                        axum::Json(PluginRpcErrorDto {
                            error: "the payload does not match the input schema".to_string(),
                            fields: vec![shared::model::PluginFieldErrorDto {
                                path: "league".to_string(),
                                message: "is required".to_string(),
                            }],
                        }),
                    )
                }),
            )
            .route(
                "/api/v1/plugins/demo/rpc/busy",
                axum::routing::post(|| async {
                    (
                        StatusCode::TOO_MANY_REQUESTS,
                        [(header::RETRY_AFTER, "3")],
                        axum::Json(PluginRpcErrorDto { error: "slow down".to_string(), fields: Vec::new() }),
                    )
                }),
            )
            .route(
                "/api/v1/plugins/demo/rpc/denied",
                axum::routing::post(|| async { (StatusCode::FORBIDDEN, "no token: s3") }),
            );
        let state = plugin_state(router, true, 262_144);
        let mut busy = plugin_tool("plugin_demo_busy", McpToolKind::Read);
        busy.path = "/plugins/demo/rpc/busy".to_string();
        let mut denied = plugin_tool("plugin_demo_denied", McpToolKind::Read);
        denied.path = "/plugins/demo/rpc/denied".to_string();
        let view = plugin_view(vec![plugin_tool("plugin_demo_ping", McpToolKind::Read), busy, denied]);

        let reply = call_plugin(&state, &view, "plugin_demo_ping", json!({})).await.unwrap();
        assert_eq!(reply["isError"], true);
        let text = result_text(&reply);
        assert!(text.starts_with("HTTP 422 Unprocessable Entity: "), "{text}");
        assert!(text.contains("the payload does not match the input schema") && text.contains("is required"), "{text}");
        assert_eq!(reply["_meta"]["tuliprox/plugin"], "demo");

        let reply = call_plugin(&state, &view, "plugin_demo_busy", json!({})).await.unwrap();
        let text = result_text(&reply);
        assert!(text.contains("slow down") && text.ends_with("(retry after 3s)"), "{text}");

        // A body that is not an rpc error (a permission layer's) is not forwarded.
        let reply = call_plugin(&state, &view, "plugin_demo_denied", json!({})).await.unwrap();
        assert_eq!(result_text(&reply), "HTTP 403 Forbidden");
    }
    // ---- input_scoped plugin tools ---------------------------------------

    /// The `ui-demo` fixture: exports `echo`, a read that returns the payload it
    /// was given. `None` when the fixtures are not built.
    #[cfg(feature = "plugins")]
    fn plugin_fixtures() -> Option<std::path::PathBuf> {
        let dir = std::env::var_os("TULIPROX_PLUGIN_FIXTURES").map_or_else(
            || std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/plugin-fixtures/plugins"),
            std::path::PathBuf::from,
        );
        dir.join("ui-demo").join("plugin.wasm").is_file().then_some(dir)
    }

    /// A loaded, MCP-approved `ui-demo` whose `echo` export is `input_scoped`,
    /// the app state it runs under, and the MCP state on the real `/api/v1`
    /// router, so a tool call goes through the rpc route into the guest.
    #[cfg(feature = "plugins")]
    struct ScopedFixture {
        state: McpState,
        view: ToolView,
        _plugins: tempfile::TempDir,
        _data: tempfile::TempDir,
    }

    #[cfg(feature = "plugins")]
    async fn scoped_fixture(inputs: Vec<tuliprox_core::model::ConfigInput>) -> Option<ScopedFixture> {
        use std::sync::Arc;
        use tuliprox_core::model::{PluginEntry, PluginsConfig};
        use tuliprox_plugins::{PluginServices, PluginSystem};

        let fixtures = plugin_fixtures()?;
        let wasm = std::fs::read(fixtures.join("ui-demo").join("plugin.wasm")).expect("fixture wasm");
        let plugins = tempfile::tempdir().expect("plugins dir");
        let data = tempfile::tempdir().expect("data dir");
        let plugin_dir = plugins.path().join("ui-demo");
        std::fs::create_dir_all(plugin_dir.join("rpc")).expect("plugin dir");
        std::fs::write(plugin_dir.join("plugin.wasm"), &wasm).expect("wasm");
        std::fs::write(
            plugin_dir.join("manifest.yml"),
            "id: ui-demo\nname: UI demo\nversion: 0.3.0\napi_version: 1\nhooks: [rpc.call]\nui:\n  rpc_exports:\n    - { name: echo, kind: read, mcp: true, input_scoped: true, description: \"Echo\", input_schema: rpc/in.json, output_schema: rpc/out.json }\n",
        )
        .expect("manifest");
        std::fs::write(
            plugin_dir.join("rpc/in.json"),
            r#"{"type":"object","properties":{"query":{"type":"string"}},"additionalProperties":false}"#,
        )
        .expect("input schema");
        std::fs::write(plugin_dir.join("rpc/out.json"), r#"{"type":"object"}"#).expect("output schema");

        let config = PluginsConfig {
            enabled: true,
            directory: plugins.path().to_string_lossy().to_string(),
            plugins: vec![PluginEntry {
                id: "ui-demo".to_string(),
                enabled: true,
                priority: 0,
                settings: serde_json::Map::new(),
                settings_version: 1,
                update_policy: shared::model::PluginUpdatePolicy::Auto,
            }],
            ..PluginsConfig::default()
        };
        let services = PluginServices::new(reqwest::Client::new(), data.path().join("plugin-data"));
        let system = PluginSystem::load(&config, services).expect("load").expect("enabled");
        // Trust on first use pinned the exact wasm without `mcp`; the admin's
        // `mcp: true` is what makes the export a tool, so add it and re-scan.
        let pins_path = data.path().join("plugin-data").join("approvals.json");
        let mut pins: Value =
            serde_json::from_slice(&std::fs::read(&pins_path).expect("pin written on load")).expect("approvals.json");
        pins["ui-demo"]["mcp"] = json!(true);
        std::fs::write(&pins_path, pins.to_string()).expect("approvals.json");
        system.reload(&config);

        let dto = shared::model::McpConfigDto {
            enabled: true,
            read_only: false,
            plugin_tools: true,
            max_response_bytes: 262_144,
            ..Default::default()
        };
        let config = Config { mcp: Some(McpConfig::from(dto.clone())), ..Config::default() };
        let app_state = crate::api::model::create_test_app_state_with_plugins(config, Some(system));
        app_state.app_config.sources.store(Arc::new(crate::model::SourcesConfig {
            inputs: inputs.into_iter().map(Arc::new).collect(),
            ..crate::model::SourcesConfig::default()
        }));
        let v1 = v1_api_register(false, &app_state, "").with_state(Arc::clone(&app_state));
        let state = McpState::new(v1, &McpConfig::from(dto), false, "", Arc::clone(&app_state));
        let view = state.tool_view().await;
        Some(ScopedFixture { state, view, _plugins: plugins, _data: data })
    }

    #[cfg(feature = "plugins")]
    fn plugin_input(name: &str, plugin: &str) -> tuliprox_core::model::ConfigInput {
        use shared::utils::Internable;
        tuliprox_core::model::ConfigInput {
            name: name.intern(),
            input_type: shared::model::InputType::Plugin,
            plugin: Some(plugin.intern()),
            url: format!("http://{name}.local/"),
            username: Some("alice".to_string()),
            password: Some("hunter2".to_string()),
            plugin_options: vec![
                ("region".to_string(), "eu".to_string()),
                ("api_token".to_string(), "t-0k3n".to_string()),
            ],
            ..tuliprox_core::model::ConfigInput::default()
        }
    }

    #[cfg(feature = "plugins")]
    #[tokio::test]
    async fn a_scoped_tool_takes_the_input_enum_and_the_guest_reads_args_and_context() {
        let Some(fixture) =
            scoped_fixture(vec![plugin_input("living-room", "ui-demo"), plugin_input("bedroom", "other-plugin")]).await
        else {
            return;
        };
        let list = tools_list(&fixture.view, false);
        let tool = list.iter().find(|tool| tool["name"] == "plugin_ui_demo_echo").expect("the scoped tool is listed");
        assert_eq!(tool["inputSchema"]["properties"]["input"]["type"], "string");
        assert_eq!(tool["inputSchema"]["properties"]["input"]["enum"], json!(["living-room"]));
        assert_eq!(tool["inputSchema"]["required"], json!(["input"]));
        assert_eq!(tool["annotations"], json!({ "readOnlyHint": true, "openWorldHint": true }));
        assert_eq!(tool["outputSchema"], json!({ "type": "object" }));

        let params = json!({ "name": "plugin_ui_demo_echo", "arguments": { "input": "living-room", "query": "news" } });
        let reply =
            call_tool_in(&fixture.state, &fixture.view, &HeaderMap::new(), Some(&params), None, &test_fingerprint())
                .await
                .expect("reply");
        assert_eq!(reply["isError"], false, "{reply}");
        let parsed: Value = serde_json::from_str(result_text(&reply)).expect("the guest's answer");
        assert_eq!(parsed["echo"]["args"], json!({ "query": "news" }));
        assert_eq!(parsed["echo"]["input"]["name"], "living-room");
        assert_eq!(parsed["echo"]["input"]["url"], "http://living-room.local/");
        assert_eq!(
            parsed["echo"]["input"]["options"],
            json!({ "region": "eu", "api_token": shared::model::REDACTED_SECRET })
        );
        // The guest echoed the credentials; none of their values leaves the host.
        for leaked in ["hunter2", "t-0k3n", "alice"] {
            assert!(!result_text(&reply).contains(leaked), "{leaked} leaked: {}", result_text(&reply));
        }
        assert_eq!(reply["structuredContent"], parsed, "the answer is a JSON object and becomes structured content");
    }

    #[cfg(feature = "plugins")]
    #[tokio::test]
    async fn a_scoped_tool_without_a_matching_input_is_not_listed_and_a_foreign_name_is_refused() {
        let Some(fixture) = scoped_fixture(vec![plugin_input("bedroom", "other-plugin")]).await else { return };
        assert!(fixture.view.find("plugin_ui_demo_echo").is_none(), "no input of this plugin: no tool");
        assert!(!tools_list(&fixture.view, false).iter().any(|tool| tool["name"] == "plugin_ui_demo_echo"));

        // `m3u-input` is configured, but it is not a `type: plugin` input.
        let mut m3u = plugin_input("m3u-input", "ui-demo");
        m3u.input_type = shared::model::InputType::M3u;
        m3u.plugin = None;
        let Some(fixture) =
            scoped_fixture(vec![plugin_input("living-room", "ui-demo"), plugin_input("bedroom", "other-plugin"), m3u])
                .await
        else {
            return;
        };
        // An input bound to another plugin is refused, and so is every name that
        // does not resolve to a plugin input.
        for (name, needle) in [
            ("bedroom", "belongs to plugin `other-plugin`"),
            ("nope", "no configured plugin input is named `nope`"),
            ("m3u-input", "no configured plugin input is named `m3u-input`"),
        ] {
            let params = json!({ "name": "plugin_ui_demo_echo", "arguments": { "input": name } });
            let reply = call_tool_in(
                &fixture.state,
                &fixture.view,
                &HeaderMap::new(),
                Some(&params),
                None,
                &test_fingerprint(),
            )
            .await
            .expect("reply");
            assert_eq!(reply["isError"], true, "{name}: {reply}");
            let text = result_text(&reply);
            assert!(text.contains("400"), "{name}: {text}");
            assert!(text.contains(needle), "{name}: {text}");
        }
    }

    #[cfg(feature = "plugins")]
    #[tokio::test]
    async fn a_scoped_tool_via_mcp_refuses_input_draft() {
        let Some(fixture) = scoped_fixture(vec![plugin_input("living-room", "ui-demo")]).await else { return };
        let params = json!({
            "name": "plugin_ui_demo_echo",
            "arguments": {
                "input_draft": {
                    "name": "living-room",
                    "url": "http://living-room.local/"
                }
            }
        });
        let reply =
            call_tool_in(&fixture.state, &fixture.view, &HeaderMap::new(), Some(&params), None, &test_fingerprint())
                .await
                .expect("reply");
        assert_eq!(reply["isError"], true);
        let text = result_text(&reply);
        assert!(text.contains("input_draft"), "{text}");
    }

    #[test]
    fn media_server_tools_are_listed_with_schemas_and_annotations() {
        let view = ToolView::default();
        let list = tools_list(&view, false);
        let find = |name: &str| list.iter().find(|tool| tool["name"] == name).expect("listed");

        for name in ["list_media_server_libraries", "get_media_server_status"] {
            let tool = find(name);
            assert_eq!(tool["inputSchema"]["properties"]["input"]["type"], "string", "{name}");
            assert_eq!(tool["inputSchema"]["required"], json!(["input"]), "{name}");
            assert_eq!(tool["annotations"], json!({ "readOnlyHint": true, "openWorldHint": false }), "{name}");
        }

        let refresh = find("refresh_media_server_input");
        assert_eq!(refresh["inputSchema"]["properties"]["action"]["enum"], json!(["normal", "refresh"]));
        assert_eq!(refresh["inputSchema"]["required"], json!(["input", "action"]));
        assert_eq!(
            refresh["annotations"],
            json!({ "readOnlyHint": false, "destructiveHint": false, "openWorldHint": false })
        );
        assert_eq!(tool("refresh_media_server_input").path, "/media-server/{input}/refresh");

        let read_only = listed(&view, true);
        assert!(read_only.contains(&"list_media_server_libraries".to_string()));
        assert!(read_only.contains(&"get_media_server_status".to_string()));
        assert!(!read_only.contains(&"refresh_media_server_input".to_string()));
    }

    #[tokio::test]
    async fn media_server_read_tools_reach_their_routes() {
        use crate::api::endpoints::media_server_api::test_support::{
            app_state_with_inputs, emby_input, start_emby_server, EMBY_TOKEN,
        };
        let (base_url, server) = start_emby_server().await;
        let app_state = app_state_with_inputs(vec![emby_input("emby_input", 1, base_url)]);
        let dto = shared::model::McpConfigDto { enabled: true, ..Default::default() };
        let v1 = v1_api_register(false, &app_state, "").with_state(Arc::clone(&app_state));
        let state = McpState::new(v1, &McpConfig::from(dto), true, "", Arc::clone(&app_state));
        let view = state.tool_view().await;

        let call_media = |name: &str| {
            let params = json!({ "name": name, "arguments": { "input": "emby_input" } });
            let state = &state;
            let view = &view;
            async move { call_tool_in(state, view, &HeaderMap::new(), Some(&params), None, &test_fingerprint()).await }
        };

        let reply = call_media("list_media_server_libraries").await.expect("tool result");
        assert_eq!(reply["isError"], false, "{reply}");
        let libraries: Value = serde_json::from_str(result_text(&reply)).expect("library list");
        assert_eq!(libraries[0]["id"], "lib-movies");
        assert_eq!(libraries[0]["selected"], true);
        assert_eq!(libraries[1]["id"], "lib-tv");
        assert_eq!(libraries[1]["selected"], false);
        assert!(!result_text(&reply).contains(EMBY_TOKEN));

        let reply = call_media("get_media_server_status").await.expect("tool result");
        assert_eq!(reply["isError"], false, "{reply}");
        assert_eq!(reply["structuredContent"]["kind"], "emby");
        assert_eq!(reply["structuredContent"]["server_id"], "server-1");
        assert_eq!(reply["structuredContent"]["reachable"], true);

        let params = json!({ "name": "refresh_media_server_input", "arguments": { "input": "emby_input", "action": "refresh" } });
        let err = call_tool_in(&state, &view, &HeaderMap::new(), Some(&params), None, &test_fingerprint())
            .await
            .expect_err("read-only");
        assert!(err.contains("read-only"), "{err}");
        server.abort();
    }

    #[tokio::test]
    async fn refresh_media_server_input_reaches_the_refresh_route() {
        use crate::{
            api::{
                endpoints::media_server_api::test_support::config_input,
                model::create_test_app_state_with_manual_update_sender,
            },
            model::SourcesConfig,
        };
        use shared::model::{InputRefreshPolicy, InputType};
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let dto = shared::model::McpConfigDto { enabled: true, read_only: false, ..Default::default() };
        let config = Config { mcp: Some(McpConfig::from(dto.clone())), ..Config::default() };
        let app_state = create_test_app_state_with_manual_update_sender(config, sender);
        let sources = SourcesConfig {
            inputs: vec![Arc::new(config_input("emby_input", 9, InputType::Emby, "http://127.0.0.1:1".into()))],
            ..SourcesConfig::default()
        };
        app_state.app_config.sources.store(Arc::new(sources));
        let v1 = v1_api_register(false, &app_state, "").with_state(Arc::clone(&app_state));
        let state = McpState::new(v1, &McpConfig::from(dto), false, "", Arc::clone(&app_state));
        let view = state.tool_view().await;

        let params = json!({ "name": "refresh_media_server_input", "arguments": { "input": "emby_input", "action": "refresh" } });
        let reply = call_tool_in(&state, &view, &HeaderMap::new(), Some(&params), None, &test_fingerprint())
            .await
            .expect("tool result");

        assert_eq!(reply["isError"], false, "{reply}");
        assert!(reply["structuredContent"]["run_id"].is_string(), "{reply}");
        let request = receiver.try_recv().expect("the refresh was queued");
        assert_eq!(
            request.input_action,
            Some(shared::model::InputUpdateRequest {
                input_id: 9,
                action: shared::model::InputUpdateAction::Provider(InputRefreshPolicy::REFRESH)
            })
        );
    }

    struct McpPlayerTestEnv {
        _dir: tempfile::TempDir,
        app_state: Arc<AppState>,
        state: McpState,
        view: ToolView,
        alice_token: String,
        carol_token: String,
        admin_token: String,
    }

    #[allow(clippy::too_many_lines)]
    async fn setup_mcp_player_test_env(player_tools: shared::model::McpPlayerTools) -> McpPlayerTestEnv {
        let dir = tempfile::TempDir::new().unwrap();
        let web_auth_dto = WebAuthConfigDto {
            enabled: true,
            issuer: "test".to_string(),
            secret: "test-secret".to_string(),
            ..Default::default()
        };
        let web_ui_dto = WebUiConfigDto {
            enabled: true,
            auth: Some(web_auth_dto.clone()),
            user_ui_enabled: true,
            ..Default::default()
        };
        let mcp_dto =
            shared::model::McpConfigDto { enabled: true, read_only: true, player_tools, ..Default::default() };
        let recording_dto = shared::model::RecordingConfigDto { enabled: true, ..Default::default() };
        let recording_runtime = crate::model::RecordingConfig::from(&recording_dto);
        let config = Config {
            video: Some(crate::model::VideoConfig {
                extensions: Vec::new(),
                web_search: None,
                recording: Some(recording_runtime),
            }),
            storage_dir: dir.path().to_string_lossy().to_string(),
            web_ui: Some((&web_ui_dto).into()),
            mcp: Some(McpConfig::from(mcp_dto.clone())),
            ..Default::default()
        };
        let mut app_state = create_test_app_state(config);
        let persistent_queue = Arc::new(
            tuliprox_dvr::recording::recording_queue::RecordingQueue::new_persistent(dir.path(), dir.path()).unwrap(),
        );
        Arc::get_mut(&mut app_state).expect("unique app state").recording.queue = persistent_queue;

        let target = Arc::new(crate::model::ConfigTarget {
            managed_channels: Vec::new(),
            curation: None,
            id: 1,
            enabled: true,
            name: "main".into(),
            options: Some(shared::model::ConfigTargetOptions {
                failover_groups: Some(shared::model::FailoverGroupsTargetConfig {
                    enabled: true,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            sort: None,
            filter: tuliprox_core::model::StagedFilter::default(),
            output: vec![crate::model::TargetOutput::M3u(crate::model::M3uTargetOutput {
                filename: None,
                include_type_in_url: false,
                mask_redirect_url: false,
                filter: None,
            })],
            rename: None,
            mapping_ids: None,
            mapping: Arc::default(),
            favourites: None,
            processing_order: shared::model::ProcessingOrder::default(),
            execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
            watch: None,
            use_memory_cache: false,
        });
        let input = Arc::new(crate::model::ConfigInput { id: 1, name: "input-a".into(), ..Default::default() });
        let sources = crate::model::SourcesConfig {
            inputs: vec![Arc::clone(&input)],
            sources: vec![crate::model::ConfigSource {
                inputs: vec!["input-a".into()],
                targets: vec![Arc::clone(&target)],
            }],
            ..Default::default()
        };
        app_state.app_config.sources.store(Arc::new(sources));

        let api_proxy_dto = shared::model::ApiProxyConfigDto {
            user: vec![shared::model::TargetUserDto {
                target: "main".to_string(),
                credentials: vec![
                    shared::model::ProxyUserCredentialsDto {
                        username: "alice".to_string(),
                        password: "pass".to_string(),
                        ui_enabled: true,
                        max_connections: 5,
                        filter: None,
                        player_features: Some(shared::model::PlayerFeaturesDto {
                            source_switch: Some(true),
                            joinable_streams: Some(true),
                            recording: Some(shared::model::PlayerRecordingLevel::Private),
                            recording_quota: Some(shared::model::ByteSize::new("10000000000")),
                            max_concurrent_recordings: Some(2),
                            free_shared_joins: Some(true),
                            max_free_shared_joins: Some(3),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    shared::model::ProxyUserCredentialsDto {
                        username: "carol".to_string(),
                        password: "pass".to_string(),
                        ui_enabled: true,
                        max_connections: 2,
                        player_features: Some(shared::model::PlayerFeaturesDto {
                            source_switch: Some(false),
                            joinable_streams: Some(false),
                            recording: None,
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                ],
            }],
            ..Default::default()
        };
        let mut api_proxy = crate::model::ApiProxyConfig::from(&api_proxy_dto);
        for tu in &mut api_proxy.user {
            for cred in &mut tu.credentials {
                Arc::make_mut(cred).resolve_plan(&std::collections::HashMap::new());
            }
        }
        app_state.app_config.set_api_proxy(api_proxy).unwrap();

        let web_auth = app_state.app_config.config.load().web_ui.as_ref().unwrap().auth.as_ref().unwrap().clone();
        let alice_token =
            crate::auth::create_jwt_api_user(&web_auth, "alice", shared::model::UserId::api("alice")).unwrap();
        let carol_token =
            crate::auth::create_jwt_api_user(&web_auth, "carol", shared::model::UserId::api("carol")).unwrap();
        let admin_token = create_jwt_web_user(
            &web_auth,
            "admin",
            Permission::UserWrite | Permission::SystemRead | Permission::PlaylistRead,
            0,
            shared::model::UserId::web("admin"),
        )
        .unwrap();

        let v1 = v1_api_register(true, &app_state, "").with_state(Arc::clone(&app_state));
        let state = McpState::new(v1, &McpConfig::from(mcp_dto), true, "", Arc::clone(&app_state));
        let view = state.tool_view().await;

        McpPlayerTestEnv { _dir: dir, app_state, state, view, alice_token, carol_token, admin_token }
    }

    async fn seed_test_channel(app_state: &Arc<AppState>, vid: u32, name: &str, group: &str) {
        use shared::model::{
            M3uPlaylistItem, PlaylistItem, PlaylistItemHeader, PlaylistItemType, VirtualId, XtreamCluster,
        };
        use tuliprox_repository::{
            bplustree::BPlusTree, ensure_target_storage_path, m3u_get_file_path_for_db, TargetPlaylistStorage,
        };
        let mut tree = BPlusTree::new();
        let pli = PlaylistItem {
            header: PlaylistItemHeader {
                virtual_id: VirtualId::new(vid),
                input_name: Arc::from("input-a"),
                name: Arc::from(name),
                group: Arc::from(group),
                title: Arc::from(name),
                url: Arc::from(format!("http://example.com/{vid}.ts")),
                item_type: PlaylistItemType::Live,
                xtream_cluster: XtreamCluster::Live,
                ..PlaylistItemHeader::default()
            },
        };
        let item = M3uPlaylistItem::from(&pli);
        tree.insert(vid, item);

        let storage_path = ensure_target_storage_path(&app_state.app_config.config.load(), "main").await.unwrap();
        let db_path = m3u_get_file_path_for_db(&storage_path);
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        tree.store(&db_path).unwrap();

        app_state
            .playlists
            .data
            .write()
            .await
            .insert("main".to_string(), TargetPlaylistStorage { m3u: Some(tree), xtream: None, id_mapping: None });
    }

    fn auth_headers(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, format!("Bearer {token}").parse().unwrap());
        headers
    }

    #[tokio::test]
    async fn test_mcp_tools_list_caller_matrix() {
        let env_write = setup_mcp_player_test_env(shared::model::McpPlayerTools::Write).await;

        let req = rpc(1, "tools/list", json!({}));

        // 1. Web admin sees no player tools, all admin tools
        let admin_headers = auth_headers(&env_write.admin_token);
        let reply_admin =
            handle_message(&env_write.state, &admin_headers, req.as_bytes(), None, &test_fingerprint()).await.unwrap();
        let tools_admin = reply_admin["result"]["tools"].as_array().unwrap();
        let admin_names: Vec<String> = tools_admin.iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
        assert!(!admin_names.iter().any(|n| n.starts_with("me_")));
        assert!(admin_names.contains(&"get_status".to_string()));

        // 2. API user Alice (all features on, player_tools = Write) sees exact list of 16 player tools, 0 admin tools
        let alice_headers = auth_headers(&env_write.alice_token);
        let reply_alice =
            handle_message(&env_write.state, &alice_headers, req.as_bytes(), None, &test_fingerprint()).await.unwrap();
        let tools_alice = reply_alice["result"]["tools"].as_array().unwrap();
        let mut alice_names: Vec<String> =
            tools_alice.iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
        alice_names.sort();
        let mut expected_player_write = vec![
            "me_cancel_recording".to_string(),
            "me_capabilities".to_string(),
            "me_create_rule".to_string(),
            "me_delete_recording".to_string(),
            "me_delete_rule".to_string(),
            "me_get_epg".to_string(),
            "me_list_joinable".to_string(),
            "me_list_recordings".to_string(),
            "me_list_rules".to_string(),
            "me_list_sources".to_string(),
            "me_list_streams".to_string(),
            "me_record_program".to_string(),
            "me_recording_conflicts".to_string(),
            "me_search_channels".to_string(),
            "me_split_stream".to_string(),
            "me_stop_stream".to_string(),
            "me_switch_source".to_string(),
        ];
        expected_player_write.sort();
        assert_eq!(alice_names, expected_player_write);

        // 3. API user Alice with player_tools = Read sees only read tools (9 tools, 0 write tools)
        let env_read = setup_mcp_player_test_env(shared::model::McpPlayerTools::Read).await;
        let reply_read =
            handle_message(&env_read.state, &alice_headers, req.as_bytes(), None, &test_fingerprint()).await.unwrap();
        let tools_read = reply_read["result"]["tools"].as_array().unwrap();
        let mut alice_read_names: Vec<String> =
            tools_read.iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
        alice_read_names.sort();
        let mut expected_player_read = vec![
            "me_capabilities".to_string(),
            "me_get_epg".to_string(),
            "me_list_joinable".to_string(),
            "me_list_recordings".to_string(),
            "me_list_rules".to_string(),
            "me_list_sources".to_string(),
            "me_list_streams".to_string(),
            "me_recording_conflicts".to_string(),
            "me_search_channels".to_string(),
        ];
        expected_player_read.sort();
        assert_eq!(alice_read_names, expected_player_read);

        // 4. API user Carol with features off sees only the 5 featureless tools
        let carol_headers = auth_headers(&env_write.carol_token);
        let reply_carol =
            handle_message(&env_write.state, &carol_headers, req.as_bytes(), None, &test_fingerprint()).await.unwrap();
        let tools_carol = reply_carol["result"]["tools"].as_array().unwrap();
        let mut carol_names: Vec<String> =
            tools_carol.iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
        carol_names.sort();
        let mut expected_carol = vec![
            "me_capabilities".to_string(),
            "me_get_epg".to_string(),
            "me_list_streams".to_string(),
            "me_search_channels".to_string(),
            "me_stop_stream".to_string(),
        ];
        expected_carol.sort();
        assert_eq!(carol_names, expected_carol);

        // 5. API user with player_tools = Off sees 0 tools
        let env_off = setup_mcp_player_test_env(shared::model::McpPlayerTools::Off).await;
        let reply_off =
            handle_message(&env_off.state, &alice_headers, req.as_bytes(), None, &test_fingerprint()).await.unwrap();
        let tools_off = reply_off["result"]["tools"].as_array().unwrap();
        assert_eq!(tools_off.len(), 0);
    }

    #[tokio::test]
    async fn test_mcp_tools_call_hidden_returns_unknown_tool() {
        let env = setup_mcp_player_test_env(shared::model::McpPlayerTools::Write).await;

        let alice_headers = auth_headers(&env.alice_token);
        let admin_headers = auth_headers(&env.admin_token);
        let carol_headers = auth_headers(&env.carol_token);

        // API user calling admin tool -> Unknown tool
        let params = json!({ "name": "get_status", "arguments": {} });
        let err = call_tool_in(&env.state, &env.view, &alice_headers, Some(&params), None, &test_fingerprint())
            .await
            .unwrap_err();
        assert_eq!(err, "Unknown tool: get_status");

        // Web admin calling player tool -> Unknown tool
        let params = json!({ "name": "me_capabilities", "arguments": {} });
        let err = call_tool_in(&env.state, &env.view, &admin_headers, Some(&params), None, &test_fingerprint())
            .await
            .unwrap_err();
        assert_eq!(err, "Unknown tool: me_capabilities");

        // Carol (source_switch disabled) calling me_list_sources -> Unknown tool
        let params = json!({ "name": "me_list_sources", "arguments": { "vid": 100 } });
        let err = call_tool_in(&env.state, &env.view, &carol_headers, Some(&params), None, &test_fingerprint())
            .await
            .unwrap_err();
        assert_eq!(err, "Unknown tool: me_list_sources");

        // When player_tools = Read, calling write tool me_record_program -> Unknown tool
        let env_read = setup_mcp_player_test_env(shared::model::McpPlayerTools::Read).await;
        let params = json!({
            "name": "me_record_program",
            "arguments": {
                "vid": 100,
                "program_start": 1_740_000_000,
                "program_end": 1_740_003_600,
                "title": "News"
            }
        });
        let err =
            call_tool_in(&env_read.state, &env_read.view, &alice_headers, Some(&params), None, &test_fingerprint())
                .await
                .unwrap_err();
        assert_eq!(err, "Unknown tool: me_record_program");
    }

    #[tokio::test]
    async fn test_mcp_end_to_end_search_epg_record_with_idempotency() {
        let env = setup_mcp_player_test_env(shared::model::McpPlayerTools::Write).await;
        seed_test_channel(&env.app_state, 100, "News Channel", "News").await;

        let alice_headers = auth_headers(&env.alice_token);

        // 1. me_search_channels
        let params = json!({ "name": "me_search_channels", "arguments": { "query": "News" } });
        let reply = call_tool_in(&env.state, &env.view, &alice_headers, Some(&params), None, &test_fingerprint())
            .await
            .unwrap();
        assert_eq!(reply["isError"], false);
        let channels: Vec<shared::model::UserChannelDto> =
            serde_json::from_str(reply["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].virtual_id, 100);

        // 2. me_get_epg (empty EPG DB returns [] but succeeds)
        let params = json!({
            "name": "me_get_epg",
            "arguments": {
                "vid": 100,
                "from": "1740000000",
                "to": "1740003600"
            }
        });
        let reply = call_tool_in(&env.state, &env.view, &alice_headers, Some(&params), None, &test_fingerprint())
            .await
            .unwrap();
        assert_eq!(reply["isError"], false);

        // 3. me_record_program -> creates exactly 1 recording
        let start = chrono::Utc::now().timestamp() + 3_600;
        let end = start + 3_600;
        let record_args = json!({
            "vid": 100,
            "program_start": start,
            "program_end": end,
            "title": "Morning News"
        });
        let params = json!({ "name": "me_record_program", "arguments": record_args });
        let reply = call_tool_in(&env.state, &env.view, &alice_headers, Some(&params), None, &test_fingerprint())
            .await
            .unwrap();
        assert_eq!(reply["isError"], false, "me_record_program reply: {reply:?}");
        assert_eq!(env.app_state.recording.queue.scheduled.read().await.len(), 1);

        // 4. Calling me_record_program a second time with exact same arguments is idempotent -> still 1 recording
        let reply2 = call_tool_in(&env.state, &env.view, &alice_headers, Some(&params), None, &test_fingerprint())
            .await
            .unwrap();
        assert_eq!(reply2["isError"], false, "reply2: {reply2:?}");
        assert_eq!(env.app_state.recording.queue.scheduled.read().await.len(), 1);
    }

    #[tokio::test]
    async fn test_mcp_me_list_recordings_json_equals_websocket_snapshot() {
        let env = setup_mcp_player_test_env(shared::model::McpPlayerTools::Write).await;
        seed_test_channel(&env.app_state, 100, "News Channel", "News").await;

        let alice_headers = auth_headers(&env.alice_token);

        // Record a program
        let start = chrono::Utc::now().timestamp() + 3_600;
        let end = start + 3_600;
        let record_args = json!({
            "vid": 100,
            "program_start": start,
            "program_end": end,
            "title": "Morning News"
        });
        let params = json!({ "name": "me_record_program", "arguments": record_args });
        let _ = call_tool_in(&env.state, &env.view, &alice_headers, Some(&params), None, &test_fingerprint())
            .await
            .unwrap();

        // Call me_list_recordings
        let params = json!({ "name": "me_list_recordings", "arguments": {} });
        let reply = call_tool_in(&env.state, &env.view, &alice_headers, Some(&params), None, &test_fingerprint())
            .await
            .unwrap();
        assert_eq!(reply["isError"], false);

        // Obtain snapshot directly as websocket handler does
        let mut claims = crate::api::auth_middleware::authenticate(&env.app_state, &env.alice_token).await.unwrap();
        claims.permissions = crate::api::auth_middleware::effective_permissions(&env.app_state, &claims);
        let ws_msg = crate::api::endpoints::recording_api::recording_snapshot_response(&env.app_state, &claims).await;
        let ws_json = serde_json::to_string(&ws_msg).unwrap();

        let tool_text = reply["content"][0]["text"].as_str().unwrap();
        assert_eq!(tool_text, ws_json);
    }

    #[tokio::test]
    async fn test_mcp_expired_api_user_sees_no_tools() {
        let env = setup_mcp_player_test_env(shared::model::McpPlayerTools::Write).await;
        let mut config = (*env.app_state.app_config.config.load_full()).clone();
        config.user_access_control = true;
        env.app_state.app_config.config.store(Arc::new(config));

        let mut api_proxy = (*env.app_state.app_config.api_proxy.load_full().unwrap()).clone();
        for tu in &mut api_proxy.user {
            for cred in &mut tu.credentials {
                if cred.username == "alice" {
                    Arc::make_mut(cred).exp_date = Some(1); // epoch 1s: expired in 1970
                }
            }
        }
        env.app_state.app_config.set_api_proxy(api_proxy).unwrap();

        let alice_headers = auth_headers(&env.alice_token);
        let req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list",
            "params": {}
        });
        let reply = handle_message(&env.state, &alice_headers, req.to_string().as_bytes(), None, &test_fingerprint())
            .await
            .unwrap();
        let tools = reply["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 0, "expired API user must see 0 MCP tools");
    }

    #[tokio::test]
    async fn test_mcp_player_rule_tools() {
        let env = setup_mcp_player_test_env(shared::model::McpPlayerTools::Write).await;
        seed_test_channel(&env.app_state, 100, "News Channel", "News").await;

        let alice_headers = auth_headers(&env.alice_token);

        // 1. Create a weekly timeslot rule via me_create_rule
        let create_args = json!({
            "vid": 100,
            "weekday": 1,
            "local_start_time": "20:00",
            "duration_secs": 3600,
            "visibility": "private"
        });
        let params = json!({ "name": "me_create_rule", "arguments": create_args });
        let reply = call_tool_in(&env.state, &env.view, &alice_headers, Some(&params), None, &test_fingerprint())
            .await
            .unwrap();
        assert_eq!(reply["isError"], false);
        let rule_json: Value = serde_json::from_str(reply["content"][0]["text"].as_str().unwrap()).unwrap();
        let rule_id = rule_json["id"].as_str().unwrap().to_string();
        assert_ne!(rule_id, "");
        assert_eq!(rule_json["source"]["virtual_id"], "100");
        assert_eq!(rule_json["visibility"], "private");

        // 2. List rules via me_list_rules
        let params = json!({ "name": "me_list_rules", "arguments": {} });
        let reply = call_tool_in(&env.state, &env.view, &alice_headers, Some(&params), None, &test_fingerprint())
            .await
            .unwrap();
        assert_eq!(reply["isError"], false);
        let rules_list: Vec<Value> = serde_json::from_str(reply["content"][0]["text"].as_str().unwrap()).unwrap();
        assert!(rules_list.iter().any(|r| r["id"] == rule_id));

        // 3. Delete rule via me_delete_rule
        let delete_args = json!({ "id": rule_id });
        let params = json!({ "name": "me_delete_rule", "arguments": delete_args });
        let reply = call_tool_in(&env.state, &env.view, &alice_headers, Some(&params), None, &test_fingerprint())
            .await
            .unwrap();
        assert_eq!(reply["isError"], false);

        // 4. Verify rule is gone via me_list_rules
        let params = json!({ "name": "me_list_rules", "arguments": {} });
        let reply = call_tool_in(&env.state, &env.view, &alice_headers, Some(&params), None, &test_fingerprint())
            .await
            .unwrap();
        assert_eq!(reply["isError"], false);
        let rules_list_after: Vec<Value> = serde_json::from_str(reply["content"][0]["text"].as_str().unwrap()).unwrap();
        assert!(!rules_list_after.iter().any(|r| r["id"] == rule_id));

        // 5. Verify Carol (no recording feature) cannot see rule tools
        let carol_headers = auth_headers(&env.carol_token);
        let req = rpc(2, "tools/list", json!({}));
        let reply_carol =
            handle_message(&env.state, &carol_headers, req.as_bytes(), None, &test_fingerprint()).await.unwrap();
        let carol_tools = reply_carol["result"]["tools"].as_array().unwrap();
        assert!(!carol_tools
            .iter()
            .any(|t| t["name"] == "me_create_rule" || t["name"] == "me_list_rules" || t["name"] == "me_delete_rule"));
    }

    #[tokio::test]
    async fn test_mcp_me_delete_rule_encodes_id_and_query() {
        let env = setup_mcp_player_test_env(shared::model::McpPlayerTools::Write).await;
        let alice_headers = auth_headers(&env.alice_token);

        let delete_args = json!({ "id": "rule/slash?question", "future": "retain" });
        let params = json!({ "name": "me_delete_rule", "arguments": delete_args });
        let reply = call_tool_in(&env.state, &env.view, &alice_headers, Some(&params), None, &test_fingerprint())
            .await
            .unwrap();
        assert_eq!(reply["isError"], true);
        let text = reply["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("recording_rule_unknown"), "Expected recording_rule_unknown but got: {text}");
    }

    #[tokio::test]
    async fn test_mcp_player_split_stream() {
        let env = setup_mcp_player_test_env(shared::model::McpPlayerTools::Write).await;
        let alice_headers = auth_headers(&env.alice_token);

        // Call me_split_stream with non-existent uid -> tool executes endpoint and returns 404 (isError: true)
        let params = json!({ "name": "me_split_stream", "arguments": { "uid": 9999 } });
        let reply = call_tool_in(&env.state, &env.view, &alice_headers, Some(&params), None, &test_fingerprint())
            .await
            .unwrap();
        assert_eq!(reply["isError"], true);

        // Carol (no source_switch feature) calling me_split_stream -> rejected / unavailable
        let carol_headers = auth_headers(&env.carol_token);
        let reply_carol =
            call_tool_in(&env.state, &env.view, &carol_headers, Some(&params), None, &test_fingerprint()).await;
        assert!(reply_carol.is_err(), "Carol without source_switch must not find me_split_stream");

        // Active shared stream with 2 subscribers for Alice -> success with password redacted in URL
        let addr: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let fp = crate::auth::Fingerprint::new("fp".into(), "127.0.0.1".into(), addr);
        let channel = shared::model::StreamChannel {
            target_id: 1,
            virtual_id: 100,
            provider_id: 1,
            input_name: "test-in".into(),
            item_type: shared::model::PlaylistItemType::Live,
            cluster: shared::model::XtreamCluster::Live,
            group: "test-group".into(),
            title: "Test Stream".into(),
            url: "http://example.com/stream.ts".into(),
            shared: true,
            shared_joined_existing: Some(true),
            ..Default::default()
        };
        let connection = crate::api::model::ConnectionParams {
            meter_uid: 1,
            username: "alice",
            max_connections: 5,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fp,
            provider: "test-prov".into(),
            stream_channel: &channel,
            user_agent: "test-ua".into(),
            session_token: None,
        };
        let _ = env.app_state.connection_manager.update_connection(connection).await;
        let streams = env.app_state.active_users.user_streams("alice").await;
        let uid = streams[0].uid;
        let key = crate::api::api_utils::failover_group::group_shared_key(1, 100);
        env.app_state.shared_stream_manager.set_test_subscriber_count(&key, 2).await;

        let params_success = json!({ "name": "me_split_stream", "arguments": { "uid": uid } });
        let reply_success =
            call_tool_in(&env.state, &env.view, &alice_headers, Some(&params_success), None, &test_fingerprint())
                .await
                .unwrap();
        assert_eq!(reply_success["isError"], false);
        let text = reply_success["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("/m3u-stream/live/alice/***/100.ts?dedicated=1"),
            "Password must be redacted in text: {text}"
        );
        assert!(!text.contains("pass"), "Plain password must not leak in text: {text}");
        let structured = &reply_success["structuredContent"];
        assert_eq!(structured["url"], "/m3u-stream/live/alice/***/100.ts?dedicated=1");
        assert_eq!(structured["reconnect_required"], true);
    }

    #[tokio::test]
    async fn test_mcp_me_record_program_rejects_unknown_argument() {
        let env = setup_mcp_player_test_env(shared::model::McpPlayerTools::Write).await;
        seed_test_channel(&env.app_state, 100, "News Channel", "News").await;

        let alice_headers = auth_headers(&env.alice_token);
        let start = chrono::Utc::now().timestamp() + 3_600;
        let end = start + 3_600;
        let record_args = json!({
            "vid": 100,
            "program_start": start,
            "program_end": end,
            "title": "Morning News",
            "bogus_arg": "invalid"
        });
        let params = json!({ "name": "me_record_program", "arguments": record_args });
        let reply = call_tool_in(&env.state, &env.view, &alice_headers, Some(&params), None, &test_fingerprint())
            .await
            .unwrap();
        assert_eq!(reply["isError"], true);
        assert_eq!(reply["content"][0]["text"], "Unknown argument 'bogus_arg'");
    }

    #[tokio::test]
    async fn test_mcp_api_user_enforces_network_access() {
        use shared::model::NetworkAccessDto;

        let env = setup_mcp_player_test_env(shared::model::McpPlayerTools::Read).await;
        // Restrict alice to 10.0.0.0/8
        {
            let mut api_proxy = (*env.app_state.app_config.api_proxy.load_full().unwrap()).clone();
            for target_user in &mut api_proxy.user {
                for cred in &mut target_user.credentials {
                    if cred.username == "alice" {
                        Arc::make_mut(cred).network_access =
                            Some(tuliprox_core::model::NetworkAccess::from(&NetworkAccessDto {
                                allowed_countries: None,
                                allowed_networks: Some(vec!["10.0.0.0/8".to_string()]),
                            }));
                    }
                }
            }
            env.app_state.app_config.set_api_proxy(api_proxy).unwrap();
        }

        let v1 = crate::api::endpoints::v1_api::v1_api_register(true, &env.app_state, "");
        let router = mcp_api_register_if_enabled::<()>(&v1, &env.app_state, true, "").unwrap();

        // 1. HTTP route: Alice from denied IP gets 403 Forbidden
        let mut denied_req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", env.alice_token))
            .body(Body::from(rpc(1, "tools/list", json!({}))))
            .unwrap();
        denied_req.extensions_mut().insert(axum::extract::ConnectInfo(SocketAddr::from(([192, 168, 1, 1], 12345))));
        let denied_resp = router.clone().oneshot(denied_req).await.unwrap();
        assert_eq!(denied_resp.status(), StatusCode::FORBIDDEN);

        // 2. HTTP route: Alice from allowed IP gets 200 OK
        let mut allowed_req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", env.alice_token))
            .body(Body::from(rpc(1, "tools/list", json!({}))))
            .unwrap();
        allowed_req.extensions_mut().insert(axum::extract::ConnectInfo(SocketAddr::from(([10, 0, 0, 1], 12345))));
        let allowed_resp = router.oneshot(allowed_req).await.unwrap();
        assert_eq!(allowed_resp.status(), StatusCode::OK);

        // 3. Direct resolve_caller: denied fingerprint yields CallerContext::Denied
        let denied_headers = auth_headers(&env.alice_token);
        let fp_denied = crate::auth::Fingerprint::from_headers_and_addr(
            &HeaderMap::new(),
            SocketAddr::from(([192, 168, 1, 1], 12345)),
        );
        let caller_denied = resolve_caller(&env.state, &denied_headers, None, &fp_denied).await;
        assert!(matches!(caller_denied, CallerContext::Denied));

        // 4. Direct resolve_caller: allowed fingerprint yields CallerContext::ApiUser
        let allowed_headers = auth_headers(&env.alice_token);
        let fp_allowed = crate::auth::Fingerprint::from_headers_and_addr(
            &HeaderMap::new(),
            SocketAddr::from(([10, 0, 0, 1], 12345)),
        );
        let caller_allowed = resolve_caller(&env.state, &allowed_headers, None, &fp_allowed).await;
        assert!(matches!(caller_allowed, CallerContext::ApiUser { .. }));
    }
}
