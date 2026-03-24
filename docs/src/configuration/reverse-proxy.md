# 🌊 Reverse Proxy (Streaming, Caching & Rate Limits)

This section documents the `reverse_proxy:` block inside `config.yml`. It is the most critical block for determining runtime
behavior when Tuliprox actively proxies video streams to clients (Reverse Proxy Mode), rather than just redirecting them.

It manages how Tuliprox establishes upstream connections, buffers video frames, handles sudden client disconnects, and caches
static resources like EPG images and channel logos.


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
> **Note:** Reverse Proxy mode can be activated for each user individually.

### General Parameters

| Parameter | Type | Default | Technical Impact & Background |
| :--- | :--- | :--- | :--- |
| `resource_rewrite_disabled`| Bool | `false` | Normally, Tuliprox rewrites all image URLs in playlists to point to itself (e.g., `http://tuliprox:8901/resource/...`). If set to `true`, original URLs are kept (clients load images directly from the provider). **Warning:** Local caching will stop working if this is enabled! |
| `rewrite_secret` | String | `""` | A 32-character Hex string (16 bytes). Tuliprox encrypts/signs the original image URLs during the rewrite process. To prevent image URLs from becoming invalid after a server restart, you MUST enter a static secret here. |

>**Note**: You can generate a random secret using:
```bash
openssl rand -hex 16
# or
node -e "console.log(require('crypto').randomBytes(16).toString('hex').toUpperCase())"
```

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
| `grace_period_timeout_secs` | Int | `4` | A hard timeout limit for overlapping "ghost sessions" to expire. |
| `grace_period_hold_stream` | Bool | `true` | Tuliprox artificially holds back the video data to the client, waiting for the grace check to finish, so it doesn't trigger the provider prematurely. |
| `hls_session_ttl_secs` | Int | `15` | Keeps the virtual provider slot open between HLS segment (`.ts`) requests to prevent provider bans for "Account Hopping". |
| `catchup_session_ttl_secs` | Int | `45` | The same session-holding principle applied to Archive/Catchup TV. See notes on section [Session TTLs for HLS & Catchup](#session-ttls-for-hls-m3u8--catchup) for details. |
| `shared_burst_buffer_mb` | Int | `12` | Minimum burst buffer size (in MB) used for shared live streams to immediately synchronize new clients without Keyframe dropouts. See notes on section [Shared Live Streams](#shared-live-streams) for details. |

### 1.1 `retry` & `buffer` (Deep Dive)
Tuliprox handles streams differently based on these settings:
* **Option A:** Both `retry: false` and `buffer.enabled: false` ➔ The provider stream is piped directly to the client with minimal overhead.
* **Option B:** Either `retry: true` or `buffer.enabled: true` ➔ Tuliprox uses complex stream handling with a higher memory footprint to ensure stability.

#### Ring-Buffer Calculation
`buffer.size` is defined in chunks of **8192 bytes (8 KB)**.
* A size of `1024` equals approx. **8 MB** of RAM per active stream.
* **Shared Streams Impact:** If `share_live_streams` is enabled, each channel consumes at least **12 MB** regardless of client count. Increasing `size` above 1024 (e.g., 2048) increases this to **24 MB** per shared channel.



### 1.2 `throttle_kbps`
Prevents provider bans by limiting "bursting" players. Supported units: `KB/s`, `MB/s`, `KiB/s`, `MiB/s`, `kbps`, `mbps`, `Mibps`.

**Reference Table for Throttling:**
| Resolution | Framerate | Bitrate (kbps) | Quality |
| :--- | :--- | :--- | :--- |
| 480p (854x480) | 30 fps | 819 – 2,457 | Low-Quality |
| 720p (1280x720) | 30 fps | 2,457 – 5,737 | HD-Streams |
| 1080p (1920x1080) | 30 fps | 5,737 – 12,288 | Full-HD |
| 4K (3840x2160) | 30 fps | 20,480 – 49,152 | Ultra-HD |

### 1.3 `grace_period` (The VLC Seek Problem)
If `max_connections` is > 0, seeking can trigger a 509/401 error because the old connection isn't closed yet.
* `grace_period_millis` (Default: `2000`): Grants a temporary over-allocation during switchover.
* `grace_period_timeout_secs` (Default: `4`): How long a grace grant lasts before a new one can be made.
* `grace_period_hold_stream`: If `true`, Tuliprox waits for the check to complete before sending data, preventing player timeouts on "exhausted" switches.

### 1.4 HLS & Catchup Session TTLs
HLS/Catchup clients connect and disconnect repeatedly. Tuliprox uses a **Virtual Reservation** to maintain account affinity:
* **HLS (`15s`):** The real provider slot is only held during active requests. The reservation keeps the provider account stable between segment fetches.
* **Catchup (`45s`):** Keeps the reservation alive during seeking and reconnects.
* **Note:** Channel switches from the same client immediately take over the reservation, bypassing the TTL.

---

## 2. Resource Caching (`cache`)

Tuliprox maintains a local disk cache for channel logos, posters, and EPG images. This reduces the load on provider servers and significantly speeds up playlist loading times for your clients.

```yaml
reverse_proxy:
  cache:
    enabled: true
    size: 1GB
    directory: ./cache
```

### Parameter Details

| Parameter | Type | Default | Technical Impact |
| :--- | :--- | :--- | :--- |
| `enabled` | Bool | `false` | Global switch for resource caching. **Note:** Requires `resource_rewrite_disabled: false`. |
| `size` | Size | `1GB` | Maximum disk space allocation. Supported units: `KB`, `MB`, `GB`, `TB`. |
| `directory` | String | `./cache` | Storage location. Relative paths are resolved against the `storage_dir`. |



### Technical Background
* **LRU Logic:** Operates as a **Least Recently Used** cache. When the `size` limit is reached, Tuliprox automatically evicts the oldest/least accessed images to make room for new content.
* **Encryption:** Cached resources are indexed using a hash derived from the `rewrite_secret`. If the secret changes, the old cache becomes orphaned.
* **Client Delivery:** Instead of the client downloading directly from the provider, Tuliprox serves the local file, acting as a high-speed CDN for your media metadata.

---

## 3. Rate Limiting (`rate_limit`)

This block implements an IP-based **Token-Bucket** rate limiter. It protects your Tuliprox instance and upstream providers from DDoS attacks, malfunctioning scrapers, or aggressive players by restricting the frequency of incoming HTTP requests.

```yaml
reverse_proxy:
  rate_limit:
    enabled: true
    period_millis: 500
    burst_size: 10
```

### Parameter Details

| Parameter | Type | Default | Technical Impact |
| :--- | :--- | :--- | :--- |
| `enabled` | Bool | `false` | Global switch for the rate limiting engine. |
| `period_millis` | Int | `500` | The refill rate. Defines how many milliseconds it takes to replenish exactly one request token. |
| `burst_size` | Int | `10` | The bucket capacity. Allows a client to send this many requests instantly before the rate limit kicks in. |

### Technical Background
* **Mechanism:** A client starts with a full "bucket" of `burst_size` tokens. Every request consumes one token. Once empty, the client must wait `period_millis` for a new token to be generated.
* **IP-Detection:** Tuliprox identifies clients by their IP address. If you are running Tuliprox behind a proxy (Nginx, Traefik), ensure headers like `X-Forwarded-For` or `X-Real-IP` are passed correctly.
* **Behavior:** When a client exceeds the limit, Tuliprox returns an `HTTP 429 (Too Many Requests)` status, protecting your CPU and provider bandwidth.

---

## 4. Header Stripping (`disabled_header`)

This block controls which HTTP headers Tuliprox removes before forwarding a client request to the upstream provider. Stripping these headers is essential to prevent the provider from detecting proxy usage, internal IP addresses, or specific player fingerprints.

```yaml
reverse_proxy:
  disabled_header:
    referer_header: true    # Removes 'Referer' (prevents leaking your Tuliprox URL/Domain)
    x_header: true          # Removes all 'X-*' headers (e.g., X-Forwarded-For, X-Real-IP)
    cloudflare_header: true # Removes 'CF-*' headers (hides Cloudflare origin details)
    custom_header:          # List of additional specific headers to be dropped
      - "X-Powered-By"
      - "my-custom-tracker"
```

### Parameter Details

| Parameter | Type | Default | Technical Impact |
| :--- | :--- | :--- | :--- |
| `referer_header` | Bool | `false` | Suppresses the source of the request. Prevents your Tuliprox instance from being logged at the provider. |
| `x_header` | Bool | `false` | **Critical:** Wildcard-strips all headers starting with `X-`. These are the most common indicators used for proxy detection. |
| `cloudflare_header`| Bool | `false` | Removes Cloudflare-specific headers. Essential if Tuliprox itself is running behind a Cloudflare proxy. |
| `custom_header` | List | `[]` | Manual blacklist for headers not covered by the automatic toggles above (e.g., vendor-specific trackers). |



> **Security Note:** Many providers analyze headers for "restreaming" patterns. Combining `x_header: true` with a neutral `user_agent` in your `source.yml` provides the best protection against account flags.

## 5. Resource Retries (`resource_retry`)

This block defines the retry behavior when Tuliprox proxies static resources (logos, EPG images). It ensures transient network issues or temporary provider timeouts don't result in broken images for your clients.

```yaml
reverse_proxy:
  resource_retry:
    max_attempts: 3
    backoff_millis: 250
    backoff_multiplier: 1.5
    failover_redirect_patterns:
      - "service-abuse"
```

### Parameter Details

| Parameter | Type | Default | Technical Impact |
| :--- | :--- | :--- | :--- |
| `max_attempts` | Int (u8) | `3` | Maximum number of download tries before giving up. Minimum is `1`. |
| `backoff_millis` | Duration | `250` | Initial wait time (ms) after the first failure. |
| `backoff_multiplier`| Float | `1.5` | Factor by which the delay grows. `> 1.0` creates an exponential backoff. |
| `failover_redirect_patterns`| List | `[]` | Regex patterns to identify "Abuse" or "Blocked" redirect URLs from providers. |

### Technical Background
* **Exponential Backoff:** The wait time for each attempt is calculated as:  
  $$ \text{delay} = \text{backoff\_millis} \times (\text{backoff\_multiplier}^{\text{attempt}-1}) $$
* **Failover Logic:** Providers sometimes redirect blocked requests (HTTP 302) to a "service-abuse.png" image. If a redirect URL matches a pattern in `failover_redirect_patterns`, Tuliprox treats it as a hard failure and triggers a retry instead of serving the abuse image.
* **Smart Handling:** If an upstream server sends a `Retry-After` header, Tuliprox prioritizes that value over the local backoff calculation.

---

## 6. GeoIP Resolution (`geoip`)

This block enables local IP-to-Country mapping. It allows the Tuliprox Web UI to display country flags in the **"Active Streams"** tab, providing immediate visual feedback on client locations.

```yaml
reverse_proxy:
  geoip:
    enabled: true
    url: "https://raw.githubusercontent.com/sapics/ip-location-db/refs/heads/main/asn-country/asn-country-ipv4.csv"
```

### Parameter Details

| Parameter | Type | Default | Technical Impact |
| :--- | :--- | :--- | :--- |
| `enabled` | Bool | `false` | Global switch for GeoIP resolution. |
| `url` | String | *(Optional)* | Source URL for the GeoIP CSV database. |



### Technical Background
* **Data Format:** Tuliprox requires a CSV format with exactly three columns:  
  `range_start, range_end, country_code`  
  *Example:* `1.0.0.0, 1.0.0.255, AU`
* **Performance:** The database is loaded into a high-speed memory-mapped structure to ensure that resolving client locations adds zero latency to stream processing.
* **Automation:** To keep the data accurate, use the `GeoIpUpdate` task type within the `schedules` block. This periodically downloads and rebuilds the local binary lookup file.
* **Privacy:** All resolution happens locally on your server; no client IPs are ever sent to external third-party APIs for location lookups.

The CSV file must have exactly 3 columns: `range_start,range_end,country_code`. (The DB is periodically updated via the
`schedules` block using the `GeoIpUpdate` task type).

---

&nbsp;

# Additional Information
## Session TTLs for HLS (`.m3u8`) & Catchup
HLS streams do not consist of an endless TCP pipe. Instead, the player downloads small `.ts` segments every few seconds (e.g., `seg1.ts`, `seg2.ts`).

If Tuliprox released and re-acquired the provider slot for every single segment, providers would block the account for "Account Hopping" or spam. Tuliprox simulates a continuous session:
* `hls_session_ttl_secs: 15`: After a `.ts` segment finishes downloading, the physical slot to the provider is closed, but the "Virtual Slot" for this specific user remains reserved for 15 seconds. No other user can steal this slot during this window. Channel switches from the same client can immediately take over the reservation.
* The same principle applies to Archive/Catchup TV (`catchup_session_ttl_secs: 45`), which shares the same fragmentation and seeking issues.

## Shared Live Streams
Tuliprox can share a live stream (`share_live_streams: true` in the target options of `source.yml`). If 5 users watch the same Live-TV channel, Tuliprox pulls the stream only 1x from the provider and multicasts the bytes locally to 5 clients.
To ensure a user who tunes in 10 seconds later doesn't get player errors due to missing I-Frames/Keyframes, Tuliprox continuously keeps the last X Megabytes (`shared_burst_buffer_mb`, default `12`) in RAM. It fires this burst buffer at new subscribers so their decoders can instantly synchronize.

## The "VLC Seek Problem" & Grace Periods
When a user fast-forwards or rewinds a VOD, the player calculates the new byte offset, drops the old TCP connection, and immediately fires a new HTTP GET request (with a `Range` header) to Tuliprox.

## The "VLC Seek Problem" & Grace Periods

When a user fast-forwards or rewinds a VOD, the player calculates the new byte offset, drops the old TCP connection, and
immediately fires a new HTTP GET request (with a `Range` header) to Tuliprox.

**The Problem:** It takes milliseconds to seconds for the upstream provider to realize the old connection is dead. If you have
a `max_connections: 1` limit at the provider, they will view this new seek-request as a *second concurrent stream* and reject
it with an HTTP 509 (Bandwidth Exceeded) or HTTP 401 error.

**The Tuliprox Solution:**

* `grace_period_millis: 2000`: Tuliprox grants the user a temporary over-allocation (Grace) for exactly this duration.
* `grace_period_hold_stream: true`: Tuliprox artificially holds back the video data to the client, waiting for the grace check
  to finish, so it doesn't trigger the provider prematurely.
* After the milliseconds expire, Tuliprox checks internally: Is the old connection truly gone now? If Yes ➔ Data flows.
  If No ➔ The new connection is hard-killed (serving the `user_connections_exhausted.ts` video) because the user is actually
  illegally watching twice.
* `grace_period_timeout_secs: 4`: A hard timeout limit for overlapping "ghost sessions" to expire.
