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
- The preview endpoint returns catalog items. Actual playback can still require Stalker `create_link` resolution later, depending
  on your `stalker_pre_resolve_playback` / `stalker_runtime_resolve_playback` settings.
- If a Stalker item has not been materialized yet, Tuliprox no longer exposes the raw portal `cmd` as the playlist URL. Runtime
  playback resolves a real media URL later through the reverse-proxy path.
- Expired temp links are refreshed automatically when `stalker_runtime_resolve_playback` is enabled. Only portal-specific extra
  header/cookie requirements remain a possible follow-up.
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

If a request is rejected, verify the logged-in Web UI user's RBAC group assignments first.

> **See also:** the full [DVR Operator Reference](./operator/dvr.md) for the authorization matrix, identity-registry
> bootstrap, token refresh, recurring-rule matching + DST, cross-store reconciliation, at-most-once notification
> protocol, and the migration checklist.
