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
| :------- | :------------------------------------ | :-------------------------------------------------- | :---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
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
| :----- | :--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
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

## Available `/api/v1` Endpoints

This is a compact operator-oriented overview of the `/api/v1` REST API groups currently registered by the backend.

### System and diagnostics

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/healthcheck` | Liveness probe — 200 while the process is running. Used by Docker via the `--healthcheck` CLI flag. |
| `GET` | `/ready` | Readiness probe — 200 when at least one input group has spare connection capacity; 503 when every input group is exhausted. |
| `GET` | `/api/v1/status` | Server status, version, active users, provider connections, cache state |
| `GET` | `/api/v1/streams` | Current active streams |
| `GET` | `/api/v1/ipinfo` | External IPv4/IPv6 check if configured |
| `GET` | `/api/v1/events/stats` | Event-bus counters and the last 256 events, with the outcome of each |
| `GET` | `/api/v1/stream-history` | Query raw persisted stream history |
| `GET` | `/api/v1/stream-history/summary` | Aggregated stream history summary |
| `GET` | `/api/v1/qos-snapshots` | List QoS snapshots |
| `GET` | `/api/v1/qos-snapshots/{stream_identity_key}` | QoS detail for one stream |
| `GET` | `/api/v1/geoip/update` | Trigger GeoIP DB update |

### Maintenance mode

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/api/v1/maintenance` | Inputs in maintenance and all known input names (`maintenance.read`) |
| `PUT` | `/api/v1/maintenance/{input}` | Put an input (or `*` for all) into maintenance, optional `{"reason": "..."}` body (`maintenance.write`) |
| `DELETE` | `/api/v1/maintenance/{input}` | Take an input (or `*` for all) out of maintenance (`maintenance.write`) |

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

| Method   | Path                                                 | Purpose                                                                         |
| -------- |------------------------------------------------------|---------------------------------------------------------------------------------|
| `POST`   | `/api/v1/recording/requests`                         | Request a recording from server-owned source ids                                |
| `PATCH`  | `/api/v1/recording/requests/{id}`                    | Edit an upcoming recording                                                      |
| `POST`   | `/api/v1/recording/requests/{id}/cancel`             | Cancel the caller's recording                                                   |
| `DELETE` | `/api/v1/recording/requests/{id}`                    | Remove the caller's library entry                                               |
| `POST`   | `/api/v1/recording/materializations/{id}/pause`      | Pause a transfer (admin; never Live)                                            |
| `POST`   | `/api/v1/recording/materializations/{id}/resume`     | Resume a paused transfer (admin; never Live)                                    |
| `POST`   | `/api/v1/recording/materializations/{id}/retry`      | Retry a failed transfer (admin; never Live)                                     |
| `DELETE` | `/api/v1/recording/materializations/{id}`            | Delete the physical recording (admin; refused while referenced)                 |
| `POST`   | `/api/v1/recording/conflicts/preview`                | Advisory conflict preview (severity, optional provider scope, overlap segments) |
| `GET`    | `/api/v1/recording/availability`                     | Whether recording is available to the caller                                    |
| `GET`    | `/api/v1/recording/health`                           | Supervisor and recovery health (admin)                                          |
| `GET`    | `/api/v1/recording/rules`                            | List visible recurring recording rules                                          |
| `POST`   | `/api/v1/recording/rules`                            | Create a weekly recurring recording rule                                        |
| `PATCH`  | `/api/v1/recording/rules/{id}`                       | Edit a recurring recording rule                                                 |
| `DELETE` | `/api/v1/recording/rules/{id}?future=retain\|cancel` | Delete a recurring recording rule                                               |

Every route is gated by `recording.enabled`. With recording switched off they
answer `501 Not Implemented` with the code `recording_disabled`, which
distinguishes "switched off here" from `403` (not allowed) and `404` (does
not exist).

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

| Method | Path | Purpose |
| --- | --- | --- |
| `POST` | `/api/v1/playlist/live` | Query live playlist content for the Web UI, including Stalker inputs |
| `POST` | `/api/v1/playlist/vod` | Query VOD playlist content |
| `POST` | `/api/v1/playlist/series` | Query series playlist content |
| `POST` | `/api/v1/playlist/resolve_url` | Resolve provider-backed stream URLs |
| `POST` | `/api/v1/playlist/update` | Trigger target updates |
| `POST` | `/api/v1/playlist/epg` | Query EPG data for the Web UI |
| `POST` | `/api/v1/playlist/series_info/{virtual_id}/{provider_id}` | Series metadata lookup |
| `POST` | `/api/v1/playlist/series/episode/{virtual_id}` | Episode item lookup |
| `POST` | `/api/v1/playlist/filter/preview` | Dry-run a filter DSL expression against a target's stored playlist |
| `GET` | `/api/v1/playlist/resource/{resource}` | Public resource access for playlist-related assets |

### Failover groups

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/api/v1/failover/targets` | Failover files, load error and per-target status (`playlist.read`) |
| `GET` | `/api/v1/failover/groups` | Effective groups and diagnostics of a target (`playlist.read`) |
| `GET` | `/api/v1/failover/overrides` | Stored section of a target (`playlist.read`) |
| `PUT` | `/api/v1/failover/overrides` | Save the section of a target (`playlist.write`) |
| `DELETE` | `/api/v1/failover/overrides` | Reset the section of a target (`playlist.write`) |
| `POST` | `/api/v1/failover/overrides/reassign` | Move a section to another target (`playlist.write`) |
| `POST` | `/api/v1/failover/preview` | Preview a draft section (`playlist.read`) |
| `GET` | `/api/v1/failover/candidates` | Search channels of a target (`playlist.read`) |
| `GET` | `/api/v1/failover/health` | Member health of a group (`playlist.read`) |
| `POST` | `/api/v1/failover/apply` | Queue a playlist update for a target (`playlist.write`) |

### Configuration

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/api/v1/config` | Read effective configuration |
| `GET` | `/api/v1/config/batchContent/{input_id}` | Inspect batch input content |
| `POST` | `/api/v1/config/xtream/login-info` | Test or inspect Xtream login information |
| `POST` | `/api/v1/config/main` | Save `config.yml` |
| `POST` | `/api/v1/config/sources` | Save `source.yml` |
| `GET` | `/api/v1/config/apiproxy` | Read `api-proxy.yml` |
| `PUT` | `/api/v1/config/apiproxy` | Save `api-proxy.yml` |

### Personal settings (`me`)

`me` identifies the caller from the authenticated session. These routes do not accept a username or user ID
to select another account. Web UI users and API proxy users with Web UI access can manage their own settings;
administrator privileges are not required. If Web UI authentication is disabled, all callers use the same local
settings and the response reports `shared: true`.

The examples request JSON with `Accept: application/json`. Responses also support `Accept: application/cbor`,
and `PUT` accepts CBOR with `Content-Type: application/cbor`.

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/api/v1/me/settings` | Read the caller's preferences and per-table version tags |
| `GET` | `/api/v1/me/settings/tables/{table}` | Read one table layout and its `ETag` header |
| `PUT` | `/api/v1/me/settings/tables/{table}` | Replace one table layout; requires `If-Match` |
| `DELETE` | `/api/v1/me/settings/tables/{table}` | Reset one table's layout overrides; requires `If-Match` |

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

| Status | Error code | Meaning |
| --- | --- | --- |
| `400` | `settings_precondition_invalid` | `If-Match` is malformed or supplied more than once |
| `400` | `settings_payload_invalid` | Invalid layout payload, unknown fields, or duplicate ordered column IDs |
| `400` | `settings_id_invalid` | Invalid table or column ID |
| `400` | `settings_limits_exceeded` | The proposed layout or resulting settings exceeds a storage limit |
| `401` / `403` | Authentication error | Invalid session or caller without Web UI access |
| `409` | `settings_invalid`, `settings_version_unsupported`, `settings_owner_mismatch`, `settings_file_unsafe`, or `settings_limits_exceeded` | Stored settings cannot be safely read or changed |
| `412` | `settings_precondition_failed` | The tag no longer matches the table |
| `413` | `settings_request_too_large` | The `PUT` request body exceeds 64 KiB |
| `428` | `settings_precondition_required` | `PUT` or `DELETE` is missing `If-Match` |
| `500` | `settings_io_error` | Settings storage I/O failed |

IDs must be nonempty, contain no control characters, and fit in 128 UTF-8 bytes. Each layout supports at most
256 distinct column IDs across its order and visibility fields. Each account supports at most 64 stored tables
and a 256 KiB settings file. Invalid or unsupported stored files are not overwritten.
For storage locations and backup guidance, see [Personal table layouts](features.md#personal-table-layouts).

### API proxy users

| Method | Path | Purpose |
| --- | --- | --- |
| `POST` | `/api/v1/user/{target}` | Create a target user |
| `PUT` | `/api/v1/user/{target}` | Update or move a target user |
| `DELETE` | `/api/v1/user/{target}/{username}` | Delete a target user |

### Library

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/api/v1/library/status` | Local library status |
| `POST` | `/api/v1/library/scan` | Trigger a library scan |
| `GET` | `/api/v1/library/thumbnail/{uuid}` | Read a generated thumbnail |

### RBAC management

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/api/v1/rbac/users` | List Web UI users |
| `POST` | `/api/v1/rbac/users` | Create a Web UI user |
| `PUT` | `/api/v1/rbac/users/{username}` | Update a Web UI user |
| `DELETE` | `/api/v1/rbac/users/{username}` | Delete a Web UI user |
| `GET` | `/api/v1/rbac/groups` | List permission groups |
| `POST` | `/api/v1/rbac/groups` | Create a permission group |
| `PUT` | `/api/v1/rbac/groups/{name}` | Update a permission group |
| `DELETE` | `/api/v1/rbac/groups/{name}` | Delete a permission group |
| `GET` | `/api/v1/rbac/permissions` | List available permissions |

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
- `maintenance.read`
- `maintenance.write`

If a request is rejected, verify the logged-in Web UI user's RBAC group assignments first.

> **See also:** the full [DVR Operator Reference](./operator/dvr.md) for the authorization matrix, identity-registry
> bootstrap, token refresh, recurring-rule matching + DST, cross-store reconciliation, at-most-once notification
> protocol, and the migration checklist.
