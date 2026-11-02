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

/// One overridable item field of an override patch: a tagged operation object
/// `{"op":"set","value":…}`, `{"op":"clear"}` or `{"op":"reset"}` instead
/// of a bare value, mirroring `OverrideFieldOp` on the wire. The description
/// comes first so a nested field never looks like a top-level property
/// declaration (see `every_schema_is_strict_with_unique_names_and_enums`).
macro_rules! override_field_property {
    ($name:literal, $type:literal, $doc:literal) => {
        concat!(
            r#"""#, $name, r#"":{"description":""#, $doc,
            r#"","type":"object","properties":{"op":{"type":"string","enum":["set","clear","reset"],"description":"set = store value and use it at the target, clear = hide the provider value, reset = drop this override and use the provider value again."},"value":{"type":"#, $type, r#","description":"New value; only with op set."}},"required":["op"],"additionalProperties":false},"#
        )
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
        description: "Read the stored playlist records of ONE target or ONE input (give exactly one of `target`/`input`, addressed by NAME; call list_playlist_sources first) for one `cluster`, with substring search, group filter, EPG-match filter, field projection and paging. Returns {source, cluster, total_matched, skipped_records, offset, limit, returned, has_more, items}. total_matched counts all records matching the filter (not just this page); continue with offset += returned while has_more is true. skipped_records is the number of stored records that could not be read: when it is greater than 0 they are missing from the listing and from total_matched, and a `note` says so. limit defaults to 50 and is clamped to 500 (the applied value is echoed and limit_clamped is true). Only stored data is read, no provider traffic is triggered; inputs without a stored database (e.g. stalker) answer an error. The provider `url` of a record is never returned. `fields: override_id` returns the id the override tools take for an item (equal to provider_id for most items; for M3U items without a numeric id a urlhash-v1:… value), and search_field override_id finds an item by exactly that id. `epg_status` works for targets only and answers an error for inputs or targets without an EPG database.",
        input_schema: concat!(
            r#"{"type":"object","properties":{"#,
            r#""target":{"type":"string","description":"Target name (from list_playlist_sources). Exactly one of `target` or `input` is required."},"#,
            r#""input":{"type":"string","description":"Input name (from list_playlist_sources). Exactly one of `target` or `input` is required."},"#,
            r#""cluster":{"type":"string","enum":["live","video","series"],"description":"Record cluster to read: live = live channels, video = VOD movies, series = series (episodes are not listed individually)."},"#,
            r#""offset":{"type":"integer","minimum":0,"description":"Number of matching records to skip. Default 0."},"#,
            r#""limit":{"type":"integer","minimum":1,"maximum":500,"description":"Maximum records per page. Default 50, values above 500 are clamped to 500."},"#,
            r#""search":{"type":"string","description":"Case-insensitive substring filter applied to the `search_field` fields. Empty means no filter."},"#,
            r#""search_field":{"type":"array","uniqueItems":true,"description":"Record fields the `search` text is matched against (any of them may match). Default: name, title, group. epg_channel_id, provider_id and input_name are matched as text too; override_id must match the whole id (exact, not a substring).","items":{"type":"string","enum":["name","title","group","epg_channel_id","provider_id","override_id","input_name"]}},"#,
            r#""group":{"type":"string","description":"Exact group (category) name; combined with `search` by AND."},"#,
            r#""fields":{"type":"array","uniqueItems":true,"description":"Record fields to return per item. Default: virtual_id, provider_id, name, title, group, item_type, cluster, epg_channel_id, category_id, channel_no. Meaning: virtual_id = id of the item in tuliprox; provider_id = id at the provider; override_id = id used as the key in the override files (what get_override_item, save_overrides and discard_overrides take; computed only when requested, equals provider_id for most items, for M3U items without a numeric id it is a urlhash-v1:… value); name = channel/stream name; title = display title; group = group (category) name; item_type = live, video, series, ...; cluster = live, video or series; epg_channel_id = EPG id assigned to the item; category_id = provider category id; channel_no = channel number; logo = logo image reference; logo_small = small logo reference; parent_code = parent control code; rec = recording/catch-up flag; input_name = name of the originating input; input_stream_id = stream id inside the originating input; source_ordinal = position within the source playlist; additional_properties = provider specific extra properties (URL-like members removed; can be large); epg_status = EPG match result (missing_id, unmatched or matched; targets only).","items":{"type":"string","enum":["virtual_id","provider_id","override_id","name","title","group","item_type","cluster","epg_channel_id","category_id","channel_no","logo","logo_small","parent_code","rec","input_name","input_stream_id","source_ordinal","additional_properties","epg_status"]}},"#,
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
        name: "get_overrides",
        description: "Read ONE override file with its current `revision`. Scope `input` (name = input name) holds the overrides of an input's items for every target that uses the input; scope `target` (name = target name) holds the overrides of one target only. Both names come from list_playlist_sources. Returns {revision, entries: [{input, kind, id, hint, fields}], field_errors, parse_error, schema_error}: `fields` are the stored field deltas of an entry, a null value meaning the field is cleared; `parse_error`/`schema_error` mean the file cannot be patched at all. The ids are the ones save_overrides and discard_overrides take (find them with query_playlist_items, fields: override_id); what one item looks like across provider, input override, target override and processed target record is answered by get_override_item. Send this `revision` back as `base_revision` of the next write.",
        input_schema: concat!(
            r#"{"type":"object","properties":{"#,
            r#""scope":{"type":"string","enum":["input","target"],"description":"Which override file to read: input = the overrides of an input, applied in every target that uses the input; target = the overrides of one target."},"#,
            r#""name":{"type":"string","description":"Input name when scope is input, target name when scope is target (from list_playlist_sources)."}"#,
            r#"},"required":["scope","name"],"additionalProperties":false}"#
        ),
        method: McpToolMethod::Get,
        path: "/playlist/overrides",
        kind: McpToolKind::Read,
        redact_secrets: true,
    },
    McpTool {
        name: "get_override_item",
        description: "Explain ONE item of an input: the raw provider record (`provider`), the deltas of the input's override file (`input_override`), the deltas of the override file of one target (`target_override`, needs `target_id`) and the record that target processed (`processed`), plus pending_per_target = [(target, pending)] telling which affected targets have not run a playlist update since the last override change, and `ambiguous` = the override key matches more than one item of the input (then edits by that id are unsafe). Use it before saving to see which layer a value comes from. `input` is an input name (from list_playlist_sources), `id` is its override_id from query_playlist_items (fields: override_id) or from get_overrides.",
        input_schema: concat!(
            r#"{"type":"object","properties":{"#,
            r#""input":{"type":"string","description":"Input name (from list_playlist_sources)."},"#,
            r#""kind":{"type":"string","enum":["live","vod","series"],"description":"Item kind of the override key: live = live channel, vod = movie, series = series."},"#,
            r#""id":{"type":"string","description":"Override id of the item: its override_id from query_playlist_items (fields: override_id)."},"#,
            r#""target_id":{"type":"integer","minimum":0,"description":"Numeric target id (from list_playlist_sources) to also report its override entry and its processed record. Omit to inspect the input alone."}"#,
            r#"},"required":["input","kind","id"],"additionalProperties":false}"#
        ),
        method: McpToolMethod::Get,
        path: "/playlist/overrides/item",
        kind: McpToolKind::Read,
        redact_secrets: true,
    },
    McpTool {
        name: "list_override_orphans",
        description: "Cleanup view of ONE input (by NAME, from list_playlist_sources): returns {input, entries: [{scope, file, input, kind, id, hint, state}], orphaned_files: [{scope, name, revision}]}. state = active (the item exists in the stored input inventory), orphaned (the input loaded successfully and no longer contains the item) or unknown (no reliable stored inventory, so nothing can be said — never trust an edit to stay effective). orphaned_files are override files whose input or target name is no longer configured, e.g. after a rename; remove them with discard_overrides using the returned scope, name and revision. Only reads stored data; no provider is contacted.",
        input_schema: r#"{"type":"object","properties":{"input":{"type":"string","description":"Input name (from list_playlist_sources) whose override entries and files are checked."}},"required":["input"],"additionalProperties":false}"#,
        method: McpToolMethod::Get,
        path: "/playlist/overrides/orphans",
        kind: McpToolKind::Read,
        redact_secrets: true,
    },
    McpTool {
        name: "search_epg_ids",
        description: "Find EPG channel ids for an item's epg_channel_id override. Give exactly one of `target_id` (searches that target's processed EPG database) or `input` (searches the stored EPG cache of that input); `q` is required. A prefix match ranks above a substring match; both the channel id and its display name are searched. Returns [{id, display_name}], at most `limit` entries. Only stored EPG data is searched, no provider is contacted, and a target or input without stored EPG data returns an empty list (not an error).",
        input_schema: concat!(
            r#"{"type":"object","properties":{"#,
            r#""q":{"type":"string","description":"Text to look for in the EPG channel id and display name; prefix matches rank first."},"#,
            r#""target_id":{"type":"integer","minimum":0,"description":"Numeric target id (from list_playlist_sources) whose EPG database is searched. Exactly one of target_id or input."},"#,
            r#""input":{"type":"string","description":"Input name (from list_playlist_sources) whose cached EPG channels are searched. Exactly one of target_id or input."},"#,
            r#""limit":{"type":"integer","minimum":1,"maximum":50,"description":"Maximum candidates returned. Default 20, values above 50 are clamped to 50."}"#,
            r#"},"required":["q"],"additionalProperties":false}"#
        ),
        method: McpToolMethod::Get,
        path: "/playlist/epg/search",
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
        name: "save_overrides",
        description: "Save manual field overrides for items of ONE input or ONE target. Read the file first with get_overrides and send its `revision` as `base_revision`: the save is rejected with 409 when the file changed in between, so re-read with get_overrides and repeat the change against the new revision (never retry blindly with the old one). Field values are tagged operations, not plain values: {\"op\":\"set\",\"value\":…} stores a value, {\"op\":\"clear\"} hides the provider value (empty field at the target), {\"op\":\"reset\"} removes the override and falls back to the provider value; an omitted field stays untouched. `id` is its override_id from query_playlist_items (fields: override_id) — a wrong or ambiguous id silently writes an override nothing matches, so verify with get_override_item. In scope target every patch names its `input`; in scope input the file itself belongs to the input, so `input` is optional and must match. A 422 lists field errors (field not allowed for the item kind, clear of name/title/group, invalid url/date/rating/channel number); a 400 means an empty patch list, a duplicate patch or a missing input. Answers {revision}: use it as the `base_revision` of the next write. Saved overrides apply on the next playlist update (update_playlists) — nothing changes in the served playlists before that.",
        input_schema: concat!(
            r#"{"type":"object","properties":{"#,
            r#""scope":{"type":"string","enum":["input","target"],"description":"Which override file to patch: input = the overrides of an input, applied in every target that uses the input; target = the overrides of one target."},"#,
            r#""name":{"type":"string","description":"Input name when scope is input, target name when scope is target (from list_playlist_sources)."},"#,
            r#""base_revision":{"type":"string","description":"`revision` of the file as read with get_overrides. A stale revision answers 409 and changes nothing; re-read and repeat."},"#,
            r#""patches":{"type":"array","minItems":1,"description":"One entry per item to change; a duplicate (input, kind, id) triple is rejected.","items":{"type":"object","properties":{"#,
            r#""input":{"description":"Parent input name of the item. Required in scope target; in scope input optional, but a different input is rejected.","type":"string"},"#,
            r#""kind":{"description":"Item kind of the entry: live = live channel, vod = movie, series = series.","type":"string","enum":["live","vod","series"]},"#,
            r#""id":{"description":"Override id of the item: its override_id from query_playlist_items (fields: override_id).","type":"string"},"#,
            r#""hint":{"description":"Informational \"<group> / <title>\" recorded with the entry; only used when the raw input item cannot be found. Normally omit it.","type":"string"},"#,
            r#""patch":{"type":"object","description":"Field deltas of the item. Every field is a tagged operation: {\"op\":\"set\",\"value\":…}, {\"op\":\"clear\"} or {\"op\":\"reset\"}; an omitted field is left as stored.","properties":{"#,
            override_field_property!("name", "\"string\"", "Item name (channel, movie or series). All item kinds. Set only, non-empty and without leading or trailing spaces; clear is rejected."),
            override_field_property!("title", "\"string\"", "Display title. All item kinds. Set only, non-empty and without leading or trailing spaces; clear is rejected."),
            override_field_property!("group", "\"string\"", "Group (category) name. All item kinds. Set only, non-empty and without leading or trailing spaces; clear is rejected."),
            override_field_property!("chno", "\"integer\"", "Channel number, 1 to 999999. Live items only."),
            override_field_property!("epg_channel_id", "\"string\"", "EPG channel id of the item, non-empty. Live items only. Find it with search_epg_ids."),
            override_field_property!("logo", "\"string\"", "Logo URL, http or https with a host. All item kinds."),
            override_field_property!("logo_small", "\"string\"", "Small logo URL, http or https with a host. Live items only."),
            override_field_property!("parent_code", "\"string\"", "Parent control code (catch-up parent) of the channel. Live items only."),
            override_field_property!("audio_track", "\"string\"", "Audio track. Live items only."),
            override_field_property!("time_shift", "\"string\"", "Time shift of the channel. Live items only."),
            override_field_property!("rec", "\"string\"", "Recording / catch-up flag of the channel. Live items only."),
            override_field_property!("tmdb", "\"integer\"", "TMDB id, 1 or greater. Video and series only."),
            override_field_property!("plot", "\"string\"", "Plot / description. Video and series only."),
            override_field_property!("genre", "\"string\"", "Genre. Video and series only."),
            override_field_property!("cast", "\"string\"", "Cast. Video and series only."),
            override_field_property!("director", "\"string\"", "Director. Video and series only."),
            override_field_property!("release_date", "\"string\"", "Release date as YYYY-MM-DD. Video and series only."),
            override_field_property!("rating", "\"number\"", "Rating between 0 and 10. Video and series only."),
            r#""remove_unknown":{"description":"Unknown keys (e.g. typos written by hand) stored in this override entry that should be removed. get_overrides reports them as field errors.","type":"array","items":{"type":"string"}}"#,
            r#"},"additionalProperties":false}"#,
            r#"},"required":["kind","id","patch"],"additionalProperties":false}}"#,
            r#"},"required":["scope","name","base_revision","patches"],"additionalProperties":false}"#
        ),
        method: McpToolMethod::Post,
        path: "/playlist/overrides",
        kind: McpToolKind::Write,
        redact_secrets: false,
    },
    McpTool {
        name: "discard_overrides",
        description: "Remove manual overrides of ONE input or ONE target, so the provider values apply again. Read the file first with get_overrides and send its `revision` as `base_revision`: a stale revision answers 409 and changes nothing (re-read and repeat), because the whole entries are dropped, not patched field by field. With `items` the listed entries are discarded; without `items` the entire override file is deleted, which is how an orphaned file from list_override_orphans is removed (use its scope, name and revision). Entries whose id matches no item can still be discarded by id. Answers {revision} for the next write. The change applies on the next playlist update (update_playlists).",
        input_schema: concat!(
            r#"{"type":"object","properties":{"#,
            r#""scope":{"type":"string","enum":["input","target"],"description":"Which override file to clean up: input = the overrides of an input, target = the overrides of one target."},"#,
            r#""name":{"type":"string","description":"Input name when scope is input, target name when scope is target (from list_playlist_sources, or the name of an orphaned file from list_override_orphans)."},"#,
            r#""base_revision":{"type":"string","description":"`revision` of the file as read with get_overrides. A stale revision answers 409 and changes nothing; re-read and repeat."},"#,
            r#""items":{"type":"array","description":"Entries to remove; a duplicate (input, kind, id) triple or a blank id is rejected. Omit or leave empty to delete the whole override file.","items":{"description":"One entry to remove.","type":"object","properties":{"#,
            r#""input":{"description":"Parent input name of the entry. Required in scope target; in scope input optional, but a different input is rejected.","type":"string"},"#,
            r#""kind":{"description":"Item kind of the entry: live = live channel, vod = movie, series = series.","type":"string","enum":["live","vod","series"]},"#,
            r#""id":{"description":"Override id of the entry: its override_id from query_playlist_items (fields: override_id), or the id from list_override_orphans.","type":"string"}"#,
            r#"},"required":["kind","id"],"additionalProperties":false}}"#,
            r#"},"required":["scope","name","base_revision"],"additionalProperties":false}"#
        ),
        method: McpToolMethod::Post,
        path: "/playlist/overrides/discard",
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
