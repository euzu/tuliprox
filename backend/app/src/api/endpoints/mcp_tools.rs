//! Static MCP tool catalog.
//!
//! Every tool is a thin adapter over one `/api/v1` REST route, so
//! authentication, permissions and validation stay in the existing router.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum McpToolKind {
    Read,
    Write,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum McpToolMethod {
    Get,
    Post,
}

impl McpToolMethod {
    pub(super) const fn as_http(self) -> axum::http::Method {
        match self {
            Self::Get => axum::http::Method::GET,
            Self::Post => axum::http::Method::POST,
        }
    }
}

pub(super) struct McpTool {
    pub name: &'static str,
    pub description: &'static str,
    /// JSON schema (object) describing the arguments. Declared properties are
    /// the only accepted arguments; `{param}` placeholders in `path` are
    /// filled from them, the rest goes to the query string (GET) or the JSON
    /// body (POST).
    pub input_schema: &'static str,
    pub method: McpToolMethod,
    /// Route below `{web_ui_path}/api/v1`.
    pub path: &'static str,
    pub kind: McpToolKind,
    /// Scrub secrets from the JSON result before it leaves the server.
    pub redact_secrets: bool,
}

impl McpTool {
    pub(super) fn schema(&self) -> serde_json::Value {
        serde_json::from_str(self.input_schema).unwrap_or_else(|_| serde_json::json!({ "type": "object" }))
    }
}

const NO_ARGUMENTS: &str = r#"{"type":"object","properties":{},"additionalProperties":false}"#;

pub(super) static MCP_TOOLS: &[McpTool] = &[
    McpTool {
        name: "get_status",
        description: "Server status: version, uptime, active users and connections, provider connection usage and cache size.",
        input_schema: NO_ARGUMENTS,
        method: McpToolMethod::Get,
        path: "/status",
        kind: McpToolKind::Read,
        redact_secrets: false,
    },
    McpTool {
        name: "list_active_streams",
        description: "Currently active client streams with user, channel, provider, address and duration.",
        input_schema: NO_ARGUMENTS,
        method: McpToolMethod::Get,
        path: "/streams",
        kind: McpToolKind::Read,
        redact_secrets: false,
    },
    McpTool {
        name: "get_config",
        description: "Effective configuration (config.yml, sources and api-proxy) as visible to the caller's permissions; secrets are redacted and the api-proxy user list is reduced to per-target counts. Large; prefer the narrower tools.",
        input_schema: NO_ARGUMENTS,
        method: McpToolMethod::Get,
        path: "/config",
        kind: McpToolKind::Read,
        redact_secrets: true,
    },
    McpTool {
        name: "get_playlist_update_status",
        description: "Per input and cluster: last playlist update time, item counts and update state.",
        input_schema: NO_ARGUMENTS,
        method: McpToolMethod::Get,
        path: "/playlist/update/status",
        kind: McpToolKind::Read,
        redact_secrets: false,
    },
    McpTool {
        name: "query_stream_history",
        description: "Paged stream history sessions. Requires reverse_proxy.stream_history to be enabled.",
        input_schema: r#"{"type":"object","properties":{"from":{"type":"string","description":"Start date (YYYY-MM-DD) or datetime."},"to":{"type":"string","description":"End date (YYYY-MM-DD) or datetime."},"page":{"type":"integer","minimum":1},"page_size":{"type":"integer","enum":[25,50,100,200]},"search":{"type":"string"},"search_mode":{"type":"string","enum":["text","regex"]},"search_field":{"type":"array","description":"Restrict the search to these record fields; all fields when omitted.","items":{"type":"string","enum":["event_ts_utc","event_type","title","group","api_username","provider_name","provider_id","bytes_sent","first_byte_latency_ms","user_agent","item_type","container","disconnect_reason","source_addr","country","cluster"]}}},"additionalProperties":false}"#,
        method: McpToolMethod::Get,
        path: "/stream-history",
        kind: McpToolKind::Read,
        redact_secrets: false,
    },
    McpTool {
        name: "get_stream_history_summary",
        description: "Per provider summary of stream history (sessions, disconnects, bytes, latency). Requires reverse_proxy.stream_history to be enabled.",
        input_schema: r#"{"type":"object","properties":{"from":{"type":"string","description":"Start date (YYYY-MM-DD) or datetime."},"to":{"type":"string","description":"End date (YYYY-MM-DD) or datetime."},"provider_name":{"type":"string"},"input_name":{"type":"string"},"api_username":{"type":"string"},"item_type":{"type":"string"},"event_type":{"type":"string"}},"additionalProperties":false}"#,
        method: McpToolMethod::Get,
        path: "/stream-history/summary",
        kind: McpToolKind::Read,
        redact_secrets: false,
    },
    McpTool {
        name: "list_qos_snapshots",
        description: "Stream quality-of-service snapshots, worst first. Requires qos_aggregation to be enabled.",
        input_schema: r#"{"type":"object","properties":{"limit":{"type":"integer","minimum":1},"stream_identity_key":{"type":"string"},"input_name":{"type":"string"},"provider_name":{"type":"string"},"item_type":{"type":"string"},"target_name":{"type":"string"}},"additionalProperties":false}"#,
        method: McpToolMethod::Get,
        path: "/qos-snapshots",
        kind: McpToolKind::Read,
        redact_secrets: false,
    },
    McpTool {
        name: "get_qos_snapshot",
        description: "One quality-of-service snapshot by stream identity key. Requires qos_aggregation to be enabled.",
        input_schema: r#"{"type":"object","properties":{"stream_identity_key":{"type":"string"}},"required":["stream_identity_key"],"additionalProperties":false}"#,
        method: McpToolMethod::Get,
        path: "/qos-snapshots/{stream_identity_key}",
        kind: McpToolKind::Read,
        redact_secrets: false,
    },
    McpTool {
        name: "get_event_stats",
        description: "Event bus diagnostics: emitted event counts and recent events with delivery outcome.",
        input_schema: NO_ARGUMENTS,
        method: McpToolMethod::Get,
        path: "/events/stats",
        kind: McpToolKind::Read,
        redact_secrets: false,
    },
    McpTool {
        name: "get_ip_info",
        description: "Public IPv4/IPv6 address of the server as seen by the configured ipcheck service.",
        input_schema: NO_ARGUMENTS,
        method: McpToolMethod::Get,
        path: "/ipinfo",
        kind: McpToolKind::Read,
        redact_secrets: false,
    },
    McpTool {
        name: "update_playlists",
        description: "Queue a playlist update. Answers 202 with a run id; a second request while one is queued answers 409.",
        input_schema: r#"{"type":"object","properties":{"targets":{"type":"array","items":{"type":"string"},"description":"Target names to update. Omit or leave empty to update all targets."}},"additionalProperties":false}"#,
        method: McpToolMethod::Post,
        path: "/playlist/update",
        kind: McpToolKind::Write,
        redact_secrets: false,
    },
    McpTool {
        name: "update_geoip",
        description: "Download and reload the GeoIP database. Requires reverse_proxy.geoip to be enabled.",
        input_schema: NO_ARGUMENTS,
        method: McpToolMethod::Get,
        path: "/geoip/update",
        kind: McpToolKind::Write,
        redact_secrets: false,
    },
];

pub(super) fn find_tool(name: &str) -> Option<&'static McpTool> { MCP_TOOLS.iter().find(|tool| tool.name == name) }
