# 🏛️ config.yml (Core System)

The `config.yml` is the primary configuration file of Tuliprox. It dictates the engine's core runtime behavior, memory
management,
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

### Global System & Storage Settings (Flat Keys)

| Parameter                             | Type   | Required | Default                | Technical Impact & Background                                                                                                                                                                                                                                                                                                                                                                                                                             |
|:--------------------------------------|:-------|:--------:|:-----------------------|:----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `process_parallel`                    | Bool   |    No    | `false`                | Activates multi-threading during playlist processing. **Background:** If you have 5 providers, Tuliprox processes them sequentially by default. Setting this to `true` processes all 5 simultaneously using multiple CPU cores. *Warning:* This establishes parallel downloads to your providers. If you process the same provider multiple times, each thread uses a connection. Keep in mind that you might hit the provider's `max-connections` limit! |
| `disk_based_processing`               | Bool   |    No    | `false`                | **Tradeoff Guidance:** Normally, Tuliprox loads playlists into RAM. With `true`, every chunk is manipulated directly on disk (using a B+Tree database). **Use this on low-end hardware (e.g., Raspberry Pi) or with massive playlists (>500k streams)** to prevent Out-Of-Memory crashes. It significantly increases Disk I/O load, so it is slower but much safer for tight memory footprints.                                                           |
| `storage_dir`                         | String |    No    | `./data`               | Root directory for all runtime data (B+Tree databases, downloads, caches). Relative paths are resolved against the Tuliprox Home Directory. Be aware that different configurations (e.g. user bouquets) alongside the playlists are stored in this directory.                                                                                                                                                                                             |
| `default_user_agent`                  | String |    No    | `Tuliprox/...`         | Fallback HTTP `User-Agent` used for upstream provider requests if the input definition or client request does not explicitly provide one.                                                                                                                                                                                                                                                                                                                 |
| `backup_dir`                          | String |    No    | `{storage_dir}/backup` | Storage location for config backups (e.g., triggered via "Save Configuration" in the Web UI).                                                                                                                                                                                                                                                                                                                                                             |
| `user_config_dir`                     | String |    No    | `{storage_dir}/user`   | Storage location for user-specific configurations (like favorites or custom bouquets created via the Web UI).                                                                                                                                                                                                                                                                                                                                             |
| `mapping_path`                        | String |    No    | `mapping.yml`          | Path to the mapping file. **Pro-Tip:** If you specify a folder path here (e.g., `./config/mappings/`), Tuliprox loads *all* `.yml` files in that folder in **alphanumeric** order and merges them. Note: This is a lexicographic sort, meaning `m_10.yml` comes before `m_2.yml`. Name files carefully (e.g., `m_01.yml`, `m_02.yml`).                                                                                                                    |
| `template_path`                       | String |    No    | `template.yml`         | Path to the template macro file. Specifying a folder here is also possible and highly recommended (see above).                                                                                                                                                                                                                                                                                                                                            |
| `update_on_boot`                      | Bool   |    No    | `false`                | Forces Tuliprox to immediately query all providers and rebuild all playlists upon startup. If `false`, the proxy serves the local DB cache from the last run until the scheduler triggers the next update.                                                                                                                                                                                                                                                |
| `config_hot_reload`                   | Bool   |    No    | `false`                | Spawns a filesystem watcher for `mapping.yml` and `api-proxy.yml`. Upon saving, mappings and user credentials become active immediately *without* requiring a server restart. **(See Bind-Mount Note below)**                                                                                                                                                                                                                                             |
| `accept_insecure_ssl_certificates`    | Bool   |    No    | `false`                | Set to `true` if your upstream provider uses expired, self-signed, or improperly configured HTTPS certificates. Otherwise, the HTTP client drops the connection securely.                                                                                                                                                                                                                                                                                 |
| `sleep_timer_mins`                    | Int    |    No    | `null`                 | Automatic kill-switch for proxied streams. Forcibly terminates active stream connections after X minutes (Protects against users falling asleep with the TV on).                                                                                                                                                                                                                                                                                          |
| `connect_timeout_secs`                | Int    |    No    | `10`                   | Maximum time (in seconds) Tuliprox waits to establish the initial TCP connection to a provider. `0` disables the timeout and the connection attempt continues until the provider closes it or a network timeout occurs (Warning: risk of hanging threads!).                                                                                                                                                                                               |
| `user_access_control`                 | Bool   |    No    | `false`                | **Security:** If `true`, Tuliprox actively enforces `status` (Active/Banned), `exp_date`, and `max_connections` constraints for users defined in `api-proxy.yml`. If false, those fields are ignored.                                                                                                                                                                                                                                                     |
| `custom_stream_response_path`         | String |    No    | `null`                 | Directory path where Tuliprox looks for custom fallback `.ts` files. See section [Custom Stream Response](#custom-stream-responses-fallback-videos) for exact filenames.                                                                                                                                                                                                                                                                                  |
| `custom_stream_response_timeout_secs` | Int    |    No    | `0`                    | Hard timeout (in seconds) that forces the fallback video stream to terminate to prevent infinite bandwidth usage. `0` means endless loop.                                                                                                                                                                                                                                                                                                                 |

---

#### ⚠️ Important: `config_hot_reload` & Bind-Mounts

If you use **Bind-Mounts** (e.g., in `fstab` or Docker), the filesystem watcher may report the **original source path**
instead of your mount point.

* **Example Setup:** `/home/tuliprox/config` (Source) → `/config` (Mount Point).
* **Behavior:** If you configure Tuliprox to watch `/config`, the watcher might still trigger events using the path
  `/home/tuliprox/config`.
* **Solution:** Ensure your internal Tuliprox paths match the paths reported by the OS kernel to ensure the hot-reload
  trigger fires correctly.

### Subsections (Object Keys)

| Block             | Description                                                                       | Link                                            |
|:------------------|:----------------------------------------------------------------------------------|:------------------------------------------------|
| `api`             | Internal web server binding settings.                                             | [See section](#1-api-server-api)                |
| `web_ui`          | Web Dashboard, RBAC, and Authentication.                                          | [See section](#2-web-ui--administration-web_ui) |
| `log`             | Console output verbosity and sanitization.                                        | [See section](#3-logging-log)                   |
| `schedules`       | Automated background tasks (Cronjobs).                                            | [See section](#4-schedules-schedules)           |
| `messaging`       | Webhooks & Push-Notifications (Telegram, Discord, etc.).                          | [See section](#5-messaging-messaging)           |
| `video`           | Extension mapping and Web UI download behavior.                                   | [See section](#6-video--web-search-video)       |
| `proxy`           | SOCKS5/HTTP proxy settings for outgoing requests.                                 | [See section](#7-outgoing-proxy-proxy)          |
| `ipcheck`         | IP detection to verify in the Web UI which public IP Tuliprox is currently using. | [See section](#8-ip-check-ipcheck)              |
| `hdhomerun`       | Virtual DVB-C/T network tuner emulation.                                          | [See section](#9-hdhomerun-emulation-hdhomerun) |
| `library`         | Local Media Library integration.                                                  | [See Local Library](./local-library.md)         |
| `reverse_proxy`   | Streaming buffers, rate limits, caching.                                          | [See Reverse Proxy](./reverse-proxy.md)         |
| `metadata_update` | TMDB matching, FFprobe processing, Job Queues.                                    | [See Metadata Update](./metadata-update.md)     |

*(Note: The advanced topics **Local Library**, **Reverse Proxy** and **Metadata Update** are extremely extensive and
have their own
dedicated subchapters. Here we cover the global base settings.)*

---

## 1. API Server (`api`)

Controls the internal web server of Tuliprox. This does *not* dictate the public URLs given to clients (those belong in
`api-proxy.yml`), but rather the physical socket binding on your host machine.

```yaml
api:
  host: 0.0.0.0
  port: 8901
  web_root: ./web
```

| Parameter  | Type   | Required | Default   | Technical Impact & Background                                                                                                                                                |
|:-----------|:-------|:--------:|:----------|:-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `host`     | String |    No    | `0.0.0.0` | Bind interface. `0.0.0.0` listens on all network cards. `127.0.0.1` restricts access to localhost (useful if you force traffic through a local Nginx/Traefik reverse proxy). |
| `port`     | Int    |    No    | `8901`    | The listening port for proxy streams, the Web UI, and all REST APIs.                                                                                                         |
| `web_root` | String |    No    | `./web`   | Physical path to the compiled Wasm/JS/CSS frontend assets of the Web UI.                                                                                                     |

---

## 2. Web UI & Administration (`web_ui`)

Tuliprox ships with a comprehensive Web Dashboard containing a Web Player, Playlist Editor, User Management, and Live
Logs.

```yaml
web_ui:
  enabled: true
  user_ui_enabled: true
  path: admin
  player_server: default
  kick_secs: 90
  combine_views_stats_streams: false
  content_security_policy:
    enabled: true
    custom-attributes:
      - "style-src 'self' 'nonce-{nonce_b64}'"
      - "img-src 'self' data:"
  auth:
    enabled: true
    issuer: tuliprox
    secret: "YOUR_SECRET_JWT_KEY_HERE"
    token_ttl_mins: 30
    userfile: user.txt
    groupfile: groups.txt
```

### 2.1 Web UI Parameters

| Parameter                     | Type   | Default   | Technical Impact & Background                                                                                                                                                                                                                      |
|:------------------------------|:-------|:----------|:---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `enabled`                     | Bool   | `true`    | Completely toggles the Web Dashboard and its REST API endpoints on or off.                                                                                                                                                                         |
| `user_ui_enabled`             | Bool   | `true`    | Allows standard proxy users (not just admins) to log into the Web UI to manage their own favorites/bouquets.                                                                                                                                       |
| `path`                        | String | `""`      | Base path for the UI (e.g., `admin`). Critical for reverse proxy subfolder setups so assets load from `example.com/admin/assets/`.                                                                                                                 |
| `player_server`               | String | `default` | Determines which virtual server block from `api-proxy.yml` is used to construct the streaming URLs when playing a channel directly within the Web UI player.                                                                                       |
| `kick_secs`                   | Int    | `90`      | **Background:** When you kick a user via the Dashboard, they are not only disconnected but hard-blocked at the IP/User level for X seconds. This prevents their IPTV player's auto-reconnect logic from instantly stealing the provider slot back. |
| `combine_views_stats_streams` | Bool   | `false`   | Combines the "Server Stats" and "Active Streams" views into a single unified window in the UI.                                                                                                                                                     |

### 2.2 Content Security Policy (`content_security_policy`)

This block enhances security by restricting which resources the browser is allowed to load.

* **Default Directives:** When `enabled: true`, Tuliprox automatically applies:
  * `default-src 'self'`
  * `script-src 'self' 'wasm-unsafe-eval' 'nonce-{nonce_b64}'`
  * `frame-ancestors 'none'`
* **Customization:** Use `custom-attributes` to add specific rules (e.g., allowing external channel logos via
  `img-src`).

### 2.3 Authentication & RBAC (`auth`)

Tuliprox features a robust Role-Based Access Control (RBAC) system.

| Parameter        |  Type  | Default      | Technical Impact & Background                                                                                                                                                                                                                                                          |
|:-----------------|:------:|:-------------|:---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `enabled`        |  Bool  | `true`       | Master switch for UI authentication. Disabling this exposes the dashboard to anyone with network access.                                                                                                                                                                               |
| `issuer`         | String | `tuliprox`   | The identifier for the JWT "iss" field.                                                                                                                                                                                                                                                |
| `secret`         | String | `(Random)`   | Critical for JWT encryption. Use a static 64-character hex string using Node.js ([see Secret Generation](#jwt-secret-generation)) to keep sessions valid across restarts. If omitted, Tuliprox generates one in-memory, but all active logins will invalidate on every server restart! |
| `token_ttl_mins` |  Int   | `30`         | How long a login session remains valid. Setting this to `0` makes the token effectively valid for 100 years (Extreme Security Risk!).                                                                                                                                                  |
| `userfile`       | String | `user.txt`   | The file storing Admins and Web Users.                                                                                                                                                                                                                                                 |
| `groupfile`      | String | `groups.txt` | The RBAC (Role-Based Access Control) definition file.                                                                                                                                                                                                                                  |

#### Technical Background

* **File Resolution:** If `userfile` is not defined with an absolute path, Tuliprox automatically looks for it within
  your global `config_dir`. Ensure the process has sufficient read permissions for this directory.
* **RBAC (Role-Based Access Control):** This system manages **Web UI access levels** (e.g., Admins vs. Bouquet-Editors)
  by assigning users to specific permission groups defined in `groups.txt`.

### Structure of `user.txt`

This file stores users, Argon2 password hashes, and RBAC groups. Generate secure passwords via CLI:
`./tuliprox --genpwd`.
The userfile has the following format per line: `username:argon2_hash[:group1,group2]`

Example:

```text

# A normal Admin (No group specified = Fallback to built-in Admin role)
admin:$argon2id$v=19$m=19456,t=2,p=1$QUp...

# An Editor assigned to specific permission groups
editor:$argon2id$v=19$m=19456,t=2,p=1$Y2F...:playlist_manager,user_manager
```

### Structure of `groups.txt`

Define group permissions here. An editor might be allowed to update playlists (`playlist.write`) but forbidden from
viewing or changing `config.yml` (`config.read`).
Format: `group_name:permission1,permission2,...`

```text
viewer:config.read,source.read,playlist.read,system.read,library.read
playlist_manager:playlist.read,playlist.write,source.read
```

Available Permissions: `config.read/write`, `source.read/write`, `user.read/write`, `playlist.read/write`,
`library.read/write`,
`system.read/write`, `epg.read/write`, `download.read/write`. Note: Write does not imply Read. A group must explicitly grant both if users need
to view
and edit content.

### Generating Passwords

To ensure security, Tuliprox does not store plain-text passwords. You must generate an encrypted hash using the built-in
generator:

**Local Installation:**

```shell
./tuliprox --genpwd
```

**Docker Installation:**

```shell
docker container exec -it tuliprox ./tuliprox --genpwd
```

After running the command, copy the generated Argon2id string and manually paste it into your `userfile` next to the
desired username.

### JWT Secret Generation

As mentioned in the table above, a static `secret` is required to keep sessions valid across restarts.
You can generate a secure 32-byte (64-character hex) key using Node.js:

```bash
node -e "console.log(require('crypto').randomBytes(32).toString('hex'))"
```

### Troubleshooting Access

* **File Location:** If Tuliprox cannot find the file, check your `storage_dir` or
  provide an absolute path in the `userfile` parameter.
* **Permissions:** Ensure the Tuliprox process has read access to the `userfile` and `groupfile`.
* **Session Invalidated:** If you change the `secret` in `config.yml`, all currently
  logged-in users will be forced to log in again.

---

## 3. Logging (`log`)

Controls console output verbosity and sanitization.

```yaml
log:
  sanitize_sensitive_info: true
  log_active_user: false
  log_level: info
```

| Parameter                 | Type   | Default | Technical Impact & Background                                                                                                                                                                                           |
|:--------------------------|:-------|:--------|:------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `log_level`               | String | `info`  | Verbosity. Possible values: `trace`, `debug`, `info`, `warn`, `error`. Can be overridden per-module (e.g., `tuliprox=debug,hyper_util=warn`).                                                                           |
| `sanitize_sensitive_info` | Bool   | `true`  | **Critical:** Masks passwords, provider URLs, and external client IPs in the logs with `***`. Highly recommended to keep `true` so you can safely share logs on GitHub/Discord for support without leaking credentials. |
| `log_active_user`         | Bool   | `false` | Periodically writes the current active client connection count as an INFO message to the log file.                                                                                                                      |

---

## 4. Schedules (`schedules`)

Automate your background updates to keep your playlists, library, and Geo-IP data synchronized.

> **⚠️ Provider Safety Warning:** Do not schedule updates to run every second or minute.
> Excessive requests can lead to your IP being banned by your provider.
> Updating twice a day is generally sufficient for most use cases.

### Cron Syntax

Tuliprox uses a standard cron syntax but **strictly requires 7 fields**, starting with **seconds**:

```txt
# ┌──────────────────────────── second (0 - 59)
# │   ┌──────────────────────── minute (0 - 59)
# │   │   ┌──────────────────── hour (0 - 23)
# │   │   │    ┌─────────────── day of month (1 - 31)
# │   │   │    │   ┌─────────── month (1 - 12)
# │   │   │    │   │   ┌─────── day of week (0 - 6) (Sunday to Saturday; 7 is also Sunday)
# │   │   │    │   │   │   ┌─── year (optional, e.g., 2026)
# │   │   │    │   │   │   │
# sec min hour dom mon dow year
  0   0   8    *   *   *   *
```

### Configuration

In current versions, schedules are defined as a list of tasks.

```yaml
schedules:
  # Every morning at 08:00:00 (Playlist Update for specific targets)
  - schedule: "0 0 8 * * * *"
    type: PlaylistUpdate
    targets: [ "m3u_target", "xtream_target" ]

  # Every evening at 20:00:00 (Full Library Scan)
  - schedule: "0 0 20 * * * *"
    type: LibraryScan

  # Every Monday at 04:00:00 (Geo-IP Database Refresh)
  - schedule: "0 0 4 * * 1 *"
    type: GeoIpUpdate

  # Every 1st of the month at 04:00:00
  - schedule: "0 0 4 1 * * *"
    type: GeoIpUpdate
```

| Parameter  | Type   | Default          | Description                                                                                                                        |
|:-----------|:-------|:-----------------|:-----------------------------------------------------------------------------------------------------------------------------------|
| `schedule` | String | -                | Cron expression with 7 fields (Seconds included at the start).                                                                     |
| `type`     | Enum   | `PlaylistUpdate` | The task to execute. See [Task Types](#task-types) below.                                                                          |
| `targets`  | List   | -                | *(Optional, only for PlaylistUpdate)* List of target names to restrict the update to. If omitted, all enabled targets are updated. |

### Task Types

* **`PlaylistUpdate`**: Triggers the processing pipeline for your target playlists.
  It downloads provider data, applies filters/maps, and updates the local databases.
* **`LibraryScan`**: Initiates a scan of the local media library.
  *Note: This requires the `library` configuration to be enabled.*
* **`GeoIpUpdate`**: Downloads the latest MaxMind/Geo-IP database and rebuilds
  the internal binary file. *Note: Requires `reverse_proxy.geoip.enabled: true`.*

---

## 5. Messaging (`messaging`)

Tuliprox can proactively notify you via Push-Notifications when updates fail, finish, or when specific channels are
added/removed
from a watched group. **Why is this useful?** Because it allows you to instantly detect upstream provider issues or
simply let you
know when new movies are added to your playlist.

### 5.1 Configuration & Opt-In

Messaging is strictly **opt-in**. You must explicitly define which event types should trigger a notification using the
`notify_on` list.

**Available Event Types:**

* `info`: General operational information.
* `stats`: Summary of processed items and performance metrics after a run.
* `error`: Alerts when processing or source fetching fails.
* `watch`: Triggered by changes in monitored groups/targets.

```yaml
messaging:
  notify_on: [ "info", "stats", "error", "watch" ]

  # Telegram: Supports Markdown and Group Topics
  telegram:
    markdown: true
    bot_token: "<TOKEN>"
    chat_ids:
      - "<CHAT_ID>"
      - "<CHAT_ID>:<MESSAGE_THREAD_ID>" # Use colon to target specific Discord-like topics/threads
    templates:
      stats: 'file:///config/messaging_templates/telegram_stats.templ'

  # Discord: Webhook integration
  discord:
    url: "<WEBHOOK_URL>"
    templates:
      info: '{"content": "🚀 Tuliprox Info: {{message}}"}'

  # Pushover: Simple mobile push alerts
  pushover:
    token: "<API_TOKEN>"
    user: "<USER_KEY>"
    # url: "https://api.pushover.net/1/messages.json" # Optional default

  # REST: Generic Webhook/API support
  rest:
    url: "https://my-api.local/alert"
    method: "POST" # Optional, defaults to POST
    headers:
      - "Content-Type: application/json"
    templates:
      error: '{"text": "Alert: {{message}}", "type": "{{kind}}"}'
```

### 5.2 Templating (Handlebars)

For **Telegram**, **Discord**, and **REST**, Tuliprox uses [Handlebars](https://handlebarsjs.com/) to format message
bodies. This allows for rich, structured notifications (e.g., Discord Embeds or Markdown tables).

#### Loading Methods

1. **Raw String:** Define the template directly in your `config.yaml` (best for simple one-liners).
2. **URI:** Reference a local file (`file://...`) or a remote resource (`http://...`).

> **Note:** If you save your configuration via the **Web UI**, raw template strings are automatically moved to
> individual files in `/config/messaging_templates/` to keep your main configuration clean.

#### Available Context Variables

The Handlebars engine provides a rich context object. Depending on the `kind` of notification, different variables are
populated:

* `{{message}}`: The primary text payload. Used for human-readable `info` messages or the summary of an `error`.
* `{{kind}}`: The event category (`info`, `stats`, `error`, `watch`). Use this in Handlebars helpers (e.g.,
  `{{#if (eq kind "error")}}`) to create conditional layouts.
* `{{timestamp}}`: The event occurrence time in UTC (ISO 8601 / RFC3339 format).
* `{{stats}}`: **Execution Metrics & Performance.** A comprehensive list of statistics for the last update cycle,
  covering both ingestion and generation phases.
  * **Structure:** A nested list containing `inputs` (Source-level metrics) and `targets` (Output-level metrics).
  * **Access:** Iterate over the main list to access individual source or target reports. Use nested loops for
    detailed input/target breakdowns: `{{#each stats}} {{#each inputs}} ... {{/each}} {{/each}}`.
  * **Key Properties:**
    * **Metadata:** Access `name`, `type`, and `took` (execution duration) for each entry.
    * **Error Tracking:** `errors` provides a count of failed items or connection issues during that specific phase.
    * **Filtering Delta:** Compare `raw` counts (total items received from the provider) vs. `processed` counts (
      items that survived your Mapping DSL and filters) to monitor your "Red Thread" efficiency.
* `{{processing}}`: **Engine State & Telemetry.** Provides insight into the *internal execution environment* during the
  task. It includes data on memory allocation peaks, active worker threads, and non-blocking diagnostic warnings.
  * **Access:** Access properties directly via dot-notation (e.g., `{{processing.memory_peak_mb}}`). Use this to
    monitor system health and resource consumption during heavy mapping cycles.
* `{{watch}}`: **Change Tracking Data.** Specifically available for the `watch` event kind. It contains a diff-style
  object showing exactly which groups or channels were added, removed, or modified compared to the previous state.
  * **Access:** Iterate over the change sets using loops. Common keys include `added`, `removed`, and `modified`.
  * **Example:** Use `{{#each watch.added}} • {{name}} {{/each}}` to list all new channels detected in the monitored
    groups.

#### Template Examples

**Telegram (Markdown Report):**

```handlebars
*🔄 Playlist Update Report*

{{#each stats}}
*📥 Source Stats*
{{#each inputs}}
• *{{name}}* (`{{type}}`)
  ⏱️ Took: `{{took}}` | ❌ Errors: `{{errors}}`
  📊 `{{raw.groups}}`/`{{raw.channels}}` ➔ *`{{processed.groups}}`*/*`{{processed.channels}}`*
{{/each}}
{{/each}}
```

**Discord (Complex Embed):**

```handlebars
{
  "content": "Tuliprox Notification",
  "embeds": [{
    "title": "Event: {{kind}}",
    "description": "{{message}}",
    "color": 3447003,
    "fields": [
      {{#each stats}}
      { "name": "Source {{@index}}", "value": "Processed {{#each inputs}}{{name}} {{/each}}", "inline": false }
      {{/each}}
    ],
    "footer": { "text": "Reported at {{timestamp}}" }
  }]
}
```

---

## 6. Video & Web Search (`video`)

Optional video-related behaviors, mostly utilized by the Web UI.

```yaml
video:
  web_search: "https://www.imdb.com/search/title/?title={}"
  extensions: [ "mkv", "mp4", "avi", "ts", "webm" ]
  download:
    directory: /tmp/tuliprox_downloads
    headers:
      User-Agent: "AppleTV/tvOS/9.1.1"
      Accept: "video/*"
    organize_into_directories: true
    episode_pattern: '.*(?P<episode>[Ss]\d{1,2}(.*?)[Ee]\d{1,2}).*'
```

* `web_search`: A template URL used in the Web UI to quickly search for a movie title (replaces `{}` with the title).
* `extensions`: Defines which file endings Tuliprox categorizes as VOD/Video content when transforming M3U to Xtream.
* `download`: Configuration for the Web UI download and recording manager.
  * `directory`: Where downloaded files and recordings are saved.
  * `headers` (optional): Custom HTTP headers used for the download request. This is useful for bypassing basic
    user-agent filters or setting specific media types.
  * `organize_into_directories`: If true, Tuliprox automatically creates neat subfolders for series.
  * `episode_pattern`: Crucial for the directory organization. It uses the mandatory Named Capture Group
    `(?P<episode>...)` in the Regex to identify and strip the episode identifier (e.g., `S01E01`)
    from the filename, ensuring all episodes of a show land in the exact same base-show folder.
  * `download_priority`: Default provider priority for VOD/series/episode downloads. Lower values mean higher priority.
  * `recording_priority`: Default provider priority for live recordings. Lower values mean higher priority.
  * `reserve_slots_for_users`: Keeps provider headroom for normal foreground users before background-priority transfers
    may consume the last slots.
  * `max_background_per_provider`: Limits how many background-priority transfers may run in parallel against one provider.
  * `retry_backoff_step_1_secs`, `retry_backoff_step_2_secs`, `retry_backoff_step_3_secs`: Retry delays for transient
    download/recording failures.
  * `retry_backoff_jitter_percent`: Randomizes retry delays to avoid retry spikes after shared upstream problems.
  * `retry_max_attempts`: Maximum number of transient retries before a transfer is marked as failed.

Tuliprox handles these transfers like provider-bound background streams:

* They respect provider limits, user priorities, and connection preemption instead of bypassing normal stream capacity.
* Waiting for provider capacity is notify-based, not polling-based.
* The Web UI loads an initial transfer snapshot and then stays synchronized through websocket updates.
* RBAC integration is explicit:
  * `download.read` allows opening the downloads view and receiving transfer snapshots.
  * `download.write` allows queueing, pausing, cancelling, retrying, and removing transfers.

> **Note:** The named capture group `(?P<episode>...)` is **mandatory** for this to function correctly.
>
> *Example:* `.*(?P<episode>[Ss]\d{1,2}(.*?)[Ee]\d{1,2}).*`

---

## 7. Outgoing Proxy (`proxy`)

If Tuliprox itself must operate behind a corporate proxy or VPN (e.g., a Gluetun WireGuard container):

```yaml
proxy:
  url: socks5://192.168.1.6:8123
  username: "opt_user"
  password: "opt_password"
```

Setting this forces **every** outgoing request (Playlist downloads, TMDB API calls, FFprobe stream analysis, and
Reverse-Proxy
Video Streaming) through this proxy.

---

## 8. IP-Check (`ipcheck`)

To verify in the Web UI which public IP Tuliprox is currently using (crucial when verifying VPN
routing or diagnosing geo-blocks), Tuliprox queries external detection APIs.
This ensures that your traffic is actually routed through the intended gateway or VPN tunnel.

```yaml
ipcheck:
  url: "[https://api64.ipify.org?format=json](https://api64.ipify.org?format=json)" # Generic URL for both IP versions
  url_ipv4: "[https://ipinfo.io/ip](https://ipinfo.io/ip)"          # Dedicated IPv4 fetch
  url_ipv6: "[https://v6.ident.me](https://v6.ident.me)"           # Dedicated IPv6 fetch
  pattern_ipv4: '(?:\d{1,3}\.){3}\d{1,3}'   # Optional Regex for extraction
  pattern_ipv6: '([0-9a-fA-F]{1,4}:){7}[0-9a-fA-F]{1,4}'
```

| Parameter      | Type   | Technical Impact                                                                                                            |
|:---------------|:-------|:----------------------------------------------------------------------------------------------------------------------------|
| `url`          | String | A generic endpoint that may return both IPv4 and IPv6 in a single response (often JSON).                                    |
| `url_ipv4`     | String | A dedicated URL to fetch only the public IPv4 address.                                                                      |
| `url_ipv6`     | String | A dedicated URL to fetch only the public IPv6 address.                                                                      |
| `pattern_ipv4` | Regex  | Optional regex pattern to extract the IPv4 string from complex API responses (e.g., if the API returns a full JSON object). |
| `pattern_ipv6` | Regex  | Optional regex pattern to extract the IPv6 string from complex API responses.                                               |

### Technical Background

* **VPN Validation:** This feature is primarily used to confirm that Tuliprox is successfully using a VPN or Proxy. If
  the displayed IP in the Web UI matches your home ISP instead of your VPN provider, your routing is likely
  misconfigured.
* **Regex Extraction:** If your preferred IP-API returns data in a format like `{"ip": "1.2.3.4", "city": "Berlin"}`,
  you can use the `pattern_ipv4` to isolate just the IP address for the Tuliprox UI.
* **Execution:** The check is performed periodically or on-demand when accessing the **Dashboard** to provide real-time
  connectivity status.

---

## 9. HDHomeRun Emulation (`hdhomerun`)

**Deep-Dive Feature:** Tuliprox can masquerade on the local network as a physical **SiliconDust HDHomeRun** DVB-C/S/T
network tuner. Media servers like **Plex, Jellyfin, Emby, or TVHeadend** will automatically discover Tuliprox via UPnP
as a real hardware antenna and ingest Live-TV natively into their Live-DVR systems.

Tuliprox utilizes standardized **UPnP/SSDP (UDP Port 1900)** for broad compatibility and the **proprietary SiliconDust
protocol (UDP Port 65001)** for compatibility with official HDHomeRun tools.

```yaml
hdhomerun:
  enabled: true
  auth: false # If true, lineup.json requires Basic Auth (using the assigned user's credentials)
  devices:
    - name: hdhr1             # MUST match the 'device' name in the source.yml hdhomerun output!
      tuner_count: 4          # Number of concurrent streams the client thinks are available
      port: 5004              # Unique TCP Port for this specific virtual tuner API
      device_id: "107ABCDF"   # 8-char hex code (Port 65001). If left blank, Tuliprox generates a valid one with correct checksums.
      device_udn: "uuid:..."  # Unique Device Name (Port 1900). Recommended to leave blank for auto-generation.
      friendly_name: "Tuliprox Living Room"
```

### Configuration Parameters

| Parameter | Type | Default | Description                                                                                     |
|:----------|:-----|:--------|:------------------------------------------------------------------------------------------------|
| `enabled` | Bool | `false` | Master switch for the entire emulation engine.                                                  |
| `auth`    | Bool | `false` | Requires HTTP Basic Auth for the `/lineup.json` endpoint using the assigned user's credentials. |
| `devices` | List | `[]`    | A list of virtual HDHomeRun devices to emulate.                                                 |

### 9.1. Device-Specific Fields

| Parameter       | Type   | Default           | Technical Impact & Background                                                                                                          |
|:----------------|:-------|:------------------|:---------------------------------------------------------------------------------------------------------------------------------------|
| `name`          | String | **Required**      | Unique internal identifier. Must match the `device` field in your `source.yml` target mapping.                                         |
| `tuner_count`   | Int    | `1`               | Number of virtual tuners reported to the client. Defines how many concurrent streams Plex/Emby thinks the "hardware" can handle.       |
| `port`          | Int    | `API+1`           | TCP port for the HTTP API (`/device.xml`, `/lineup.json`). Each virtual device **must** have a unique port.                            |
| `friendly_name` | String | `(Auto)`          | The display name in client applications (e.g., "Tuliprox Living Room").                                                                |
| `device_id`     | Hex    | `(Auto)`          | 8-char hex ID for SiliconDust protocol (Port 65001). Tuliprox automatically corrects invalid IDs by calculating the required checksum. |
| `device_udn`    | UUID   | `(Auto)`          | Unique Device Name for UPnP/SSDP (Port 1900). Recommended to leave blank for auto-generation.                                          |
| `manufacturer`  | String | `SiliconDust`     | Customizes the manufacturer string reported to clients.                                                                                |
| `model_name`    | String | `HDTC-2US`        | Mimics a specific hardware model for maximum compatibility with official apps.                                                         |
| `firmware_name` | String | `hdhomerun3_atsc` | The firmware type reported during the discovery handshake.                                                                             |

> **Note:** Advanced metadata fields like `model_number` and `firmware_version` can also be overridden but are safe to
> leave at their defaults to ensure the best "plug-and-play" experience with media servers.

### 9.2. Linking Devices to Playlists

The `name` of each device in `config.yml` must correspond to a `device` reference in a `hdhomerun` output target within
your `source.yml`.

**Example `source.yml` snippet:**

```yaml
targets:
  - name: my-tv-lineup
    output:
      - type: hdhomerun
        device: hdhr1      # Link to the device defined above
        username: local    # The user whose credentials/playlist will be served
```

### Discovery Protocols: A Technical Distinction

To satisfy both official SiliconDust hardware scanners and generic third-party UPnP discovery, Tuliprox handles two
distinct layers:

1. **`device_id` (Port 65001):** Used by proprietary SiliconDust tools. It requires a specific hexadecimal format and
   checksum.
2. **`device_udn` (Port 1900):** Used by the standard SSDP/UPnP protocol (e.g., by Plex or VLC). This identifies the
   device as a unique UUID on the network.

> **Pro-Tip:** If Plex fails to find your device, ensure that the UDP ports **1900** and **65001** are not blocked by
> your firewall and that Tuliprox is running on the same network subnet as your media server.

&nbsp;

## Additional Information

### Custom Stream Responses (Fallback Videos)

When a stream fails to load at the provider (HTTP 404/502) or a user reaches their connection limit, Tuliprox can
seamlessly
substitute a fallback info-video (as a `.ts` stream) instead of brutally closing the TCP connection. Dropping
connections often
causes hardware players or Smart TVs to freeze.

```yaml
custom_stream_response_path: /home/tuliprox/resources
custom_stream_response_timeout_secs: 20
```

| Name                                  | Type   | Default   | Technical Impact & Background                                                                                                                                                              |
|:--------------------------------------|:-------|:----------|:-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `custom_stream_response_path`         | String | `(Empty)` | Directory path where Tuliprox looks for exactly named `.ts` files.                                                                                                                         |
| `custom_stream_response_timeout_secs` | Int    | `0`       | Hard timeout (in seconds) that forces the fallback video stream to terminate to prevent infinite bandwidth usage. `0` means the fallback loops endlessly until the user switches channels. |

**Filenames searched for in the directory:**

* `channel_unavailable.ts` (Provider returns 404/502/Timeout)
* `user_connections_exhausted.ts` (User hit their `max_connections` limit)
* `provider_connections_exhausted.ts` (Provider has no free slots left)
* `low_priority_preempted.ts` (User was kicked by an Admin with higher priority)
* `user_account_expired.ts` (User's `exp_date` reached)
* `panel_api_provisioning.ts` (Loops while a new Provider Account is generated via Panel API)

> **Note**: These Video files are all available in the docker image.

**How to create your own fallback video stream:**
You can simply convert an image with `ffmpeg`.

```shell
ffmpeg -y -nostdin -loop 1 -framerate 30 -i blank_screen.jpg -f lavfi \
  -i anullsrc=channel_layout=stereo:sample_rate=48000 -t 10 -shortest -c:v libx264 \
  -pix_fmt yuv420p -preset veryfast -crf 23 -x264-params "keyint=30:min-keyint=30:scenecut=0:bframes=0:open_gop=0" \
  -c:a aac -b:a 128k -ac 2 -ar 48000 -mpegts_flags +resend_headers -muxdelay 0 -muxpreload 0 -f mpegts blank_screen.ts
```

The filename identifies the file inside the path `custom_stream_response_path`.

**How it works:**
Tuliprox searches the specified folder for exactly named `.ts` files. If found, they are looped back to the client. The
`custom_stream_response_timeout_secs` parameter hard-kills the fallback stream after X seconds to prevent infinite
bandwidth usage.
