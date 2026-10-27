# MCP Server Operator Reference

Tuliprox can expose a [Model Context Protocol](https://modelcontextprotocol.io) (MCP) endpoint so that AI assistants
and agent-capable IDEs (Claude Desktop, Cursor, ChatGPT and similar clients) can inspect, and optionally operate, a
running instance through a fixed set of tools. This page covers enabling the endpoint, the security model, the tool
catalog, client setup and troubleshooting.

## 1. What it is

* The MCP server is a **stateless JSON-RPC 2.0 endpoint** at `POST {web_ui_path}/mcp` (for example
  `http://localhost:8901/mcp`). It is served by the Web UI router, so it uses the same listener, TLS setup and reverse
  proxy as the Web UI.
* Every tool is a thin adapter over **one existing `/api/v1` REST route**. A tool call is dispatched in-process into the
  same router with the caller's `Authorization` header, so authentication, RBAC permissions, validation and behavior are
  exactly those of the REST API. Nothing is re-implemented.
* It is **off by default** and **read-only by default**. With the default configuration no `/mcp` route exists and
  nothing else changes.
* A bundled **stdio bridge** (`tuliprox --mcp-stdio`) lets clients that can only start local commands talk to a running
  instance over HTTP.

## 2. Enabling the endpoint

Add an `mcp:` block to `config.yml`:

```yaml
mcp:
  enabled: true
  read_only: true
  allowed_origins: []
  max_response_bytes: 262144
```

| Parameter            |       Type      | Default  | Technical Impact & Background                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
|:---------------------|:---------------:|:---------|:----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `enabled`            |       Bool      | `false`  | Mounts `POST {web_ui_path}/mcp`. When `false` (or the block is absent) no MCP route exists.                                                                                                                                                                                                                                                                                                                                                                                                         |
| `read_only`          |       Bool      | `true`   | Hides write tools from `tools/list` and rejects them in `tools/call`. Set to `false` to expose `update_playlists` and `update_geoip`. Forced to `true` when Web UI authentication is disabled (see [Security model](#3-security-model)).                                                                                                                                                                                                                                                            |
| `allowed_origins`    | List of Strings | `[]`     | Additional `Origin` values accepted besides the request `Host`, for example `https://claude.ai`. Entries are trimmed and a trailing `/` is removed; for `http`, `https`, `ws` and `wss` the host is lowercased and a default port is dropped (`https://claude.ai:443` is stored as `https://claude.ai`), as browsers send it. Each entry must be an origin (`scheme://host[:port]`, custom schemes allowed) without path, query, fragment or user info; anything else fails the configuration load. |
| `max_response_bytes` |       Int       | `262144` | Tool results larger than this many bytes are truncated and end with an explicit marker. Must be at least `1024`, otherwise the configuration is rejected while loading.                                                                                                                                                                                                                                                                                                                             |

* Unknown keys inside `mcp:` are rejected while loading the configuration.
* The same settings can be edited in the Web UI under **Config → MCP**. Saving a block that equals the defaults
  (disabled, read-only, no origins, `262144` bytes) removes the `mcp:` block from `config.yml`. The page shows a
  restart hint and, while Web UI authentication is disabled, a note that the endpoint is forced to read-only.
  The first-run setup wizard has an **MCP** step with the same fields (the restart and read-only notes are hidden there,
  because the instance is not running with the new configuration yet).
* The endpoint is mounted at startup. Restart Tuliprox after changing the `mcp` block.
* The endpoint is part of the Web UI router. If the Web UI is disabled, `mcp.enabled` is ignored and a warning is logged.
* At startup Tuliprox logs the endpoint and its mode, for example
  `MCP endpoint: POST /mcp (read-only)`.

## 3. Security model

### 3.1 Authentication

* **Web UI authentication is required for anything that changes state.** The MCP endpoint is protected by the same
  authentication layer as the authenticated `/api/v1` routes. When `web_ui.auth` is absent or disabled, the endpoint
  has no authentication at all, so the server is **forced read-only regardless of `mcp.read_only`** and logs a warning
  at startup. Do not expose such an instance beyond a trusted network.
* With web authentication enabled, a **missing, invalid or expired token is a real HTTP `401`** with
  `WWW-Authenticate: Bearer`, for every JSON-RPC method including `initialize` and `tools/list`. Clients and the stdio
  bridge can therefore detect an expired session at the transport level and sign in again.
* There is no OAuth flow. Obtain a token from `POST {web_ui_path}/auth/token` (see [Remote clients](#52-remote-streamable-http-clients)).
  The token lifetime is `web_ui.auth.token_ttl_mins` (default `30`); see
  [Authentication & RBAC](../configuration/config.md#24-authentication--rbac-auth).

### 3.2 Permissions

* Each tool call runs with the caller's own token against the matching REST route. The permission a tool needs is the
  permission of that REST route (see the [tool catalog](#4-tool-catalog)).
* A valid token that lacks the permission gets an MCP tool result with `isError: true` and text starting with
  `HTTP 403`. No data is returned. Other REST failures are reported the same way, for example `HTTP 503` when a feature
  such as QoS aggregation is disabled.
* Use a dedicated Web UI user with a minimal group (for example only `system.read,playlist.read`) for assistants. See
  [Structure of `groups.txt`](../configuration/config.md#structure-of-groupstxt).
* Keep `read_only: true` unless an assistant must trigger playlist updates or the GeoIP refresh.

### 3.3 Origin check (DNS rebinding protection)

The Streamable HTTP transport requires servers to validate the `Origin` header.

* A request **without** an `Origin` header (command line tools, desktop clients, the stdio bridge) passes.
* A request **with** an `Origin` header must match the request `Host` (compared case-insensitively, scheme ignored) or
  an entry of `mcp.allowed_origins`. Otherwise the answer is `403`.
* A reverse proxy that **rewrites the `Host` header** makes a legitimate browser origin differ from the `Host` Tuliprox
  sees. List the public origin in `allowed_origins`, for example `https://tuliprox.example.com`.

### 3.4 Other protections

* Tool arguments are validated against the tool's schema. Unknown arguments are rejected, and path parameters are
  percent-encoded, so an argument can never change the route that is called.
* There are no tools that accept a free-form path, shell command or configuration payload.
* **`get_config` redacts secrets**, because its result is sent to the AI provider. Values of keys containing
  `password`, `secret`, `token`, `api_key`, `authorization` or `cookie`, and non-empty `username` / `user` values, are
  replaced by `********`. Passwords in URL userinfo and the query parameters `password`, `pass`, `username`, `user`,
  `token` and `api_key` are masked in every string. The `api_proxy` user list is reduced to a credential count per
  target. A result that cannot be parsed is withheld instead of returned unredacted.
* Redaction is key-name based. It does not catch secrets under unrelated key names, secrets in free text or credentials
  embedded in URL paths (for example Xtream `/live/user/pass/...`). Other tools such as `list_active_streams` return
  user names and client IPs unredacted; grant the MCP user only the permissions you accept an AI provider seeing.

## 4. Tool catalog

Tools marked **write** are only listed and callable when `read_only: false` and Web UI authentication is enabled.
The permission column names the permission the REST route requires (see `groups.txt`).

| Tool                         |  Kind | REST route                    | Required permission                                                                               | Arguments                                                                                                                                                                                                     |
|:-----------------------------|:-----:|:------------------------------|:--------------------------------------------------------------------------------------------------|:--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `get_status`                 |  read | `GET /status`                 | `system.read`                                                                                     | none                                                                                                                                                                                                          |
| `list_active_streams`        |  read | `GET /streams`                | `system.read`                                                                                     | none                                                                                                                                                                                                          |
| `get_config`                 |  read | `GET /config`                 | any of `config.read`, `source.read`, `user.read`, `user.write`, `playlist.read`, `playlist.write` | none. Secrets are redacted (see 3.4). Filtered by the caller's permissions; can be large.                                                                                                                     |
| `get_playlist_update_status` |  read | `GET /playlist/update/status` | `playlist.read`                                                                                   | none                                                                                                                                                                                                          |
| `query_stream_history`       |  read | `GET /stream-history`         | `system.read`                                                                                     | optional `from`, `to` (date `YYYY-MM-DD` or datetime), `page` (integer >= 1), `page_size` (25, 50, 100 or 200), `search`, `search_mode` (`text` or `regex`), `search_field` (list of record fields to search) |
| `get_stream_history_summary` |  read | `GET /stream-history/summary` | `system.read`                                                                                     | optional `from`, `to`, `provider_name`, `input_name`, `api_username`, `item_type`, `event_type`                                                                                                               |
| `get_hls_origin_telemetry`   | read  | `GET /hls/origin-telemetry`   | `system.read`                                                                                     | none                                                                                                                                                                                                          |
| `list_qos_snapshots`         |  read | `GET /qos-snapshots`          | `system.read`                                                                                     | optional `limit` (integer >= 1), `stream_identity_key`, `input_name`, `provider_name`, `item_type`, `target_name`                                                                                             |
| `get_qos_snapshot`           |  read | `GET /qos-snapshots/{key}`    | `system.read`                                                                                     | required `stream_identity_key`                                                                                                                                                                                |
| `get_event_stats`            |  read | `GET /events/stats`           | `system.read`                                                                                     | none                                                                                                                                                                                                          |
| `get_ip_info`                |  read | `GET /ipinfo`                 | `system.read`                                                                                     | none                                                                                                                                                                                                          |
| `list_playlist_sources`      | read  | `GET /playlist/sources`       | `playlist.read`                                                                                   | none. Lists targets and inputs by name (see 4.1).                                                                                                                                                             |
| `query_playlist_items`       | read  | `GET /playlist/items`         | `playlist.read`                                                                                   | required `cluster`, exactly one of `target`/`input`; optional paging, filter and field projection (see 4.1).                                                                                                  |
| `query_target_epg_channels`  | read  | `GET /playlist/epg/channels`  | `playlist.read`                                                                                   | required `target`; optional `output`, paging, `search`, `issue` (see 4.2).                                                                                                                                    |
| `get_target_epg_channel`     | read  | `GET /playlist/epg/channel`   | `playlist.read`                                                                                   | required `target`, `channel_id`; optional `output`, `from`, `to`, `limit` (see 4.2).                                                                                                                          |
| `update_playlists`           | write | `POST /playlist/update`       | `playlist.write`                                                                                  | optional `targets` (list of target names; omit or leave empty to update all targets)                                                                                                                          |
| `update_geoip`               | write | `GET /geoip/update`           | `system.write`                                                                                    | none                                                                                                                                                                                                          |

Notes:

* `query_stream_history` and `get_stream_history_summary` need `reverse_proxy.stream_history` to be enabled.
  `list_qos_snapshots` and `get_qos_snapshot` need `qos_aggregation` to be enabled.
  `get_hls_origin_telemetry` needs `reverse_proxy.hls_origin_telemetry` to be enabled. `update_geoip` needs
  `reverse_proxy.geoip` to be enabled. Otherwise the REST route answers with an error status that is returned as an
  `isError` result.
* `update_playlists` queues a playlist update (the REST route answers `202` with a run id). A second request while one
  is queued answers `409`.
* The playlist and EPG tools read stored data only and never trigger provider traffic.
* Tools declare `readOnlyHint` in their annotations, so clients can ask for confirmation before calling write tools.
* Only the `tools` capability is implemented. Prompts and resources are not offered.

### 4.1 Reading stored playlists

How a client learns to use these tools: `tools/list` carries a description and a JSON Schema per tool (parameters, enums
and one line per selectable field), and the `initialize` result carries an `instructions` text with the recommended
workflow. The names below are exactly those enums.

Start with `list_playlist_sources`. It returns the names that the other tools expect, never credentials or URLs:

```json
{
  "targets": [{"id": 1, "name": "main", "outputs": ["xtream", "m3u"],
               "epg": [{"output": "xtream", "available": true}, {"output": "m3u", "available": false}]}],
  "inputs": [{"id": 1, "name": "provider-a", "input_type": "xtream", "clusters": ["live", "video", "series"]}]
}
```

`clusters` is empty for input types without a stored playlist database (for example `stalker`); `epg.available` tells
whether the processed EPG database of that output exists.

`query_playlist_items` reads the **stored** records (the same data the Web UI lists) of one target or one input. No
provider is contacted. Parameters:

| Parameter        | Meaning                                                                                                                               |
|:-----------------|:--------------------------------------------------------------------------------------------------------------------------------------|
| `target`/`input` | Name of the source. Exactly one is required; both or neither answer an error.                                                         |
| `cluster`        | Required. `live`, `video` or `series`. A target lists series without episode rows, like the Web UI.                                   |
| `offset`         | Matching records to skip (default 0).                                                                                                 |
| `limit`          | Page size, default 50, clamped to 500. The response echoes the applied `limit` and sets `limit_clamped`.                              |
| `search`         | Case-insensitive substring, matched against `search_field`.                                                                           |
| `search_field`   | List of `name`, `title`, `group`, `epg_channel_id`, `provider_id`, `input_name`. Default `name`, `title`, `group`; any one may match. |
| `group`          | Exact (case-sensitive) group name, combined with `search` by AND.                                                                     |
| `fields`         | List of fields to return (table below). Default: the first ten.                                                                       |
| `epg_status`     | Targets only. List of `missing_id`, `unmatched`, `matched` (OR). See 4.2.                                                             |

Selectable `fields`:

| Field                   | Meaning                                                                                              |
|:------------------------|:-----------------------------------------------------------------------------------------------------|
| `virtual_id`            | Id of the item inside Tuliprox (default)                                                             |
| `provider_id`           | Id at the provider, always a string (default)                                                        |
| `name`, `title`         | Channel/stream name and display title (default)                                                      |
| `group`                 | Group (category) name (default)                                                                      |
| `item_type`             | `live`, `video`, `series`, `series_info`, `catchup` (default)                                        |
| `cluster`               | `live`, `video` or `series` (default)                                                                |
| `epg_channel_id`        | EPG id assigned to the item, `null` when none (default)                                              |
| `category_id`           | Provider category id; `null` for M3U records (default)                                               |
| `channel_no`            | Channel number (default)                                                                             |
| `logo`, `logo_small`    | Logo references as stored                                                                            |
| `parent_code`, `rec`    | Parental code and recording/catch-up flag                                                            |
| `input_name`            | Name of the originating input                                                                        |
| `input_stream_id`       | Stream id inside the originating input                                                               |
| `source_ordinal`        | Position inside the source playlist                                                                  |
| `additional_properties` | Provider specific extras (video/series details, can be large); URL-like members are removed          |
| `epg_status`            | `missing_id`, `unmatched` or `matched` (targets with an EPG database only)                           |

Response:

```json
{
  "source": {"kind": "target", "name": "main"},
  "cluster": "live",
  "total_matched": 1342, "skipped_records": 0, "offset": 0, "limit": 50, "limit_clamped": false,
  "returned": 50, "has_more": true,
  "items": [{"virtual_id": 1001, "provider_id": "4711", "name": "Das Erste HD", "title": "Das Erste HD",
             "group": "DE | Public", "item_type": "live", "cluster": "live",
             "epg_channel_id": "daserste.de", "category_id": 12, "channel_no": 1}]
}
```

`total_matched` counts every record matching the filter, not only the page; continue with `offset += returned` while
`has_more` is true. The call makes one pass over the stored database, so its cost grows with the playlist size. A source
that was never updated answers `total_matched: 0` plus a `note`.

`skipped_records` is always present. It is `0` for a healthy database; a greater value is the number of stored records
that could not be read and are therefore missing from the listing and not part of `total_matched`. The response then
carries a `note` saying so, so a damaged database is not mistaken for a short playlist.

### 4.2 Finding EPG problems

Every target with an `xtream` or `m3u` output has a processed EPG database. Two tools inspect it, and `epg_status`
connects it to the playlist:

1. `query_playlist_items` with `epg_status=missing_id` lists items without an EPG id, `epg_status=unmatched` lists items
   whose id does not exist in the EPG database (a failed EPG mapping), `epg_status=matched` the healthy ones.
2. `query_target_epg_channels` lists EPG channels as `{id, title, has_icon, programme_count, first_start, last_stop,
   covers_now, referenced_by_playlist}` (timestamps RFC 3339 UTC, no raw programmes). Parameters: `target` (required),
   `output` (`xtream` or `m3u`, default: the first output with an EPG database), `offset`, `limit` (default 50,
   max 500), `search` (id or title substring) and `issue` (list, OR):

   | `issue`         | Matches channels that                                                         |
   |:----------------|:------------------------------------------------------------------------------|
   | `no_programmes` | have no programme at all                                                      |
   | `expired`       | have only programmes that ended in the past                                   |
   | `not_current`   | have no programme covering the current time (includes channels without any)   |
   | `no_title`      | have no title                                                                 |
   | `unreferenced`  | are used by no playlist item of the target                                    |

3. `get_target_epg_channel` shows the programmes of one channel: `target`, `channel_id` (exact EPG id, canonicalized
   like the XMLTV output), optional `output`, `from`/`to` (RFC 3339, default now to now + 24 h) and `limit` (default
   100, max 1000). The response has the channel metadata, `total_in_window`, `truncated` and `programmes` with
   `start`, `stop`, `title`, `desc` (cut at 500 characters), `categories`, `is_live`, `is_new`. A programme belongs to
   the window when it ends after `from` and starts before `to`.

The response of `query_target_epg_channels` also has `skipped_playlist_records`, always present: the number of stored
playlist records that could not be read while collecting the EPG ids the playlist uses. It is `0` for a healthy
database; a greater value means `referenced_by_playlist` and the `unreferenced` filter may be wrong for channels used
only by those records, and the `note` says so. `referenced_by_playlist` is `null` and the `unreferenced` filter matches
nothing when the playlist of the target is not stored (the `note` says that, too).

A target without an EPG database answers `200` with `total_matched: 0` and a `note` that says why, so that "0" is not
mistaken for "healthy". Only the processed per-target EPG is inspectable; the raw input EPG files are not.

Scanning an EPG database is bounded: at most two scans run at the same time (further calls wait), a scan stops when the
client disconnects, and the set of EPG channel ids behind `epg_status` is cached (the least recently used of up to eight
databases is evicted) until the EPG database is rewritten (for example by the next playlist update). A cached set is
answered without a scan slot or opening the database, so repeated `epg_status` queries are cheap.

Typical debugging flow: `list_playlist_sources`, then `query_playlist_items` with `epg_status=unmatched`, then
`query_target_epg_channels` with `issue=expired` or `unreferenced`, then `get_target_epg_channel` for a suspicious id.

### 4.3 What is never returned

* The provider `url` of a record is not selectable and never returned; Xtream provider URLs carry credentials in their
  path, which the generic redaction cannot detect.
* In `additional_properties`, the members `direct_source`, catch-up `source` and `extra_attributes`, the binary probe
  blobs `video` and `audio`, every key named `url` or ending in `_url`, and every string value that looks like
  credentials are removed. `logo` and `logo_small` become `null` when they contain credentials.
* Results still pass the generic redaction and the `mcp.max_response_bytes` limit. A large `limit` combined with
  `additional_properties` can hit that limit and is then truncated with an explicit marker.
* A stored record that cannot be decoded is skipped by the database reader and logged on the server. It is missing from
  the pages, and the response reports how many were skipped in `skipped_records` together with a `note`.

## 5. Client setup

### 5.1 Local clients over stdio (Claude Desktop, Cursor)

Clients that start a local command use the built-in bridge. It reads newline-delimited JSON-RPC on stdin, forwards each
message to the HTTP endpoint of a running instance and writes the answers to stdout. It signs in with a Web UI user via
`/auth/token` and signs in again once when the endpoint answers `401`. When the target has no web authentication the
credentials are optional. If only one of the two environment variables is set, the bridge warns on stderr and continues
without credentials.

| Setting                               | Meaning                                                                                                                                                                                                                             |
|:--------------------------------------|:------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `--mcp-stdio`                         | Run the bridge instead of the server.                                                                                                                                                                                               |
| `--mcp-url <base-url>`                | Base URL of the running instance including the Web UI path prefix, if any, for example `http://localhost:8901` or `https://tv.example.com/admin`. The bridge appends `/mcp` and `/auth/token`. Defaults to `http://127.0.0.1:8901`. |
| `TULIPROX_MCP_USER` (environment)     | Web UI user name used for the sign-in.                                                                                                                                                                                              |
| `TULIPROX_MCP_PASSWORD` (environment) | Password of that user.                                                                                                                                                                                                              |

Claude Desktop (`claude_desktop_config.json`) and Cursor (`.cursor/mcp.json` or the global `mcp.json`) use the same
shape:

```json
{
  "mcpServers": {
    "tuliprox": {
      "command": "/usr/local/bin/tuliprox",
      "args": ["--mcp-stdio", "--mcp-url", "http://localhost:8901"],
      "env": {
        "TULIPROX_MCP_USER": "assistant",
        "TULIPROX_MCP_PASSWORD": "change-me"
      }
    }
  }
}
```

* Only protocol JSON is written to **stdout**. Diagnostics go to stderr. Do not wrap the command in a script that
  prints anything to stdout (banners, `echo`, `set -x`), because MCP clients treat every stdout line as a protocol
  message.
* The bridge exits cleanly when the client closes stdin.
* Passing the password through the environment keeps it out of the process list. Restrict the permissions of the client
  config file.

### 5.2 Remote Streamable HTTP clients

Clients that support remote MCP servers over Streamable HTTP can connect directly. Use a bearer token:

```bash
TOKEN=$(curl -s -X POST http://localhost:8901/auth/token \
  -H 'Content-Type: application/json' \
  -d '{"username":"assistant","password":"change-me"}' | jq -r .token)
```

Then configure the client with the URL `https://tuliprox.example.com/mcp` and the header
`Authorization: Bearer <token>`. Example for a client that accepts a JSON server definition:

```json
{
  "mcpServers": {
    "tuliprox": {
      "type": "http",
      "url": "https://tuliprox.example.com/mcp",
      "headers": { "Authorization": "Bearer <token>" }
    }
  }
}
```

The token expires after `web_ui.auth.token_ttl_mins` (default 30 minutes) and there is no refresh flow on the MCP
endpoint, so the client has to be given a new token after it expires. For unattended local use prefer the
[stdio bridge](#51-local-clients-over-stdio-claude-desktop-cursor), which signs in again on its own. Always use HTTPS
when the endpoint is reachable from outside a trusted network.

### 5.3 Verifying with curl

```bash
curl -s http://localhost:8901/mcp \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"get_status","arguments":{}}}' | jq
```

## 6. Protocol behavior and limits

* **Methods:** `initialize`, `notifications/initialized`, `ping`, `tools/list` and `tools/call`. Any other method
  answers JSON-RPC error `-32601`.
* **Protocol versions:** Tuliprox echoes the client's `protocolVersion` when it is one of `2025-06-18`, `2025-03-26`
  or `2024-11-05` and answers `2025-06-18` otherwise.
* **Stateless:** there are no MCP sessions and no `Mcp-Session-Id` header. Every request stands alone.
* **JSON only:** responses are plain `application/json`. There is no SSE stream, no server-initiated notification and
  no resumability. `GET` and `DELETE` on the endpoint answer `405` (with `Allow: POST`).
* **No batching:** a JSON-RPC batch array is rejected with `-32600`.
* **Notifications** (messages without an `id`) answer `202 Accepted` with an empty body.
* **Errors:** malformed JSON answers `-32700`, an invalid envelope `-32600`, an unknown tool, a missing `params` or a
  write tool in read-only mode `-32602`. Missing or invalid tool arguments, and any non-success answer of the REST
  route, are returned as an `isError: true` tool result so the model can read the reason.
* **Truncation:** a tool result larger than `mcp.max_response_bytes` is cut on a character boundary and ends with
  `[truncated: response exceeded N bytes]`. Narrow the query (date range, `page_size`, `limit`, provider filter) or
  raise `max_response_bytes`.

## 7. Troubleshooting

| Symptom                                                           | Cause and fix                                                                                                                                                                                                      |
|:------------------------------------------------------------------|:-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `404` (or the Web UI page) on `/mcp`                              | `mcp.enabled` is `false`, the Web UI is disabled, or Tuliprox has not been restarted since the change. Check the startup log for the `MCP endpoint:` line. `web_ui.path` is part of the URL (`{web_ui_path}/mcp`). |
| `401` with `WWW-Authenticate: Bearer`                             | No token, an invalid token or an expired token (`web_ui.auth.token_ttl_mins`). Request a new token from `/auth/token`. After an update that changes permissions, tokens issued earlier are rejected as well.       |
| `403` with body `Origin not allowed`                              | The request carries an `Origin` header that matches neither the `Host` nor `mcp.allowed_origins`. Behind a reverse proxy that rewrites `Host`, add the public origin to `allowed_origins`.                         |
| Tool result `isError: true` starting with `HTTP 403`              | The token is valid but the user's group lacks the permission of the underlying REST route. See the [tool catalog](#4-tool-catalog) and `groups.txt`.                                                               |
| Tool result `isError: true` starting with `HTTP 503`              | The feature behind the tool is disabled (for example `qos_aggregation` or `reverse_proxy.stream_history`).                                                                                                         |
| `405 Method Not Allowed` on `GET`                                 | Expected. The server offers no SSE stream. Clients must use `POST`. A client that insists on `GET` for a stream tolerates the `405`.                                                                               |
| Write tool missing from `tools/list` or `-32602` on `tools/call`  | `mcp.read_only` is `true`, or Web UI authentication is disabled (forced read-only). Set `read_only: false` and enable `web_ui.auth`.                                                                               |
| Tool output ends with `[truncated: ...]`                          | The result exceeded `mcp.max_response_bytes`. Use filters or raise the limit.                                                                                                                                      |
| Claude Desktop or Cursor reports invalid JSON from the stdio tool | Something other than protocol JSON reached stdout. Run the bridge command directly in a terminal, check stderr, and make sure no wrapper script prints to stdout.                                                  |
| The stdio bridge keeps failing with `401`                         | Wrong `TULIPROX_MCP_USER` / `TULIPROX_MCP_PASSWORD`, or the user is throttled after repeated failed sign-ins. Check the `auth.sign_in.throttled` event in the log.                                                 |

## 8. Non-goals and follow-ups

* **No configuration write tools.** The REST save endpoints take whole configuration documents and offer no diff or
  preview, which is unsafe for an LLM. Configuration is readable through `get_config` only.
* **No SSE, sessions or OAuth.** Long-lived access relies on `token_ttl_mins` or the stdio bridge's re-login.
* **No prompts or resources** MCP capabilities.
* Planned follow-ups: configuration write tools with a dry-run mode, and per-tool permission toggles.
