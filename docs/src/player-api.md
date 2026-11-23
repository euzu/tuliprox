# Player & Integration API

This guide is the developer companion to the OpenAPI description at
[`api/tuliprox-integration-api.openapi.yaml`](api/tuliprox-integration-api.openapi.yaml). It describes the integration
surface that a playlist user client needs: authentication, the personal player API, recordings, playback URLs, the
diagnostic stream headers, and the MCP player tools.

The audience is third-party developers who build IPTV player applications, set-top-box front ends, home automation,
or AI agents on top of tuliprox. Every route, field, enum value, status code, and header in this guide was verified
against the backend sources listed in the final report.

**Out of scope:** administrator APIs for configuration, users, plugins, panel access, system status, and stream
administration. They are documented in the [REST API Cookbook](./rest-api-cookbook.md) and the operator references.
A playlist user token cannot call them.

## 1. Overview

### What an integrator can build

A playlist user (a credential from `api_proxy.yml`) can build a client that:

- discovers its own plan features and limits at runtime;
- lists, monitors, and stops its own playback streams across devices;
- queries failover group members and switches the source of a running stream;
- finds running shared streams that can be joined without a new provider connection, including compatible streams
  started by other targets;
- splits an own shared stream off into a dedicated provider connection;
- searches the channels of its own playlist and reads their EPG;
- reads and writes its personal bouquet (category selection);
- creates, edits, cancels, and deletes its own DVR recordings;
- creates and deletes recurring weekly recording rules;
- stores and resumes watch progress per user;
- subscribes to a personal JSON event stream for stream, failover, and recording changes;
- plays, downloads, and (when available) thumbnails recording media;
- receives the recording snapshot over the WebSocket;
- uses the same operations as MCP tools from an AI agent.

### Base URL and `web_ui_path`

All REST routes in this guide live under the Web UI path. Let `BASE_URL` be `scheme://host:port` plus `web_ui_path`.
The path comes from `web_ui.path` in `config.yml`; the default (`/`, an empty value, or no `path`) means no prefix.

| `web_ui.path` | Token endpoint              | Player API                          | MCP endpoint         |
|:--------------|:----------------------------|:------------------------------------|:---------------------|
| unset or `/`  | `POST /auth/token`          | `GET /api/v1/me/capabilities`       | `POST /mcp`          |
| `admin`       | `POST /admin/auth/token`    | `GET /admin/api/v1/me/capabilities` | `POST /admin/mcp`    |
| `tuliprox`    | `POST /tuliprox/auth/token` | `GET /tuliprox/api/v1/me/streams`   | `POST /tuliprox/mcp` |

The WebSocket is `GET {BASE_URL}/ws`; playback routes (`player_api.php`, `get.php`, `live/`, `m3u-stream/`, ...) are
mounted at the server root and do **not** use `web_ui_path`.

### Prerequisites and access control

Player API routes (`/api/v1/me/*`), personal player event streams (`/api/v1/me/events`), and MCP player tools are
governed by dedicated Player API switches rather than being coupled to the Web UI bouquet editor switches.

The player API requires:

1. `web_ui.enabled: true` in `config.yml`. Without it the router root, including `/api/v1/me`, `/ws`, and `/mcp`,
   is not mounted.
2. Web UI authentication enabled: `web_ui.auth.enabled: true` with a non-empty `issuer` and `secret`. The token
   secret and authentication machinery reside here. With authentication disabled, `/auth/token` returns the internal
   no-auth token rather than an API user token.
3. Global Player API switch: `web_ui.player_api_enabled: Option<bool>`. When unset (`None`), it defaults to and follows
   `web_ui.user_ui_enabled` (default `true`). Setting `player_api_enabled: false` disables the Player API globally even
   if `user_ui_enabled` is true. The `/me` router is always mounted; requests with a disabled Player API answer `403 Forbidden`.
4. User Player API feature: `player_features.api_enabled: Option<bool>`. Evaluated with field-wise inheritance
   (`user.or(plan).unwrap_or(user.ui_enabled)`). When unset, it follows the user's `ui_enabled` (default `true`). Setting
   `api_enabled: false` denies Player API access for the user (`403 Forbidden`).
5. The user's target has an Xtream or M3U output in `source.yml`; otherwise channel, EPG, and playback data are empty
   or absent even though authentication succeeds.

`web_ui_path`, `web_ui.enabled`, `web_ui.auth.*`, `web_ui.user_ui_enabled`, and `web_ui.player_api_enabled` live in
`config.yml`. `ui_enabled` and `player_features` (including `api_enabled`) live in `api_proxy.yml`. Plans are defined in
`plans.yml` (legacy inline plans in `api_proxy.yml` are still accepted).

#### Access matrix

Access is divided into four distinct classes evaluated per request:

| Access class | Routes & tools | Required switch condition | Behavior when disabled |
| :--- | :--- | :--- | :--- |
| **Player routes** | `/api/v1/me/*`, `/api/v1/me/events` | Effective `player_features.api_enabled` AND effective `web_ui.player_api_enabled` | `403 Forbidden` |
| **MCP player tools** | MCP player tool catalog | Effective `player_features.api_enabled` AND effective `web_ui.player_api_enabled` | No tools listed (tool calls fail with "Unknown tool", not 403) |
| **Recording routes** | `/api/v1/recording/*`, `/library/recording/*` | Web UI allowed OR Player API allowed (`Any`) | `403 Forbidden` |
| **Web UI routes** | `/api/v1/user/*` (bouquets, categories), `/api/v1/me/settings` | `user.ui_enabled: true` AND `web_ui.user_ui_enabled: true` (`WebUi`) | `403 Forbidden`, also when `user_ui_enabled` is `false` at startup and `/api/v1/user/*` is not mounted |
| **Token login & refresh** | `POST /auth/token`, `POST /auth/refresh` | `user.ui_enabled: true` OR effective `user.api_enabled: true` | `403 Forbidden` |

#### Dedicated player-only user example

You can configure a player-only user who can sign in with an external IPTV player app to consume streams, switch failover
sources, and track playback progress without having access to the Web UI bouquet management interface:

```yaml
# config.yml
web_ui:
  enabled: true
  user_ui_enabled: true
  player_api_enabled: true
  path: /
  auth:
    enabled: true
    issuer: tuliprox
    secret: "change-me-to-a-long-random-value"
    token_ttl_mins: 30
```

```yaml
# plans.yml (legacy inline plans in api_proxy.yml are still accepted)
plans:
  - name: family
    output_clusters: [live, vod, series]
    max_connections: 2
    player_features:
      api_enabled: true
      source_switch: true
      joinable_streams: true
      recording: private

user:
  - target: main
    credentials:
      - username: living_room_tv
        password: player-secret-pass
        ui_enabled: false              # Disables Web UI bouquet editing (/api/v1/user/...)
        player_features:
          api_enabled: true             # Grants Player API access (/api/v1/me/...)
```

In this setup:

- `living_room_tv` successfully logs in via `POST /auth/token`.
- Calling `/api/v1/me/*` (capabilities, streams, source switches, events) succeeds.
- Calling `/api/v1/user/*` returns `403 Forbidden`.
- Note that a player-only user who signs into the Web UI in a browser sees 403 errors on Web UI pages
  because Web UI bouquet editing and user settings require `user.ui_enabled: true`.

```yaml
# config.yml
web_ui:
  enabled: true
  user_ui_enabled: true
  player_api_enabled: true
  path: /
  auth:
    enabled: true
    issuer: tuliprox
    secret: "change-me-to-a-long-random-value"
    token_ttl_mins: 30

mcp:
  enabled: true
  read_only: true
  player_tools: write
  allowed_origins:
    - https://player.example.com
  max_response_bytes: 262144
```

`mcp.player_tools` accepts `off`, `read`, or `write` and defaults to `off`; the endpoint itself is mounted only when
`mcp.enabled: true` and the Web UI is enabled. `mcp.read_only` applies to administrator tools only; player tools follow
`mcp.player_tools` and the caller's own features.

```yaml
# plans.yml (legacy inline plans in api_proxy.yml are still accepted)
plans:
  - name: family
    output_clusters: [live, vod, series]
    max_connections: 2
    player_features:
      api_enabled: true
      source_switch: true
      joinable_streams: true
      recording: private
      recording_quota: 20GB
      max_concurrent_recordings: 2
      max_recording_rules: 10
      free_shared_joins: true
      max_free_shared_joins: 1
      watch_progress: true
      event_stream: true

user:
  - target: main
    credentials:
      - username: alice
        password: example-password
        ui_enabled: true
        plan: family
        player_features:
          recording: shared
```

`player_features` is resolved field by field: an explicit user value wins, an unset user value inherits from the
referenced plan, and an unset plan value uses the built-in default (`false` for the booleans, `none` for `recording`,
`1` for `max_free_shared_joins`, `10` for `max_recording_rules`). Setting a numeric field to `0` is rejected while the configuration loads.

### Endpoint map

| Method   | Path                                         | Area               |
|:---------|:---------------------------------------------|:-------------------|
| `POST`   | `/auth/token`                                | Authentication     |
| `POST`   | `/auth/refresh`                              | Authentication     |
| `GET`    | `/api/v1/me/capabilities`                    | Feature discovery  |
| `GET`    | `/api/v1/me/channels`                        | Channels           |
| `GET`    | `/api/v1/me/epg`                             | EPG                |
| `GET`    | `/api/v1/me/streams`                         | Own streams        |
| `GET`    | `/api/v1/me/streams/joinable`                | Shared streams     |
| `DELETE` | `/api/v1/me/streams/{uid}`                   | Own streams        |
| `GET`    | `/api/v1/me/streams/{uid}/failover-members`  | Failover           |
| `POST`   | `/api/v1/me/streams/{uid}/failover-switch`   | Failover           |
| `POST`   | `/api/v1/me/streams/{uid}/split`             | Own streams        |
| `GET`    | `/api/v1/me/channels/{vid}/failover-members` | Failover           |
| `GET`    | `/api/v1/me/events`                          | Events (WebSocket) |
| `GET`    | `/api/v1/me/progress`                        | Watch progress     |
| `DELETE` | `/api/v1/me/progress`                        | Watch progress     |
| `GET`    | `/api/v1/me/progress/{kind}/{item_id}`       | Watch progress     |
| `PUT`    | `/api/v1/me/progress/{kind}/{item_id}`       | Watch progress     |
| `DELETE` | `/api/v1/me/progress/{kind}/{item_id}`       | Watch progress     |
| `GET`    | `/api/v1/user/playlist/categories`           | Bouquet            |
| `GET`    | `/api/v1/user/playlist/bouquet`              | Bouquet            |
| `POST`   | `/api/v1/user/playlist/bouquet`              | Bouquet            |
| `POST`   | `/api/v1/recording/requests`                 | Recordings         |
| `PATCH`  | `/api/v1/recording/requests/{id}`            | Recordings         |
| `DELETE` | `/api/v1/recording/requests/{id}`            | Recordings         |
| `POST`   | `/api/v1/recording/requests/{id}/cancel`     | Recordings         |
| `POST`   | `/api/v1/recording/conflicts/preview`        | Recordings         |
| `GET`    | `/api/v1/recording/availability`             | Recordings         |
| `GET`    | `/api/v1/recording/rules`                    | Recording rules    |
| `POST`   | `/api/v1/recording/rules`                    | Recording rules    |
| `PATCH`  | `/api/v1/recording/rules/{id}`               | Recording rules    |
| `DELETE` | `/api/v1/recording/rules/{id}`               | Recording rules    |
| `GET`    | `/api/v1/library/recording/playback/{uuid}`  | Recording media    |
| `GET`    | `/api/v1/library/recording/download/{uuid}`  | Recording media    |
| `GET`    | `/api/v1/library/recording/thumbnail/{uuid}` | Recording media    |
| `GET`    | `/ws`                                        | Recording snapshot |
| `POST`   | `/mcp`                                       | MCP player tools   |

## 2. Authentication

Playlist users authenticate with their Xtream credentials against the same token endpoint as Web UI users.

### Sign in

```bash
BASE_URL="http://localhost:8901"
USERNAME="alice"
PASSWORD="example-password"

TOKEN=$(curl -s -X POST "$BASE_URL/auth/token" \
    -H 'Content-Type: application/json' \
    -d "{\"username\":\"$USERNAME\",\"password\":\"$PASSWORD\"}" | jq -r '.token')
```

Request body:

```json
{
  "username": "alice",
  "password": "example-password"
}
```

Response (`200 OK`):

```json
{
  "token": "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.example.payload.signature",
  "username": "alice"
}
```

The token is an HS256 JWT with the configured issuer, the subject `api:<username>`, and the role `API_USER`. It carries
no permissions of its own: every request recomputes the effective permissions from the live `player_features`, so a
configuration reload changes what the token can do without a new sign-in.

The response has no `exp` field. The lifetime is `web_ui.auth.token_ttl_mins` (default `30` minutes). Read `exp` from the
JWT payload if you need to refresh proactively.

### Use the token

```bash
curl -s "$BASE_URL/api/v1/me/capabilities" -H "Authorization: Bearer $TOKEN" | jq .
```

### Refresh

`POST /auth/refresh` takes the old token in the `Authorization` header, without a body, and returns a new
token response (`{"token":"...","username":"..."}`).

```bash
TOKEN=$(curl -s -X POST "$BASE_URL/auth/refresh" \
    -H "Authorization: Bearer $TOKEN" | jq -r '.token')
```

Refresh fails with `401` when the token is invalid, expired, revoked, or belongs to a user that no longer exists. It
fails with `403` when the user still exists but neither `ui_enabled` nor effective `api_enabled` is true. A password change on a Web UI user also
invalidates the old token; playlist user tokens carry no password version and are not affected by Web UI passwords.

### Error semantics

| Status | Header                      | Body                                                                      | Meaning                                           | Client action                    |
|:-------|:----------------------------|:--------------------------------------------------------------------------|:--------------------------------------------------|:---------------------------------|
| `200`  | ---                         | `{"token":"...","username":"..."}`                                        | Sign-in or refresh succeeded                      | Store the token                  |
| `401`  | `WWW-Authenticate: Bearer`  | empty (sign-in) or text (authenticated request)                           | Wrong credentials, invalid or expired token       | Sign in again                    |
| `401`  | `X-Token-Refresh: required` | text: `token was issued for an older permission schema; refresh required` | Token schema is stale                             | Call `/auth/refresh`, then retry |
| `403`  | ---                         | empty (sign-in) or text `principal does not have the required role`       | Correct principal, no Web UI access or permission | Show message, do not retry       |
| `429`  | `Retry-After: <seconds>`    | empty                                                                     | Sign-in throttled after repeated failures         | Wait, then retry                 |

`X-Token-Refresh: required` is sent **only** for a stale permission schema or a missing `subject_id`. Those two cases
are recoverable with a refresh. A revoked token, a changed password, or an invalid signature is a plain `401` and
requires a new sign-in.

At the authentication layer, a principal that is authenticated but lacks the role or permission gets `403`, never `401`.
Routes that hide an item return `404` instead (for example a foreign stream).

## 3. Feature discovery

Start every client with `GET /api/v1/me/capabilities` and derive the UI from the answer. Do not hard-code a plan.

```bash
curl -s "$BASE_URL/api/v1/me/capabilities" -H "Authorization: Bearer $TOKEN" | jq .
```

```json
{
  "source_switch": true,
  "joinable_streams": true,
  "recording": "private",
  "recording_quota_bytes": 21474836480,
  "max_concurrent_recordings": 2,
  "max_recording_rules": 10,
  "free_shared_joins": true,
  "max_free_shared_joins": 1,
  "watch_progress": true,
  "event_stream": true,
  "stream_split": true,
  "effective_permissions": ["recording.read", "recording.create", "recording.manage", "recording.delete"],
  "max_connections": 2,
  "active_connections": 1,
  "shared_joins": 0,
  "free_shared_joins_in_use": 0,
  "share_live_streams": true,
  "source_switch_protocols": ["ts", "hls"],
  "recording_enabled": true
}
```

| Field                       | Type                             | Meaning                                                                                       |
|:----------------------------|:---------------------------------|:----------------------------------------------------------------------------------------------|
| `source_switch`             | bool                             | Failover member listing and manual switching are allowed                                      |
| `joinable_streams`          | bool                             | The joinable shared stream endpoint is allowed, including compatible streams of other targets |
| `recording`                 | `none`/`read`/`private`/`shared` | Effective DVR level                                                                           |
| `recording_quota_bytes`     | integer, omitted when unset      | Configured personal private-recording quota                                                   |
| `max_concurrent_recordings` | integer, omitted when unset      | Configured concurrent recording limit                                                         |
| `max_recording_rules`       | integer, omitted when unset      | Per-user rule limit enforced on create (omitted when recording level is `none` or `read`)     |
| `free_shared_joins`         | bool                             | Free shared joins are allowed                                                                 |
| `max_free_shared_joins`     | integer                          | Maximum concurrent free shared joins                                                          |
| `watch_progress`            | bool                             | Playback progress storage under `/api/v1/me/progress` is allowed                              |
| `event_stream`              | bool                             | The personal event WebSocket `/api/v1/me/events` is allowed                                   |
| `stream_split`              | bool                             | `POST /api/v1/me/streams/{uid}/split` is allowed (mirrors `source_switch`)                    |
| `effective_permissions`     | string array                     | Permission names implied by `recording` (see the DVR table)                                   |
| `max_connections`           | integer                          | Connection limit; `0` means unlimited                                                         |
| `active_connections`        | integer                          | Counted connections right now                                                                 |
| `shared_joins`              | integer                          | Shared streams the user is currently attached to                                              |
| `free_shared_joins_in_use`  | integer                          | Free shared joins in use right now                                                            |
| `share_live_streams`        | bool                             | Live streams of this target are currently shared by the server                                |
| `source_switch_protocols`   | string array                     | Protocols that support a switch; currently always `["ts", "hls"]`                             |
| `recording_enabled`         | bool                             | The DVR is enabled globally (`video.recording.enabled`)                                       |

Branch on the values rather than on assumptions:

```js
const caps = await api.get("/api/v1/me/capabilities");

showSourceSwitch = caps.source_switch;
showJoinable = caps.joinable_streams;

const dvrVisible = caps.recording_enabled && caps.recording !== "none";
const canCreate = caps.effective_permissions.includes("recording.create");
const canShare = caps.effective_permissions.includes("recording.share");
const canUseRules = dvrVisible && caps.max_recording_rules !== undefined;

showResume = caps.watch_progress;
showLiveEvents = caps.event_stream;
showSplit = caps.stream_split;

const atLimit = caps.max_connections > 0 && caps.active_connections >= caps.max_connections;
const mayFreeJoin = caps.free_shared_joins && caps.free_shared_joins_in_use < caps.max_free_shared_joins;
```

`effective_permissions` is derived from the DVR level only. A playlist user never receives `playlist.read`,
`playlist.write`, or any administrator permission. Do not test for those names on a player token.

## 4. Player API areas

All HTTP routes in this section require `Authorization: Bearer <token>` and answer `401`/`403` as described in
section 2. The recording WebSocket (section 4.7) authenticates with an in-band `Auth` message instead; the event
WebSocket (section 4.12) accepts the Bearer token at the upgrade and an in-band `auth` message otherwise. Unless a route
says otherwise, the body of an error is a plain-text stable code (for example `feature_disabled`).

### 4.1 Active streams

#### List own streams

```bash
curl -s "$BASE_URL/api/v1/me/streams" -H "Authorization: Bearer $TOKEN" | jq .
```

```json
[
  {
    "uid": 104,
    "virtual_id": 1042,
    "group_virtual_id": 9001,
    "member_vid": 2001,
    "title": "Sports 1 HD",
    "item_type": "Live",
    "protocol": "ts",
    "shared": true,
    "session_active": true,
    "started_at": 1763678400,
    "user_agent": "ExamplePlayer/1.0"
  }
]
```

| Field              | Type    | Notes                                                                                |
|:-------------------|:--------|:-------------------------------------------------------------------------------------|
| `uid`              | integer | Stable stream id; use it in `DELETE` and the failover routes                         |
| `virtual_id`       | integer | Playlist id of the stream                                                            |
| `group_virtual_id` | integer | Present for a failover group; omitted otherwise                                      |
| `member_vid`       | integer | Active group member; omitted for non-group streams or before binding                 |
| `title`            | string  | Channel title                                                                        |
| `item_type`        | string  | `Live`, `LiveHls`, `LiveDash`, `Video`, `Series`, and other playlist item types      |
| `protocol`         | string  | `ts`, `hls`, or `dash`                                                               |
| `shared`           | bool    | The stream is shared with other viewers                                              |
| `session_active`   | bool    | `false` while the stream is preserved between HLS requests                           |
| `started_at`       | integer | Unix seconds                                                                         |
| `user_agent`       | string  | User agent of the client that opened the stream                                      |
| `session_token`    | string  | Opaque session token, omitted when the server has none; never use it as a credential |

Only the caller's streams are returned. Another user's `uid` is indistinguishable from a missing one.

#### Stop an own stream

```bash
curl -s -X DELETE "$BASE_URL/api/v1/me/streams/104" -H "Authorization: Bearer $TOKEN"
```

`204 No Content` releases the connection slot immediately. `404` means the stream does not exist or belongs to another
user. For HLS the session is terminated under a transition guard; the next segment poll receives the
`hls_session_or_lease_expired` fallback. With `player_features.event_stream: true` the caller's event socket receives
`{"type":"stream_ended","uid":104,"virtual_id":1042,"reason":"stopped"}`.

### 4.2 Failover members and manual switch

Both member routes and the switch require `player_features.source_switch: true`; otherwise they answer
`403 feature_disabled`.

#### Members of a channel, before playback

```bash
curl -s "$BASE_URL/api/v1/me/channels/9001/failover-members" \
    -H "Authorization: Bearer $TOKEN" | jq .
```

```json
{
  "group_name": "Sports 1",
  "members": [
    {
      "member_vid": 2001,
      "index": 0,
      "title": "Sports 1 HD",
      "has_archive": true,
      "current": false,
      "health": {
        "state": "ok",
        "resolution": "P1080",
        "video_codec": "H264",
        "bitrate_kbps": 5500,
        "qos_24h": 92,
        "qos_7d": 88
      }
    },
    {
      "member_vid": 2002,
      "index": 1,
      "title": "Sports 1 Backup",
      "has_archive": false,
      "current": false,
      "health": {
        "state": "ok"
      }
    }
  ]
}
```

`current_member_vid` is omitted when unknown, and each member's `current` is `false` in this variant. The response is
redacted: it never contains input names, provider ids, or upstream stream ids.

Status codes: `200`, `403 feature_disabled`, `404` (empty body) when the virtual id is not a failover group, is not in the caller's
target, or is hidden by the user's content filter.

#### Members of a running stream

```bash
curl -s "$BASE_URL/api/v1/me/streams/104/failover-members" \
    -H "Authorization: Bearer $TOKEN" | jq .
```

```json
{
  "group_name": "Sports 1",
  "current_member_vid": 2001,
  "members": [
    {
      "member_vid": 2001,
      "index": 0,
      "title": "Sports 1 HD",
      "has_archive": true,
      "current": true,
      "health": {
        "state": "ok",
        "resolution": "P1080",
        "video_codec": "H264",
        "bitrate_kbps": 5500,
        "qos_24h": 92,
        "qos_7d": 88
      }
    },
    {
      "member_vid": 2002,
      "index": 1,
      "title": "Sports 1 Backup",
      "has_archive": false,
      "current": false,
      "health": {
        "state": "ok"
      }
    }
  ]
}
```

Same shape, with `current_member_vid` set when the server knows the member and `current: true` on that member.
Status codes: `200`, `400` (plain text `not a failover group stream` or `manual switch only supported for live streams`),
`403 feature_disabled`, `404` (plain text `stream not found` or `target not found`).

#### Member health and quality

Each member in the failover list carries an optional `health` object describing real-time quality and
circuit breaker status:

| Field              | Type                              | Description                                                                                            |
|:-------------------|:----------------------------------|:-------------------------------------------------------------------------------------------------------|
| `state`            | `ok`, `degraded`, `failing`       | Operational state derived from circuit breaker and QoS metrics                                         |
| `resolution`       | string, omitted when unknown      | Video resolution tier: `Unknown`, `SD`, `P720`, `P1080`, `P1440`, `P2160`, `P4320` (`Unknown` omitted) |
| `video_codec`      | string, omitted when unknown      | Video codec: `Other`, `H264`, `H265`, `MPEG4`, `VC1`, `AV1` (`Other` omitted)                          |
| `bitrate_kbps`     | integer, omitted when unknown     | Video stream bitrate in kbps from stream probe inspection                                              |
| `qos_24h`          | integer, omitted when unknown     | 24-hour rolling Quality of Service score (`0..100`)                                                    |
| `qos_7d`           | integer, omitted when unknown     | 7-day rolling Quality of Service score (`0..100`)                                                      |
| `retry_after_secs` | integer, omitted when not failing | Remaining backoff seconds before retry (present only when `state` is `failing`)                        |

State derivation:

- **`failing`**: The member's circuit breaker is in `Open` state due to repeated consecutive connection failures.
  `retry_after_secs` carries the remaining backoff duration (rounded up to the nearest whole second) before the circuit
  transitions to half-open.
- **`degraded`**: The circuit breaker is probing (a probe request is actively being attempted), counting failures
  (failure count `n > 0`), or the QoS observation detects a failure streak (`failure_streak >= 2`).
- **`ok`**: The circuit breaker is closed, no failures are recorded, and the QoS failure streak is below 2.

QoS scores (rolling scores `qos_24h` and `qos_7d`, confidence, and successive failure streak) survive a server restart
and are restored from storage on startup. In contrast, probe media metrics (resolution, video codec, bitrate) and circuit breaker
state are maintained in memory only and reset when the server restarts.

#### Switch the source

```bash
curl -s -X POST "$BASE_URL/api/v1/me/streams/104/failover-switch" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"member_vid":2002}'
```

Request body: `{"member_vid": 2002}`.

| Status | Body / header                                                                                                                    | Meaning                                                    |
|:-------|:---------------------------------------------------------------------------------------------------------------------------------|:-----------------------------------------------------------|
| `200`  | empty                                                                                                                            | The new member binding is published                        |
| `400`  | `member not in group`                                                                                                            | `member_vid` is not a member of this group                 |
| `400`  | `not a failover group stream` / `manual switch only supported ...`                                                               | The stream is not a live group stream                      |
| `403`  | `feature_disabled`                                                                                                               | `source_switch` is off                                     |
| `404`  | `stream not found` / `target not found`                                                                                          | Unknown or foreign stream                                  |
| `409`  | `switch_in_progress`                                                                                                             | A switch is already running for this stream                |
| `409`  | `shared_stream_switch_refused`                                                                                                   | Other viewers are attached; an exclusive switch is refused |
| `429`  | `switch_cooldown`, `Retry-After: <seconds>`                                                                                      | A switch was attempted within the 2-second cooldown        |
| `502`  | `driver failure`, `playback session not active`, `origin runtime not available`, `session token missing`, `legacy switch failed` | The backend could not perform the handoff                  |

The cooldown is two seconds per stream. Two switches within it are refused by the switch itself, with the same rule
as the admin switch route. A switch within two seconds after `POST /api/v1/me/streams/{uid}/split` is refused as well.
`Retry-After` is at least `1`. A `409 shared_stream_switch_refused` means the stream is shared and another viewer would be affected;
retrying does not help until the stream is no longer shared.

An HLS client observes a successful switch on its next manifest poll: the manifest contains an `#EXT-X-DISCONTINUITY`
tag and an increased `#EXT-X-DISCONTINUITY-SEQUENCE`. Keep the playback pipeline; do not reset it.

### 4.3 Joinable shared streams and free joins

```bash
curl -s "$BASE_URL/api/v1/me/streams/joinable" -H "Authorization: Bearer $TOKEN" | jq .
```

```json
[
  {
    "virtual_id": 1042,
    "title": "Sports 1 HD",
    "logo": "https://example.com/logos/sports1.png",
    "category": "Sports",
    "protocol": "ts",
    "requires_free_slot": false
  }
]
```

The route requires `player_features.joinable_streams: true` and answers `403 feature_disabled` otherwise. It lists
running shared streams that pass the caller's content filter and item-type permissions. Usernames and viewer counts are
never included.

Besides streams of the caller's own target, the list also contains compatible streams started by **other targets**:

- The caller's target must publish the same provider item (same input and upstream URL, matched through the target's
  provider UUID index), so playing the returned `virtual_id` really joins that stream.
- Only single-source streams qualify. Group streams of another target are not listed, because group membership keys are
  target-scoped and playback would not join them.
- `virtual_id`, `title`, `logo`, and `category` always come from the caller's target; no foreign target name, provider
  id, or virtual id is exposed.
- The caller's target must share the stream protocol: `options.share_live_streams.mpeg_ts` for MPEG-TS or
  `options.share_live_streams.hls` for HLS (`source.yml`, target options).

Playback itself needs no special parameter: GET the returned `virtual_id` as usual and admission joins the running
provider stream.

`requires_free_slot` tells the client whether joining costs a connection slot:

- `false`: joining reuses the running shared stream and consumes no additional provider connection. The client may
  start playback immediately.
- `true`: the caller is at `max_connections` and has no free join left. Offer to stop another own stream with
  `DELETE /api/v1/me/streams/{uid}`, or wait.

Free shared joins: with `free_shared_joins: true`, a caller at the connection limit may still join a running shared
stream until `free_shared_joins_in_use` reaches `max_free_shared_joins`. When all paying viewers disconnect, a free join
becomes a regular connection if the caller has a spare slot; otherwise the free join ends. A recording attached to the
shared stream counts as a paying viewer.

### 4.4 Channels and EPG

#### Search channels

```bash
curl -s -G "$BASE_URL/api/v1/me/channels" \
    -H "Authorization: Bearer $TOKEN" \
    --data-urlencode "query=sports" \
    --data-urlencode "group=Sports" \
    --data-urlencode "limit=20" | jq .
```

```json
[
  {
    "virtual_id": 1042,
    "name": "Sports 1 HD",
    "group": "Sports",
    "epg_channel_id": "sports1.example.com",
    "is_group": true
  }
]
```

| Parameter | Type    | Default | Notes                                                   |
|:----------|:--------|:--------|:--------------------------------------------------------|
| `query`   | string  | none    | Case-insensitive match on name, title, group, or EPG id |
| `group`   | string  | none    | Exact group name                                        |
| `limit`   | integer | `50`    | Clamped to `1..500`                                     |

Only live channels of the caller's target are returned, after the user's content filter and cluster permissions. If the
user has no live cluster the result is `200 []`, not `403`. Failures answer `500 failed_reading_channels`.

#### EPG for one channel

```bash
curl -s -G "$BASE_URL/api/v1/me/epg" \
    -H "Authorization: Bearer $TOKEN" \
    --data-urlencode "vid=1042" \
    --data-urlencode "from=1763679600" \
    --data-urlencode "to=1763683200" | jq .
```

```json
[
  {
    "start": "2026-11-20T21:00:00+01:00",
    "start_ts": 1763679600,
    "stop": "2026-11-20T22:00:00+01:00",
    "stop_ts": 1763683200,
    "title": "Morning Match",
    "desc": "League fixture",
    "categories": ["Sports"],
    "is_live": false,
    "is_new": true
  }
]
```

`vid` is required. `from` and `to` accept Unix seconds or RFC 3339; `from` defaults to now and `to` to `from + 24h`. The
window must be non-empty and at most 48 hours. `start`/`stop` are formatted with the user's `epg_timeshift`; `start_ts`/
`stop_ts` are the unshifted Unix timestamps. A channel without an EPG id returns `200 []`.

| Status | Body                                            | Meaning                                         |
|:-------|:------------------------------------------------|:------------------------------------------------|
| `200`  | array of programme objects                      | Programmes in the window                        |
| `400`  | `invalid_from_timestamp`                        | `from` is not a timestamp or RFC 3339           |
| `400`  | `invalid_to_timestamp`                          | `to` is not a timestamp or RFC 3339             |
| `400`  | `to_must_be_after_from`                         | Empty or reversed window                        |
| `400`  | `window_exceeds_48_hours`                       | Window is longer than 48 hours                  |
| `404`  | `channel_not_found`                             | Channel is absent, filtered, or no live cluster |
| `500`  | `failed_reading_channel` / `failed_reading_epg` | Server-side read failure                        |

### 4.5 Bouquet and categories

These routes are mounted at `{web_ui_path}/api/v1/user` and require Web UI access (`ui_enabled: true` and `user_ui_enabled: true`). They are the
player-facing bouquet API. The administrator target-bouquet routes under `/api/v1/target-bouquets/*` require
`playlist.read`/`playlist.write`, which a playlist user never has, so they answer `403` for player tokens.

#### List available categories

```bash
curl -s "$BASE_URL/api/v1/user/playlist/categories" -H "Authorization: Bearer $TOKEN" | jq .
```

```json
{
  "xtream": {
    "live": ["Sports", "News"],
    "vod": ["Movies"],
    "series": ["Drama"]
  },
  "m3u": {
    "live": ["Sports", "News"],
    "vod": [],
    "series": []
  }
}
```

Clusters the user may not see are empty arrays. A target without Xtream output returns empty Xtream arrays; a target
without M3U output returns empty M3U arrays. When the user exists but the target playlist is not loaded yet, the route
returns `200` with the same empty structure. `403` means the user or target cannot be resolved, or the user is denied.

#### Read and write the bouquet

```bash
curl -s "$BASE_URL/api/v1/user/playlist/bouquet" -H "Authorization: Bearer $TOKEN" | jq .
```

```json
{
  "xtream": {
    "live": ["Sports"],
    "vod": [],
    "series": null
  },
  "m3u": null
}
```

```bash
curl -s -X POST "$BASE_URL/api/v1/user/playlist/bouquet" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"xtream":{"live":["Sports"],"vod":[],"series":null},"m3u":null}'
```

The request body accepts optional `xtream` and `m3u` objects, each with optional `live`, `vod`, and `series`
string arrays. An omitted or `null` cluster means unrestricted; an empty array for one cluster excludes that cluster
while another cluster has a selection. The route answers `200` on success, `403` when the user or target is not
resolvable or the user is denied, and `500` when the bouquet cannot be written. A GET that cannot resolve the user
answers `200 {}`; do not treat that as an empty but valid bouquet.

Bouquets only personalize category listings. They are **not** an access-control boundary: a direct stream URL outside
the bouquet still works if the content filter allows it.

### 4.6 Recordings

Recordings are commands. There is no route that lists tasks or reports the quota over REST. Every successful command
answers `204 No Content`; the authoritative state arrives on the WebSocket snapshot (section 4.7) or through the MCP
tool `me_list_recordings`.

The recording commands, rules, health, and media routes are wrapped by the global DVR gate: the `video.recording`
block must exist and `video.recording.enabled` must be `true`. With the DVR switched off, they answer `501` with
`{"error":"recording_disabled"}`. `GET /api/v1/recording/availability` is the preflight: `204` when the DVR is
usable for the caller, `403 recording_forbidden` when the caller has no recording permission, `501 recording_disabled`
when it is switched off. Recurring weekly rules share these gates and are documented in section 4.10.

#### Levels

| `player_features.recording` | Permissions granted                                                          | Capabilities                         |
|:----------------------------|:-----------------------------------------------------------------------------|:-------------------------------------|
| `none`                      | none                                                                         | No DVR access                        |
| `read`                      | `recording.read`                                                             | Play, download, snapshot             |
| `private`                   | `recording.read`, `recording.create`, `recording.manage`, `recording.delete` | Also create and manage private items |
| `shared`                    | the `private` set plus `recording.share`                                     | Also create shared recordings        |

Each level includes the previous one. `none` makes `/me/capabilities` report `recording: "none"` and
`effective_permissions: []`, and the WebSocket answers with error code `recording_forbidden`
(`{"RecordingWsError": {"code": "recording_forbidden"}}`).

#### Quota and concurrency

Two independent limits apply when a recording is admitted:

- **Quota.** Private recordings are charged to the owner's private pool. The limit is resolved in this order:
  `video.recording.quota.per_user["api:<user>"]`, then `player_features.recording_quota`, then
  `video.recording.quota.default_private`. No value means unlimited. Shared recordings are charged to the shared
  pool (`video.recording.quota.shared`). Over-quota creates answer `400 recording_quota_exceeded`.
- **Concurrency.** `player_features.max_concurrent_recordings` limits overlapping active recordings in the padded
  window. Entries that attach to an existing materialization do not consume a slot. Over-limit creates answer
  `400 recording_concurrency_exceeded`.

A create that does not fit on disk answers `507` with `recording_disk_full`.

#### Create a recording

The source is named by server-owned identifiers; a stream URL is never accepted.

```bash
curl -s -X POST "$BASE_URL/api/v1/recording/requests" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -H "Idempotency-Key: 5f1c0f1e-2b7a-4f5d-9d2e-6c3f0f1b2a44" \
    -d '{
          "source": {
            "target_id": "main",
            "virtual_id": "1042",
            "cluster": "Live",
            "input_name": ""
          },
          "program_title": "Morning Match",
          "program_start": 1763679600,
          "program_end": 1763683200,
          "pre_roll_secs": 30,
          "post_roll_secs": 60,
          "visibility": "private"
        }'
```

| Field            | Type   | Required | Notes                                                                     |
|:-----------------|:-------|:---------|:--------------------------------------------------------------------------|
| `source`         | object | yes      | `target_id` (target name), `virtual_id` (string), `cluster`, `input_name` |
| `program_title`  | string | yes      | Display title                                                             |
| `program_start`  | int    | live     | Programme start, Unix seconds                                             |
| `program_end`    | int    | live     | Programme end, Unix seconds                                               |
| `pre_roll_secs`  | int    | no       | Padding before the programme                                              |
| `post_roll_secs` | int    | no       | Padding after the programme                                               |
| `visibility`     | enum   | yes      | `private` or `shared`                                                     |
| `channel_id`     | string | no       | EPG channel id, for rows without a playlist id                            |
| `channel_name`   | string | no       | Display title that picks among streams sharing one EPG id                 |
| `epg`            | object | no       | Optional EPG episode metadata, passed through to the metadata record      |

`source.cluster` is `Live`, `Video`, or `Series`. `source.input_name` may be empty; the server then resolves the input
from the playlist item. For a `Live` source the programme window is mandatory. For `Video` and `Series` the programme
fields, pre-roll, and post-roll must be absent; the recording is a resumable transfer of the whole item.

The response is `204 No Content`. The new id is **not** in the response; read it from the next snapshot.

Idempotency: the optional `Idempotency-Key` header is scoped to the principal. The same key with the same body returns
the original `204` without creating a second recording. The same key with a different body answers
`409 recording_idempotency_conflict`. The record survives a restart and is kept for 24 hours.

Status codes:

| Status | Body code                                                                            | Meaning                                           |
|:-------|:-------------------------------------------------------------------------------------|:--------------------------------------------------|
| `204`  | ---                                                                                  | Accepted (or idempotent replay)                   |
| `400`  | `recording_invalid_source`                                                           | Source does not resolve in the caller's target    |
| `400`  | `recording_invalid_interval`                                                         | Window is empty or reversed                       |
| `400`  | `recording_padding_limit_exceeded`                                                   | Padding exceeds the configured maximum            |
| `400`  | `recording_duplicate`                                                                | Same principal, target, and window already exist  |
| `400`  | `recording_quota_exceeded`                                                           | Quota would be exceeded                           |
| `400`  | `recording_concurrency_exceeded`                                                     | Concurrent recording limit would be exceeded      |
| `400`  | `recording_invalid_state`                                                            | Task is not in a state that accepts the operation |
| `400`  | `recording_invalid_path`                                                             | Path policy violation                             |
| `403`  | `recording_forbidden`                                                                | Missing permission                                |
| `403`  | `recording_shared_not_administrator`                                                 | Shared visibility without `recording.share`       |
| `409`  | `recording_idempotency_conflict`                                                     | Same key, different body                          |
| `422`  | `recording_invalid_interval` / `recording_invalid_source` / `recording_invalid_path` | Live fields missing or on the wrong cluster       |
| `501`  | `recording_disabled`                                                                 | DVR is switched off                               |
| `507`  | `recording_disk_full`                                                                | Not enough free disk space                        |
| `500`  | `recording_persistence_failed` / `recording_io_error`                                | Server-side failure                               |

Error bodies are JSON: `{"error":"<code>"}`.

#### Edit, cancel, delete

```bash
curl -s -X PATCH "$BASE_URL/api/v1/recording/requests/3f6a1e0c-7d3a-4b5e-9c1e-0a2b6c8d9e10" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"program_start":1763679900,"program_end":1763683500,"pre_roll_secs":15}'

curl -s -X POST \
    "$BASE_URL/api/v1/recording/requests/3f6a1e0c-7d3a-4b5e-9c1e-0a2b6c8d9e10/cancel" \
    -H "Authorization: Bearer $TOKEN"

curl -s -X DELETE \
    "$BASE_URL/api/v1/recording/requests/3f6a1e0c-7d3a-4b5e-9c1e-0a2b6c8d9e10" \
    -H "Authorization: Bearer $TOKEN"
```

The edit body accepts optional `program_start`, `program_end`, `pre_roll_secs`,
`post_roll_secs`, `program_title`, `channel_id`, and `channel_name`. All three commands answer `204` and return
`404 recording_unknown` for an unknown id, `403 recording_forbidden` for a foreign entry, and
`400 recording_invalid_state` when the task state does not allow the command.

The materialization routes (`/api/v1/recording/materializations/{id}/pause|resume|retry|restart` and
`DELETE /api/v1/recording/materializations/{id}`) and `/api/v1/recording/health` are administrator-only. A playlist
user gets `403 recording_forbidden` from them and cannot address them from a snapshot. The recurring rule routes
(`/api/v1/recording/rules`) are the exception: playlist users reach them as documented in section 4.10.

#### Conflict preview

```bash
curl -s -X POST "$BASE_URL/api/v1/recording/conflicts/preview" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{
          "source": {"target_name":"main","virtual_id":"1042","input_name":""},
          "candidate": {"padded_start":1763679570,"padded_end":1763683260,
                        "pre_roll_secs":30,"post_roll_secs":60,"priority":0}
        }'
```

```json
{
  "severity": "possible_capacity_wait",
  "provider_scope": "provider_a",
  "overlap_segments": [{"start": 1763679600, "end": 1763683200, "peak_demand": 3}]
}
```

`severity` is `no_known_conflict`, `possible_capacity_wait`, or `likely_missed_window`. `provider_scope` and
`overlap_segments` are omitted when empty. The preview is advisory and writes nothing. The REST route checks
authentication and source visibility only; the MCP tool additionally requires `recording.create`. Errors use
`{"error":"<code>"}`, for example `400 recording_invalid_source`.

### 4.7 Recording snapshot over WebSocket

The WebSocket is `GET {BASE_URL}/ws`. It uses binary frames.

1. Open the connection.
2. Send one binary frame containing a single byte `0x04` (protocol version 4). The server echoes the same byte.
3. Send a binary frame containing the JSON text `{"Auth":"<token>"}`. The server answers the JSON text `"Authorized"`.
4. Send a binary frame containing the JSON text `"RecordingSnapshotRequest"`.
5. Read the binary frame with the snapshot.

Every frame after the version byte is UTF-8 JSON; only the version handshake is a single raw byte. Do not wrap
messages in a `type` field: send each message directly as an externally keyed JSON object or string (such as
`{"Auth": "<token>"}` or `"RecordingSnapshotRequest"`).

Exact snapshot message:

```json
{
  "RecordingSnapshotResponse": {
    "revision": 12,
    "available": true,
    "quota": {
      "private_used_bytes": 4294967296,
      "private_limit_bytes": 21474836480,
      "shared_used_bytes": 0,
      "shared_limit_bytes": null
    },
    "tasks": [
      {
        "id": "3f6a1e0c-7d3a-4b5e-9c1e-0a2b6c8d9e10",
        "title": "Morning Match",
        "kind": "live",
        "priority": "normal",
        "status": "completed",
        "retry_attempts": 0,
        "transferred_bytes": 4294967296,
        "restart_from_beginning_required": false,
        "owner_id": "api:alice",
        "visibility": "private",
        "channel_id": "sports1.example.com",
        "channel_name": "Sports 1 HD",
        "program_title": "Morning Match",
        "program_start": 1763679600,
        "program_end": 1763683200,
        "scheduled_start": 1763679570,
        "scheduled_end": 1763683260,
        "pre_roll_secs": 30,
        "post_roll_secs": 60,
        "completed_at": 1763683300,
        "filename": "Morning-Match-2026-11-20.ts",
        "allowed_actions": {
          "pause": false,
          "resume": false,
          "cancel": false,
          "retry": false,
          "edit": false,
          "remove": true
        }
      }
    ]
  }
}
```

| Field                       | Notes                                                                                                                                    |
|:----------------------------|:-----------------------------------------------------------------------------------------------------------------------------------------|
| `revision`                  | Monotonic queue revision; a gap means you missed a change                                                                                |
| `available`                 | The DVR can accept work                                                                                                                  |
| `quota.private_limit_bytes` | Resolved private limit; `null` means unlimited                                                                                           |
| `quota.shared_limit_bytes`  | Resolved shared limit; `null` means unlimited                                                                                            |
| `tasks[].id`                | Request id; use it for edit, cancel, delete, and media URLs                                                                              |
| `tasks[].kind`              | `live`, `vod`, or `series`                                                                                                               |
| `tasks[].priority`          | `high`, `normal`, or `background`                                                                                                        |
| `tasks[].status`            | `scheduled`, `queued`, `waiting_for_capacity`, `retry_waiting`, `running`, `paused`, `cancelling`, `completed`, `failed`, or `cancelled` |
| `tasks[].owner_id`          | Present only for the owner's own entries                                                                                                 |
| `tasks[].allowed_actions`   | Server-computed controls; render exactly these                                                                                           |

All other task fields are optional and omitted when absent. The snapshot is owner-filtered: a private entry of another
user never appears, and an API user sees shared entries only from its own target and inside its content filter.

Failure frames:

- `"Unauthorized"`: no token, or an invalid or expired token.
- `{"RecordingWsError":{"code":"recording_token_refresh_required"}}`: the token schema is stale; refresh the token.
- `{"RecordingWsError":{"code":"recording_disabled"}}`: the DVR is switched off.
- `{"RecordingWsError":{"code":"recording_forbidden"}}`: the caller has no `recording.read`.

### 4.8 Recording media

The `id` is the same request id that appears in the snapshot.

```bash
# Playback; HTTP Range is supported
curl -s -D - -H "Authorization: Bearer $TOKEN" \
    -H "Range: bytes=0-1048575" \
    "$BASE_URL/api/v1/library/recording/playback/3f6a1e0c-7d3a-4b5e-9c1e-0a2b6c8d9e10" \
    --output segment.bin

# Download
curl -s -OJ -H "Authorization: Bearer $TOKEN" \
    "$BASE_URL/api/v1/library/recording/download/3f6a1e0c-7d3a-4b5e-9c1e-0a2b6c8d9e10"
```

`playback` and `download` accept a single RFC 7233 byte range (`bytes=START-END`, `bytes=START-`, `bytes=-N`). A
satisfiable range answers `206 Partial Content` with `Content-Range` and `Content-Length`. An unsatisfiable range answers
`416 Range Not Satisfiable` with `Content-Range: bytes */<total>`. Without a range the whole file is served with `200`.
Both routes set `Accept-Ranges: bytes` and `Content-Type: application/octet-stream`; `download` also sets a
`Content-Disposition: attachment` with an ASCII fallback name and a UTF-8 `filename*`.

`thumbnail` is not implemented in this build: after an access check it answers `404` with an empty body. Do not retry it
in a loop.

| Status | Body                                                               | Meaning                                                       |
|:-------|:-------------------------------------------------------------------|:--------------------------------------------------------------|
| `200`  | media                                                              | Full file                                                     |
| `206`  | media                                                              | Requested range                                               |
| `401`  | `invalid token`                                                    | Missing or invalid token; `X-Token-Refresh` when schema stale |
| `403`  | `{"error":"recording_missing_permission"}` / `recording_forbidden` | Not allowed for this recording                                |
| `404`  | `{"error":"recording_not_found"}`                                  | Unknown id, or the file is gone                               |
| `409`  | `{"error":"recording_in_deleting_state"}`                          | The recording is being deleted                                |
| `416`  | empty                                                              | Unsatisfiable `Range`                                         |
| `501`  | `{"error":"recording_disabled"}`                                   | DVR is switched off                                           |

### 4.9 MCP player tools

The MCP endpoint is `POST {BASE_URL}/mcp`, stateless JSON-RPC 2.0 over `application/json`. It is mounted only when
`mcp.enabled: true`. Supported protocol versions: `2025-06-18`, `2025-03-26`, `2024-11-05`.

Authentication is the same Bearer token. A missing or invalid token is a transport-level `401`. An `Origin` header, when
present, must match the request `Host` or an entry in `mcp.allowed_origins`; otherwise the answer is `403`.

Tools follow `mcp.player_tools` and the caller's features:

- `off`: no player tools.
- `read`: read-only player tools whose feature is effective.
- `write`: read and write player tools whose feature is effective.

When Web UI authentication is disabled, the endpoint is forced read-only and no player tools are offered.

Administrator tools are never listed for an API user, and calling one returns `-32602` with `Unknown tool: <name>`.

```json
{"jsonrpc":"2.0","id":0,"method":"initialize",
 "params":{"protocolVersion":"2025-06-18","capabilities":{},
           "clientInfo":{"name":"example-player","version":"1.0.0"}}}
```

```json
{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}
```

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "tools": [
      {
        "name": "me_capabilities",
        "description": "Discovers effective capabilities and limits of the authenticated playlist user (connection limits, active connections, recording level, quota, and permitted features).",
        "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false},
        "annotations": {"readOnlyHint": true, "openWorldHint": false}
      }
    ]
  }
}
```

```json
{"jsonrpc":"2.0","id":2,"method":"tools/call",
 "params":{"name":"me_search_channels","arguments":{"query":"NoSuchChannel"}}}
```

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "result": {
    "content": [
      {
        "type": "text",
        "text": "[]"
      }
    ],
    "isError": false
  }
}
```

| Tool                     | Kind  | Underlying route                          | Feature gate       | Arguments                                                                                         |
|:-------------------------|:------|:------------------------------------------|:-------------------|:--------------------------------------------------------------------------------------------------|
| `me_capabilities`        | read  | `GET /me/capabilities`                    | none               | none                                                                                              |
| `me_search_channels`     | read  | `GET /me/channels`                        | none               | optional `query`, `group`, `limit`                                                                |
| `me_get_epg`             | read  | `GET /me/epg`                             | none               | `vid`; optional `from`, `to`                                                                      |
| `me_list_streams`        | read  | `GET /me/streams`                         | none               | none                                                                                              |
| `me_stop_stream`         | write | `DELETE /me/streams/{uid}`                | none               | `uid`                                                                                             |
| `me_list_sources`        | read  | `GET /me/channels/{vid}/failover-members` | `source_switch`    | `vid`                                                                                             |
| `me_switch_source`       | write | `POST /me/streams/{uid}/failover-switch`  | `source_switch`    | `uid`, `member_vid`                                                                               |
| `me_split_stream`        | write | `POST /me/streams/{uid}/split`            | `source_switch`    | `uid`                                                                                             |
| `me_list_joinable`       | read  | `GET /me/streams/joinable`                | `joinable_streams` | none                                                                                              |
| `me_list_recordings`     | read  | recording snapshot (internal)             | `recording.read`   | none                                                                                              |
| `me_record_program`      | write | `POST /recording/requests`                | `recording.create` | `vid`, `program_start`, `program_end`, `title`; optional padding and `visibility`                 |
| `me_cancel_recording`    | write | `POST /recording/requests/{id}/cancel`    | `recording.create` | `id`                                                                                              |
| `me_delete_recording`    | write | `DELETE /recording/requests/{id}`         | `recording.create` | `id`                                                                                              |
| `me_recording_conflicts` | read  | `POST /recording/conflicts/preview`       | `recording.create` | `vid`, `program_start`, `program_end`; optional padding, `priority`                               |
| `me_list_rules`          | read  | `GET /recording/rules`                    | `recording.read`   | none                                                                                              |
| `me_create_rule`         | write | `POST /recording/rules`                   | `recording.create` | `vid`, `weekday`, `local_start_time`, `duration_secs`; optional `timezone`, `visibility`, padding |
| `me_delete_rule`         | write | `DELETE /recording/rules/{id}`            | `recording.create` | `id`; optional `future` (`retain` default)                                                        |

`me_split_stream` returns the route response with the password in the reconnect `url` replaced by `***`, so an
assistant can hand the URL to a client without the credential appearing in the transcript.

The tool `me_record_program` builds the REST body itself: it fills `source.target_id` with the caller's target name,
`source.virtual_id` with `vid`, `source.cluster` with `Live`, and `source.input_name` with an empty string. It also sends
an `Idempotency-Key` derived from the caller identity and the arguments, so repeating the same call does not create a
duplicate. `me_recording_conflicts` is a read tool by kind, but it requires the write capability because it previews a
create.

Tool failures are returned as `isError: true` with the REST status and code in the text; they are not JSON-RPC errors.
Unknown or hidden tool names, invalid arguments, and malformed requests are JSON-RPC errors (`-32602`, `-32600`,
`-32700`); an unknown method is `-32601`. Notifications (no `id`) receive `202 Accepted` with no body.

### 4.10 Recurring recording rules

Rules schedule a live recording on every matching occurrence. Unlike one-off recording requests they live on the
recording routes, not under `/me`, but an API user reaches them with the same Bearer token:

```text
GET    /api/v1/recording/rules
POST   /api/v1/recording/rules
PATCH  /api/v1/recording/rules/{id}
DELETE /api/v1/recording/rules/{id}?future=retain|cancel
```

The DVR gate applies: with `video.recording.enabled: false` every rule route answers `501 recording_disabled`.
Otherwise the recording level decides what the caller may do:

- `read`: list rules (own rules plus shared rules of the own target that pass the content filter).
- `private`: additionally create, edit, and delete own private rules.
- `shared`: additionally create, edit, and delete own shared rules.
- `none`: `403 recording_forbidden`.

Another user's private rule is invisible in the list, and every mutation of a foreign rule answers
`403 recording_rule_not_owner`. Changing the target of an existing rule is impossible; a rule keeps the source it was
created with.

#### List rules

```bash
curl -s "$BASE_URL/api/v1/recording/rules" -H "Authorization: Bearer $TOKEN" | jq .
```

Response (`200 OK`) is an array; `revision` is the queue revision observed while the list was assembled:

```json
[
  {
    "revision": 12,
    "id": "rule-1763678400-ab12cd34",
    "owner_id": "api:alice",
    "own": true,
    "visibility": "private",
    "enabled": true,
    "source": {
      "target_id": "main",
      "virtual_id": "1042",
      "is_group": false
    },
    "channel_id": "sports1.example.com",
    "body": {
      "kind": "weekly_timeslot",
      "weekday": 3,
      "local_start_time": "20:00",
      "duration_secs": 3600,
      "timezone": "Europe/Berlin"
    },
    "pre_roll_secs": 0,
    "post_roll_secs": 0,
    "created_at": 1763678400,
    "updated_at": 1763678400
  }
]
```

`own` indicates whether the rule belongs to the caller. `owner_id` is included only for the caller's own rules
(omitted for shared rules of other users). For playlist users, `source` contains `target_id`, `virtual_id`, and
`is_group`; provider internals (`input_name`, `provider_id`) are omitted. Administrators receive the full rule shape
instead, including `source.input_name` and `source.provider_id`. `channel_id` and the group flag are server-resolved
and read-only. A shared rule of the own target is listed only when the source channel passes the caller's content
filter.

#### Create a rule

```bash
curl -s -X POST "$BASE_URL/api/v1/recording/rules" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{
          "virtual_id": "1042",
          "body": {
            "kind": "weekly_timeslot",
            "weekday": 3,
            "local_start_time": "20:00",
            "duration_secs": 3600,
            "timezone": "Europe/Berlin"
          },
          "pre_roll_secs": 0,
          "post_roll_secs": 60,
          "visibility": "private"
        }'
```

| Field            | Type   | Required | Notes                                                             |
|:-----------------|:-------|:---------|:------------------------------------------------------------------|
| `target_id`      | string | no       | Own target name or numeric id; defaults to the caller's target    |
| `virtual_id`     | string | yes      | Playlist id of the channel, or an EPG channel id                  |
| `input_name`     | string | no       | Playlist users: ignored (server-derived); Web UI: empty resolves  |
| `channel_id`     | string | no       | Stable EPG channel id used as matching fallback                   |
| `body`           | object | yes      | `weekly_timeslot` (below); `new_episode` is rejected              |
| `visibility`     | enum   | no       | `private` (default) or `shared`; `shared` needs `recording.share` |
| `pre_roll_secs`  | int    | no       | At most `900`                                                     |
| `post_roll_secs` | int    | no       | At most `1800`                                                    |

For playlist users the server stores `target_id` as the resolved target name and derives `source.input_name` from
the playlist; a client-supplied `input_name` is ignored, and `source.provider_id` is never exposed.

The weekly body is `{"kind":"weekly_timeslot","weekday":3,"local_start_time":"20:00","duration_secs":3600,"timezone":"Europe/Berlin"}`.
`weekday` is ISO 8601 (`1` = Monday .. `7` = Sunday), `local_start_time` is exactly `HH:MM` in 24-hour form, and
`timezone` is an IANA name. A successful create answers `200 OK` with the same rule shape as the list entry.

Rules are limited per user by `player_features.max_recording_rules` (default `10`, counting private and shared rules).
The N+1st create answers `400 recording_rule_limit_exceeded`. Only users with recording level `private` or `shared`
can create rules at all.

The scheduler re-checks the owner before each materialization: a disabled, banned, pending, or expired account, a
recording level below `private`, or a rule whose target is no longer the user's target is skipped silently. The rule
stays stored and resumes when the user is active again. Deleting a playlist user removes every rule they own, together
with their watch progress.

#### Edit a rule

```bash
curl -s -X PATCH "$BASE_URL/api/v1/recording/rules/rule-1763678400-ab12cd34" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"pre_roll_secs":120,"enabled":false}'
```

Editable fields: `body`, `visibility`, `enabled`, `channel_id`, `clear_channel_id`, `pre_roll_secs`, `post_roll_secs`.
The source, owner, and target are immutable. A `private`-only user cannot promote a rule to `shared`
(`403 recording_shared_not_administrator`). The answer is `200 OK` with the updated rule, in the same shape as the list
entry (redacted for playlist users, the full rule for administrators).

#### Delete a rule

```bash
curl -s -X DELETE "$BASE_URL/api/v1/recording/rules/rule-1763678400-ab12cd34?future=retain" \
    -H "Authorization: Bearer $TOKEN"
```

`future` is **required** on REST: `retain` keeps already scheduled recordings, `cancel` cancels upcoming inactive
occurrences and removes the rule. A missing parameter answers `400 recording_rule_invalid_future`. Success is
`204 No Content`. If cancelling the queue succeeds but deleting the rule fails, the answer is
`500 recording_rule_partial_operation`; re-read the rule list before retrying.

#### Rule errors

| Status | Code                                     | Meaning                                            |
|:-------|:-----------------------------------------|:---------------------------------------------------|
| `400`  | `recording_invalid_source`               | Channel is not in the own target or content filter |
| `400`  | `recording_rule_invalid`                 | Malformed rule body, source, timezone, or padding  |
| `400`  | `recording_rule_invalid_future`          | `future` missing or not `retain`/`cancel`          |
| `400`  | `recording_rule_limit_exceeded`          | `max_recording_rules` reached                      |
| `400`  | `recording_rule_new_episode_unsupported` | `new_episode` body is not implemented              |
| `403`  | `recording_forbidden`                    | Recording level `none` or no `recording.manage`    |
| `403`  | `recording_rule_not_owner`               | The rule belongs to another user                   |
| `403`  | `recording_shared_not_administrator`     | Shared visibility without `recording.share`        |
| `404`  | `recording_rule_unknown`                 | Rule id does not exist                             |
| `500`  | `recording_persistence_failed`           | Rule store could not be written                    |
| `501`  | `recording_disabled`                     | DVR is switched off                                |

The MCP tools `me_list_rules` (read), `me_create_rule` and `me_delete_rule` (write) call the same routes. The MCP
create tool builds the weekly body from `vid`, `weekday`, `local_start_time`, `duration_secs`, and `timezone`. The MCP
delete tool defaults `future` to `retain`, so agents do not have to pass it.

### 4.11 Watch progress

With `player_features.watch_progress: true` a user can store playback position per media item and resume later. The
gate answers `403 feature_disabled` when the feature is off.

```text
GET    /api/v1/me/progress
DELETE /api/v1/me/progress
GET    /api/v1/me/progress/{kind}/{item_id}
PUT    /api/v1/me/progress/{kind}/{item_id}
DELETE /api/v1/me/progress/{kind}/{item_id}
```

`kind` is `movie`, `episode`, or `recording`. `item_id` is the Xtream stream id the player actually plays: the movie
id, the episode id from `get_series_info`, or the recording's persisted `vod_id`. It must be greater than `0`; `0`
answers `400 invalid_item_id`. Progress is purely personal data: no playlist lookup happens, so ids are accepted without
a visibility check and never leak another user's library.

#### Save progress

```bash
curl -s -X PUT "$BASE_URL/api/v1/me/progress/episode/4711" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"position_secs":1500,"duration_secs":3000,"series_id":88}'
```

| Field           | Type | Required | Notes                                                     |
|:----------------|:-----|:---------|:----------------------------------------------------------|
| `position_secs` | int  | yes      | Current playback position in seconds                      |
| `duration_secs` | int  | no       | Total length; enables the automatic `watched` calculation |
| `watched`       | bool | no       | Explicit override; wins over the automatic calculation    |
| `series_id`     | int  | no       | Series id, so a client can group episodes                 |

Success is `204 No Content`. The request body limit is 4 KiB (`413` above it). A missing JSON content type answers `415`,
malformed JSON `400`, and a well-formed body with the wrong field types `422`. If the user was deleted while the request
was being processed, the answer is `401`. Omitted `duration_secs` and `series_id` keep the stored values; omit them to
update only the position.

`watched` is derived when the client does not send it:

- an explicit `watched` always wins;
- an item becomes watched at 90 % of `duration_secs` (duration must be greater than `0`);
- otherwise an already watched item stays watched while `position_secs >= 60` and drops back to unwatched below 60 s.

#### Read progress

```bash
curl -s "$BASE_URL/api/v1/me/progress/episode/4711" -H "Authorization: Bearer $TOKEN" | jq .
curl -s "$BASE_URL/api/v1/me/progress?kind=episode&watched=false&limit=20" \
    -H "Authorization: Bearer $TOKEN" | jq .
```

```json
{
  "kind": "episode",
  "item_id": 4711,
  "position_secs": 1500,
  "duration_secs": 3000,
  "watched": false,
  "series_id": 88,
  "updated_at": 1763678400
}
```

The list returns an array of the same objects, newest `updated_at` first. Query parameters: `kind` (filter), `watched`
(`true`/`false` filter), and `limit` (default `50`, clamped to `1..=200`). A single unknown item answers `404` with an
empty body.

#### Delete progress

`DELETE /api/v1/me/progress/{kind}/{item_id}` removes one entry; `DELETE /api/v1/me/progress` removes all entries of
the caller. Both answer `204 No Content`, also when nothing existed.

#### Storage and limits

Progress is stored per user in `{config_path}/user_settings/api/<encoded-username>.json`, written atomically under the
file lock. It is capped at 500 entries per user and 180 days of retention; the oldest entries are dropped first. The
file is removed together with the user, like the user settings; deleting a playlist user also deletes all of their
recording rules. There is no server-side merge across devices beyond last-write-wins per item.

#### Recommended client flow: resume watching

1. On start, request `GET /api/v1/me/capabilities`; continue only when `watch_progress` is `true`.
2. When the user opens a movie, episode, or recording, call `GET /api/v1/me/progress/{kind}/{item_id}`; on `404` start
   at `0`.
3. Seek to `position_secs` (offer "resume" when it is below `duration_secs - 60`).
4. Send `PUT` every ~30 seconds and once when playback pauses or ends. Include `duration_secs` so the server can set
   `watched` automatically.
5. Wipe an item with `DELETE .../{kind}/{item_id}` when the user chooses "start over"; use the collection `DELETE` for
   a logout-repair action.

### 4.12 Player event stream

With `player_features.event_stream: true` a client can subscribe to a personal JSON event stream. This is a WebSocket
at `GET {BASE_URL}/api/v1/me/events`; it is **not** the binary `/ws` recording protocol.

#### Authentication

Two forms are accepted:

1. `Authorization: Bearer <token>` on the WebSocket upgrade. An invalid token answers `401` before the upgrade, a
   disabled feature `403`, and a fifth concurrent socket `429` (empty body).
2. No header: the server accepts the upgrade and waits up to 5 seconds for the first **text** frame:

```json
{"type":"auth","token":"<token>"}
```

There is no token in the query string; tokens in URLs end up in access logs. On success the server sends the
`connected` event. On failure it sends an `error` frame and closes with code `1008`:

| Frame                                            | Close reason           | Meaning                               |
|:-------------------------------------------------|:-----------------------|:--------------------------------------|
| `{"type":"error","code":"unauthorized"}`         | `Unauthorized`         | Invalid or expired token              |
| `{"type":"error","code":"feature_disabled"}`     | `Feature disabled`     | `event_stream` is off                 |
| `{"type":"error","code":"too_many_connections"}` | `Too many connections` | Four sockets are already open         |
| ---                                              | `Auth timeout`         | No text frame within 5 seconds        |
| ---                                              | `Expected text frame`  | First frame was not text              |
| ---                                              | `Invalid auth message` | First frame was not an `auth` message |

A socket is closed with `1008` and the reason `Feature disabled` or `Permission denied` when the user loses the
feature or the account while connected, and with `User deleted` when the user disappears.

#### Frames

Every frame is JSON text with a `type` tag. The first frame is always:

```json
{"type":"connected","server_time":1763678400}
```

| Event                      | Frame                                                                                                     |
|:---------------------------|:----------------------------------------------------------------------------------------------------------|
| `stream_ended`             | `{"type":"stream_ended","uid":104,"virtual_id":1042,"reason":"kicked"}`                                   |
| `failover_member_switched` | `{"type":"failover_member_switched","group_virtual_id":9001,"from_member_vid":2001,"to_member_vid":2002}` |
| `recording_finished`       | `{"type":"recording_finished","id":"3f6a1e0c-...","state":"completed"}`                                   |
| `connection_denied`        | `{"type":"connection_denied","max_connections":2}`                                                        |
| `resync_required`          | `{"type":"resync_required"}`                                                                              |
| `error`                    | `{"type":"error","code":"unauthorized"}`                                                                  |

- `stream_ended`: one of the caller's streams ended; `reason` is `kicked` (replaced by another device), `preempted`
  (higher priority or capacity), or `stopped` (the caller ended it with `DELETE /api/v1/me/streams/{uid}`).
- `failover_member_switched`: an own active stream on a failover group moved to another member. Only target virtual ids
  are sent. Requires `source_switch`.
- `recording_finished`: a recording of the caller or a visible shared recording reached `completed` or `failed`.
  Requires recording level `read` or higher; the visibility rule is the same as for the recording snapshot.
- `connection_denied`: one of the caller's connection attempts was refused; `max_connections` is the limit that applied.
- `resync_required`: the client fell behind the broadcast buffer. Refetch `/me/streams`, the recording snapshot, and
  the progress list you display, then continue.

Events are filtered per user; another user's stream end, switch, or private recording never appears. The server sends a
WebSocket Ping every 30 seconds and answers a client Ping with Pong; a client that stops reading is dropped after the
5-second send timeout. At most four sockets per user are open at once.

#### JavaScript example

The client opens the socket without a header and authenticates with the first message. Browsers cannot set an
`Authorization` header on a WebSocket, so this is the portable form.

```js
const token = localStorage.getItem("tuliprox_token");
const ws = new WebSocket(`${BASE_URL.replace(/^http/, "ws")}/api/v1/me/events`);

ws.addEventListener("open", () => {
  ws.send(JSON.stringify({ type: "auth", token }));
});

ws.addEventListener("message", (message) => {
  const event = JSON.parse(message.data);
  switch (event.type) {
    case "connected":
      console.log("event stream ready at", event.server_time);
      break;
    case "stream_ended":
      // The stream with this uid is gone; stop the player and re-list /me/streams.
      handleOwnStreamEnded(event.uid, event.reason);
      break;
    case "recording_finished":
      refreshRecordings();
      break;
    case "resync_required":
      refreshAllPlayerState();
      break;
    case "error":
      console.warn("event stream error", event.code);
      break;
  }
});

ws.addEventListener("close", (close) => {
  // Reconnect with exponential backoff; 1008 means auth or policy, so sign in again first.
  scheduleReconnect(close.code === 1008);
});
```

The stream is best-effort: it carries no replay buffer for slow clients. Treat `resync_required` and a reconnect as
"state may have changed" and refresh from REST rather than trying to reconstruct missed frames.

### 4.13 Split a shared stream

A shared stream costs one provider connection for several viewers. A user with `source_switch` can split an own shared
stream off into a dedicated connection, for example when the shared provider connection is flaky or when the user is
the last paying viewer and wants to keep watching.

```bash
curl -s -X POST "$BASE_URL/api/v1/me/streams/104/split" \
    -H "Authorization: Bearer $TOKEN" | jq .
```

```json
{
  "reconnect_required": true,
  "url": "/live/alice/example-password/1042.ts?dedicated=1"
}
```

The route requires `player_features.source_switch: true` (`403 feature_disabled` otherwise) and works only when:

- `uid` belongs to the caller and exists (`404 stream not found` otherwise);
- the stream is a live MPEG-TS stream; catch-up (timeshift), VOD, series, HLS, and DASH answer
  `409 split_unsupported_protocol`;
- the stream is shared (`409 stream_not_shared` otherwise);
- more than one viewer is attached (`409 stream_not_shared` when the caller is alone);
- the shared two-second switch cooldown has passed (`429 switch_cooldown` with `Retry-After`).

No provider connection is reserved by the call. The returned `url` is server-relative: prepend `scheme://host:port`,
not `web_ui_path`, because playback routes live at the server root. For a target with an Xtream output it is
`/live/<user>/<pass>/<vid>.ts?dedicated=1`; otherwise it is the M3U stream route
`/m3u-stream/live/<user>/<pass>/<vid>.ts?dedicated=1`. The marker `dedicated=1` is honoured only for users with
`source_switch`; for everyone else it is ignored, and playback joins or registers the shared stream as usual. Two
splits within two seconds, or a failover member switch within two seconds after a split, answer
`429 switch_cooldown`.

#### Recommended client flow: split then reconnect

1. Call `POST /api/v1/me/streams/{uid}/split` and read `url`.
2. Close the old playback connection first. The split does not pre-reserve capacity, so leaving the old stream open
   keeps the connection count high and admission may refuse the new one.
3. Open the returned URL with the same playlist credentials. It creates a new dedicated provider connection; existing
   admission rules (connection limit, priority, fallback slates) apply.
4. Re-read `GET /api/v1/me/streams` for the new `uid`; the old `uid` is gone.
5. On `429`, wait for `Retry-After` seconds and retry the split once. On `409 split_unsupported_protocol`, keep the
   shared playback instead of falling back to a second connection.

## 5. Playback contracts

The player API manages sessions; playback itself uses the Xtream Codes and M3U endpoints. Both are de-facto standards,
so this section documents the URL shapes and tuliprox-specific behaviour, not every format field.

### Xtream Codes API

| Route                                                             | Purpose                                |
|:------------------------------------------------------------------|:---------------------------------------|
| `GET\|POST /player_api.php`                                       | Account info, categories, streams, EPG |
| `GET\|POST /panel_api.php`                                        | Compatibility alias                    |
| `GET\|POST /xtream`                                               | Compatibility alias                    |
| `GET /live/{user}/{pass}/{stream_id}.{ext}`                       | Live stream                            |
| `GET /movie/{user}/{pass}/{stream_id}.{ext}`                      | VOD stream                             |
| `GET /series/{user}/{pass}/{stream_id}.{ext}`                     | Series episode                         |
| `GET /{user}/{pass}/{stream_id}.{ext}`                            | Live stream, alternate form            |
| `GET /timeshift/{user}/{pass}/{duration}/{start}/{stream_id}`     | Catch-up stream                        |
| `GET /timeshift.php` / `/streaming/timeshift.php`                 | Catch-up query form                    |
| `GET /resource/live\|movie\|series/{user}/{pass}/{id}/{resource}` | Logos, EPG icons, and other resources  |
| `GET /token/{token}/{target_id}/{cluster}/{stream_id}`            | Token-addressed stream                 |

Handled `action` values on `player_api.php`: `get_account_info`, `get_live_categories`, `get_vod_categories`,
`get_series_categories`, `get_live_streams`, `get_vod_streams`, `get_series`, `get_series_info`, `get_vod_info`,
`get_epg`, `get_short_epg`, and `get_simple_data_table`.

```bash
curl -s "$BASE_URL/player_api.php?username=alice&password=example-password&action=get_live_categories"
curl -s "$BASE_URL/live/alice/example-password/1042.ts"
```

`{ext}` selects the container. Use `.ts` for MPEG-TS and `.m3u8` for HLS; the playlist entry already contains the
correct extension. Playlist credentials can also be passed as `?token=<token>` instead of user/password.

`stream_id` is the virtual id from the playlist. `get_series_info` takes `series_id`, `get_vod_info` takes `vod_id`, and
`get_epg`/`get_short_epg`/`get_simple_data_table` take `stream_id`.

### Recordings as Xtream VOD

A playlist user whose effective `player_features.recording` is `read`, `private`, or `shared` also accesses their
completed recordings through the standard Xtream VOD endpoints (`Feature F2`), alongside regular movies.

#### Prerequisites and gate

Recording VOD behavior is active when all of the following prerequisites are met:

1. Global DVR is enabled: `recording.enabled: true` in `config.yml`.
2. The user authenticates as an API proxy user defined in `api_proxy.yml` (the internal Web UI user is excluded).
3. The user's effective recording level is `read`, `private`, or `shared`.
4. The user's plan or credentials allow the `Video` output cluster (`output_clusters`).
5. The assigned target has an Xtream output in `source.yml`.

When any requirement is not met, the endpoints behave exactly as without DVR; no recording queue state is accessed.
The `skip_vod` input flag does not hide recordings.

#### Invariants and behavior

- **Synthetic category:** `get_vod_categories` appends the category `Recordings` with `category_id: "2000000000"`.
  If an upstream provider category already has ID `2000000000`, the provider category wins in both `get_vod_categories`
  and `get_vod_streams`, and no recordings are served.
- **Fast-path listing:** `get_vod_streams?category_id=2000000000` returns only visible completed recordings directly
  without scanning or parsing the upstream target playlist (provided the provider has no category `2000000000`).
- **Unfiltered listing:** `get_vod_streams` without `category_id` streams target playlist VOD movies first and appends
  visible completed recordings at the end.
- **Completed recordings only:** Only recordings with state `completed` are listed. A recording whose file was removed
  externally is still listed until the queue notices; playback of such a recording then answers like an unknown movie ID.
  Scheduled, running, paused, cancelling, failed, or cancelled tasks are neither listed nor playable.
- **Visibility rules:** Own private recordings are always visible to their creator. Shared recordings are visible only
  when the recording's source target matches the user's assigned target and the recorded channel passes the user's
  content and item-type filters.
- **Persistent stream IDs:** Each recording is assigned a persistent VOD stream ID sequentially starting at `1000000000`,
  stored with the recording task in the queue. Only IDs `>= 1000000000` are looked up as recordings. IDs are stable
  across restarts and never reused.
- **Lookup and fallback behavior:** An invisible or unknown recording ID answers like any unknown movie ID: playback
  returns the normal fallback response (e.g. `X-Tuliprox-Fallback: channel_unavailable`), NOT a bare `404`; `get_vod_info`
  for such IDs answers the normal empty info.
- **Playlist item precedence:** Target playlist items always take precedence over recordings. If a playlist movie has the
  same ID as a recording, the playlist item is returned and played; recordings are consulted only when no playlist item matches.
- **Connection accounting and throttling:** Playback via `/movie/{user}/{pass}/{id}.{ext}` counts as one user connection like
  a library file. It occupies 1 connection slot against the user's `max_connections` for the duration of the stream.
  Recording playback records no provider for the session (it plays from local storage) and consumes 0 provider connections.
  If `max_connections` is exhausted, playback is rejected with `502 Bad Gateway` (or custom error status),
  `X-Tuliprox-Fallback: user_connections_exhausted`, and `X-Tuliprox-Rejection: admission_rejected`.
  When stream throttling is configured (`throttle_kbps`), recording playback is throttled like library files.
- **Range and containers:** Playback supports RFC 7233 byte ranges (`Range: bytes=START-END`), returning `206 Partial Content`.
  The `Content-Type` header matches the recording file container: `video/mp2t` (`.ts`), `video/mp4` (`.mp4`), or
  `video/x-matroska` (`.mkv`).

#### Examples

##### 1. List VOD categories

```bash
curl -s "$BASE_URL/player_api.php?username=alice&password=example-password&action=get_vod_categories" | jq .
```

Response (`200 OK`):

```json
[
  {
    "category_id": "1",
    "category_name": "Action Movies",
    "parent_id": 0
  },
  {
    "category_id": "2000000000",
    "category_name": "Recordings",
    "parent_id": 0
  }
]
```

##### 2. List recordings in the VOD category

```bash
curl -s "$BASE_URL/player_api.php?username=alice&password=example-password&action=get_vod_streams&category_id=2000000000" | jq .
```

Response (`200 OK`):

```json
[
  {
    "num": 0,
    "name": "Evening News",
    "stream_type": "movie",
    "stream_id": 1000000000,
    "stream_icon": "",
    "rating": "0",
    "rating_5based": 0,
    "added": "1700000000",
    "is_adult": 0,
    "category_id": "2000000000",
    "category_ids": [
      2000000000
    ],
    "container_extension": "ts",
    "custom_sid": null,
    "direct_source": ""
  }
]
```

`added` is the Unix time of the recording completion as a string (e.g. `"1700000000"`).

##### 3. Query VOD item information

```bash
curl -s "$BASE_URL/player_api.php?username=alice&password=example-password&action=get_vod_info&vod_id=1000000000" | jq .
```

Response (`200 OK`):

```json
{
  "info": {
    "name": "Evening News",
    "duration": "01:00:00",
    "duration_secs": "3600"
  },
  "movie_data": {
    "stream_id": 1000000000,
    "name": "Evening News",
    "added": "1700000000",
    "category_id": "2000000000",
    "category_ids": [
      2000000000
    ],
    "container_extension": "ts",
    "custom_sid": null
  }
}
```

`duration` is `HH:MM:SS` (e.g. `"01:00:00"`), `duration_secs` is the number of seconds as a string (e.g. `"3600"`), and
`added` is the Unix time of the recording completion as a string (e.g. `"1700000000"`).

##### 4. Playback with HTTP Range request

```bash
curl -s -D - -H "Range: bytes=0-1048575" \
    "$BASE_URL/movie/alice/example-password/1000000000.ts" \
    --output part.ts
```

Response headers (`206 Partial Content`):

```http
HTTP/1.1 206 Partial Content
Content-Type: video/mp2t
Content-Range: bytes 0-1048575/524288000
Content-Length: 1048576
Accept-Ranges: bytes
Connection: keep-alive
```

##### 5. Connection limit rejection

When the user has already reached `max_connections` (for example, two active streams running):

```bash
curl -s -D - -o /dev/null "$BASE_URL/movie/alice/example-password/1000000000.ts"
```

Response headers (`502 Bad Gateway`):

```http
HTTP/1.1 502 Bad Gateway
Content-Length: 0
Cache-Control: no-store
X-Tuliprox-Fallback: user_connections_exhausted
X-Tuliprox-Rejection: admission_rejected
Connection: close
```

### M3U

| Route                                                    | Purpose                                         |
|:---------------------------------------------------------|:------------------------------------------------|
| `GET\|POST /get.php`                                     | Playlist download                               |
| `GET\|POST /apiget` / `/m3u`                             | Compatibility aliases                           |
| `GET /m3u-stream/live/{user}/{pass}/{stream_id}.{ext}`   | Live stream                                     |
| `GET /m3u-stream/movie/{user}/{pass}/{stream_id}.{ext}`  | VOD stream                                      |
| `GET /m3u-stream/series/{user}/{pass}/{stream_id}.{ext}` | Series episode                                  |
| `GET /m3u-stream/{user}/{pass}/{stream_id}.{ext}`        | Live stream, alternate form                     |
| `GET /m3u-stream/live/{user}/{pass}/{stream_id}/{file}`  | Native provider file (for example `index.m3u8`) |
| `GET /resource/m3u/{user}/{pass}/{id}/{resource}`        | Resources                                       |
| `GET /m3u-catchup/{token}`                               | Catch-up                                        |

```bash
curl -s "$BASE_URL/get.php?username=alice&password=example-password&type=m3u_plus"
curl -s "$BASE_URL/m3u-stream/live/alice/example-password/1042.ts"
```

The generated playlist contains absolute stream URLs with the user's credentials embedded. Treat a downloaded playlist
as a secret.

### HLS behaviour

An HLS entry URL is either `/live/{user}/{pass}/{vid}.m3u8` (target with Xtream output) or
`/m3u-stream/live/{user}/{pass}/{vid}.m3u8` (M3U-only target). The response is a media playlist.

- **Manifest polling.** Re-request the entry URL at the target duration. Do not cache a media playlist.
- **Opaque child URLs.** Segment, map, key, and resource URIs point under `/hls/shared/live/{proxy_session_id}/{lease}/...`
  and carry a lease id. Follow them exactly; do not construct them.
- **Identity.** An HLS playback is identified by a session token, not by the TCP connection. `GET /me/streams` exposes it
  as `session_token` when present; the `uid` stays stable across manifest and segment polls.
- **Source switch.** After a successful `failover-switch`, the next manifest carries `#EXT-X-DISCONTINUITY` and a higher
  `#EXT-X-DISCONTINUITY-SEQUENCE`. Continue with the same player pipeline.
- **Stop.** `DELETE /me/streams/{uid}` ends the session. The next manifest or segment request answers with the
  `hls_session_or_lease_expired` fallback.
- **Terminal responses.** When a session ends, HLS fallback and access-denial responses keep the diagnostic headers on
  every poll, so a client notices without reconnecting.

### Diagnostic headers

Fallback and rejection responses carry one or both headers. Browser clients can read them because the CORS layer
exposes both names through `Access-Control-Expose-Headers`.

```http
HTTP/1.1 502 Bad Gateway
X-Tuliprox-Fallback: provider_connections_exhausted
X-Tuliprox-Rejection: admission_rejected
Cache-Control: no-store

```

`X-Tuliprox-Fallback` values:

| Value                            | Meaning                                                  | Client action                                              |
|:---------------------------------|:---------------------------------------------------------|:-----------------------------------------------------------|
| `channel_unavailable`            | The upstream channel failed to start                     | Show a message; retry later or choose another channel      |
| `user_connections_exhausted`     | The user's `max_connections` is reached                  | Stop another own stream, or wait; retry after a slot frees |
| `provider_connections_exhausted` | All provider connections are in use                      | Check `/me/streams/joinable`, or retry later               |
| `low_priority_preempted`         | A higher-priority stream or recording preempted this one | Inform the user; do not retry immediately                  |
| `user_account_expired`           | The account expiry has passed                            | Re-login or renew; retrying cannot help                    |
| `provisioning`                   | The provider account is being provisioned                | Retry later                                                |
| `hls_session_or_lease_expired`   | The HLS session or lease expired or was terminated       | Start a new playback session                               |
| `maintenance`                    | The origin input is in maintenance mode                  | Retry later                                                |

`X-Tuliprox-Rejection` has one value: `admission_rejected`. It means the stream was refused before a body was produced
because the session could not be admitted (capacity, expiry, priority, or an internal registration failure). Treat it as
a hard refusal for this request: show the reason, back off, and retry later. It is not a signal to re-login.

HTTP status depends on configuration:

- With a configured custom video and `custom_stream_response_enabled: true` (the default), a fallback is served as
  `200 OK` with an MPEG-TS body or an HLS manifest, plus `X-Tuliprox-Fallback` and `Cache-Control: no-store`.
- Without a usable custom video, the same condition answers with `custom_stream_response_error_status` (default `502`)
  and the same headers. The four admission reasons (`user_connections_exhausted`, `provider_connections_exhausted`,
  `low_priority_preempted`, `user_account_expired`) additionally carry `X-Tuliprox-Rejection: admission_rejected`.
- An internal admission rejection with no fallback asset answers `503` with `X-Tuliprox-Rejection: admission_rejected`.

For MPEG-TS the headers arrive with the initial response. A mid-stream preemption cannot change headers; the socket
closes. Reconnect and inspect the headers of the new request.

Playlist login failures on the Xtream/M3U routes use `api_proxy.yml`'s `auth_error_status` (default `403`), not the
Bearer `401`/`403` semantics of this API.

## 6. Error reference

| Status | Body                                                   | Meaning                                                                      | Client action                            |
|:-------|:-------------------------------------------------------|:-----------------------------------------------------------------------------|:-----------------------------------------|
| `400`  | `member not in group`                                  | Switch target is not a member                                                | Refresh the member list                  |
| `400`  | `invalid_from_timestamp` / `invalid_to_timestamp`      | EPG time is not a timestamp or RFC 3339                                      | Fix the parameter                        |
| `400`  | `to_must_be_after_from` / `window_exceeds_48_hours`    | Invalid EPG window                                                           | Fix the window                           |
| `400`  | `{"error":"recording_..."}`                            | Recording command rejected (see the create and rule tables)                  | Show the code; fix the request           |
| `400`  | `invalid_item_id`                                      | Watch progress item id is `0`                                                | Use a real Xtream id                     |
| `401`  | text                                                   | Missing, invalid, expired, or revoked token                                  | Sign in again                            |
| `401`  | text + `X-Token-Refresh: required`                     | Stale permission schema or missing subject                                   | Refresh, then retry                      |
| `403`  | `feature_disabled`                                     | The plan does not allow this feature                                         | Hide the feature                         |
| `403`  | `principal does not have the required role`            | `api_enabled: false`, `ui_enabled: false`, wrong role, or missing permission | Show a message; do not retry             |
| `403`  | `recording_forbidden` / `recording_missing_permission` | No DVR permission for this action                                            | Hide the action                          |
| `403`  | `recording_shared_not_administrator`                   | Shared visibility without `recording.share`                                  | Offer `private` instead                  |
| `403`  | `recording_rule_not_owner`                             | The rule belongs to another user                                             | Hide the action                          |
| `404`  | empty or `stream not found` / `channel_not_found`      | Unknown, foreign, filtered, or not a group                                   | Refresh lists                            |
| `404`  | `{"error":"recording_not_found"}`                      | Unknown recording id                                                         | Re-read the snapshot                     |
| `404`  | `{"error":"recording_rule_unknown"}`                   | Unknown recording rule id                                                    | Re-read the rule list                    |
| `403`  | `principal does not have the required role`            | Player API disabled globally (`player_api_enabled: false`)                   | Server administrator disabled Player API |
| `409`  | `switch_in_progress`                                   | A switch is running                                                          | Wait and retry                           |
| `409`  | `shared_stream_switch_refused`                         | Shared stream cannot switch exclusively                                      | Do not retry while shared                |
| `409`  | `{"error":"recording_idempotency_conflict"}`           | Same idempotency key, different body                                         | Use a new key or the original body       |
| `409`  | `{"error":"recording_in_deleting_state"}`              | Recording is being deleted                                                   | Wait; then refresh                       |
| `409`  | `split_unsupported_protocol`                           | Split is only implemented for MPEG-TS                                        | Keep the shared playback                 |
| `409`  | `stream_not_shared`                                    | Stream is not shared or has no second viewer                                 | Do not split                             |
| `413`  | empty                                                  | Watch progress body exceeds 4 KiB                                            | Send a smaller body                      |
| `416`  | empty + `Content-Range`                                | Unsatisfiable range                                                          | Request a valid range                    |
| `422`  | `{"error":"recording_invalid_interval"}` and similar   | Live fields missing or on the wrong cluster                                  | Fix the body                             |
| `429`  | `switch_cooldown` + `Retry-After`                      | Manual switch or split cooldown                                              | Wait the given seconds                   |
| `429`  | empty + `Retry-After`                                  | Sign-in throttled                                                            | Wait the given seconds                   |
| `429`  | empty                                                  | Fifth concurrent `/me/events` WebSocket for one user                         | Close an unused socket                   |
| `500`  | `failed_reading_channels` / `failed_reading_epg`       | Server read failure                                                          | Retry with backoff                       |
| `500`  | `{"error":"recording_persistence_failed"}`             | Server-side persistence failure                                              | Retry with backoff                       |
| `500`  | `{"error":"recording_rule_partial_operation"}`         | Rule cancelled future recordings but the rule store failed                   | Re-read the rule list before retrying    |
| `501`  | `{"error":"recording_disabled"}`                       | DVR switched off                                                             | Hide the DVR UI                          |
| `502`  | fallback body + `X-Tuliprox-Fallback`                  | Fallback status (default) without a custom video, or a driver failure        | Follow the fallback table                |
| `503`  | empty + `X-Tuliprox-Rejection: admission_rejected`     | Admission refused                                                            | Back off and retry later                 |
| `507`  | `{"error":"recording_disk_full"}`                      | Recording does not fit on disk                                               | Free space, then retry                   |

## 7. End-to-end recipe

A minimal integration from sign-in to a source switch. The script uses `curl` and `jq`; it assumes a target with Xtream
output, named `main`, and a failover group channel.

```bash
#!/usr/bin/env bash
set -euo pipefail
BASE_URL="http://localhost:8901"
USERNAME="alice"
PASSWORD="example-password"

# 1. Sign in and keep the Bearer token.
TOKEN=$(curl -s -X POST "$BASE_URL/auth/token" \
    -H 'Content-Type: application/json' \
    -d "{\"username\":\"$USERNAME\",\"password\":\"$PASSWORD\"}" | jq -r '.token')
AUTH=(-H "Authorization: Bearer $TOKEN")

# 2. Discover what this plan allows.
CAPS=$(curl -s "$BASE_URL/api/v1/me/capabilities" "${AUTH[@]}")
echo "$CAPS" | jq '{recording, source_switch, joinable_streams, max_connections, active_connections}'

# 3. Find a channel and read its programme guide.
CHANNEL=$(curl -s -G "$BASE_URL/api/v1/me/channels" "${AUTH[@]}" \
    --data-urlencode "query=Sports" --data-urlencode "limit=1" | jq '.[0]')
VID=$(echo "$CHANNEL" | jq -r '.virtual_id')
curl -s -G "$BASE_URL/api/v1/me/epg" "${AUTH[@]}" \
    --data-urlencode "vid=$VID" | jq '.[0] | {title, start, stop}'

# 4. Start playback with the playlist URL. HLS uses .m3u8, MPEG-TS uses .ts.
curl -s -D - -o /dev/null "$BASE_URL/live/$USERNAME/$PASSWORD/$VID.m3u8"

# 5. Find the resulting stream and list its failover members.
UID=$(curl -s "$BASE_URL/api/v1/me/streams" "${AUTH[@]}" \
    | jq --argjson vid "$VID" -r 'map(select(.virtual_id == $vid)) | first | .uid')
curl -s "$BASE_URL/api/v1/me/streams/$UID/failover-members" "${AUTH[@]}" | jq '.members'

# 6. Switch to another member and wait for the next manifest poll.
MEMBER=$(curl -s "$BASE_URL/api/v1/me/streams/$UID/failover-members" "${AUTH[@]}" \
    | jq -r '.members[] | select(.current == false) | .member_vid' | head -n1)
curl -s -X POST "$BASE_URL/api/v1/me/streams/$UID/failover-switch" "${AUTH[@]}" \
    -H 'Content-Type: application/json' -d "{\"member_vid\":$MEMBER}"
echo "switched stream $UID to member $MEMBER"

# 7. Clean up: stop the stream and free the connection slot.
curl -s -X DELETE "$BASE_URL/api/v1/me/streams/$UID" "${AUTH[@]}" -o /dev/null -w '%{http_code}\n'
```

Notes on the recipe:

- Step 4 uses the playlist credentials, not the Bearer token.
- Re-read `/me/streams` after a reconnect instead of caching the `uid` forever.
- A `429` in step 6 means the two-second switch cooldown is still active; wait for `Retry-After`.
- If step 5 returns no stream, the playback request in step 4 did not reach the server with the same credentials. Check
  the response of step 4 for a fallback header before retrying.
