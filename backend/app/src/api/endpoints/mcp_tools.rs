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

/// `output` property shared by the two target EPG tools.
macro_rules! epg_output_property {
    () => {
        r#""output":{"type":"string","enum":["xtream","m3u"],"description":"Which output EPG database to read. Default: the first output of the target that has an EPG database."},"#
    };
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
        name: "get_hls_origin_telemetry",
        description: "Shared HLS origin transfer quality per input (network time, first byte, throughput, availability) from existing fetches, with sample coverage and freshness. Transfer-quality values are null when unmeasured or stale (older than freshness_secs); availability values such as success_ratio are not nulled by freshness, so check availability.last_sample_age_ms before trusting them. Requires reverse_proxy.hls_origin_telemetry to be enabled.",
        input_schema: NO_ARGUMENTS,
        method: McpToolMethod::Get,
        path: "/hls/origin-telemetry",
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
        name: "list_playlist_sources",
        description: "Start here for playlist questions. Lists the playlist sources whose stored data can be read: targets (by name, with their outputs xtream/m3u and per-output EPG availability) and inputs (by name, with input_type and the clusters they hold). Use the returned names as the `target` or `input` argument of query_playlist_items and as `target` of the EPG tools. Small response, no credentials, no URLs.",
        input_schema: NO_ARGUMENTS,
        method: McpToolMethod::Get,
        path: "/playlist/sources",
        kind: McpToolKind::Read,
        redact_secrets: true,
    },
    McpTool {
        name: "query_playlist_items",
        description: "Read the stored playlist records of ONE target or ONE input (give exactly one of `target`/`input`, addressed by NAME; call list_playlist_sources first) for one `cluster`, with substring search, group filter, EPG-match filter, field projection and paging. Returns {source, cluster, total_matched, skipped_records, offset, limit, returned, has_more, items}. total_matched counts all records matching the filter (not just this page); continue with offset += returned while has_more is true. skipped_records is the number of stored records that could not be read: when it is greater than 0 they are missing from the listing and from total_matched, and a `note` says so. limit defaults to 50 and is clamped to 500 (the applied value is echoed and limit_clamped is true). Only stored data is read, no provider traffic is triggered; inputs without a stored database (e.g. stalker) answer an error. The provider `url` of a record is never returned. `epg_status` works for targets only and answers an error for inputs or targets without an EPG database.",
        input_schema: concat!(
            r#"{"type":"object","properties":{"#,
            r#""target":{"type":"string","description":"Target name (from list_playlist_sources). Exactly one of `target` or `input` is required."},"#,
            r#""input":{"type":"string","description":"Input name (from list_playlist_sources). Exactly one of `target` or `input` is required."},"#,
            r#""cluster":{"type":"string","enum":["live","video","series"],"description":"Record cluster to read: live = live channels, video = VOD movies, series = series (episodes are not listed individually)."},"#,
            r#""offset":{"type":"integer","minimum":0,"description":"Number of matching records to skip. Default 0."},"#,
            r#""limit":{"type":"integer","minimum":1,"maximum":500,"description":"Maximum records per page. Default 50, values above 500 are clamped to 500."},"#,
            r#""search":{"type":"string","description":"Case-insensitive substring filter applied to the `search_field` fields. Empty means no filter."},"#,
            r#""search_field":{"type":"array","uniqueItems":true,"description":"Record fields the `search` text is matched against (any of them may match). Default: name, title, group. epg_channel_id, provider_id and input_name are matched as text too.","items":{"type":"string","enum":["name","title","group","epg_channel_id","provider_id","input_name"]}},"#,
            r#""group":{"type":"string","description":"Exact group (category) name; combined with `search` by AND."},"#,
            r#""fields":{"type":"array","uniqueItems":true,"description":"Record fields to return per item. Default: virtual_id, provider_id, name, title, group, item_type, cluster, epg_channel_id, category_id, channel_no. Meaning: virtual_id = id of the item in tuliprox; provider_id = id at the provider; name = channel/stream name; title = display title; group = group (category) name; item_type = live, video, series, ...; cluster = live, video or series; epg_channel_id = EPG id assigned to the item; category_id = provider category id; channel_no = channel number; logo = logo image reference; logo_small = small logo reference; parent_code = parent control code; rec = recording/catch-up flag; input_name = name of the originating input; input_stream_id = stream id inside the originating input; source_ordinal = position within the source playlist; additional_properties = provider specific extra properties (URL-like members removed; can be large); epg_status = EPG match result (missing_id, unmatched or matched; targets only).","items":{"type":"string","enum":["virtual_id","provider_id","name","title","group","item_type","cluster","epg_channel_id","category_id","channel_no","logo","logo_small","parent_code","rec","input_name","input_stream_id","source_ordinal","additional_properties","epg_status"]}},"#,
            r#""epg_status":{"type":"array","uniqueItems":true,"description":"Targets only. Keep only items whose EPG match is one of these (OR): missing_id = item has no epg_channel_id; unmatched = epg_channel_id is set but absent from the target EPG database (EPG problem); matched = epg_channel_id exists in the target EPG database.","items":{"type":"string","enum":["missing_id","unmatched","matched"]}}"#,
            r#"},"required":["cluster"],"additionalProperties":false}"#
        ),
        method: McpToolMethod::Get,
        path: "/playlist/items",
        kind: McpToolKind::Read,
        redact_secrets: true,
    },
    McpTool {
        name: "query_target_epg_channels",
        description: "Inspect the processed EPG of ONE target (addressed by NAME; call list_playlist_sources first) to find EPG problems. Lists EPG channels with {id, title, has_icon, programme_count, first_start, last_stop, covers_now, referenced_by_playlist}; no raw programmes. Filter with `issue` (OR when several): no_programmes, expired, not_current, no_title, unreferenced = no playlist item of the target uses this EPG id. Returns {target, output, total_matched, skipped_playlist_records, offset, limit, returned, has_more, channels}; continue with offset += returned while has_more is true. skipped_playlist_records is the number of stored playlist records that could not be read: when it is greater than 0, referenced_by_playlist and the unreferenced filter may be wrong for channels used only by those records, and a `note` says so. limit defaults to 50 and is clamped to 500. A `note` also explains an empty result when the target has no EPG-capable output or no EPG database yet (0 then does NOT mean healthy). Use get_target_epg_channel to look at the programmes of one channel.",
        input_schema: concat!(
            r#"{"type":"object","properties":{"#,
            r#""target":{"type":"string","description":"Target name (from list_playlist_sources)."},"#,
            epg_output_property!(),
            r#""offset":{"type":"integer","minimum":0,"description":"Number of matching channels to skip. Default 0."},"#,
            r#""limit":{"type":"integer","minimum":1,"maximum":500,"description":"Maximum channels per page. Default 50, values above 500 are clamped to 500."},"#,
            r#""search":{"type":"string","description":"Case-insensitive substring filter on the EPG channel id and title."},"#,
            r#""issue":{"type":"array","uniqueItems":true,"description":"Keep only channels with one of these issues (OR): no_programmes = channel has no programmes; expired = last programme ended in the past; not_current = no programme covers the current time; no_title = channel has no title; unreferenced = no playlist item of the target maps to this EPG id.","items":{"type":"string","enum":["no_programmes","expired","not_current","no_title","unreferenced"]}}"#,
            r#"},"required":["target"],"additionalProperties":false}"#
        ),
        method: McpToolMethod::Get,
        path: "/playlist/epg/channels",
        kind: McpToolKind::Read,
        redact_secrets: true,
    },
    McpTool {
        name: "get_target_epg_channel",
        description: "Programmes of ONE EPG channel of a target (by NAME; call list_playlist_sources first), for debugging EPG data. `channel_id` is the exact EPG channel id as returned by query_target_epg_channels or as epg_channel_id of a playlist item (it is canonicalized like the XMLTV output). Returns the channel metadata and programmes {start, stop, title, desc, categories, is_live, is_new} inside the window `from`..`to` (default now to now+24h), plus total_in_window and truncated (true when more programmes exist than `limit`). desc is truncated to 500 characters. Timestamps are RFC 3339 UTC.",
        input_schema: concat!(
            r#"{"type":"object","properties":{"#,
            r#""target":{"type":"string","description":"Target name (from list_playlist_sources)."},"#,
            epg_output_property!(),
            r#""channel_id":{"type":"string","description":"EPG channel id (from query_target_epg_channels or an item's epg_channel_id)."},"#,
            r#""from":{"type":"string","description":"Window start, RFC 3339 (e.g. 2026-10-23T18:00:00Z). Default: now."},"#,
            r#""to":{"type":"string","description":"Window end, RFC 3339. Default: now + 24 hours."},"#,
            r#""limit":{"type":"integer","minimum":1,"maximum":1000,"description":"Maximum programmes returned. Default 100, values above 1000 are clamped to 1000."}"#,
            r#"},"required":["target","channel_id"],"additionalProperties":false}"#
        ),
        method: McpToolMethod::Get,
        path: "/playlist/epg/channel",
        kind: McpToolKind::Read,
        redact_secrets: true,
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
