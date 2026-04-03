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

## Example 5: Query QoS snapshots

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

## Example 6: Inspect a download target before queueing

```bash
#!/bin/bash

BASE_URL="http://localhost:8901"
TOKEN="PUT_YOUR_TOKEN_HERE"
URL_TO_DOWNLOAD="https://example.invalid/file.mp4"

curl -s -G "$BASE_URL/api/v1/file/download/info" \
    --data-urlencode "url=$URL_TO_DOWNLOAD" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Accept: application/json" | jq .
```

Typical use:

- inspect filename and metadata before queueing
- verify that the remote file is reachable
- test `download.read` access

## Example 7: Queue a file download

```bash
#!/bin/bash

BASE_URL="http://localhost:8901"
TOKEN="PUT_YOUR_TOKEN_HERE"

curl -s -X POST "$BASE_URL/api/v1/file/download" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Accept: application/json" \
    -H "Content-Type: application/json" \
    --data-raw '{
      "url": "https://example.invalid/file.mp4",
      "filename": "example.mp4",
      "input_name": null,
      "priority": null
    }' | jq .
```

Typical use:

- queue a normal background download
- confirm that duplicate queue requests return the existing task instead of creating a second one
- test `download.write` access

## Example 8: Trigger a playlist update

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

## Available `/api/v1` Endpoints

This is a compact operator-oriented overview of the `/api/v1` REST API groups currently registered by the backend.

### System and diagnostics

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/api/v1/status` | Server status, version, active users, provider connections, cache state |
| `GET` | `/api/v1/streams` | Current active streams |
| `GET` | `/api/v1/ipinfo` | External IPv4/IPv6 check if configured |
| `GET` | `/api/v1/stream-history` | Query raw persisted stream history |
| `GET` | `/api/v1/stream-history/summary` | Aggregated stream history summary |
| `GET` | `/api/v1/qos-snapshots` | List QoS snapshots |
| `GET` | `/api/v1/qos-snapshots/{stream_identity_key}` | QoS detail for one stream |
| `GET` | `/api/v1/geoip/update` | Trigger GeoIP DB update |

### Downloads and recordings

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/api/v1/file/download/info` | Inspect remote file/download info |
| `POST` | `/api/v1/file/download` | Queue a file download |
| `POST` | `/api/v1/file/record` | Queue a live recording |
| `POST` | `/api/v1/file/download/pause` | Pause a queued or active download |
| `POST` | `/api/v1/file/download/resume` | Resume a paused download |
| `POST` | `/api/v1/file/download/cancel` | Cancel a queued or active download |
| `POST` | `/api/v1/file/download/remove` | Remove a task from the download database |
| `POST` | `/api/v1/file/download/retry` | Retry a failed download |

### Playlist and web-player helpers

| Method | Path | Purpose |
| --- | --- | --- |
| `POST` | `/api/v1/playlist/live` | Query live playlist content for the Web UI |
| `POST` | `/api/v1/playlist/vod` | Query VOD playlist content |
| `POST` | `/api/v1/playlist/series` | Query series playlist content |
| `POST` | `/api/v1/playlist/resolve_url` | Resolve provider-backed stream URLs |
| `POST` | `/api/v1/playlist/update` | Trigger target updates |
| `POST` | `/api/v1/playlist/epg` | Query EPG data for the Web UI |
| `POST` | `/api/v1/playlist/series_info/{virtual_id}/{provider_id}` | Series metadata lookup |
| `POST` | `/api/v1/playlist/series/episode/{virtual_id}` | Episode item lookup |
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
- `download.read`
- `download.write`

If a request is rejected, verify the logged-in Web UI user's RBAC group assignments first.
