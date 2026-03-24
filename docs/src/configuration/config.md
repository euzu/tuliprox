# 🏛️ config.yml (Core System)

The `config.yml` is the primary configuration file of Tuliprox. It dictates the engine's core runtime behavior, memory management,
caching mechanisms, background schedulers, and external integrations (like HDHomeRun, GeoIP, and the Web UI).

## Top-level entries

```yaml
process_parallel: false
disk_based_processing: false
storage_dir: ./data
default_user_agent: Tuliprox/...
backup_dir: ./data/backup
user_config_dir: ./data/user
mapping_path: mapping.yml
template_path: template.yml
update_on_boot: false
config_hot_reload: false
accept_insecure_ssl_certificates: false
sleep_timer_mins: null
connect_timeout_secs: 10
user_access_control: false
custom_stream_response_path: null
custom_stream_response_timeout_secs: 0

api:
web_ui:
log:
schedules:
messaging:
video:
proxy:
ipcheck:
hdhomerun:
library:
reverse_proxy:
metadata_update:
```

### 1. Global System & Storage Settings (Flat Keys)

| Parameter | Type | Required | Default | Technical Impact & Background |
| :--- | :--- | :---: | :--- | :--- |
| `process_parallel` | Bool | No | `false` | Activates multi-threading during playlist processing. **Background:** If you have 5 providers, Tuliprox processes them sequentially by default. Setting this to `true` processes all 5 simultaneously using multiple CPU cores. *Warning:* This establishes parallel downloads to your providers. Verify this does not violate your provider's connection limits! |
| `disk_based_processing` | Bool | No | `false` | **Tradeoff Guidance:** Normally, Tuliprox loads playlists into RAM. With `true`, every chunk is manipulated directly on disk (using a B+Tree database). **Use this on low-end hardware (e.g., Raspberry Pi) or with massive playlists (>500k streams)** to prevent Out-Of-Memory crashes. It significantly increases Disk I/O load, so it is slower but much safer for tight memory footprints. |
| `storage_dir` | String | No | `./data` | Root directory for all runtime data (B+Tree databases, downloads, caches). Relative paths are resolved against the Tuliprox Home Directory. |
| `default_user_agent` | String | No | `Tuliprox/...` | Fallback HTTP `User-Agent` used for upstream provider requests if the input definition or client request does not explicitly provide one. |
| `backup_dir` | String | No | `{storage_dir}/backup` | Storage location for config backups (e.g., triggered via "Save Configuration" in the Web UI). |
| `user_config_dir` | String | No | `{storage_dir}/user` | Storage location for user-specific configurations (like favorites or custom bouquets created via the Web UI). |
| `mapping_path` | String | No | `mapping.yml` | Path to the mapping file. **Pro-Tip:** If you specify a folder path here (e.g., `./config/mappings/`), Tuliprox loads *all* `.yml` files in that folder in alphanumeric order! Ideal for structuring complex setups. |
| `template_path` | String | No | `template.yml` | Path to the template macro file. Specifying a folder here is also possible and highly recommended. |
| `update_on_boot` | Bool | No | `false` | Forces Tuliprox to immediately query all providers and rebuild all playlists upon startup. If `false`, the proxy serves the local DB cache from the last run until the scheduler triggers the next update. |
| `config_hot_reload` | Bool | No | `false` | **Background:** Spawns a filesystem watcher for `mapping.yml` and `api-proxy.yml`. Upon saving, mappings and user credentials become active immediately *without* requiring a server restart. |
| `accept_insecure_ssl_certificates` | Bool | No | `false` | Set to `true` if your upstream provider uses expired, self-signed, or improperly configured HTTPS certificates. Otherwise, the HTTP client drops the connection securely. |
| `sleep_timer_mins` | Int | No | `null` | Automatic kill-switch for proxied streams. Forcibly terminates active stream connections after X minutes (Protects against users falling asleep with the TV on). |
| `connect_timeout_secs` | Int | No | `10` | Maximum time (in seconds) Tuliprox waits to establish the initial TCP connection to a provider. `0` disables the timeout (Warning: risk of hanging threads!). |
| `user_access_control` | Bool | No | `false` | **Security:** If `true`, Tuliprox actively enforces `status` (Active/Banned), `exp_date`, and `max_connections` constraints for users defined in `api-proxy.yml`. If false, those fields are ignored. |
| `custom_stream_response_path` | String | No | `null` | Directory path where Tuliprox looks for custom fallback `.ts` files (e.g., `user_connections_exhausted.ts`, `channel_unavailable.ts`). See section [Custom Stream Response](#custom-stream-responses-fallback-videos) for more details. |
| `custom_stream_response_timeout_secs` | Int | No | `0` | Hard timeout (in seconds) that forces the fallback video stream to terminate to prevent infinite bandwidth usage. `0` means endless loop. |

### 2. Subsections (Object Keys)

| Block | Description | Link |
| :--- | :--- | :--- |
| `api` | Internal web server binding settings. | [See section](#3-api-server-api) |
| `web_ui` | Web Dashboard, RBAC, and Authentication. | [See section](#4-web-ui--administration-web_ui) |
| `log` | Console output verbosity and sanitization. | [See section](#5-logging-log) |
| `schedules` | Automated background tasks (Cronjobs). | [See section](#6-schedules-schedules) |
| `messaging` | Webhooks & Push-Notifications (Telegram, Discord, etc.). | [See section](#7-messaging-messaging) |
| `video` | Extension mapping and Web UI download behavior. | [See section](#8-video--web-search-video) |
| `proxy` | SOCKS5/HTTP proxy settings for outgoing requests. | [See section](#9-outgoing-proxy-proxy) |
| `ipcheck` | IP detection to verify in the Web UI which public IP Tuliprox is currently using. | [See section](#10-ip-check-ipcheck) |
| `hdhomerun` | Virtual DVB-C/T network tuner emulation. | [See section](#11-hdhomerun-emulation-hdhomerun) |
| `library` | Local Media Library integration. | [See Local Library](../local-library.md) |
| `reverse_proxy` | Streaming buffers, rate limits, caching. | [See Reverse Proxy](./reverse-proxy.md) |
| `metadata_update` | TMDB matching, FFprobe processing, Job Queues. | [See Metadata Update](./metadata-update.md) |

*(Note: The advanced topics **Local Library**, **Reverse Proxy** and **Metadata Update** are extremely extensive and have their own
dedicated subchapters. Here we cover the global base settings.)*

---

## 3. API Server (`api`)

Controls the internal web server of Tuliprox. This does *not* dictate the public URLs given to clients (those belong in
`api-proxy.yml`), but rather the physical socket binding on your host machine.

```yaml
api:
  host: 0.0.0.0
  port: 8901
  web_root: ./web
```

| Parameter | Type | Required | Default | Technical Impact & Background |
| :--- | :--- | :---: | :--- | :--- |
| `host` | String | No | `0.0.0.0` | Bind interface. `0.0.0.0` listens on all network cards. `127.0.0.1` restricts access to localhost (useful if you force traffic through a local Nginx/Traefik reverse proxy). |
| `port` | Int | No | `8901` | The listening port for proxy streams, the Web UI, and all REST APIs. |
| `web_root` | String | No | `./web` | Physical path to the compiled Wasm/JS/CSS frontend assets of the Web UI. |

---

## 4. Web UI & Administration (`web_ui`)

Tuliprox ships with a comprehensive Web Dashboard containing a Web Player, Playlist Editor, User Management, and Live Logs.

```yaml
web_ui:
  enabled: true
  user_ui_enabled: true
  path: admin
  player_server: default
  kick_secs: 90
  combine_views_stats_streams: false
  auth:
    enabled: true
    issuer: tuliprox
    secret: "YOUR_SECRET_JWT_KEY_HERE"
    token_ttl_mins: 30
    userfile: user.txt
    groupfile: groups.txt
```

### Web UI Parameters

| Parameter | Type | Default | Technical Impact & Background |
| :--- | :--- | :--- | :--- |
| `enabled` | Bool | `true` | Completely toggles the Web Dashboard and its REST API endpoints on or off. |
| `user_ui_enabled` | Bool | `true` | Allows standard proxy users (not just admins) to log into the Web UI to manage their own favorite bouquets. |
| `path` | String | `""` | Base path for the UI (e.g., `admin`). Critical for reverse proxy subfolder setups so assets load from `example.com/admin/assets/`. |
| `player_server` | String | `default` | Determines which virtual server block from `api-proxy.yml` is used to construct the streaming URLs when playing a channel directly within the Web UI player. |
| `kick_secs` | Int | `90` | **Background:** When you kick a user via the Dashboard, they are not only disconnected but hard-blocked at the IP/User level for X seconds. This prevents their IPTV player's auto-reconnect logic from instantly stealing the provider slot back. |
| `combine_views_stats_streams` | Bool | `false` | Combines the "Server Stats" and "Active Streams" views into a single unified window in the UI. |

### Authentication & RBAC (`web_ui.auth`)

Tuliprox features a robust Role-Based Access Control (RBAC) system.

| Parameter | Type | Required | Default | Technical Impact & Background |
| :--- | :--- | :---: | :--- | :--- |
| `secret` | String | Yes | `(Random)` | Critical for JWT cookie encryption. Generate a random 32-char hex string (e.g., `openssl rand -hex 16`). If omitted, Tuliprox generates one in-memory, but all active logins will invalidate on every server restart! |
| `token_ttl_mins` | Int | No | `30` | How long a login session remains valid. Setting this to `0` makes the token effectively valid for 100 years (Extreme Security Risk!). |
| `userfile` | String | No | `user.txt` | The file storing Admins and Web Users. |
| `groupfile` | String | No | `groups.txt` | The RBAC (Role-Based Access Control) definition file. |

#### Structure of `user.txt`

This file stores users, Argon2 password hashes, and RBAC groups. Generate secure passwords via CLI: `./tuliprox --genpwd`.
Format: `username:argon2_hash[:group1,group2]`

```text
# A normal Admin (No group specified = Fallback to built-in Admin role)
admin:$argon2id$v=19$m=19456,t=2,p=1$QUp...

# An Editor assigned to specific permission groups
editor:$argon2id$v=19$m=19456,t=2,p=1$Y2F...:playlist_manager,user_manager
```

#### Structure of `groups.txt`

Define group permissions here. An editor might be allowed to update playlists (`playlist.write`) but forbidden from viewing or
changing `config.yml` (`config.read`).
Format: `group_name:permission1,permission2,...`

```text
viewer:config.read,source.read,playlist.read,system.read,library.read
playlist_manager:playlist.read,playlist.write,source.read
```

Available Permissions: `config.read/write`, `source.read/write`, `user.read/write`, `playlist.read/write`, `library.read/write`,
`system.read/write`, `epg.read/write`. Note: Write does not imply Read. A group must explicitly grant both if users need to view
and edit content.

---

## 5. Logging (`log`)

Controls console output verbosity and sanitization.

```yaml
log:
  sanitize_sensitive_info: true
  log_active_user: false
  log_level: info
```

| Parameter | Type | Default | Technical Impact & Background |
| :--- | :--- | :--- | :--- |
| `log_level` | String | `info` | Verbosity. Possible values: `trace`, `debug`, `info`, `warn`, `error`. Can be overridden per-module (e.g., `tuliprox=debug,hyper_util=warn`). |
| `sanitize_sensitive_info` | Bool | `true` | **Critical:** Masks passwords, provider URLs, and external client IPs in the logs with `***`. Highly recommended to keep `true` so you can safely share logs on GitHub/Discord for support without leaking credentials. |
| `log_active_user` | Bool | `false` | Periodically writes the current active client connection count as an INFO message to the log file. |

---

## 6. Schedules (`schedules`)

Automate your background updates here.
*Important:* Tuliprox uses standard cron syntax, **but strictly includes seconds as the first field**
(7 fields in total: `Sec Min Hour Day Month Day-of-Week Year`)!

```yaml
schedules:
  # Every morning at 08:00:00 (Seconds = 0, Minutes = 0, Hours = 8)
  - schedule: "0 0 8 * * * *"
    type: PlaylistUpdate
    targets: [ "m3u_target", "xtream_target" ] # Optional: Only update specific targets

  # Every evening at 20:00:00
  - schedule: "0 0 20 * * * *"
    type: LibraryScan

  # Every Monday at 04:00:00
  - schedule: "0 0 4 * * 1 *"
    type: GeoIpUpdate
```

| Parameter | Type | Description |
| :--- | :--- | :--- |
| `schedule` | String | Cron expression with 7 fields (Seconds included at the start). |
| `type` | Enum | The task to execute. Valid values: `PlaylistUpdate`, `LibraryScan`, `GeoIpUpdate`. |
| `targets` | List | *(Optional, only for PlaylistUpdate)* List of target names to restrict the update to. If omitted, all enabled targets are updated. |

---

## 7. Messaging (`messaging`)

Tuliprox can proactively notify you via Push-Notifications when updates fail, finish, or when specific channels are added/removed
from a watched group. **Why is this useful?** Because it allows you to instantly detect upstream provider issues or simply let you
know when new movies are added to your playlist.

*(Note: You must explicitly opt-in via `notify_on` to receive messages!)*

```yaml
messaging:
  notify_on:[ "info", "stats", "error", "watch" ]
  telegram:
    markdown: true
    bot_token: "<TOKEN>"
    chat_ids:
      - "<CHAT_ID>"
      - "<CHAT_ID>:<MESSAGE_THREAD_ID>" # For group topics
    templates:
      stats: 'file:///config/messaging_templates/telegram_stats.templ'
  discord:
    url: "<WEBHOOK_URL>"
  pushover:
    token: "<API_TOKEN>"
    user: "<USER_KEY>"
  rest:
    url: "https://my-api.local/alert"
    method: "POST"
    headers:
      - "Content-Type: application/json"
```

**Template Auto-File Behavior & Variables (Handlebars):**
You can reference custom formatting templates (`file://` or HTTP URLs). If the file doesn't exist, Tuliprox can automatically
populate the `messaging_templates/` folder with default templates if missing. Variables injected during runtime:

* `{{message}}`: The raw text.
* `{{kind}}`: The event type (`info`, `error`, `stats`, `watch`).
* `{{timestamp}}`: RFC3339 timestamp.
* `{{stats}}`: Array of metrics per Source/Input (`raw.groups`, `processed.channels`, `took`).
* `{{processing.errors}}`: A combined string of all errors encountered during the update run.
* `{{watch}}`: Contains data about added/removed items triggered by the `watch` event in your targets.

---

## 8. Video & Web Search (`video`)

Optional video-related behaviors, mostly utilized by the Web UI.

```yaml
video:
  web_search: "https://www.imdb.com/search/title/?title={}"
  extensions: ["mkv", "mp4", "avi", "ts", "webm"]
  download:
    directory: /tmp/tuliprox_downloads
    organize_into_directories: true
    episode_pattern: '.*(?P<episode>[Ss]\d{1,2}(.*?)[Ee]\d{1,2}).*'
```

* `extensions`: Defines which file endings Tuliprox categorizes as VOD/Video content when transforming M3U to Xtream.
* `web_search`: A template URL used in the Web UI to quickly search for a movie title (replaces `{}` with the title).
* `download`: Configuration for the Web UI's "Download Video" button.
  * `directory`: Where the downloaded files are saved.
  * `organize_into_directories`: If true, Tuliprox automatically creates neat subfolders for series.
  * `episode_pattern`: Crucial for the directory organization. It uses the mandatory Named Capture Group `(?P<episode>...)` in the
    Regex to identify and strip the episode identifier (e.g., `S01E01`) from the filename, ensuring all episodes of a show land in
    the exact same base-show folder.

---

## 9. Outgoing Proxy (`proxy`)

If Tuliprox itself must operate behind a corporate proxy or VPN (e.g., a Gluetun WireGuard container):

```yaml
proxy:
  url: socks5://192.168.1.6:8123
  username: "opt_user"
  password: "opt_password"
```

Setting this forces **every** outgoing request (Playlist downloads, TMDB API calls, FFprobe stream analysis, and Reverse-Proxy
Video Streaming) through this proxy.

---

## 10. IP-Check (`ipcheck`)

To verify in the Web UI which public IP Tuliprox is currently using (crucial when verifying VPN routing or diagnosing geo-blocks),
Tuliprox queries a detection API:

```yaml
ipcheck:
  url_ipv4: https://ipinfo.io/ip
```

You can also define `url_ipv6`, or generic `url` alongside `pattern_ipv4` or `pattern_ipv6` regexes to extract the IP from
complex JSON responses.

---

## 11. HDHomeRun Emulation (`hdhomerun`)

**Deep-Dive Feature:** Tuliprox can masquerade on the local network as a physical "SiliconDust HDHomeRun" DVB-C/T network tuner.
Media servers like **Plex**, **Jellyfin**, **Emby**, or **TVHeadend** will automatically discover Tuliprox via UPnP as a real
hardware antenna and ingest Live-TV natively into their Live-DVR systems.

Tuliprox utilizes standardized UPnP/SSDP (UDP Port 1900) and the proprietary SiliconDust UDP discovery protocol (UDP Port 65001).

```yaml
hdhomerun:
  enabled: true
  auth: false # If true, lineup.json requires Basic Auth (using the assigned user's credentials)
  devices:
    - name: hdhr1             # MUST match the 'device' name in the output target of source.yml!
      tuner_count: 4          # Tells Plex that 4 channels can be watched simultaneously
      port: 5004              # Unique TCP Port for this specific virtual antenna API
      device_id: "107ABCDF"   # 8-char hex code (Port 65001). If left blank, Tuliprox generates a valid one with correct checksums.
      device_udn: "uuid:..."  # Unique Device Name (Port 1900). Recommended to leave blank for auto-generation.
      friendly_name: "Tuliprox Living Room"
```

Both `device_id` and `device_udn` are necessary for Tuliprox to satisfy both the official SiliconDust apps and third-party UPnP
scanners simultaneously.

---

&nbsp;

## Additional Information

## Custom Stream Responses (Fallback Videos)

When a stream fails to load at the provider (HTTP 404/502) or a user reaches their connection limit, Tuliprox can seamlessly
substitute a fallback info-video (as a `.ts` stream) instead of brutally closing the TCP connection. Dropping connections often
causes hardware players or Smart TVs to freeze.

```yaml
custom_stream_response_path: /home/tuliprox/resources
custom_stream_response_timeout_secs: 20
```

| Name | Type | Default | Technical Impact & Background |
| :--- | :--- | :--- | :--- |
| `custom_stream_response_path` | String | `(Empty)` | Directory path where Tuliprox looks for exactly named `.ts` files. |
| `custom_stream_response_timeout_secs` | Int | `0` | Hard timeout (in seconds) that forces the fallback video stream to terminate to prevent infinite bandwidth usage. `0` means the fallback loops endlessly until the user switches channels. |

**Filenames searched for in the directory:**

* `channel_unavailable.ts` (Provider returns 404/502/Timeout)
* `user_connections_exhausted.ts` (User hit their `max_connections` limit)
* `provider_connections_exhausted.ts` (Provider has no free slots left)
* `low_priority_preempted.ts` (User was kicked by an Admin with higher priority)
* `user_account_expired.ts` (User's `exp_date` reached)
* `panel_api_provisioning.ts` (Loops while a new Provider Account is generated via Panel API)

**How it works:**
Tuliprox searches the specified folder for exactly named `.ts` files. If found, they are looped back to the client. The
`custom_stream_response_timeout_secs` parameter hard-kills the fallback stream after X seconds to prevent infinite bandwidth usage.

---
