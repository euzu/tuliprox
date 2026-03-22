# 🌊 Reverse Proxy (Streaming, Caching & Rate Limits)

This section documents the `reverse_proxy:` block inside `config.yml`. It is the most critical block for determining runtime behavior when Tuliprox actively proxies video streams to clients (Reverse Proxy Mode), rather than just redirecting them.

It manages how Tuliprox establishes upstream connections, buffers video frames, handles sudden client disconnects, and caches static resources like EPG images and channel logos.

## Top-level entries

```yaml
reverse_proxy:
  resource_rewrite_disabled: false
  rewrite_secret: A1B2C3D4E5F60718293A4B5C6D7E8F90
  stream:
  cache:
  rate_limit:
  disabled_header:
  resource_retry:
  geoip:
```

### General Parameters

| Parameter | Type | Default | Technical Impact & Background |
| :--- | :--- | :--- | :--- |
| `resource_rewrite_disabled`| Bool | `false` | Normally, Tuliprox rewrites all image URLs in playlists to point to itself (e.g., `http://tuliprox:8901/resource/...`). If set to `true`, original URLs are kept (clients load images directly from the provider). **Warning:** Local caching will stop working if this is enabled! |
| `rewrite_secret` | String | `""` | A 32-character Hex string (16 bytes). Tuliprox encrypts/signs the original image URLs during the rewrite process. To prevent image URLs from becoming invalid after a server restart, you MUST enter a static secret here (generate via `openssl rand -hex 16`). |

---

## 1. Stream Management (`stream`)

This sub-block defines how Tuliprox maintains stream stability, buffers data, and handles HLS/Catchup session affinity.

```yaml
reverse_proxy:
  stream:
    retry: true
    buffer:
      enabled: true
      size: 1024
    throttle_kbps: 12500
    grace_period_millis: 2000
    grace_period_timeout_secs: 4
    grace_period_hold_stream: true
    hls_session_ttl_secs: 15
    catchup_session_ttl_secs: 45
    shared_burst_buffer_mb: 12
    metrics_enabled: false
```

### Stream Parameters in Detail

| Parameter | Type | Default | Technical Impact & Background |
| :--- | :--- | :--- | :--- |
| `retry` | Bool | `true` | **Background:** If the upstream provider unexpectedly drops the connection or a TCP network timeout occurs, Tuliprox immediately opens a new connection to the provider in the background and seamlessly pipes the new bytes to the end-client. The client's player (e.g., VLC) might stutter for a fraction of a second but will not abort playback. |
| `buffer.enabled` | Bool | `false` | Enables an asynchronous ring-buffer in RAM between the provider download stream and the client upload stream. Necessary if the provider stream is faster than the consumer can process. |
| `buffer.size` | Int | `0` | The size of the buffer in *Chunks* (1 Chunk = 8192 Bytes). A value of `1024` equals approximately 8 Megabytes of RAM per active stream. |
| `throttle_kbps` | Int | `0` | **Background:** Some players download VODs (Movies) at maximum line speed ("Bursting"). Providers often view this as abuse or scraping and will ban the IP. By throttling (e.g., to `12500` kbps), you force the download into a constant, inconspicuous flow. Supports units like `KB/s`, `MB/s`, `kbps`, `Mibps`. |
| `metrics_enabled` | Bool | `false` | **Monitoring:** If active, Tuliprox samples the live bandwidth (in kbps) and transferred bytes for every active reverse-proxied stream and pushes them via WebSockets to the Web UI. It adds a tiny bit of CPU overhead but is invaluable for debugging buffering issues. |
| `grace_period_millis` | Int | `2000` | The exact time window in ms where a temporary over-allocation is allowed (see notes on [The VLC Seek Problem](#the-vlc-seek-problem--grace-periods) for details). |
| `grace_period_timeout_secs`| Int | `4` | A hard timeout limit for overlapping "ghost sessions" to expire. |
| `grace_period_hold_stream`| Bool | `true` | Tuliprox artificially holds back the video data to the client, waiting for the grace check to finish, so it doesn't trigger the provider prematurely. |
| `hls_session_ttl_secs` | Int | `15` | Keeps the virtual provider slot open between HLS segment (`.ts`) requests to prevent provider bans for "Account Hopping". |
| `catchup_session_ttl_secs`| Int | `45` | The same session-holding principle applied to Archive/Catchup TV. See notes on section [Session TTLs for HLS & Catchup](#session-ttls-for-hls-m3u8--catchup) for details. |
| `shared_burst_buffer_mb`| Int | `12` | Minimum burst buffer size (in MB) used for shared live streams to immediately synchronize new clients without Keyframe dropouts. See notes on section [Shared Live Streams](#shared-streams) for details. |

---

## 2. Resource Caching (`cache`)

Tuliprox caches channel logos, posters, and EPG images on your disk so your clients don't stress the provider's servers on every playlist reload.

```yaml
reverse_proxy:
  cache:
    enabled: true
    size: 1GB
    directory: ./cache
```

An LRU (Least Recently Used) disk cache. If it hits the limit (e.g., `1GB`), Tuliprox automatically deletes the oldest images. Note: Fails if `resource_rewrite_disabled` is true.

---

## 3. Rate Limiting (`rate_limit`)

```yaml
reverse_proxy:
  rate_limit:
    enabled: true
    period_millis: 500
    burst_size: 10
```
Implements an IP-based Token-Bucket rate limiter. In this example, an IP can fire 10 requests immediately (`burst_size`). After that, it receives exactly one new token every 500ms (`period_millis`). This prevents DDOS attacks from malfunctioning scrapers. *(Ensure your upstream Nginx/Traefik passes `X-Forwarded-For` for this to work correctly!)*

---

## 4. Header Stripping (`disabled_header`)

When Tuliprox makes requests to the upstream provider, it can strip revealing headers that might expose which player you are actually using or the fact that you are proxying traffic.

```yaml
reverse_proxy:
  disabled_header:
    referer_header: true   # Removes the 'Referer' header
    x_header: true         # Removes all 'X-*' headers (like X-Real-IP)
    cloudflare_header: true# Removes 'CF-*' headers
    custom_header:
      - my-custom-tracker
```

---

## 5. Resource Retries (`resource_retry`)

Defines how aggressively Tuliprox tries to retry failed logo or EPG downloads when proxying them from the provider.

```yaml
reverse_proxy:
  resource_retry:
    max_attempts: 3
    backoff_millis: 250
    backoff_multiplier: 1.5
    failover_redirect_patterns:
      - "service-abuse"
```
* **Retries:** After the first error, Tuliprox waits 250ms. After the second error, it waits `250 * 1.5 = 375ms`, and so on.
* **`failover_redirect_patterns`:** A list of Regex patterns. If an upstream resource responds with an HTTP Redirect (302) containing these patterns (e.g., pointing to a "service-abuse" warning image from the provider), Tuliprox treats it as a failure instead of blindly serving the abuse image to your clients.

---

## 6. GeoIP Resolution (`geoip`)

To see the country flags of connected clients in the Web UI ("Active Streams" tab), Tuliprox can resolve IP addresses locally.

```yaml
reverse_proxy:
  geoip:
    enabled: true
    url: "https://raw.githubusercontent.com/sapics/ip-location-db/refs/heads/main/asn-country/asn-country-ipv4.csv"
```
The CSV file must have exactly 3 columns: `range_start,range_end,country_code`. (The DB is periodically updated via the `schedules` block using the `GeoIpUpdate` task type).

----
&nbsp;

# Additional Information
## The "VLC Seek Problem" & Grace Periods
When a user fast-forwards or rewinds a VOD, the player calculates the new byte offset, drops the old TCP connection, and immediately fires a new HTTP GET request (with a `Range` header) to Tuliprox.

**The Problem:** It takes milliseconds to seconds for the upstream provider to realize the old connection is dead. If you have a `max_connections: 1` limit at the provider, they will view this new seek-request as a *second concurrent stream* and reject it with an HTTP 509 (Bandwidth Exceeded) or HTTP 401 error.

**The Tuliprox Solution:**
* `grace_period_millis: 2000`: Tuliprox grants the user a temporary over-allocation (Grace) for exactly this duration.
* `grace_period_hold_stream: true`: Tuliprox artificially holds back the video data to the client, waiting for the grace check to finish, so it doesn't trigger the provider prematurely.
* After the milliseconds expire, Tuliprox checks internally: Is the old connection truly gone now? If Yes ➔ Data flows. If No ➔ The new connection is hard-killed (serving the `user_connections_exhausted.ts` video) because the user is actually illegally watching twice.
* `grace_period_timeout_secs: 4`: A hard timeout limit for overlapping "ghost sessions" to expire.

## Session TTLs for HLS (`.m3u8`) & Catchup
HLS streams do not consist of an endless TCP pipe. Instead, the player downloads small `.ts` segments every few seconds (e.g., `seg1.ts`, `seg2.ts`).

If Tuliprox released and re-acquired the provider slot for every single segment, providers would block the account for "Account Hopping" or spam. Tuliprox simulates a continuous session:
* `hls_session_ttl_secs: 15`: After a `.ts` segment finishes downloading, the physical slot to the provider is closed, but the "Virtual Slot" for this specific user remains reserved for 15 seconds. No other user can steal this slot during this window. Channel switches from the same client can immediately take over the reservation.
* The same principle applies to Archive/Catchup TV (`catchup_session_ttl_secs: 45`), which shares the same fragmentation and seeking issues.

## Shared Live Streams
Tuliprox can share a live stream (`share_live_streams: true` in the target options of `source.yml`). If 5 users watch the same Live-TV channel, Tuliprox pulls the stream only 1x from the provider and multicasts the bytes locally to 5 clients.
To ensure a user who tunes in 10 seconds later doesn't get player errors due to missing I-Frames/Keyframes, Tuliprox continuously keeps the last X Megabytes (`shared_burst_buffer_mb`, default `12`) in RAM. It fires this burst buffer at new subscribers so their decoders can instantly synchronize.