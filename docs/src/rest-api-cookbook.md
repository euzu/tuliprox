# REST API Cookbook

This page contains copy-paste examples for the Tuliprox Web UI REST API.

The examples use:

- `curl` for HTTP requests
- `jq` for token extraction and pretty-printing JSON

## Requirements

- Tuliprox is running in server mode
- the Web UI REST API is enabled
- `curl` and `jq` are installed
- you have a valid Web UI username and password

## Base URLs

By default, the login endpoint is:

```text
http://localhost:8901/auth/token
```

If you configured a custom Web UI path such as `web`, the login endpoint becomes:

```text
http://localhost:8901/web/auth/token
```

The protected REST API lives below:

```text
http://localhost:8901/api/v1
```

or, with a Web UI path:

```text
http://localhost:8901/web/api/v1
```

## Example 1: Get a JWT token

```bash
#!/bin/bash

BASE_URL="http://localhost:8901"
USERNAME="admin"
PASSWORD="12345678"

TOKEN=$(curl -s -X POST "$BASE_URL/auth/token" \
    -H 'accept: application/json' \
    -H 'content-type: application/json' \
    --data-raw "{\"username\":\"$USERNAME\",\"password\":\"$PASSWORD\"}" | jq -r '.token')

if [ "$TOKEN" = "null" ] || [ -z "$TOKEN" ]; then
    echo "Error: no token available"
    exit 1
fi

echo "$TOKEN"
```

If your local setup uses HTTPS with a self-signed certificate, add `--insecure` to the `curl` commands.

## Example 2: Query server status

```bash
#!/bin/bash

BASE_URL="http://localhost:8901"
USERNAME="admin"
PASSWORD="12345678"

TOKEN=$(curl -s -X POST "$BASE_URL/auth/token" \
    -H 'accept: application/json' \
    -H 'content-type: application/json' \
    --data-raw "{\"username\":\"$USERNAME\",\"password\":\"$PASSWORD\"}" | jq -r '.token')

curl -s -X GET "$BASE_URL/api/v1/status" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Accept: application/json" | jq .
```

Typical use:

- verify that login works
- use as a simple health and diagnostics check
- inspect active users, active provider connections, cache state, and current server time

From this point onward, the examples use:

```bash
TOKEN="PUT_YOUR_TOKEN_HERE"
```

This is only to keep the examples shorter and easier to copy-paste.
Example 1 above already shows how to obtain the JWT token, so it is not repeated in every script block below.

## Example 3: List active streams

```bash
#!/bin/bash

BASE_URL="http://localhost:8901"
TOKEN="PUT_YOUR_TOKEN_HERE"

curl -s -X GET "$BASE_URL/api/v1/streams" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Accept: application/json" | jq .
```

Typical use:

- inspect currently active streams
- see who is connected
- correlate user activity with provider slot pressure

## Example 4: Query stream history summary

```bash
#!/bin/bash

BASE_URL="http://localhost:8901"
TOKEN="PUT_YOUR_TOKEN_HERE"

curl -s -X GET "$BASE_URL/api/v1/stream-history/summary?from=2026-04-01&to=2026-04-03" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Accept: application/json" | jq .
```

Typical use:

- get an aggregated view over recent stream activity
- inspect disconnect patterns and provider churn
- verify that stream history collection is working

## Example 5: Diagnose the event bus

```bash
#!/bin/bash

BASE_URL="http://localhost:8901"
TOKEN="PUT_YOUR_TOKEN_HERE"

curl -s -X GET "$BASE_URL/api/v1/events/stats" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Accept: application/json" | jq .
```

Every runtime event — playlist updates, config reloads, recording changes,
metadata refreshes — passes through one in-process broadcast bus on its way to
the Web UI websocket and the notification pipeline. This endpoint reports what
that bus has carried:

- `emitted` — a count per event, using the same stable names plugins and
  notification subscriptions use.
- `no_subscribers` — events published while nothing was listening. Normal when
  the Web UI is closed and notifications are off; a large number beside "I never
  got that notification" is the answer.
- `coalesced` — payload-free nudges suppressed because an identical one had just
  gone out. Deleting a recording naturally produces a few.
- `lagged` — events a subscriber was told it had missed. Should be `0`. If it is
  not, a subscriber cannot keep up: raise
  [`event_channel_capacity`](configuration/config.md).
- `recent` — the last 256 events with the outcome of each, so you can see
  whether a specific event fired at all.

Typical use:

- confirm an event fired before hunting for a bug in whatever consumes it
- tell "no notification was sent" apart from "no notification was configured"
- size `event_channel_capacity` from observed lag rather than by guessing

## Example 6: Query QoS snapshots

```bash
#!/bin/bash

BASE_URL="http://localhost:8901"
TOKEN="PUT_YOUR_TOKEN_HERE"

curl -s -X GET "$BASE_URL/api/v1/qos-snapshots" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Accept: application/json" | jq .
```

Typical use:

- inspect per-stream reliability snapshots
- compare `24h`, `7d`, and `30d` quality windows
- prepare later failover and ranking analysis

## Example 7: Request a recording

Recording requests name server-owned source ids. The client never supplies a
URL, a filename or a path: the server resolves all three, so a caller cannot
point a recording at arbitrary storage.

```bash
#!/bin/bash

BASE_URL="http://localhost:8901"
TOKEN="PUT_YOUR_TOKEN_HERE"

curl -s -i -X POST "$BASE_URL/api/v1/recording/requests" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -H "Idempotency-Key: 5f1c0f1e-2b7a-4f5d-9d2e-6c3f0f1b2a44" \
    --data-raw '{
      "source": {
        "target_id": "1",
        "virtual_id": "4242",
        "cluster": "live",
        "input_name": "provider-a"
      },
      "program_title": "Example Programme",
      "program_start": 1756000000,
      "program_end": 1756003600,
      "visibility": "private"
    }'
```

The response is `204 No Content` — a command, not a query. The new entry
reaches the client on the next recording snapshot over the WebSocket, so
there is one description of a recording rather than two that can disagree.

`Idempotency-Key` is optional. Repeating the same key with the same body
returns `204` again without creating a second recording, for 24 hours from
the first accepted request. The same key with a *different* body returns
`409 Conflict` rather than quietly answering with the first request's result.

Typical use:

- queue a recording without polling for its state afterwards
- make a client retry safe across a dropped connection or a server restart
- test `recording.create` access

## Example 8: Control an existing recording

```bash
#!/bin/bash

BASE_URL="http://localhost:8901"
TOKEN="PUT_YOUR_TOKEN_HERE"
REQUEST_ID="PUT_A_REQUEST_ID_HERE"

# A user acts on their own library entry.
curl -s -i -X POST "$BASE_URL/api/v1/recording/requests/$REQUEST_ID/cancel" \
    -H "Authorization: Bearer $TOKEN"

# Removing a finished entry detaches it; the file survives while anyone
# else still holds it.
curl -s -i -X DELETE "$BASE_URL/api/v1/recording/requests/$REQUEST_ID" \
    -H "Authorization: Bearer $TOKEN"
```

Every command answers `204`. Pause, resume and retry act on the *file*
rather than one user's entry, so they are administrator routes under
`/materializations/{id}` and are rejected for Live captures, which cannot be
paused, resumed or retried.

Typical use:

- leave a shared recording without stopping it for anyone else
- test `recording.manage` and `recording.delete` access

## Example 9: Trigger a playlist update

```bash
#!/bin/bash

BASE_URL="http://localhost:8901"
TOKEN="PUT_YOUR_TOKEN_HERE"

curl -s -X POST "$BASE_URL/api/v1/playlist/update" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Accept: application/json" \
    -H "Content-Type: application/json" \
    --data-raw '["main"]'
```

Typical use:

- trigger a manual processing run for one or more targets
- verify `playlist.write` permission
- integrate Tuliprox into external automation

## Example 10: Preview a Stalker live input in the Web UI playlist API

```bash
#!/bin/bash

BASE_URL="http://localhost:8901"
TOKEN="PUT_YOUR_TOKEN_HERE"

curl -s -X POST "$BASE_URL/api/v1/playlist/live" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Accept: application/json" \
    -H "Content-Type: application/json" \
    --data-raw '{"Input":"stalker"}' | jq .
```

Typical use:

- verify that a configured Stalker input handshakes and returns playlist preview rows
- confirm that the Web UI playlist explorer can inspect Stalker live content
- distinguish preview/catalog problems from later playback-resolution problems

Notes:

- Replace `"stalker"` with the configured input name.
- The preview endpoint returns catalog items. Actual playback resolves the Stalker `create_link` URL when the request reaches
  the stream endpoint.
- If a Stalker item has not been materialized yet, Tuliprox does not expose the raw portal `cmd` as the playlist URL. Runtime
  playback resolves a real media URL later through the reverse-proxy path.
- Expired temp links are refreshed automatically, including one re-handshake when the portal rejects the session. Only
  portal-specific extra header/cookie requirements remain a possible follow-up.
- Runtime refresh only produces `http`/`https` playback URLs; Stalker `rtmp://` / `rtsp://` commands are rejected explicitly rather
  than proxied half-supported.

## Example 11: Dry-run a filter expression against a target

```bash
#!/bin/bash

BASE_URL="http://localhost:8901"
TOKEN="PUT_YOUR_TOKEN_HERE"

curl -s -X POST "$BASE_URL/api/v1/playlist/filter/preview" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    --data-raw '{"target": 1, "filter": "Group ~ \"^DE.*\" AND NOT Title CONTAINS \"Shopping\"", "limit": 10}' | jq .
```

Typical use:

- test a filter DSL expression against a target's stored playlist before writing it into `source.yml`
- see matched/total counts overall and per cluster (live/vod/series)
- inspect sample matched and excluded channels to verify the expression does what you expect

Notes:

- `target` is the numeric target id (same id the playlist explorer uses).
- `filter` supports the full filter DSL including `!TEMPLATE!` references from your configured templates.
- Optional `limit` caps the sample lists (default 25, max 50); optional `match_as_ascii` mirrors the target option.
- An invalid filter expression returns HTTP 422 with `{"error": "...", "line": n, "column": n}`; `line`/`column`
  are `null` for semantic errors without a source position (e.g. an invalid regex value).
- The preview reads the target's already-processed playlist; it never contacts providers or triggers an update.

## Example 12: Inspect stored playlists and find EPG problems

```bash
#!/bin/bash

BASE_URL="http://localhost:8901"
TOKEN="PUT_YOUR_TOKEN_HERE"
AUTH=(-H "Authorization: Bearer $TOKEN" -H "Accept: application/json")

# Which targets and inputs can be inspected?
curl -s "${AUTH[@]}" "$BASE_URL/api/v1/playlist/sources" | jq .

# First page of the live channels of a target whose name contains "ard"
curl -s "${AUTH[@]}" -G "$BASE_URL/api/v1/playlist/items" \
    --data-urlencode "target=main" --data-urlencode "cluster=live" \
    --data-urlencode "search=ard" --data-urlencode "limit=20" | jq .

# Items whose EPG id does not exist in the target's EPG database
curl -s "${AUTH[@]}" -G "$BASE_URL/api/v1/playlist/items" \
    --data-urlencode "target=main" --data-urlencode "cluster=live" \
    --data-urlencode "epg_status=unmatched" --data-urlencode "fields=name" \
    --data-urlencode "fields=group" --data-urlencode "fields=epg_channel_id" | jq .

# EPG channels without programmes or already expired
curl -s "${AUTH[@]}" -G "$BASE_URL/api/v1/playlist/epg/channels" \
    --data-urlencode "target=main" --data-urlencode "issue=no_programmes" \
    --data-urlencode "issue=expired" | jq .

# Programmes of one EPG channel for the next 24 hours
curl -s "${AUTH[@]}" -G "$BASE_URL/api/v1/playlist/epg/channel" \
    --data-urlencode "target=main" --data-urlencode "channel_id=daserste.de" | jq .
```

Typical use:

- check what a target really stored after processing, without downloading the whole playlist
- find channels with a missing or unmatched EPG id, and EPG channels that no playlist item uses
- the same routes back the MCP tools `list_playlist_sources`, `query_playlist_items`, `query_target_epg_channels` and
  `get_target_epg_channel`

Notes:

- Sources are addressed by **name** (`target` or `input`, exactly one for `/playlist/items`), not by id.
- `cluster` is `live`, `video` or `series`. `offset`/`limit` page the matches (default 50, max 500); the response has
  `total_matched`, `skipped_records` (stored records that could not be read and are missing from the listing, with a
  `note` when greater than `0`), `returned`, `has_more` and `limit_clamped`. `/playlist/epg/channels` has the same
  paging fields plus `skipped_playlist_records` (stored playlist records that could not be read, so
  `referenced_by_playlist` and the `unreferenced` filter may be wrong for channels used only by them; a `note` says so).
- Repeatable parameters (`search_field`, `fields`, `epg_status`, `issue`) are given as repeated keys, not `key[]`.
- `search_field` accepts `name`, `title`, `group`, `epg_channel_id`, `provider_id`, `input_name`. `epg_status` is
  `missing_id`, `unmatched` or `matched` and works for targets with an EPG database only.
- The provider `url` is never returned; `additional_properties` must be requested through `fields` and has URL-like
  members removed.
- Only stored data is read, no provider is contacted. A source or target without a stored database answers `200` with
  `total_matched: 0` and a `note`; unknown names answer `404`, invalid parameters `400`.
- Field tables, EPG `issue` values and the response shapes are described in the
  [MCP operator reference](operator/mcp.md#41-reading-stored-playlists).

## Example 13: Query target analytics and export unmapped channels

```bash
#!/bin/bash

BASE_URL="http://localhost:8901"
TOKEN="PUT_YOUR_TOKEN_HERE"
AUTH=(-H "Authorization: Bearer $TOKEN" -H "Accept: application/json")

# Retrieve analytics summary and next scheduled update for all targets
curl -s "${AUTH[@]}" "$BASE_URL/api/v1/playlist/analytics" | jq .

# Download unmapped live channels CSV for target ID 1
curl -s -H "Authorization: Bearer $TOKEN" \
    "$BASE_URL/api/v1/playlist/analytics/1/unmapped.csv" -o unmapped_channels.csv
```

Typical use:

- inspect per-target item counts (live, VOD, series, episodes), input-to-output ratios, and EPG coverage
- monitor EPG source health, failure timestamps, and guide horizon windows
- export channels lacking EPG data for offline mapping adjustments

Notes:

- Requires `playlist.read` permission.
- Targets that have not yet undergone a refresh cycle return `analytics: null`.
- The CSV export endpoint validates the target ID against configuration and returns HTTP 404 if no unmapped CSV is available.

## Maintenance mode

An input can be taken out of service while its provider is down. While an input is in maintenance:

- every **new** stream request for it (live, VOD, series, catch-up, HLS entry, redirect mode) gets the
  `maintenance.ts` custom video instead of a provider connection,
- playlist and EPG updates keep serving the last persisted playlist and EPG without contacting the provider,
- probes, metadata resolution and Stalker refreshes for the input are paused,
- DVR recordings that would start fail with `Input is in maintenance mode`; use **Retry** afterwards.

Running streams, seeks within them, and running recordings continue. The state is stored in
`<storage_dir>/maintenance.json` and survives restarts. A corrupt file is moved aside as
`maintenance.json.corrupt` and the server starts with no input in maintenance. A file that cannot be read at all
(for example because of its permissions) stays in place; changes are then refused with `500` instead of overwriting
it. A one-shot CLI update reads the file but never changes it. Entries for inputs that are removed
from `source.yml` are dropped on the next source reload.

The dashboard shows a **Maintenance** card that turns red while any input is in maintenance.

```bash
#!/bin/bash

BASE_URL="http://localhost:8901"
TOKEN="PUT_YOUR_TOKEN_HERE"

# Put one input into maintenance, with an optional remark
curl -s -X PUT "$BASE_URL/api/v1/maintenance/provider_a" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    --data-raw '{"reason":"Provider down"}'

# Take every input out of maintenance (`*`, URL-encoded as %2A)
curl -s -X DELETE "$BASE_URL/api/v1/maintenance/%2A" -H "Authorization: Bearer $TOKEN"

# Current state
curl -s "$BASE_URL/api/v1/maintenance" -H "Authorization: Bearer $TOKEN"
```

Every call answers with the complete state:

```json
{
  "entries": { "provider_a": { "since": 1760350000, "reason": "Provider down" } },
  "inputs": ["provider_a", "provider_b"]
}
```

- `*` addresses all inputs known at the time of the call; an input literally named `*` cannot be addressed alone.
- Putting an input that is already in maintenance updates its remark and keeps `since`.
- Unknown inputs answer `404`, remarks longer than 500 characters `400`.
- Without Web UI authentication the endpoints are as open as the other `/api/v1` routes.

## Failover groups editor

The Web UI page [Failover groups](configuration/failover-groups.md#editor-web-ui) is built on the routes below. They
read and write the [failover files](configuration/failover-groups.md#corrections-and-own-groups-failover-files), so
they are also useful for scripts. Reading needs `playlist.read`, everything that changes something (`PUT`,
`DELETE`, `reassign`, `apply`) needs `playlist.write`. All answers are sent with `Cache-Control: no-store`.

The `target` parameter is the target **name** from `source.yml`. Only targets with failover groups enabled are
accepted; others answer `404`.

```bash
#!/bin/bash

BASE_URL="http://localhost:8901"
TOKEN="PUT_YOUR_TOKEN_HERE"
AUTH=(-H "Authorization: Bearer $TOKEN")

# Targets with failover groups, their section etag, the applied etag and unknown (orphan) sections
curl -s "$BASE_URL/api/v1/failover/targets" "${AUTH[@]}" | jq .

# Effective groups of the last playlist update, plus diagnostics
curl -s -G "$BASE_URL/api/v1/failover/groups" "${AUTH[@]}" --data-urlencode "target=Main TV" | jq .

# The stored section of the target (the base of an edit) with its etag and owning file
curl -s -G "$BASE_URL/api/v1/failover/overrides" "${AUTH[@]}" --data-urlencode "target=Main TV" | jq .

# Page through the channels of the target: filter = all | ungrouped | grouped | excluded
curl -s -G "$BASE_URL/api/v1/failover/candidates" "${AUTH[@]}" \
    --data-urlencode "target=Main TV" --data-urlencode "q=zdf" \
    --data-urlencode "filter=ungrouped" --data-urlencode "offset=0" --data-urlencode "limit=50" | jq .

# Preview a draft section: the groups it would produce, nothing is written
curl -s -X POST -G "$BASE_URL/api/v1/failover/preview" "${AUTH[@]}" --data-urlencode "target=Main TV" \
    -H "Content-Type: application/json" \
    --data-raw '{"target":"Main TV","excluded":[{"input":"prov-c","stream_id":"55"}]}' | jq .

# Save the section; expected_etag is the etag you based the change on (null for a new section)
curl -s -X PUT -G "$BASE_URL/api/v1/failover/overrides" "${AUTH[@]}" --data-urlencode "target=Main TV" \
    -H "Content-Type: application/json" \
    --data-raw '{"expected_etag":null,"overrides":{"target":"Main TV","excluded":[{"input":"prov-c","stream_id":"55"}]}}' | jq .

# Start a playlist update for the target so the saved section is used
curl -s -X POST -G "$BASE_URL/api/v1/failover/apply" "${AUTH[@]}" --data-urlencode "target=Main TV" | jq .
```

| Method   | Path                                  | Parameters                                          | Purpose                                                                                                                                                                                                                                               |
|:---------|:--------------------------------------|:----------------------------------------------------|:------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `GET`    | `/api/v1/failover/targets`            | ---------------------------------------------       | Used files, load error, targets with `etag`, `applied_etag`, `file`, `group_count`, `problem_count`, validation findings; sections of targets without failover groups or of unknown targets are listed with `state: "disabled"` or `state: "orphan"`. |
| `GET`    | `/api/v1/failover/groups`             | `target`                                            | Effective groups (`key`, origin, policy, metadata, members) of the saved state, diagnostics, `etag`, `applied_etag` and `display_stale`.                                                                                                              |
| `GET`    | `/api/v1/failover/overrides`          | `target`                                            | The stored section with `etag` and `file`; an empty section without `etag` when none exists.                                                                                                                                                          |
| `PUT`    | `/api/v1/failover/overrides`          | `target`, body `{ expected_etag, overrides }`       | Validates and writes the section. An empty section removes it. Answers `{ etag, file, hot_applied }`.                                                                                                                                                 |
| `DELETE` | `/api/v1/failover/overrides`          | `target`, `expected_etag`                           | Removes the whole section of the target (also of a target that no longer exists).                                                                                                                                                                     |
| `POST`   | `/api/v1/failover/overrides/reassign` | body `{ from, to, expected_etag }`                  | Moves a section to a target with failover groups enabled by rewriting its `target:` field (orphan sections).                                                                                                                                          |
| `POST`   | `/api/v1/failover/preview`            | `target`, body: draft section                       | Groups and diagnostics the draft would produce. Writes nothing.                                                                                                                                                                                       |
| `GET`    | `/api/v1/failover/candidates`         | `target`, `q`, `regex`, `filter`, `offset`, `limit` | Channel catalog of the target; `regex=true` reads `q` as a regular expression (`400` when invalid); `limit` defaults to 100, at most 500.                                                                                                             |
| `GET`    | `/api/v1/failover/health`             | `target`, `group` (virtual id of the group)         | Per member: rank, QoS, probed quality, circuit state, connection slots, score; plus the capacity of the group. Reads in-memory data only.                                                                                                             |
| `POST`   | `/api/v1/failover/apply`              | `target`                                            | Queues a manual playlist update for the target. Answers `202` with the run id.                                                                                                                                                                        |

Status codes:

| Status | Meaning                                                                                                                                                                                                                                                                                                                                              |
|:-------|:-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `404`  | Target unknown or failover groups not enabled for it; `health`: group not found; `reassign`: `to` is no target with failover groups enabled.                                                                                                                                                                                                         |
| `409`  | `PUT`, `DELETE`, `reassign`: the section changed since `expected_etag` (also by a hand edit on disk). `reassign` also answers `409` when the target `to` already has a section. The body is `{"current_etag": "..."}`. `groups`, `preview`, `candidates`: no channel catalog yet, run a playlist update first. `apply`: an update is already queued. |
| `422`  | `PUT`: the section is invalid. The body lists the findings, for example `[{"error": "invalid_manual_id", "detail": "Zdf"}]`.                                                                                                                                                                                                                         |
| `423`  | The failover files failed to load, so writing is refused until they are fixed.                                                                                                                                                                                                                                                                       |

Notes:

- `preview` uses the same grouping code as a playlist update, so the result matches what an applied save produces.
- `PUT` rejects a section with validation findings (`422`); `preview` does not validate, it shows what the draft would do.
- `hot_applied: true` means the change only reordered members or switched the policy and is already active, also when
  it changes the first member of a group. Otherwise call `apply`, which respects the input cache like a normal manual
  update.
- `display_stale` lists the keys of groups whose first member changed by such a change while name, logo, EPG id or
  playlist group are not all set explicitly: the published playlist entry still shows the old first member until the
  next playlist update, which clears the list.
- `PUT` and `DELETE` need the current etag. Read it with `GET /failover/overrides` immediately before the change; a
  `409` returns the etag on disk.
- Every save, reset, reassign and every change of the files on disk sends the WebSocket event
  `failover.overrides_changed` with `{"targets": [...]}` to clients with `playlist.read`.

## Player integration

External IPTV players (Android TV, mobile, desktop) can integrate directly with Tuliprox using the `/api/v1/me`
player API routes. These endpoints allow authenticated playlist proxy users (`api_proxy.yml`) to discover their effective
capabilities, monitor and stop their own active streams across devices, inspect failover group sources, switch sources
without interrupting playback, find running shared streams that can be joined without consuming provider slots, search
filtered channels and EPG, and inspect personal DVR recordings.

### 1. Authenticating as a playlist user

Playlist users authenticate using their Xtream login credentials:

```bash
#!/bin/bash
BASE_URL="http://localhost:8901"
USERNAME="alice"
PASSWORD="secretpassword"

TOKEN=$(curl -s -X POST "$BASE_URL/auth/token" \
    -H 'accept: application/json' \
    -H 'content-type: application/json' \
    --data-raw "{\"username\":\"$USERNAME\",\"password\":\"$PASSWORD\"}" | jq -r '.token')

echo "Bearer token: $TOKEN"
```

Requirements:

- Web UI authentication must be enabled on the server.
- The user must have `ui_enabled: true` or effective `player_features.api_enabled: true` in `api-proxy.yml`.
  If both are false, sign-in returns `403 Forbidden` (with an empty body).
- The global settings `user_ui_enabled` and `player_api_enabled` live under `web_ui` in `config.yml` (not `api-proxy.yml`).
  When unset, `player_api_enabled` follows `user_ui_enabled`. The `/me` router is always mounted on the server; if
  Player API access is disabled globally (`player_api_enabled: false`) or for the user (`api_enabled: false`),
  requests return `403 Forbidden`.
- The user's `network_access` rules apply to sign-in, refresh, and every request with the token. From a network the
  rules do not allow, the answer is `403 Forbidden`.
- A player that only knows the playlist host reads the base path from `server_info.player_api_path` in the Xtream
  `player_api.php` account info (`/ui` for `web_ui.path: ui`, `/` without a path). Append it to the URL used for
  `player_api.php` without `/player_api.php`, so a reverse proxy prefix is kept.
- The token bears the subject `api:<username>`. Permissions and `player_features` are evaluated dynamically from the live
  configuration on every request; changing a plan or user setting takes effect immediately without needing a new token.

### 2. Player API routes (`/api/v1/me`)

#### Query effective capabilities (`GET /api/v1/me/capabilities`)

Reports the effective features, limits, and quotas of the authenticated caller:

```bash
curl -s -X GET "$BASE_URL/api/v1/me/capabilities" \
    -H "Authorization: Bearer $TOKEN" | jq .
```

Example response:

```json
{
  "source_switch": true,
  "joinable_streams": true,
  "recording": "private",
  "recording_quota_bytes": 21474836480,
  "max_concurrent_recordings": 1,
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

Status codes:

| Status | Meaning                                                                                                                                         |
|:-------|:------------------------------------------------------------------------------------------------------------------------------------------------|
| `200`  | Success.                                                                                                                                        |
| `401`  | Missing or invalid Bearer token.                                                                                                                |
| `403`  | Player API disabled for user (`api_enabled: false` or global `player_api_enabled: false`; returns `principal does not have the required role`). |
| `404`  | Resource not found.                                                                                                                             |

#### List own active streams (`GET /api/v1/me/streams`)

Lists running playback streams owned by the caller:

```bash
curl -s -X GET "$BASE_URL/api/v1/me/streams" \
    -H "Authorization: Bearer $TOKEN" | jq .
```

Example response:

```json
[
  {
    "uid": 104,
    "virtual_id": 1001,
    "group_virtual_id": 1001,
    "member_vid": 2001,
    "title": "Sports HD",
    "item_type": "Live",
    "protocol": "ts",
    "shared": false,
    "session_active": true,
    "started_at": 1740045000,
    "user_agent": "TiviMate/4.7.0"
  }
]
```

Status codes:

| Status   | Meaning                                                                                   |
|:---------|:------------------------------------------------------------------------------------------|
| `200`    | Success. Only streams belonging to the caller are returned (never other users' streams).  |

#### Stop an own stream (`DELETE /api/v1/me/streams/{uid}`)

Terminates an own active stream on another device and frees the user connection slot immediately:

```bash
curl -s -X DELETE "$BASE_URL/api/v1/me/streams/104" \
    -H "Authorization: Bearer $TOKEN"
```

Status codes:

| Status   | Meaning                                                                   |
|:---------|:--------------------------------------------------------------------------|
| `204`    | Stream terminated successfully.                                           |
| `404`    | Stream `uid` does not exist or belongs to another user (foreign stream).  |

#### Query failover sources for a channel (`GET /api/v1/me/channels/{vid}/failover-members`)

Lists available source members for a failover group channel of the user's target before playback starts:

```bash
curl -s -X GET "$BASE_URL/api/v1/me/channels/1001/failover-members" \
    -H "Authorization: Bearer $TOKEN" | jq .
```

Example response:

```json
{
  "group_name": "Sports HD",
  "members": [
    {
      "member_vid": 2001,
      "index": 0,
      "title": "Sports HD (Main Feed)",
      "has_archive": true,
      "current": false,
      "health": {
        "state": "ok",
        "resolution": "P1080",
        "video_codec": "H264",
        "bitrate_kbps": 6500,
        "qos_24h": 98,
        "qos_7d": 95
      }
    },
    {
      "member_vid": 2002,
      "index": 1,
      "title": "Sports FHD (Backup Feed)",
      "has_archive": false,
      "current": false,
      "health": {
        "state": "degraded"
      }
    }
  ]
}
```

Each member includes an optional `health` object reflecting in-memory telemetry (no provider I/O is performed):

- `state`: Overall health status:
  - `"ok"`: Circuit is closed with normal operation.
  - `"degraded"`: Circuit is half-open, actively counting errors, or QoS failure streak is 2 or higher.
  - `"failing"`: Circuit is open with an active cooldown.
- `resolution`: Probed video resolution tier (e.g. `"P1080"`, `"P720"`, `"SD"`), or omitted if unknown.
- `video_codec`: Probed video codec (e.g. `"H264"`, `"H265"`), or omitted if unknown.
- `bitrate_kbps`: Probed video bitrate in kbps, or omitted if unmeasured or 0.
- `qos_24h`: Quality-of-service score over the last 24 hours (0-100), or omitted if uncalculated.
- `qos_7d`: Quality-of-service score over the last 7 days (0-100), or omitted if uncalculated.
- `retry_after_secs`: Estimated seconds remaining until circuit cooldown expires (present when `state` is `"failing"`).

Status codes:

| Status   | Meaning                                                                                           |
|:---------|:--------------------------------------------------------------------------------------------------|
| `200`    | Success. Returns redacted member DTO without input names, provider IDs, or internal stream IDs.   |
| `403`    | `feature_disabled` (`player_features.source_switch: false`).                                      |
| `404`    | Channel is not a failover group or is hidden/excluded by user's content filter.                   |

#### Query failover sources for an active stream (`GET /api/v1/me/streams/{uid}/failover-members`)

Returns group members for an active playback stream of the caller, with `current: true` marking the current source:

```bash
curl -s -X GET "$BASE_URL/api/v1/me/streams/104/failover-members" \
    -H "Authorization: Bearer $TOKEN" | jq .
```

Status codes:

| Status   | Meaning                                                                      |
|:---------|:-----------------------------------------------------------------------------|
| `200`    | Success.                                                                     |
| `400`    | `not a failover group stream` (channel is not a failover group).             |
| `403`    | `feature_disabled` (`player_features.source_switch: false`).                 |
| `404`    | `stream not found` (stream `uid` does not exist or belongs to another user). |

#### Switch failover stream source (`POST /api/v1/me/streams/{uid}/failover-switch`)

Manually switches an active MPEG-TS or live HLS failover stream to another member:

```bash
curl -s -X POST "$BASE_URL/api/v1/me/streams/104/failover-switch" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"member_vid": 2002}'
```

Status codes:

| Status   | Meaning                                                                                                 |
|:---------|:--------------------------------------------------------------------------------------------------------|
| `200`    | Switch completed and new member binding published.                                                      |
| `400`    | `member not in group` (target member VID is not in this group).                                         |
| `403`    | `feature_disabled` (`player_features.source_switch: false`).                                            |
| `404`    | Stream not found or foreign.                                                                            |
| `409`    | `switch_in_progress` (another switch is already executing on this stream).                              |
| `409`    | `shared_stream_switch_refused` (the stream is shared with other viewers and cannot switch exclusively). |
| `429`    | `switch_cooldown` with `Retry-After: <seconds>` header (switch attempted within 2-second cooldown).     |
| `502`    | Driver failure or timeout.                                                                              |

#### List joinable shared streams (`GET /api/v1/me/streams/joinable`)

Discovers active shared streams on the caller's target that can be watched without consuming an additional provider connection:

```bash
curl -s -X GET "$BASE_URL/api/v1/me/streams/joinable" \
    -H "Authorization: Bearer $TOKEN" | jq .
```

Example response:

```json
[
  {
    "virtual_id": 1001,
    "title": "Sky Sports Main Event HD",
    "logo": "http://example.com/logo.png",
    "category": "Sports",
    "protocol": "ts",
    "requires_free_slot": false
  }
]
```

Status codes:

| Status   | Meaning                                                                                                           |
|:---------|:------------------------------------------------------------------------------------------------------------------|
| `200`    | Success. Returns running shared streams passing the user's content filter. Redacts usernames and viewer counts.   |
| `403`    | `feature_disabled` (`player_features.joinable_streams: false`).                                                   |

#### Search channels (`GET /api/v1/me/channels`)

Searches visible live channels in the user's target playlist with user content filtering applied:

```bash
curl -s -X GET "$BASE_URL/api/v1/me/channels?query=Sports&limit=20" \
    -H "Authorization: Bearer $TOKEN" | jq .
```

Parameters: `query` (filter by channel name), `group` (filter by category name), `limit` (max 500, default 50).

#### Query EPG for a channel (`GET /api/v1/me/epg`)

Fetches programme listings for a visible channel with the user's EPG timeshift applied:

```bash
curl -s -X GET "$BASE_URL/api/v1/me/epg?vid=1001&from=1740045600&to=1740052800" \
    -H "Authorization: Bearer $TOKEN" | jq .
```

Parameters: `vid` (virtual ID), `from`, `to` (Unix timestamps or RFC 3339; max 48-hour window).
Returns `404` if the channel is not in the caller's target or is hidden by content filters.

### 3. Fallback headers and stream diagnostics

When a stream cannot start, is preempted, or terminates due to capacity limits, Tuliprox serves a fallback video slate or
rejection response carrying diagnostic headers:

```http
X-Tuliprox-Fallback: provider_connections_exhausted
Cache-Control: no-store
```

CORS preflight and stream responses expose both `X-Tuliprox-Fallback` and `X-Tuliprox-Rejection` via `Access-Control-Expose-Headers`.

Possible values for `X-Tuliprox-Fallback`:

| Value                              | Meaning                                                                   |
|:-----------------------------------|:--------------------------------------------------------------------------|
| `channel_unavailable`              | Upstream provider channel failed to start or returned an error.           |
| `user_connections_exhausted`       | Client reached their `max_connections` concurrent limit.                  |
| `provider_connections_exhausted`   | All upstream provider connections are in use.                             |
| `low_priority_preempted`           | Stream was preempted by a higher-priority stream or recording.            |
| `user_account_expired`             | Account expiry date has passed.                                           |
| `provisioning`                     | Provider account is currently undergoing provisioning.                    |
| `hls_session_or_lease_expired`     | HLS lease or session expired or was explicitly terminated.                |
| `maintenance`                      | Origin input is currently in maintenance mode.                            |

#### MPEG-TS vs HLS behavior

- **MPEG-TS**: The fallback header is returned on the initial HTTP handshake (e.g. `200 OK` with a fallback MPEG-TS video or
  rejection response). If preemption occurs mid-stream during active TS playback, HTTP headers cannot be sent mid-body; the TCP socket
  is closed. The player detects disconnection and checks `GET /api/v1/me/streams` or inspects `X-Tuliprox-Fallback` upon reconnect.
- **HLS**: Because clients poll playlists and segments cyclically, terminal responses (`403`) and fallback manifests carry
  `X-Tuliprox-Fallback` on every request. Mid-session preemption or termination is immediately visible on the player's next poll.

### 4. The joinable shared stream flow

When a player receives `X-Tuliprox-Fallback: provider_connections_exhausted`, it can call `GET /api/v1/me/streams/joinable` to discover
what channels other users on the same target are already watching:

1. The player checks `requires_free_slot`:
   - `false`: The user can start streaming this channel immediately. It reuses an existing provider connection and costs 0 extra provider slots.
   - `true`: The user has exhausted their personal `max_connections` limit. The player can prompt the user to stop another device using
     `DELETE /api/v1/me/streams/{uid}`.
2. **Free shared joins (`player_features.free_shared_joins: true`)**:
   Users with free shared joins enabled can watch running shared channels even with `max_connections` exhausted, up to `max_free_shared_joins`.
   When all paying viewers disconnect from the stream:
   - If the free joiner has a spare personal connection slot, the free join converts into a regular connection.
   - If no personal slot is available, the stream gracefully ends with `user_connections_exhausted`.
   - Running recordings attached to the shared stream count as paying viewers and keep the stream alive.

### 5. HLS session specifics

- **Identity**: HLS streams are identified by their `session_token`, not the ephemeral client TCP socket.
- **Preserved state**: Between segment requests, the stream is preserved in the active user manager. The `uid` remains stable across
  segment and manifest polls throughout playback.
- **Stopping HLS streams**: Calling `DELETE /api/v1/me/streams/{uid}` terminates the underlying session token under a transition guard.
  The player's next segment request receives a terminal response (`hls_session_or_lease_expired`).
- **Manual source switch**: `capabilities.source_switch_protocols` lists `["ts", "hls"]`. An HLS switch triggers origin member handoff.
  The player observes the switch on its next manifest poll via an `EXT-X-DISCONTINUITY` tag without resetting its playback pipeline.

### 6. Recording via the WebSocket one-shot pattern

External players without a persistent WebSocket connection can inspect DVR recordings using the one-shot WebSocket pattern.
All communication on the WebSocket uses binary frames exclusively (text frames are ignored):

1. Connect to `GET {web_ui_path}/ws`. (Authentication is not passed via HTTP request headers; it is handled in-band after connection).
2. Send the protocol version handshake as a single-byte binary frame containing `0x04` (protocol version 4).
   On success, the server echoes the single byte `0x04`.
3. Send an authentication binary frame containing the serde JSON encoding of `ProtocolMessage::Auth`: `{"Auth":"<token>"}`.
   The server responds with a binary frame containing `"Authorized"`.
4. Send the snapshot request as a binary frame containing the serde JSON encoding of
   `ProtocolMessage::RecordingSnapshotRequest`: `"RecordingSnapshotRequest"`.
5. Read the incoming binary frame, which decodes to `ProtocolMessage::RecordingSnapshotResponse`:

   ```json
   {
     "RecordingSnapshotResponse": {
       "revision": 12,
       "available": true,
       "quota": {
         "private_used_bytes": 4294967296,
         "private_limit_bytes": 10737418240,
         "shared_used_bytes": 0,
         "shared_limit_bytes": null
       },
       "tasks": [
         {
           "id": "rec_01h7x8...",
           "title": "Championship Final",
           "kind": "live",
           "priority": "normal",
           "status": "completed",
           "retry_attempts": 0,
           "transferred_bytes": 4294967296,
           "visibility": "private",
           "program_start": 1740045600,
           "program_end": 1740052800,
           "pre_roll_secs": 0,
           "post_roll_secs": 0
         }
       ]
     }
   }
   ```

6. Close the WebSocket connection.

If the user lacks DVR permissions or the feature is disabled (`player_features.recording: none`), the socket returns
a binary frame containing `{"RecordingWsError":{"code":"recording_forbidden"}}`.
Permissions are verified dynamically on every snapshot request.

### 7. Playlist Bouquet API (F9)

The Bouquet API allows players to inspect and personalize category listings for Xtream and M3U clients:

- `GET /api/v1/user/playlist/categories`: Returns all categories available to the user for Live, VOD, and Series clusters.
- `GET /api/v1/user/playlist/bouquet`: Returns the caller's personalized category selection (`{"xtream": ..., "m3u": ...}`).
- `POST /api/v1/user/playlist/bouquet`: Updates the user's category selection (`PlaylistBouquetDto`).

> **Note on Access Control:** Bouquets personalize which categories appear in client listings, but are **not** an access control boundary.
> Direct stream URLs for channels outside the bouquet still resolve if permitted by the user's plan. Access control is strictly
> enforced by the user's and plan's content `filter`.

### 8. Recurring rules, watch progress, events, and split

Four player-facing features extend the integration surface. Each is gated by its own `player_features` flag and is
documented in full, with client flows and the complete error table, in the
[Player & Integration API](./player-api.md) guide.

**Recurring recording rules** (`/api/v1/recording/rules`, recording level `private` or higher): the same routes the Web UI
uses, reachable by the player token. `GET` lists own rules plus visible shared rules, `POST` creates a `weekly_timeslot`
rule, `PATCH /{id}` edits it, and `DELETE /{id}?future=retain|cancel` removes it. Playlist users receive the redacted
`UserRecordingRuleDto` shape (no `source.provider_id` or `source.input_name`, `own`, and `owner_id` only for own rules);
the server derives the input name and stores `target_id` as the target name. The per-user limit is
`player_features.max_recording_rules`. See [Recurring recording rules](./player-api.md#410-recurring-recording-rules).

```bash
curl -s "$BASE_URL/api/v1/recording/rules" -H "Authorization: Bearer $TOKEN" | jq .
curl -s -X POST "$BASE_URL/api/v1/recording/rules" -H "Authorization: Bearer $TOKEN" \
    -H 'Content-Type: application/json' \
    -d '{"virtual_id":"1042","body":{"kind":"weekly_timeslot","weekday":3,"local_start_time":"20:00","duration_secs":3600,"timezone":"Europe/Berlin"}}'
```

**Watch progress** (`player_features.watch_progress: true`): store and resume playback positions. `PUT` a position every
~30 s, read it back with `GET` when the item is opened, and use `DELETE` to start over. Omitted `duration_secs` and
`series_id` keep the stored values, and a `PUT` for a user deleted in the meantime answers `401`. See
[Watch progress](./player-api.md#411-watch-progress).

```bash
curl -s -X PUT "$BASE_URL/api/v1/me/progress/episode/4711" -H "Authorization: Bearer $TOKEN" \
    -H 'Content-Type: application/json' \
    -d '{"position_secs":1500,"duration_secs":3000,"series_id":88}'
curl -s "$BASE_URL/api/v1/me/progress?kind=episode&limit=20" -H "Authorization: Bearer $TOKEN" | jq .
```

**Event stream** (`player_features.event_stream: true`): a JSON WebSocket at `GET /api/v1/me/events` with stream-ended
(`kicked`, `preempted`, or `stopped`), failover-switch, recording-finished, connection-denied, and resync events.
Authenticate with the Bearer header on the upgrade or with a first `{"type":"auth","token":"..."}` text frame. See
[Player event stream](./player-api.md#412-player-event-stream).

```js
const ws = new WebSocket(`${BASE_URL.replace(/^http/, "ws")}/api/v1/me/events`);
ws.addEventListener("open", () => ws.send(JSON.stringify({ type: "auth", token: TOKEN })));
ws.addEventListener("message", (m) => console.log(JSON.parse(m.data)));
```

**Split a shared stream** (`player_features.source_switch: true`, live MPEG-TS only): `POST /api/v1/me/streams/{uid}/split`
returns `{"reconnect_required":true,"url":"...ts?dedicated=1"}`. The URL is `/live/<user>/<pass>/<vid>.ts?dedicated=1`
for a target with an Xtream output and `/m3u-stream/live/<user>/<pass>/<vid>.ts?dedicated=1` otherwise. Catch-up, VOD,
series, HLS, and DASH answer `409 split_unsupported_protocol`; split and the failover member switch share one
two-second cooldown (`429 switch_cooldown`). Close the old connection before opening `url`. See
[Split a shared stream](./player-api.md#413-split-a-shared-stream).

```bash
curl -s -X POST "$BASE_URL/api/v1/me/streams/104/split" -H "Authorization: Bearer $TOKEN" | jq .
```

## Available `/api/v1` Endpoints

This is a compact operator-oriented overview of the `/api/v1` REST API groups currently registered by the backend.

The optional MCP endpoint (`POST {web_ui_path}/mcp`) is not an `/api/v1` route and is not listed below. It exposes
several of these routes as tools for AI assistants; see the [MCP operator reference](./operator/mcp.md).

### System and diagnostics

| Method | Path                                          | Purpose                                                                                                                     |
|--------|-----------------------------------------------|-----------------------------------------------------------------------------------------------------------------------------|
| `GET`  | `/healthcheck`                                | Liveness probe — 200 while the process is running. Used by Docker via the `--healthcheck` CLI flag.                         |
| `GET`  | `/ready`                                      | Readiness probe — 200 when at least one input group has spare connection capacity; 503 when every input group is exhausted. |
| `GET`  | `/api/v1/status`                              | Server status, version, active users, provider connections, cache state                                                     |
| `GET`  | `/api/v1/streams`                             | Current active streams                                                                                                      |
| `GET`  | `/api/v1/ipinfo`                              | External IPv4/IPv6 check if configured                                                                                      |
| `GET`  | `/api/v1/events/stats`                        | Event-bus counters and the last 256 events, with the outcome of each                                                        |
| `GET`  | `/api/v1/stream-history`                      | Query raw persisted stream history                                                                                          |
| `GET`  | `/api/v1/stream-history/summary`              | Aggregated stream history summary                                                                                           |
| `GET`  | `/api/v1/qos-snapshots`                       | List QoS snapshots                                                                                                          |
| `GET`  | `/api/v1/qos-snapshots/{stream_identity_key}` | QoS detail for one stream                                                                                                   |
| `GET`  | `/api/v1/geoip/update`                        | Trigger GeoIP DB update                                                                                                     |

### Maintenance mode

| Method   | Path                          | Purpose                                                                                                 |
|----------|-------------------------------|---------------------------------------------------------------------------------------------------------|
| `GET`    | `/api/v1/maintenance`         | Inputs in maintenance and all known input names (`maintenance.read`)                                    |
| `PUT`    | `/api/v1/maintenance/{input}` | Put an input (or `*` for all) into maintenance, optional `{"reason": "..."}` body (`maintenance.write`) |
| `DELETE` | `/api/v1/maintenance/{input}` | Take an input (or `*` for all) out of maintenance (`maintenance.write`)                                 |

### Recordings

Recording REST is **commands only**. There is no list, task, status or quota
endpoint to poll: every successful command answers `204 No Content`, and all
list, progress, quota and availability data reaches a client on the recording
WebSocket snapshot. That way a recording has one description rather than two
that can disagree.

Requests are addressed by the caller's own library-entry id. The three
materialization routes act on the shared *file* and are administrator-only;
the id for them appears solely in the administrator block of a snapshot, so
a regular user's DTO can never be used to reach one.

| Method   | Path                                                   | Purpose                                                                           |
|----------|--------------------------------------------------------|-----------------------------------------------------------------------------------|
| `POST`   | `/api/v1/recording/requests`                           | Request a recording from server-owned source ids                                  |
| `PATCH`  | `/api/v1/recording/requests/{id}`                      | Edit an upcoming recording                                                        |
| `POST`   | `/api/v1/recording/requests/{id}/cancel`               | Cancel the caller's recording                                                     |
| `DELETE` | `/api/v1/recording/requests/{id}`                      | Remove the caller's library entry                                                 |
| `POST`   | `/api/v1/recording/materializations/{id}/pause`        | Pause a transfer (admin; never Live)                                              |
| `POST`   | `/api/v1/recording/materializations/{id}/resume`       | Resume a paused transfer (admin; never Live)                                      |
| `POST`   | `/api/v1/recording/materializations/{id}/retry`        | Retry a failed transfer (admin; never Live)                                       |
| `DELETE` | `/api/v1/recording/materializations/{id}`              | Delete the physical recording (admin; refused while referenced)                   |
| `POST`   | `/api/v1/recording/conflicts/preview`                  | Advisory conflict preview (severity, optional provider scope, overlap segments)   |
| `GET`    | `/api/v1/recording/availability`                       | Whether recording is available to the caller                                      |
| `GET`    | `/api/v1/recording/health`                             | Supervisor and recovery health (admin)                                            |
| `GET`    | `/api/v1/recording/rules`                              | List visible recurring recording rules                                            |
| `POST`   | `/api/v1/recording/rules`                              | Create a weekly recurring recording rule                                          |
| `PATCH`  | `/api/v1/recording/rules/{id}`                         | Edit a recurring recording rule                                                   |
| `DELETE` | `/api/v1/recording/rules/{id}?future=retain\           | cancel`                                                                           |

Every route is gated by `recording.enabled`. With recording switched off they
answer `501 Not Implemented` with the code `recording_disabled`, which
distinguishes "switched off here" from `403` (not allowed) and `404` (does
not exist).

#### Recurring recording rules

When creating or editing a recurring recording rule (`POST /api/v1/recording/rules`,
`PATCH /api/v1/recording/rules/{id}`), `provider_id` and `is_group` on `RuleSource` are set by the server from the
resolved source and are ignored on client input. For playlist users the server also derives `input_name` and ignores
a client-supplied value; it stores `target_id` as the resolved target name. Their responses use the redacted
`UserRecordingRuleDto` shape, while administrator responses keep the full `RecordingRuleDto` shape.

#### Idempotent creates

`POST /api/v1/recording/requests` honours an optional `Idempotency-Key`
header:

- the same principal, key and body returns the original `204` without
  creating a second recording;
- the same key with a different body returns `409 Conflict`, because
  answering it with the first request's result would hide a caller bug;
- the record is kept for 24 hours from first acceptance and survives a
  restart, so a client retrying across one cannot duplicate its recording;
- after that it is forgotten, and the request is treated as new.

#### DVR WebSocket protocol

The DVR layer ships its own scoped protocol messages on the same WebSocket
connection as the rest of the backend. The client sends
`ProtocolMessage::RecordingSnapshotRequest` to subscribe and receives
`ProtocolMessage::RecordingSnapshotResponse { revision, available, quota, tasks }`
— the complete filtered list for the caller's session, sent on connect and
after every change the session is permitted to see.

There is no delta message. A snapshot is always complete, so a client that
misses one loses nothing.

Revisions are global across the server, so a session sees gaps whenever a
change touched somebody else. A gap is normal and must be applied; a
revision that goes *backwards* is a reordered delivery and must be ignored,
or the list regresses to a state the server has already moved past.

Progress-only updates are coalesced to at most one snapshot per second per
session. State transitions, command results, quota and availability changes,
and terminal states bypass that timer and publish immediately — a finished
recording must never look like it is still running.

#### DVR conflict preview

`POST /api/v1/recording/conflicts/preview` is **advisory only** — the response carries a severity bucket
(`none` / `soft` / `hard`) plus optional provider scope and overlap segments, but the server does not
reject the create call based on it. The frontend renders the preview as a hint next to the recording
form's scheduled interval, never as a hard block.

### Playlist and web-player helpers

| Method | Path                                                      | Purpose                                                                                             |
|--------|-----------------------------------------------------------|-----------------------------------------------------------------------------------------------------|
| `POST` | `/api/v1/playlist/live`                                   | Query live playlist content for the Web UI, including Stalker inputs                                |
| `POST` | `/api/v1/playlist/vod`                                    | Query VOD playlist content                                                                          |
| `POST` | `/api/v1/playlist/series`                                 | Query series playlist content                                                                       |
| `POST` | `/api/v1/playlist/resolve_url`                            | Resolve provider-backed stream URLs                                                                 |
| `POST` | `/api/v1/playlist/update`                                 | Trigger target updates                                                                              |
| `POST` | `/api/v1/playlist/epg`                                    | Query EPG data for the Web UI                                                                       |
| `GET`  | `/api/v1/playlist/sources`                                | Targets and inputs whose stored playlists can be inspected, with EPG availability (`playlist.read`) |
| `GET`  | `/api/v1/playlist/items`                                  | Paged, filtered stored records of one target or input (`playlist.read`)                             |
| `GET`  | `/api/v1/playlist/epg/channels`                           | Paged EPG channels of a target with issue filters (`playlist.read`)                                 |
| `GET`  | `/api/v1/playlist/epg/channel`                            | Programmes of one EPG channel of a target (`playlist.read`)                                         |
| `GET`  | `/api/v1/playlist/analytics`                              | Target content and EPG analytics with next scheduled runs (`playlist.read`)                         |
| `GET`  | `/api/v1/playlist/analytics/{target_id}/unmapped.csv`     | Export unmapped live channels for a target as CSV (`playlist.read`)                                 |
| `POST` | `/api/v1/playlist/series_info/{virtual_id}/{provider_id}` | Series metadata lookup                                                                              |
| `POST` | `/api/v1/playlist/series/episode/{virtual_id}`            | Episode item lookup                                                                                 |
| `POST` | `/api/v1/playlist/filter/preview`                         | Dry-run a filter DSL expression against a target's stored playlist                                  |
| `GET`  | `/api/v1/playlist/resource/{resource}`                    | Public resource access for playlist-related assets                                                  |

### Media-server inputs

The routes address one core media-server input (`plex`, `emby`, `jellyfin`, `silo`) by name. An unknown name answers
`404`, an input of another type `400`. The status route contacts the configured server once (15 s bound), while the
libraries route makes two sequential calls, each with a 15-second bound (total up to ~30 s). A hanging server
answers `504`. The routes never return tokens, passwords or URLs.

| Method | Path                                     | Purpose                                                                                                                                        |
|--------|------------------------------------------|------------------------------------------------------------------------------------------------------------------------------------------------|
| `GET`  | `/api/v1/media-server/{input}/libraries` | Visible libraries as `[{id, name, kind, selected}]`, `selected` marking the libraries the input imports (`playlist.read`)                      |
| `GET`  | `/api/v1/media-server/{input}/status`    | Discovered server as `{kind, server_id, name, version, reachable}` (`playlist.read`)                                                           |
| `POST` | `/api/v1/media-server/{input}/refresh`   | Queue a playlist update for that input; body `{"action": "normal"}` or `{"action": "refresh"}`; answers `202` with a run id (`playlist.write`) |

### Failover groups

| Method   | Path                                  | Purpose                                                            |
|----------|---------------------------------------|--------------------------------------------------------------------|
| `GET`    | `/api/v1/failover/targets`            | Failover files, load error and per-target status (`playlist.read`) |
| `GET`    | `/api/v1/failover/groups`             | Effective groups and diagnostics of a target (`playlist.read`)     |
| `GET`    | `/api/v1/failover/overrides`          | Stored section of a target (`playlist.read`)                       |
| `PUT`    | `/api/v1/failover/overrides`          | Save the section of a target (`playlist.write`)                    |
| `DELETE` | `/api/v1/failover/overrides`          | Reset the section of a target (`playlist.write`)                   |
| `POST`   | `/api/v1/failover/overrides/reassign` | Move a section to another target (`playlist.write`)                |
| `POST`   | `/api/v1/failover/preview`            | Preview a draft section (`playlist.read`)                          |
| `GET`    | `/api/v1/failover/candidates`         | Search channels of a target (`playlist.read`)                      |
| `GET`    | `/api/v1/failover/health`             | Member health of a group (`playlist.read`)                         |
| `POST`   | `/api/v1/failover/apply`              | Queue a playlist update for a target (`playlist.write`)            |

### Configuration

| Method | Path                                     | Purpose                                  |
|--------|------------------------------------------|------------------------------------------|
| `GET`  | `/api/v1/config`                         | Read effective configuration             |
| `GET`  | `/api/v1/config/batchContent/{input_id}` | Inspect batch input content              |
| `POST` | `/api/v1/config/xtream/login-info`       | Test or inspect Xtream login information |
| `POST` | `/api/v1/config/main`                    | Save `config.yml`                        |
| `POST` | `/api/v1/config/sources`                 | Save `source.yml`                        |
| `GET`  | `/api/v1/config/apiproxy`                | Read `api-proxy.yml`                     |
| `PUT`  | `/api/v1/config/apiproxy`                | Save `api-proxy.yml`                     |

### Personal settings (`me`)

`me` identifies the caller from the authenticated session. These routes do not accept a username or user ID
to select another account. Web UI users and API proxy users with Web UI access can manage their own settings;
administrator privileges are not required. If Web UI authentication is disabled, all callers use the same local
settings and the response reports `shared: true`.

The examples request JSON with `Accept: application/json`. Responses also support `Accept: application/cbor`,
and `PUT` accepts CBOR with `Content-Type: application/cbor`.

| Method   | Path                                 | Purpose                                                  |
|----------|--------------------------------------|----------------------------------------------------------|
| `GET`    | `/api/v1/me/settings`                | Read the caller's preferences and per-table version tags |
| `GET`    | `/api/v1/me/settings/tables/{table}` | Read one table layout and its `ETag` header              |
| `PUT`    | `/api/v1/me/settings/tables/{table}` | Replace one table layout; requires `If-Match`            |
| `DELETE` | `/api/v1/me/settings/tables/{table}` | Reset one table's layout overrides; requires `If-Match`  |

#### Read all preferences

```bash
BASE_URL="http://localhost:8901"
TOKEN="PUT_YOUR_TOKEN_HERE"

curl -sS "$BASE_URL/api/v1/me/settings" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Accept: application/json" | jq .
```

An account without stored preferences returns:

```json
{
  "preferences": {
    "web_ui": {
      "tables": {}
    }
  },
  "section_etags": {},
  "shared": false
}
```

Stored layouts appear in `preferences.web_ui.tables`, keyed by table ID. `section_etags` contains the corresponding
quoted version tags, also keyed by table ID. This endpoint does not return a top-level `ETag` header.
To obtain a version tag for a table without stored overrides, read that table's endpoint.

#### Read, save, and reset a table layout

`{table}` is the stable table ID used by the Web UI, for example `users` for the API proxy user table.
Percent-encode the ID when putting it in the URL. Column IDs are stable identifiers such as `username` and `enabled`,
rather than translated display labels.

A table layout has two fields:

```json
{
  "column_order": ["username", "enabled"],
  "column_visibility": {
    "enabled": false
  }
}
```

`column_order` specifies the preferred order. `column_visibility` maps column IDs to visibility flags.
Columns without overrides use the Web UI defaults, and required columns remain visible.
`PUT` replaces both fields: omitted fields default to an empty list or map. Unknown payload fields and duplicate
IDs in `column_order` are rejected.

The following sequence reads the layout, saves an override, and then resets it. It uses the token obtained above.
When authentication is disabled, omit the `Authorization` headers.

```bash
TABLE_URL="$BASE_URL/api/v1/me/settings/tables/users"
HEADERS_FILE=$(mktemp)
trap 'rm -f "$HEADERS_FILE"' EXIT

# Read the layout and capture its quoted ETag, including for an unsaved table.
curl --fail-with-body -sS -D "$HEADERS_FILE" "$TABLE_URL" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Accept: application/json" || exit 1

ETAG=$(awk 'tolower($1) == "etag:" {gsub("\r", "", $2); print $2}' "$HEADERS_FILE")

# Save only if this table still matches the version just read.
curl --fail-with-body -sS -D "$HEADERS_FILE" -X PUT "$TABLE_URL" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Accept: application/json" \
    -H "Content-Type: application/json" \
    -H "If-Match: $ETAG" \
    --data-raw '{"column_order":["username","enabled"],"column_visibility":{"enabled":false}}' || exit 1

# Use the version returned by the successful save for the next change.
ETAG=$(awk 'tolower($1) == "etag:" {gsub("\r", "", $2); print $2}' "$HEADERS_FILE")

curl --fail-with-body -sS -i -X DELETE "$TABLE_URL" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Accept: application/json" \
    -H "If-Match: $ETAG" || exit 1
```

All three table operations return `200 OK`, the resulting layout, and its `ETag` header.
An unsaved or reset layout is `{"column_order":[],"column_visibility":{}}`; the Web UI applies its defaults.
`DELETE` resets only the selected table's layout overrides. It does not delete the account or other tables' settings.
Successful settings responses use `Cache-Control: private, no-store`.

#### Concurrent changes and errors

Send exactly one `If-Match` header with the quoted tag returned by the server. Tags are scoped to the account and table;
changes to another table do not invalidate this table's tag. Weak tags, `*`, unquoted tags, and lists of tags are rejected.

If another client changes the same table before your save or reset, the server returns `412 Precondition Failed`
without applying your change. Read the table again, reconcile your draft with the current layout, and use the fresh
tag only after deciding to apply the change again.

Settings validation and storage errors return a JSON object such as `{"error":"settings_precondition_failed"}`
when requesting JSON. Authentication errors return a plain-text error message.

| Status        | Error code                                                                                                                           | Meaning                                                                 |
|---------------|--------------------------------------------------------------------------------------------------------------------------------------|-------------------------------------------------------------------------|
| `400`         | `settings_precondition_invalid`                                                                                                      | `If-Match` is malformed or supplied more than once                      |
| `400`         | `settings_payload_invalid`                                                                                                           | Invalid layout payload, unknown fields, or duplicate ordered column IDs |
| `400`         | `settings_id_invalid`                                                                                                                | Invalid table or column ID                                              |
| `400`         | `settings_limits_exceeded`                                                                                                           | The proposed layout or resulting settings exceeds a storage limit       |
| `401` / `403` | Authentication error                                                                                                                 | Invalid session or caller without Web UI access                         |
| `409`         | `settings_invalid`, `settings_version_unsupported`, `settings_owner_mismatch`, `settings_file_unsafe`, or `settings_limits_exceeded` | Stored settings cannot be safely read or changed                        |
| `412`         | `settings_precondition_failed`                                                                                                       | The tag no longer matches the table                                     |
| `413`         | `settings_request_too_large`                                                                                                         | The `PUT` request body exceeds 64 KiB                                   |
| `428`         | `settings_precondition_required`                                                                                                     | `PUT` or `DELETE` is missing `If-Match`                                 |
| `500`         | `settings_io_error`                                                                                                                  | Settings storage I/O failed                                             |

IDs must be nonempty, contain no control characters, and fit in 128 UTF-8 bytes. Each layout supports at most
256 distinct column IDs across its order and visibility fields. Each account supports at most 64 stored tables
and a 256 KiB settings file. Invalid or unsupported stored files are not overwritten.
For storage locations and backup guidance, see [Personal table layouts](features.md#personal-table-layouts).

### API proxy users

| Method   | Path                               | Purpose                      |
|----------|------------------------------------|------------------------------|
| `POST`   | `/api/v1/user/{target}`            | Create a target user         |
| `PUT`    | `/api/v1/user/{target}`            | Update or move a target user |
| `DELETE` | `/api/v1/user/{target}/{username}` | Delete a target user         |

Deleting a playlist user also removes their personal settings, their watch progress file, and every
recurring recording rule they own.

### Player API (/api/v1/me)

| Method   | Path                                         | Purpose                                                          |
|----------|----------------------------------------------|------------------------------------------------------------------|
| `GET`    | `/api/v1/me/capabilities`                    | Effective features, permissions, connection limits, and quotas   |
| `GET`    | `/api/v1/me/streams`                         | Active playback streams owned by the caller                      |
| `DELETE` | `/api/v1/me/streams/{uid}`                   | Terminate caller's own stream and release connection slot        |
| `GET`    | `/api/v1/me/channels/{vid}/failover-members` | Redacted failover members for a channel before playback          |
| `GET`    | `/api/v1/me/streams/{uid}/failover-members`  | Redacted failover members for an active stream                   |
| `POST`   | `/api/v1/me/streams/{uid}/failover-switch`   | Manually switch stream source member (TS & HLS)                  |
| `GET`    | `/api/v1/me/streams/joinable`                | Active shared streams joinable without extra provider connection |
| `GET`    | `/api/v1/me/channels`                        | Search visible live channels with content filtering              |
| `GET`    | `/api/v1/me/epg`                             | Programme guide for a visible channel with caller timeshift      |

### Library

| Method | Path                               | Purpose                    |
|--------|------------------------------------|----------------------------|
| `GET`  | `/api/v1/library/status`           | Local library status       |
| `POST` | `/api/v1/library/scan`             | Trigger a library scan     |
| `GET`  | `/api/v1/library/thumbnail/{uuid}` | Read a generated thumbnail |

### RBAC management

| Method   | Path                            | Purpose                    |
|----------|---------------------------------|----------------------------|
| `GET`    | `/api/v1/rbac/users`            | List Web UI users          |
| `POST`   | `/api/v1/rbac/users`            | Create a Web UI user       |
| `PUT`    | `/api/v1/rbac/users/{username}` | Update a Web UI user       |
| `DELETE` | `/api/v1/rbac/users/{username}` | Delete a Web UI user       |
| `GET`    | `/api/v1/rbac/groups`           | List permission groups     |
| `POST`   | `/api/v1/rbac/groups`           | Create a permission group  |
| `PUT`    | `/api/v1/rbac/groups/{name}`    | Update a permission group  |
| `DELETE` | `/api/v1/rbac/groups/{name}`    | Delete a permission group  |
| `GET`    | `/api/v1/rbac/permissions`      | List available permissions |

## Permissions

With Web UI authentication enabled, many endpoints require matching permissions such as:

- `system.read`
- `system.write`
- `playlist.read`
- `playlist.write`
- `config.read`
- `config.write`
- `source.read`
- `source.write`
- `user.read`
- `user.write`
- `library.read`
- `library.write`
- `recording.read`
- `recording.create`
- `recording.manage`
- `recording.delete`
- `recording.share`
- `maintenance.read`
- `maintenance.write`

If a request is rejected, verify the logged-in Web UI user's RBAC group assignments first.

> **See also:** the full [DVR Operator Reference](./operator/dvr.md) for the authorization matrix, identity-registry
> bootstrap, token refresh, recurring-rule matching + DST, cross-store reconciliation, at-most-once notification
> protocol, and the migration checklist.
