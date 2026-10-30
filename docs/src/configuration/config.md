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
interner_gc_interval_secs: 180
interner_gc_min_pool_size: 100
user_access_control: false
custom_stream_response_path: null
custom_stream_response_timeout_secs: 0
custom_stream_response_enabled: true   # default: true (serve the fallback videos)
custom_stream_response_error_status: 502  # 4xx/5xx only; returned when custom_stream_response_enabled is false

api:
web_ui:
log:
schedules:
messaging:
video:
proxy:
ipcheck:
hdhomerun:
mcp:
library:
reverse_proxy:
metadata_update:
```

### Global System & Storage Settings (Flat Keys)

| Parameter                             | Type   | Required | Default                | Technical Impact & Background                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
|:--------------------------------------|:-------|:--------:|:-----------------------|:------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `process_parallel`                    | Bool   |    No    | `false`                | Enables dependency-aware parallel updates. Inputs from the same source and independent sources may download concurrently. A source's targets start as soon as all of its enabled inputs are ready; disjoint target outputs may then finalize concurrently. With `false`, inputs and targets keep configured sequential order. Use `inputs[].sequential_group` to serialize credentials that must never overlap.                                                                                                                                             |
| `disk_based_processing`               | Bool   |    No    | `false`                | **Tradeoff Guidance:** Normally, Tuliprox loads playlists into RAM. With `true`, every chunk is manipulated directly on disk (using a B+Tree database). **Use this on low-end hardware (e.g., Raspberry Pi) or with massive playlists (>500k streams)** to prevent Out-Of-Memory crashes. It significantly increases Disk I/O load, so it is slower but much safer for tight memory footprints.                                                                                                                                                             |
| `storage_dir`                         | String |    No    | `./data`               | Root directory for all runtime data (B+Tree databases, downloads, caches). Relative paths are resolved against the Tuliprox Home Directory. Be aware that different configurations (e.g. user bouquets) alongside the playlists are stored in this directory.                                                                                                                                                                                                                                                                                               |
| `default_user_agent`                  | String |    No    | `Tuliprox/...`         | Fallback HTTP `User-Agent` used for upstream provider requests if the input definition or client request does not explicitly provide one.                                                                                                                                                                                                                                                                                                                                                                                                                   |
| `backup_dir`                          | String |    No    | `{storage_dir}/backup` | Storage location for config backups (e.g., triggered via "Save Configuration" in the Web UI).                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| `user_config_dir`                     | String |    No    | `{storage_dir}/user`   | Storage location for user-specific configurations (like favorites or custom bouquets created via the Web UI).                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| `mapping_path`                        | String |    No    | `mapping.yml`          | Path to the mapping file. **Pro-Tip:** If you specify a folder path here (e.g., `./config/mappings/`), Tuliprox loads *all* `.yml` files in that folder in **alphanumeric** order and merges them. Note: This is a lexicographic sort, meaning `m_10.yml` comes before `m_2.yml`. Name files carefully (e.g., `m_01.yml`, `m_02.yml`).                                                                                                                                                                                                                      |
| `template_path`                       | String |    No    | `template.yml`         | Path to the template macro file. Specifying a folder here is also possible and highly recommended (see above).                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| `update_on_boot`                      | Bool   |    No    | `false`                | Forces Tuliprox to immediately query all providers and rebuild all playlists upon startup. If `false`, the proxy serves the local DB cache from the last run until the scheduler triggers the next update.                                                                                                                                                                                                                                                                                                                                                  |
| `config_hot_reload`                   | Bool   |    No    | `false`                | Spawns a filesystem watcher for `mapping.yml` and `api-proxy.yml`. Upon saving, mappings and user credentials become active immediately *without* requiring a server restart. **(See Bind-Mount Note below)**                                                                                                                                                                                                                                                                                                                                               |
| `accept_insecure_ssl_certificates`    | Bool   |    No    | `false`                | Set to `true` if your upstream provider uses expired, self-signed, or improperly configured HTTPS certificates. Otherwise, the HTTP client drops the connection securely.                                                                                                                                                                                                                                                                                                                                                                                   |
| `sleep_timer_mins`                    | Int    |    No    | `null`                 | Automatic kill-switch for proxied streams. Forcibly terminates active stream connections after X minutes (Protects against users falling asleep with the TV on).                                                                                                                                                                                                                                                                                                                                                                                            |
| `connect_timeout_secs`                | Int    |    No    | `10`                   | Maximum time (in seconds) Tuliprox waits to establish the initial TCP connection to a provider. `0` disables the timeout and the connection attempt continues until the provider closes it or a network timeout occurs (Warning: risk of hanging threads!).                                                                                                                                                                                                                                                                                                 |
| `interner_gc_interval_secs`           | Int    |    No    | `180`                  | Interval in seconds between background string interner garbage-collection checks. Lower values reclaim unused interned strings sooner, but run the GC gate more often. Higher values reduce GC overhead, but can retain more memory for longer. Must be greater than `0`.                                                                                                                                                                                                                                                                                   |
| `interner_gc_min_pool_size`           | Int    |    No    | `100`                  | Minimum number of interned strings required before the background interner GC runs. This avoids paying the interner write-lock cost when the pool is very small. Lower values favor aggressive cleanup; higher values favor throughput and lower lock contention at the cost of extra retained memory.                                                                                                                                                                                                                                                      |
| `event_channel_capacity`              | Int    |    No    | `256`                  | Buffer depth of the in-process event broadcast channel, per subscriber. Every runtime event — playlist-update progress, config reloads, download deltas, recording changes — passes through it on the way to the Web UI websocket and the notification pipeline. A subscriber that falls this many events behind is told it lagged and resumes from the newest event, having missed the gap; the Web UI recovers by re-requesting a status snapshot. Raise it if the bus reports lag under load. `0` is clamped to `1`.                                     |
| `user_access_control`                 | Bool   |    No    | `false`                | **Security:** If `true`, Tuliprox actively enforces `status` (Active/Banned), `exp_date`, and `max_connections` constraints for users defined in `api-proxy.yml`. If false, those fields are ignored.                                                                                                                                                                                                                                                                                                                                                       |
| `custom_stream_response_path`         | String |    No    | `null`                 | Directory path where Tuliprox looks for custom fallback `.ts` files. See section [Custom Stream Response](#custom-stream-responses-fallback-videos) for exact filenames.                                                                                                                                                                                                                                                                                                                                                                                    |
| `custom_stream_response_timeout_secs` | Int    |    No    | `0`                    | Hard timeout (in seconds) that forces the fallback video stream to terminate to prevent infinite bandwidth usage. `0` means endless loop.                                                                                                                                                                                                                                                                                                                                                                                                                   |
| `custom_stream_response_enabled`      | Bool   |    No    | `true`                 | Enables configured custom MPEG-TS fallback videos for stream errors. When `false`, factories skip the video body and call sites return `custom_stream_response_error_status`; useful behind reverse proxies that intercept 4xx/5xx responses instead of keeping fallback sockets open.                                                                                                                                                                                                                                                                      |
| `custom_stream_response_error_status` | Int    |    No    | `502`                  | HTTP status code returned when `custom_stream_response_enabled` is `false`. Must be a 4xx or 5xx code (the `prepare()` step rejects anything outside that range; `0` is silently clamped to `502`). Common choices: `404` (channel not found), `502` (bad gateway — upstream failed), `503` (service unavailable — overloaded).                                                                                                                                                                                                                             |

**TMDB discovery exception:** [TMDB curation discovery](source.md#target-wide-curation-curation) always verifies TLS
certificates and hostnames, even with `accept_insecure_ssl_certificates: true`. TLS-inspecting proxies require valid CA
trust to complete discovery. Trakt, metadata and playback clients retain this flag's behavior.

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
  landing_page: dashboard
  stream_info:
    hide_group: false
    hide_ip: false
    hide_country: false
    hide_shared: false
    hide_duration: false
    hide_bandwidth: false
    hide_transferred: false
    hide_player: false
    hide_user_comment: false
    hide_epg: false
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

| Parameter                     | Type   | Default     | Technical Impact & Background                                                                                                                                                                                                                            |
|:------------------------------|:-------|:------------|:---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `enabled`                     | Bool   | `true`      | Completely toggles the Web Dashboard and its REST API endpoints on or off.                                                                                                                                                                               |
| `user_ui_enabled`             | Bool   | `true`      | Allows standard proxy users (not just admins) to log into the Web UI to manage their own favorites/bouquets.                                                                                                                                             |
| `path`                        | String | `""`        | Base path for the UI (e.g., `admin`). Critical for reverse proxy subfolder setups so assets load from `example.com/admin/assets/`.                                                                                                                       |
| `player_server`               | String | `default`   | Determines which virtual server block from `api-proxy.yml` is used to construct the streaming URLs when playing a channel directly within the Web UI player.                                                                                             |
| `kick_secs`                   | Int    | `90`        | **Background:** When you kick a user via the Dashboard, they are not only disconnected but hard-blocked at the IP/User level for X seconds. This prevents their IPTV player's auto-reconnect logic from instantly stealing the provider slot back.       |
| `combine_views_stats_streams` | Bool   | `false`     | Combines the "Server Stats" and "Active Streams" views into a single unified window in the UI.                                                                                                                                                           |
| `landing_page`                | String | `dashboard` | Set the initial landing page for the webui. Possible values are `dashboard`, `stats`, `streams`, `stream_history`, `downloads`, `users`, `config`, `source_editor`, `playlist_update`, `playlist_settings`, `playlist_explorer`, `playlist_epg`, `rbac`  |
| `stream_info`                 | Object | `null`      | Optional visibility controls for fields shown in the active stream display. If omitted, or if all nested `hide_*` flags are `false`, the default dashboard view is unchanged.                                                                            |

### 2.2 Stream Display Visibility (`stream_info`)

Use this optional block to hide selected fields in the active stream display shown in the dashboard.

```yaml
web_ui:
  stream_info:
    hide_group: true
    hide_ip: true
    hide_epg: true
```

| Parameter             | Type | Default | Technical Impact & Background                                                                                  |
|:----------------------|:-----|:--------|:---------------------------------------------------------------------------------------------------------------|
| `hide_group`          | Bool | `false` | Hides the stream group/category label.                                                                         |
| `hide_ip`             | Bool | `false` | Hides the client IP shown for an active stream.                                                                |
| `hide_country`        | Bool | `false` | Hides the detected client country flag/code.                                                                   |
| `hide_shared`         | Bool | `false` | Hides the shared-stream indicator.                                                                             |
| `hide_duration`       | Bool | `false` | Hides the running playback duration.                                                                           |
| `hide_bandwidth`      | Bool | `false` | Hides the live bandwidth badge when metrics are available.                                                     |
| `hide_transferred`    | Bool | `false` | Hides the transferred-bytes badge when metrics are available.                                                  |
| `hide_player`         | Bool | `false` | Hides the player / user-agent field.                                                                           |
| `hide_user_comment`   | Bool | `false` | Hides the user comment next to the active stream title.                                                        |
| `hide_epg`            | Bool | `false` | Hides per-stream EPG information and skips the associated dashboard EPG fetch/update work for that display.    |

* If every `hide_*` flag is `false`, Tuliprox treats the whole `stream_info` block as empty/default.
* This setting is global for the Web UI. It is not currently per-user.

### 2.3 Content Security Policy (`content_security_policy`)

This block enhances security by restricting which resources the browser is allowed to load.

* **Default Directives:** When `enabled: true`, Tuliprox automatically applies:
  * `default-src 'self'`
  * `script-src 'self' 'wasm-unsafe-eval' 'nonce-{nonce_b64}'`
  * `frame-ancestors 'none'`
* **Customization:** Use `custom-attributes` to add specific rules (e.g., allowing external channel logos via
  `img-src`).

### 2.4 Authentication & RBAC (`auth`)

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
`system.read/write`, `epg.read/write`, `recording.read/create/manage/delete`, `maintenance.read/write`.
Note: Write does not imply Read. A group must explicitly
grant both if users need
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
  runtime_config_report_enabled: false
  runtime_config_report_format: yaml

  # Optional asynchronous syslog / NDJSON log shipping over UDP (default: disabled)
  syslog:
    enabled: false
    host: "192.168.1.50"
    port: 514
    format: rfc5424
    facility: local0
    app_name: "tuliprox"
    # hostname: "node1"
    # log_level: debug
    queue_capacity: 4096
    max_message_len: 1024
```

| Parameter                       | Type   | Default | Technical Impact & Background                                                                                                                                                                                                          |
|:--------------------------------|:-------|:--------|:---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `log_level`                     | String | `info`  | Verbosity. Possible values: `trace`, `debug`, `info`, `warn`, `error`. Can be overridden per-module (e.g., `tuliprox=debug,hyper_util=warn`).                                                                                          |
| `sanitize_sensitive_info`       | Bool   | `true`  | **Critical:** Masks passwords, provider URLs, and external client IPs in the logs with `***`. Highly recommended to keep `true` so you can safely share logs on GitHub/Discord for support without leaking credentials.                |
| `log_active_user`               | Bool   | `false` | Periodically writes the current active client connection count as an INFO message to the log file.                                                                                                                                     |
| `runtime_config_report_enabled` | Bool   | `false` | Opt-in startup dump of the complete effective runtime configuration after defaults, path resolution, and runtime preparation have been applied. Sensitive values such as passwords, secrets, tokens, and API keys are masked as `***`. |
| `runtime_config_report_format`  | String | `yaml`  | Output format for the runtime config report. Supported values: `json` and `yaml`. Both formats are emitted as pretty, multi-line output. Ignored unless `runtime_config_report_enabled` is `true`.                                     |
| `syslog`                        | Object | *empty* | Optional external UDP log shipping configuration block. Default disabled (`enabled: false`). See [External Log Shipping (`log.syslog`)](#external-log-shipping-logsyslog).                                                             |

If enabled, Tuliprox keeps the existing startup summary and prints the additional full runtime report once during
startup. The report includes:

* prepared `config.yml`
* prepared `source.yml`
* loaded mappings/templates/api-proxy sections when present
* resolved runtime `paths`

The report is intended for diagnostics and verification of effective defaults. Sensitive values are redacted before
logging.

### External Log Shipping (`log.syslog`)

Tuliprox can stream application log records directly and asynchronously to an external log collector (such as Vector,
rsyslog, syslog-ng, Logstash, Fluent Bit, Loki, Graylog, or Splunk) via UDP. Streaming and media paths (HLS, MPEG-TS,
DVR) never block on log I/O.

| Field             | Type   | Default      | Description                                                                                                                                                                                   |
|:------------------|:-------|:-------------|:----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `enabled`         | Bool   | `false`      | Enables asynchronous UDP log shipping to an external collector. Opt-in; when `false`, no socket or worker thread is spawned.                                                                  |
| `host`            | String | `""`         | Destination IP address or hostname. Required when `enabled` is `true`. IPv6 literals are specified without square brackets.                                                                   |
| `port`            | Int    | `514`        | Destination UDP port (range `1..=65535`).                                                                                                                                                     |
| `format`          | String | `rfc5424`    | Wire format for transmitted datagrams. Supported values: `rfc5424`, `rfc3164`, `json`.                                                                                                        |
| `facility`        | String | `local0`     | Syslog facility: all 24 RFC 5424 values are accepted, but **only `local0`..`local7` make sense here** — see [Facility](#facility).                                                            |
| `app_name`        | String | `"tuliprox"` | Application name header (`1..=48` ASCII characters without spaces, range `0x21..=0x7E`).                                                                                                      |
| `hostname`        | String | `null`       | Optional hostname override (`1..=255` characters). If omitted, falls back to the system hostname, or NILVALUE (`-`) if undetectable.                                                          |
| `log_level`       | String | `null`       | Optional independent remote log level: `debug`, `info`, `warn`, `error`. If omitted, follows the root logger filter (including module rules) but never ships `trace`; `trace` is rejected.    |
| `queue_capacity`  | Int    | `4096`       | Capacity of the bounded in-memory datagram channel (range `1..=65536`). When full, new datagrams are dropped immediately without blocking the caller (`dropped_full`).                        |
| `max_message_len` | Int    | `1024`       | Maximum datagram size in bytes (range `480..=65507`, minimum `1024` for `json`). Datagrams exceeding this size have their message payload truncated at a character boundary.                  |

### Facility

`facility` is the left half of the syslog `PRI` byte (`PRI = facility * 8 + severity`). It does **not** say how
important a message is — that is the severity — it says **which subsystem the message came from**, and collectors
route by it.

All 24 values of RFC 5424 §6.2.1 are accepted, so existing collector rules keep working:

| Code | Value | Meaning |
| :--- | :--- | :--- |
| 0 | `kern` | Kernel messages |
| 1 | `user` | Generic user-space application |
| 2 | `mail` | Mail transport |
| 3 | `daemon` | Long-running system service |
| 4 | `auth` | Login/authentication (superseded by `authpriv`) |
| 5 | `syslog` | The syslog daemon itself (reserved by RFC 5424) |
| 6 | `lpr` | Line-printer subsystem |
| 7 | `news` | Usenet |
| 8 | `uucp` | UUCP |
| 9 | `cron` | Cron daemon |
| 10 | `authpriv` | Security/authorization log |
| 11 | `ftp` | FTP daemon |
| 12 | `ntp` | Time synchronization |
| 13 | `audit` | Security audit log |
| 14 | `alert` | Console alert |
| 15 | `clock2` | Second clock daemon (legacy) |
| 16–23 | `local0`..`local7` | Reserved for site-local use |

**In practice only `local0`..`local7` make sense for Tuliprox, and `local0` is the default.** Two reasons:

1. **The rest name subsystems this application does not have.** Tuliprox is one process, not a collection of mail,
   print, cron or kernel components. `syslog` is even reserved for the syslog daemon itself, and a user-space
   process cannot honestly emit `kern` messages.
2. **A foreign facility moves the records.** A stock rsyslog configuration writes `cron.*` to `/var/log/cron` and
   `auth,authpriv.*` to the auth log, and excludes those facilities from the general log. Choosing `facility: cron`
   would not merely label the output oddly — it would take Tuliprox out of `/var/log/syslog`.

`local0`..`local7` exist exactly for this case: they are reserved for local use and are the conventional slot for a
third-party service. Use different `local0`..`local7` values if you run several instances and want the collector to
separate them.

For anything finer, route on `app_name` in the collector instead. That is the field your rules should key on; it does
not change the PRI and does not need a config reload when the routing changes.

The remaining values stay available for two cases: an existing collector rule that already buckets on a specific
facility, and an operator who deliberately wants Tuliprox records in a dedicated log (for example `authpriv`). Because
one sink ships every record to one destination, such a choice applies to all Tuliprox output, not to a subset.

#### Container and journald setups ignore this setting

`facility` — and the rest of `log.syslog` — only affects datagrams that Tuliprox sends itself (the `log.syslog` path).
When a container runtime or journald forwards the stdout stream instead (see
[Infrastructure-Side Shipping](#infrastructure-side-shipping-option-0)), the application never emits syslog at all, so
there is no `PRI` to fill and no header to shape. The forwarding component chooses those fields by its own rules:

* **Docker `syslog` logging driver:** the facility is a driver option (`syslog-facility`, documented with `daemon`),
  and the driver writes the entire header itself. Its `tag` option becomes the app name, and the host name is the one
  the driver sees — `app_name` and `hostname` from `config.yml` are ignored as well.
* **journald:** the facility comes from the journal's own configuration. For a native systemd unit it is
  `SyslogFacility=`, which defaults to `daemon` (`systemd.exec(5)`).

Under such a setup the value in `config.yml` neither helps nor hurts: it is simply unused. The one way it can hurt is
running both paths at once. If the container's log driver forwards stdout **and** `log.syslog.enabled` is `true`, every
record is shipped twice — once by Tuliprox with your facility, once by the driver with the driver's facility. Pick one
path.

### Wire Formats

Tuliprox implements three standard wire formats for UDP log datagrams:

#### 1. RFC 5424 (`format: rfc5424`, default)

Standard modern syslog format conforming to RFC 5424 / RFC 5426 over UDP:

```text
<134>1 2026-10-27T17:45:00.123456+01:00 node1 tuliprox 12345 main_api - Client stream allocated
```

* `PRI` (`<134>`): Calculated as `facility_code * 8 + severity`. Severity maps Error to 3, Warn to 4, Info to 6, and Debug/Trace to 7.
* `VERSION`: Fixed at `1`.
* `TIMESTAMP`: High-precision RFC 3339 timestamp with microsecond resolution and explicit numeric timezone offset.
* `HOSTNAME`: Configured hostname, falling back to the operating system DNS hostname, or `-` (NILVALUE). Sanitized and capped at 255 bytes.
* `APP-NAME`: Configured application identifier (up to 48 ASCII characters without spaces).
* `PROCID`: Process ID of the running Tuliprox instance (`std::process::id()`).
* `MSGID`: The last `::` segment of the logging target (`main_api`), sanitized and capped at 32 bytes (or `-` when empty).
* `STRUCTURED-DATA`: Emitted as `-` (NILVALUE).
* `MSG`: Log message payload sanitized of C0 control characters and truncated to the remaining byte budget.

#### 2. RFC 3164 (`format: rfc3164`)

Legacy BSD syslog format for older appliances and log collectors:

```text
<134>Oct 27 17:45:00 node1 tuliprox[12345]: Client stream allocated
```

* `PRI`: `<facility * 8 + severity>`.
* `TIMESTAMP`: Local time formatted as `Mmm dd hh:mm:ss`, where single-digit days are space-padded (e.g., `Oct  7`).
* `HOSTNAME`: Configured or detected hostname. Because RFC 3164 lacks a formal NILVALUE, an unresolvable hostname sends literal `-`.
* `TAG`: Application name and process ID in BSD format: `app_name[pid]:` (space-delimited from message). `app_name` is
  cut to 32 characters, the TAG limit of RFC 3164 §4.1.3.
* Note: RFC 3164 has no `MSGID` field; the target module is not emitted on the wire in this format.

#### 3. NDJSON (`format: json`)

Single-line newline-delimited JSON (NDJSON) object ideal for ingestion by Vector, Fluent Bit, Loki, or Logstash:

```json
{"@timestamp":"2026-10-27T17:45:00.123456+01:00","level":"info","facility":"local0","app_name":"tuliprox","hostname":"node1","pid":12345,"target":"tuliprox_app::api::main_api","message":"Client stream allocated","truncated":false}
```

The field set and insertion order are fixed: `@timestamp`, `level`, `facility`, `app_name`, `hostname`, `pid`, `target`,
`message`, and `truncated`. The `target` field preserves the full Rust module path (sanitized and capped at 255 bytes).
The `truncated` boolean field indicates whether the message payload was shortened to fit `max_message_len`.
Every datagram ends with a newline (`\n`) delimiter, which counts toward `max_message_len`.

### Shipping Semantics and Filter Hierarchy

Log shipping behavior is governed by a three-level filter model:

1. **Global Maximum Level (`log::max_level`):** The process-wide gate computed as `root_max.max(syslog_max)`. Records
   below this level are discarded immediately at the logging macro call site before any allocation occurs.
2. **Root Filter (Stdout and Web UI Ring Buffer):** Evaluates `log.log_level` (and module rules such as
   `tuliprox=debug,hyper_util=warn`). Accepted records are formatted for stdout and pushed into the in-memory ring buffer
   for the Web UI Live Logs console.
3. **Syslog Filter (External Collector):** Evaluates whether the record should be formatted and queued for UDP
   transmission to the remote collector.

#### Configuration and Filter Rules

* **Default Disabled:** The feature is opt-in (`enabled: false`). It can be toggled on or off in the Web UI under
  **Config → Log** (in the Syslog card) or directly in `config.yml`. When disabled, no worker thread is spawned, no
  UDP socket is bound, and no syslog datagram formatting occurs.
* **Host Requirement:** Setting `enabled: true` requires `host` to be non-empty; saving an empty host triggers a
  configuration validation error.
* **Omitted `log_level`:** When `log_level` is omitted (`null`), syslog follows the **resolved root filter** (including
  CLI `-l` overrides, `TULIPROX_LOG`, module-level directives, and the internal noise list). Remote syslog receives
  identically what stdout receives.
* **Explicit `log_level`:** When set to `debug`, `info`, `warn`, or `error`, the sink uses an independent filter at that
  level while stdout and Web UI remain at the root level. Module-specific syntax (`module=level`) is not supported for
  the remote sink level.
* **`trace` Rejection:** Setting `log.syslog.log_level: trace` is rejected by configuration validation. The trace log
  path dumps raw HTTP request headers (`headers.rs:176`), including `Authorization` headers, session cookies, and provider
  credentials. Transmitting these credentials unencrypted over UDP is deliberately unsupported.
* **Noise List Mitigation:** When `log_level` is configured independently, Tuliprox automatically suppresses known noisy
  third-party crates (`reqwest`, `hyper_util`, `tungstenite`, `tokio_tungstenite`, `rustls_platform_verifier`, `notify`,
  `mio`) by capping them at `Error` level. **Important:** This noise list is a mitigation for known libraries, **not a
  general leak guarantee**. Third-party debug logs may still expose URLs, headers, or tokens in error strings over UDP.

#### Operational Warnings and Invariants

* **Shipped Example Config is Loud:** The default `config/config.yml` shipped with Tuliprox sets `log_level: debug` and
  `sanitize_sensitive_info: false`. Combining these settings with remote `debug` will transmit upstream provider URLs and
  query credentials unmasked over the network. Always set `sanitize_sensitive_info: true` and recommend `info` as the
  remote sink level outside a controlled laboratory environment.
* **Root `trace` Exposure:** A sink configured to follow the root level (omitted `log.syslog.log_level`) will transmit
  full HTTP request-header dumps if the root logger is set to `trace` (e.g. via `tuliprox -l trace` or `log_level: trace`).
  Use `debug` as the finest root level in production.
* **Remote Debug Overhead:** Setting `log.syslog.log_level: debug` enables `StepMeasure` and per-request diagnostic
  builders (redirect, retry, and download diagnostics) process-wide via the `log_enabled!` macro check, even when stdout
  is set to `info`. This incurs additional CPU cycles and Web UI event broadcast traffic.
* **Module Path Filtering:** RFC 5424 carries only the final segment of the target module as `MSGID` (and RFC 3164
  omits target information entirely). If your collector needs to filter or route logs based on the full Rust module path
  (e.g., `tuliprox_app::api::main_api`), use `format: json`.

### Sizing, Truncation, and MTU Limits

UDP datagram transmission requires strict payload bounding to prevent packet fragmentation and network drops:

* **Two-Phase Truncation:** Headers are never truncated. The formatter builds the prefix header first, computes the
  remaining byte budget up to `max_message_len`, and truncates only the message body to fit that budget at a valid UTF-8
  character boundary.
* **Valid JSON Guarantee:** For `format: json`, the message is truncated before JSON serialization, followed by up to
  three iterative adjustments if JSON escaping expands the payload. The resulting datagram is guaranteed to be valid,
  parseable JSON and carries `"truncated": true` when shortened.
* **Minimum Floors:** Config validation enforces `max_message_len >= 480` for `rfc5424` and `rfc3164` (worst-case RFC
  headers occupy up to 391 and 339 bytes respectively, guaranteeing at least an 89-byte message). For `format: json`,
  the JSON envelope with maximal header fields reaches 734 bytes including the newline delimiter, so validation enforces `max_message_len >= 1024`.
* **MTU Thresholds:** On standard 1500-byte Ethernet networks, the maximum unfragmented UDP payload is **1472 bytes for
  IPv4** (1500 - 20 IP header - 8 UDP header) and **1452 bytes for IPv6** (without extension headers). Datagrams
  exceeding these thresholds are fragmented at the IP layer; Docker bridge networks, NAT gateways, and cloud firewalls
  frequently drop UDP fragments, leading to silent message loss. The default `max_message_len: 1024` ensures safe headroom
  even across VPN tunnels (such as WireGuard or IPsec). Values up to 65507 are permitted for loopback or jumbo-frame LANs.

### Queueing, Observability, and Lifecycle

* **Bounded Queue:** Datagrams are handed off from application threads to a dedicated worker thread via a bounded
  channel (`queue_capacity`, default `4096`). Calls to `offer()` use `try_send` and never block the caller. If the queue
  is full, `dropped_full` is incremented and the datagram is dropped immediately.
* **Throttled Worker Summary:** The worker thread tracks transmission counters (`dropped_full`, `dropped_unresolved`,
  `dropped_send_error`, `truncated`, `sent`). Based on an absolute deadline (every 5 minutes), the worker logs a `warn!`
  report to stdout and the Web UI if any drop or truncation counter has changed:
  `syslog worker report for <host>:<port>: dropped_full=..., dropped_unresolved=..., dropped_send_error=..., truncated=..., sent=...`.
  Under continuous load, the absolute deadline ensures reports are never starved.
* **Bounded Shutdown & Live Reload:** A `WorkerMsg::Shutdown` sentinel drains queued datagrams in FIFO order. On
  process exit, including fatal startup errors, Tuliprox waits for the drain up to a strict 2-second timeout
  (`SYSLOG_SHUTDOWN_TIMEOUT = 2 s`), so an unresponsive collector cannot hang the process. A configuration reload does
  not wait: the previous worker drains its queue in the background while the new sink takes over.
* **No Periodic DNS Re-Resolution:** Destination hostnames are resolved lazily on the background worker thread (never
  delaying application startup or `/healthcheck` probes). The resolved IP address is cached for the sender's lifetime.
  After an address-related socket error (`NetworkUnreachable`, `HostUnreachable`, `InvalidInput`), the worker moves to
  the next resolved address, for example from an unroutable IPv6 address to IPv4. Re-resolution occurs only when every
  resolved address has failed, at once if one of them delivered before, otherwise every 30 seconds. Normal DNS changes are **not detected** automatically
  because UDP sends continue to succeed silently. To force re-resolution, restart Tuliprox or modify any transport field
  (`host`, `port`, `format`, `facility`, `app_name`, `hostname`, `queue_capacity`, `max_message_len`) to trigger a fresh
  sender.

#### Security Notice

UDP/514 transmission is unauthenticated, unencrypted, and lossy. Packets may be dropped, intercepted, or spoofed on
untrusted networks. For secure, authenticated, or loss-free transport (such as TCP with TLS), use
[Option 0 (Infrastructure-Side Shipping)](#infrastructure-side-shipping-option-0).

### Infrastructure-Side Shipping (Option 0)

When operating in accordance with 12-factor application design, Tuliprox logs to stdout, and the container runtime or
host supervisor forwards the log stream to a central collector. This provides TCP/TLS encryption, local buffering, and
retry semantics without application-level UDP shipping.

The application-level block `log.syslog` is unused in this mode: Tuliprox writes no syslog header, so `facility`,
`app_name` and `hostname` come from the forwarding component instead (see
[Container and journald setups ignore this setting](#container-and-journald-setups-ignore-this-setting)).

#### 1. Docker / Docker Compose

Configure Docker's built-in syslog logging driver in your `compose.yml`:

```yaml
services:
  tuliprox:
    image: ghcr.io/euzu/tuliprox:latest
    logging:
      driver: syslog
      options:
        syslog-address: "udp://192.168.1.50:514"   # or tcp://, or tcp+tls://
        syslog-format: "rfc5424micro"
        tag: "tuliprox"
```

#### 2. systemd / journald

When running bare-metal under systemd, stdout is captured by `journald`. You can forward entries to remote syslog servers
via `rsyslog` (using `imjournal` and `omfwd`) or via `systemd-journal-upload`.

#### 3. Vector / Fluent Bit DaemonSet

Collector agents can tail container logs directly. Tuliprox formats stdout records as:

```text
[TIMESTAMP LEVEL TARGET] MESSAGE
```

For example: `[2026-10-27T17:45:00+01:00 INFO tuliprox_app::api::main_api] Client stream allocated`.

To parse this with Vector, use a `remap` transform with a VRL `parse_regex` (Fluent Bit takes the equivalent
`regex` parser definition):

```yaml
# vector.yaml
transforms:
  parse_tuliprox:
    type: remap
    inputs: [container_logs]
    source: |
      . = parse_regex!(.message, r'^\[(?P<timestamp>[^ ]+) (?P<level>[^ ]+) (?P<target>[^ ]+)\] (?P<message>.*)$')
```

### Verification (Manual Smoke Test)

You can verify syslog transmission using a local netcat listener:

```bash
# Terminal 1: Start a local UDP listener on port 5514
nc -u -l 5514

# Terminal 2: Run Tuliprox with syslog enabled
# (Set log.syslog: { enabled: true, host: "127.0.0.1", port: 5514, log_level: debug } in config.yml)
cargo run --bin tuliprox -- -c config/config.yml -i config/source.yml
```

1. **Datagram Delivery:** Check that datagrams arrive in real time on Terminal 1 formatted as `<PRI>1 ...`.
2. **Independent Level:** With `log.log_level: info` and `log.syslog.log_level: debug`, verify debug records appear in
   `nc` while the Web UI Live Logs and stdout remain at `info`.
3. **Noise Suppression:** Verify that third-party crates (`reqwest`, `hyper`) do not emit debug records to `nc`.
4. **Live Reload:** Set `log.syslog.enabled: false` via the Web UI or config reload; verify `nc` falls silent immediately.

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

Tuliprox can proactively notify you via Push-Notifications when updates fail, finish, when channels are added to or
removed from a watched group, or when watched groups appear or disappear (see
[`watch`](source.md#329-watch-watch)). **Why is this useful?** Because it allows you to instantly detect upstream
provider issues, or simply let you know when new movies or a new event group are added to your playlist.

### 5.1 Configuration & Opt-In

Messaging is strictly **opt-in**. Nothing is sent until `notify_on` asks for it.

`notify_on` is a list of **glob patterns** over event ids. An event id is a
dotted `domain.event` string:

| Pattern                 | Matches                                    |
|:------------------------|:-------------------------------------------|
| `*`                     | every event                                |
| `recording.*`           | every event in the `recording` domain      |
| `recording.completed`   | that event only                            |
| `provider.*.expired`    | one wildcard segment                       |
| `!system.info`          | **excludes**, whatever else matched        |

A subscription matches when at least one positive pattern matches and no
negative pattern does, so `["*", "!system.info"]` reads the way it looks.

> **Upgrading:** the old event names (`info`, `stats`, `error`, `watch`,
> `disk_alert`, `recording_started`, `recording_completed`,
> `recording_failed`) are still accepted and are rewritten to their
> canonical ids the next time the config is saved. Existing configs and
> existing template files keep working untouched.

#### Available Events

<!-- BEGIN GENERATED EVENT TABLE -->

| Event id | Default severity | Description |
| :------- | :--------------- | :---------- |
| `system.info` | info | A general informational message. |
| `system.error` | error | A general error message. |
| `system.disk.alert` | warn | Disk usage crossed the warn or critical threshold. |
| `system.started` | info | The server finished starting up. |
| `system.shutdown` | info | The server is shutting down cleanly. |
| `playlist.update.completed` | info | A playlist update finished; carries per-source statistics. |
| `playlist.update.failed` | error | A playlist update failed. |
| `playlist.watch.changed` | info | Channels or groups were added to or removed from a watched target. |
| `playlist.watch.disabled` | warn | A target's watch configuration is set but not working. |
| `playlist.watch.unmatched` | warn | Watch patterns matched no group in the refreshed playlist. |
| `recording.started` | info | A recording started. |
| `recording.completed` | info | A recording completed. |
| `recording.failed` | error | A recording failed. |
| `provider.account.status_changed` | warn | A provider reported a changed account status. |
| `provider.account.expiring` | warn | A provider account is approaching its expiry date. |
| `provider.account.expired` | error | A provider account has expired. |
| `provider.fetch.failed` | error | An input's playlist could not be fetched. |
| `provider.pool.exhausted` | warn | Every provider behind an input was at capacity. |
| `provider.priority.fallback` | warn | An input started being served from a different provider priority group. |
| `config.changed` | info | A configuration file was changed and reloaded. |
| `config.reload_failed` | error | A configuration file changed but could not be reloaded. |
| `library.scan.completed` | info | A local library scan finished. |
| `library.scan.failed` | error | A local library scan could not complete. |
| `metadata.update.started` | info | A metadata update started for an input. |
| `metadata.update.completed` | info | A metadata update finished for an input. |
| `metadata.update.failed` | error | A metadata update cycle ended with tasks it could not finish. |
| `user.connection.changed` | info | A user connected or disconnected. High frequency - subscribe deliberately. |
| `provider.connections.changed` | info | A provider's active connection count changed. High frequency - subscribe deliberately. |
| `user.connection.denied` | warn | A user was refused a connection because their limits were full. |
| `recording.queue.changed` | info | The recording queue changed. |
| `recording.rules.changed` | info | The recording rule set changed. |
| `maintenance.changed` | warn | An input was put into or taken out of maintenance mode. |
| `scheduled_task.failed` | error | A scheduled task could not complete. |
| `notification.dead_lettered` | error | A notification was permanently undeliverable and has been dropped. |
| `user.created` | info | An API-proxy user account was created. |
| `user.updated` | info | An API-proxy user account was changed. |
| `user.deleted` | warn | An API-proxy user account was deleted. |
| `auth.sign_in.succeeded` | info | A principal signed in and was issued a token. |
| `auth.sign_in.failed` | warn | A sign-in was rejected. Deduplicated per principal and address, so a password-guessing run notifies once rather than per attempt. |
| `auth.sign_in.throttled` | warn | A sign-in was refused without checking credentials because the caller is backing off after repeated failures. |
| `auth.permission.denied` | warn | An authenticated principal asked for something its permissions do not cover. |
| `stream.probe.failed` | warn | A stream probe returned no metadata; the stream may be dead. Deduplicated per input, so a provider outage notifies once rather than per channel. |

<!-- END GENERATED EVENT TABLE -->

Severity is one of `info`, `warn`, `error`, `critical`, in that order.

#### Example

```yaml
messaging:
  notify_on: [ "playlist.update.failed", "recording.*", "provider.account.*", "system.disk.alert" ]

  # Telegram: Supports Markdown and Group Topics
  telegram:
    markdown: true
    bot_token: "<TOKEN>"
    chat_ids:
      - "<CHAT_ID>"
      - "<CHAT_ID>:<MESSAGE_THREAD_ID>" # Use colon to target a specific topic/thread
    templates:
      playlist.update.completed: 'file:///config/messaging_templates/telegram_playlist_update_completed.templ'
      recording.completed: 'file:///config/messaging_templates/telegram_recording_completed.templ'

  # Discord: Webhook integration
  discord:
    url: "<WEBHOOK_URL>"
    templates:
      system.info: '{"content": "🚀 Tuliprox Info: {{message}}"}'

  # Pushover: Simple mobile push alerts
  pushover:
    token: "<API_TOKEN>"
    user: "<USER_KEY>"

  # ntfy: Self-hosted push, no account needed
  ntfy:
    url: "https://ntfy.sh"
    topic: "my-tuliprox"
    # token: "<BEARER_TOKEN>"   # only for a protected topic

  # Gotify
  gotify:
    url: "https://gotify.local"
    token: "<APP_TOKEN>"

  # Slack incoming webhook
  slack:
    url: "<WEBHOOK_URL>"

  # REST: Generic webhook/API support
  rest:
    url: "https://my-api.local/alert"
    method: "POST"
    headers:
      - "Content-Type: application/json"
    signing_secret: "<SHARED_SECRET>"   # optional, see 5.4
    templates:
      system.error: '{"text": "Alert: {{message}}", "type": "{{kind}}"}'

  # command: run a local program with the event JSON on stdin
  command:
    program: "/config/scripts/notify.sh"
    args: [ "--from-tuliprox" ]
    timeout_secs: 30
```

### 5.1.1 Per-Channel Routing

Every channel takes an optional `routing` block. Without one it inherits the
global `notify_on`, so existing configs are unaffected.

```yaml
messaging:
  notify_on: [ "*" ]

  pushover:
    token: "<API_TOKEN>"
    user: "<USER_KEY>"
    routing:
      min_severity: critical        # only wake me for real problems

  discord:
    url: "<WEBHOOK_URL>"
    routing:
      notify_on: [ "playlist.*", "recording.*" ]
      quiet_hours: "23:00-07:00"    # deferred, never dropped
      max_per_hour: 20              # circuit breaker
      dedup_window_secs: 3600       # suppress a repeated event
```

| Field | Meaning |
| :---- | :------ |
| `notify_on` | Overrides the global subscription for this channel. Same glob grammar. |
| `min_severity` | Drop anything below `info` \| `warn` \| `error` \| `critical`. |
| `quiet_hours` | `HH:MM-HH:MM` local time. Wrapping windows (`23:00-07:00`) work. Notifications are **deferred until the window closes, never dropped** - an overnight outage must not be silently invisible. |
| `max_per_hour` | Circuit breaker. On tripping, the channel emits one "further notifications suppressed" audit line and then goes quiet for the rest of the hour, so the silence is distinguishable from a broken notifier. |
| `dedup_window_secs` | Suppress a repeated event with the same dedup key for this long. Generalizes what used to be `disk_alert.repeat_interval_secs`. |

### 5.1.2 Delivery, Retry and the Outbox

Every notification goes through a durable outbox before it is sent. Entries
are persisted to `{storage_dir}/notification_outbox.json` before the first
attempt, retried per channel with capped exponential backoff, and
dead-lettered after `max_attempts`.

Retry is at-most-once **per channel**: a message that reached Telegram but
not Discord is retried only against Discord, so a retry never duplicates a
delivered message.

* A `429` or `503` is retried, and a provider's `Retry-After` is honoured
  rather than being retried straight back into the same rate limit.
* A `404` or `401` is **permanent** - retrying a malformed webhook URL
  cannot help, so it is dead-lettered immediately instead of consuming
  every attempt.
* Each channel has a 30 second request timeout, and channels are attempted
  concurrently, so one unresponsive host cannot stall the others.

Tuning lives under `video.recording.notifications`
(`outbox_buffer`, `max_attempts`, `backoff_initial_secs`,
`backoff_max_secs`).

### 5.1.3 Testing Your Configuration

`POST /api/v1/config/messaging/test` renders and optionally sends a chosen
event, and returns the exact rendered body per channel.

```json
{ "event": "recording.completed", "channel": "telegram", "preview": true }
```

* `event` - any event id. Defaults to `system.info`.
* `channel` - restrict to one channel id. Omit for all configured channels.
* `preview` - render only, send nothing. Use this to iterate on a template
  without spamming a channel.

The test deliberately bypasses `notify_on` and the suppression window: you
asked for this one explicitly.

### 5.2 Templating (Handlebars)

**Every** channel supports templates - Telegram, Discord, REST, Pushover, ntfy, Gotify, Slack and `command`.
Tuliprox uses [Handlebars](https://handlebarsjs.com/) to format message bodies, which allows rich, structured
notifications (e.g. Discord embeds or Markdown tables).

Templates are keyed by event id:

```yaml
telegram:
  templates:
    recording.completed: 'file:///config/messaging_templates/telegram_recording_completed.templ'
```

A `{prefix}_{event}.templ` file dropped into `/config/messaging_templates/` is picked up automatically -
`telegram_recording_completed.templ`, `discord_playlist_update_failed.templ`, and so on. Dots in the event id become
underscores in the filename.

Templates are resolved and compiled **once** and then cached: local files are re-read when their mtime changes,
remote templates revalidate on a 5 minute TTL, and a remote template that cannot be refreshed serves the cached copy
rather than silently falling back to the built-in text.

If no template applies, the channel sends the event's built-in `title` and `body`, which are always populated and
always human-readable.

#### Loading Methods

1. **Raw String:** Define the template directly in your `config.yaml` (best for simple one-liners).
2. **URI:** Reference a local file (`file://...`) or a remote resource (`http://...`).

> **Note:** If you save your configuration via the **Web UI**, raw template strings are automatically moved to
> individual files in `/config/messaging_templates/` to keep your main configuration clean.

#### Available Context Variables

The Handlebars engine provides a rich context object. Depending on the `kind` of notification, different variables are
populated:

Every template sees a uniform `event` object, plus the legacy top-level keys documented below:

* `{{event.id}}`: The canonical event id, e.g. `recording.completed`.
* `{{event.severity}}`: `info`, `warn`, `error` or `critical`.
* `{{event.title}}`: One-line summary. Always present.
* `{{event.body}}`: Plain-text body. Always present.
* `{{event.fields}}`: The typed payload for this event.
* `{{event.timestamp}}`: Unix seconds. Use `{{timestamp}}` for the RFC 3339 form.

The legacy keys below continue to work exactly as before, so existing templates need no change:

* `{{message}}`: The primary text payload. Used for human-readable `info` messages or the summary of an `error`.
* `{{kind}}`: The event category (`Info`, `Stats`, `Error`, `Watch`, ...). Use this in Handlebars helpers (e.g.,
  `{{#if (eq kind "error")}}`) to create conditional layouts.
* `{{severity}}`: The event severity.
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
    * **Target Processing:** A target may include a `processing` object with exact filter inspected/retained/removed
      counts, renamed item/field counts, matched mapping rules, emitted virtual items, and mapping diagnostics. The
      object is omitted when no processing work was recorded, so existing templates remain compatible.
* `{{processing}}`: **Engine State & Telemetry.** Provides insight into the *internal execution environment* during the
  task. It includes data on memory allocation peaks, active worker threads, and non-blocking diagnostic warnings.
  * **Access:** Access properties directly via dot-notation (e.g., `{{processing.memory_peak_mb}}`). Use this to
    monitor system health and resource consumption during heavy mapping cycles.
* `{{watch}}`: **Change Tracking Data.** Available for `playlist.watch.changed`. One event per target and update.
  * `watch.target`: the target name.
  * `watch.groups_added` / `watch.groups_removed`: `{ items, total, truncated }`, absent when there is none.
  * `watch.channels_added` / `watch.channels_removed`: list of `{ group, items, total, truncated }`, absent when
    there is none.
  * `watch.changes`: the raw list; each entry has a `kind` (`groups_added`, `groups_removed`, `channels_added`,
    `channels_removed`).
  * `items` may be shorter than `total` (`truncated: true`), or empty when an update changed more than 500 channels.
  * **Example:**

    ```handlebars
    {{#if watch.groups_added}}
    🆕 New groups:
    {{#each watch.groups_added.items}}
    • {{this}}
    {{/each}}
    {{/if}}
    {{#each watch.channels_added}}
    ➕ {{group}} ({{total}}):
    {{#each items}}
    • {{this}}
    {{/each}}
    {{/each}}
    ```

* `{{recording}}`: **DVR Lifecycle Data.** Available for `recording_started`, `recording_completed`, and
  `recording_failed`. Common fields are `programme_title`, `channel`, `effective_start`, `effective_end`,
  `visibility`, `output_filename`, and `failure_reason` for failed recordings.

* `{{disk}}`: **Disk Alert Data.** Available for `system.disk.alert`: `level`, `percent`, `total_bytes`,
  `used_bytes`, `free_bytes`.

Recording lifecycle notifications are global-channel notifications. Tuliprox sends them for shared recordings,
legacy administrator recordings, and built-in administrator private recordings. Private recordings owned by regular
users are suppressed.

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

### 5.3 Disk-Space Alerts (`messaging.disk_alert`)

Tuliprox monitors the free space on the **current working directory's mount** and notifies you via the
standard messaging channels (Telegram, Discord, Pushover, REST) before the disk fills up. The feature is
**opt-in**: no `disk_alert` block means no monitoring and no notifications.

**How it works:** A background loop in `backend/app/src/api/sys_usage.rs` ticks every **2 seconds** (fixed
sampling interval — see `SYSTEM_USAGE_INTERVAL` in that file, **not configurable**) and compares the
current working directory's percent-used against two thresholds. The `DiskAlertMonitor` state machine
(`backend/app/src/api/sys_usage.rs::DiskAlertMonitor`) decides when to emit a `DiskAlert`:

| Level      | Default Trigger                         | Operator Action                                |
|:-----------|:----------------------------------------|:-----------------------------------------------|
| `Normal`   | < `warn_percent`                        | All good.                                      |
| `Warn`     | ≥ `warn_percent` (default `80.0`)       | Plan a cleanup.                                |
| `Critical` | ≥ `critical_percent` (default `95.0`)   | Cleanup is urgent; stream generation may fail. |

A `DiskAlert` is emitted whenever the level is non-Normal **and** either:

1. the level just changed (Normal → Warn, Warn → Critical, or any step back down), **or**
2. the disk has stayed in the same alert state for at least `repeat_interval_secs` seconds since the
   previous notification.

This **periodic re-notification** is intentional: a long-running full-disk situation that nobody fixes
would otherwise go silent after the first transition, defeating the purpose of the feature. With the
default `repeat_interval_secs: 3600` (1 hour), a stuck-at-87% disk produces one `Warn` notification per
hour instead of a single one-shot alert.

```yaml
messaging:
  notify_on: [ "info", "stats", "error", "watch" ]

  # Disk-space alerts are sent through the channels above. Configure the
  # thresholds and the re-arm interval here. The 2-second sampling tick is
  # internal and not configurable.
  disk_alert:
    # Percent-used at which the Warn level is reached. Default: 80.0.
    # Must be in [0, 100].
    warn_percent: 80.0
    # Percent-used at which the Critical level is reached. Default: 95.0.
    # Must be in (warn_percent, 100].
    critical_percent: 95.0
    # Re-arm interval in seconds. While the disk stays in the same alert
    # state, the alert is re-sent after this many seconds. Default: 3600
    # (1 hour). This is NOT the sampling interval — sampling is fixed at
    # 2 seconds and is not currently configurable.
    repeat_interval_secs: 3600

  telegram:
    bot_token: "<TOKEN>"
    chat_ids: [ "<CHAT_ID>" ]
    templates:
      # Custom templates per level. Plain-text defaults are used if omitted.
      disk_alert_warn: "⚠️ Disk is {{percent}}% full (Warn threshold)."
      disk_alert_critical: "🚨 Disk is {{percent}}% full — cleanup now!"
      disk_alert_normal: "✅ Disk back to normal at {{percent}}%."
```

> **Note on `mounts`:** Earlier drafts of this section described a `mounts` config field. The current
> implementation has no such field — the disk probe is hard-coded to sample the current working directory's
> mount (see `DiskProbe::for_cwd()` in `backend/app/src/api/sys_usage.rs`). If you need to monitor a different
> path, change the working directory of the Tuliprox process.

**Available Handlebars Variables** (all `disk_alert_*` templates):

* `{{level}}` — The new level after the transition (`Warn`, `Critical`, or `Normal`).
* `{{percent}}` — Current percent-used, rounded to one decimal place.
* `{{free_bytes}}` / `{{total_bytes}}` / `{{used_bytes}}` — Raw byte counts for the sampled mount.
* `{{timestamp}}` — Event time in UTC (ISO 8601 / RFC3339).

**Available template keys per channel** (Handlebars): `disk_alert_warn`, `disk_alert_critical`,
`disk_alert_normal`. If a template is not configured for a level, Tuliprox falls back to a clear plain-text
default line, e.g.:

> `[WARN] Disk is 82.3% full (free=3.4GB total=19.0GB).`
> **Note:** Disk alerts are sent through **every** configured messaging channel. If you want a channel to
> ignore disk alerts, simply do not declare the matching `disk_alert_*` template key for it and it will still
> receive the plain-text default. Use the standard `notify_on` list to control which event kinds are sent at
> all.

---

### 5.4 Webhook Signing

Set `rest.signing_secret` and every request carries:

```text
X-Tuliprox-Timestamp: 1700000000
X-Tuliprox-Signature: sha256=<hex>
```

The signature is HMAC-SHA256 over `{timestamp}.{body}` using the shared secret. The timestamp is inside the signed
payload, so a captured request cannot be replayed later with a fresh timestamp header. Verify it by recomputing the
HMAC and comparing in constant time, and reject requests whose timestamp is too far from your clock.

### 5.5 Running a Local Program

The `command` channel runs a local program with the event JSON on stdin, plus `TULIPROX_EVENT_ID`,
`TULIPROX_EVENT_SEVERITY` and `TULIPROX_EVENT_TITLE` in the environment.

```yaml
messaging:
  command:
    program: "/config/scripts/notify.sh"
    args: [ "--from-tuliprox" ]
    timeout_secs: 30
```

The program is executed **directly, never through a shell**, so there are no quoting rules to get wrong and event
content cannot be interpreted as shell syntax.

> **Security:** this runs arbitrary code as the tuliprox process user. It is opt-in and never configured by default.
> Treat the script as part of your trusted computing base.

### 5.6 Secrets in the Web UI

Channel secrets - the Telegram bot token, Pushover token and user key, ntfy and Gotify tokens, the REST signing
secret, and any `Authorization`-style REST header - are masked as `********` when the configuration is read back
through the API. Leaving a masked value untouched when you save keeps the stored secret; replacing it writes the new
one through.

## 6. Video & Web Search (`video`)

Optional video-related behaviors, mostly utilized by the Web UI.

```yaml
video:
  web_search: "https://www.imdb.com/search/title/?title={}"
  extensions: [ "mkv", "mp4", "avi", "ts", "webm" ]
  recording:
    directory: /tmp/tuliprox_recordings
    headers:
      User-Agent: "AppleTV/tvOS/9.1.1"
      Accept: "video/*"
    organize_into_directories: true
    episode_pattern: '.*(?P<episode>[Ss]\d{1,2}(.*?)[Ee]\d{1,2}).*'
```

* `web_search`: A template URL used in the Web UI to quickly search for a movie title (replaces `{}` with the title).
* `extensions`: Defines which file endings Tuliprox categorizes as VOD/Video content when transforming M3U to Xtream.
* `recording`: Configuration for the DVR.
  * `directory`: Where recordings are saved.
  * `headers` (optional): Custom HTTP headers used for the download request. This is useful for bypassing basic
    user-agent filters or setting specific media types.
  * `organize_into_directories`: If true, recordings are filed under the playlist group of the recorded item
    (`<group>/<file>`); series episodes share one folder per series inside their group (`<group>/<series>/<file>`).
  * `episode_pattern`: Crucial for the directory organization. It uses the mandatory Named Capture Group
    `(?P<episode>...)` in the Regex to identify and strip the episode identifier (e.g., `S01E01`)
    from the filename, ensuring all episodes of a show land in the same base-show folder.
  * `priority`: Provider priority for recordings. Lower values mean higher priority.
  * `reserve_slots_for_users`: Keeps provider headroom for normal foreground users before background-priority
    transfers
    may consume the last slots.
  * `max_background_per_provider`: Limits how many background-priority transfers may run in parallel against one
    provider.
  * `retry_backoff_initial_secs`: Initial retry delay for transient download/recording failures.
  * `retry_backoff_multiplier`: Growth factor applied to each later retry delay.
  * `retry_backoff_max_secs`: Maximum retry delay once the backoff curve reaches its cap.
  * `retry_backoff_jitter_percent`: Randomizes retry delays to avoid retry spikes after shared upstream problems.
  * `retry_max_attempts`: Maximum number of transient retries before a transfer is marked as failed.

Tuliprox handles these transfers like provider-bound background streams:

* They respect provider limits, user priorities, and connection preemption instead of bypassing normal stream capacity.
* Waiting for provider capacity is notify-based, not polling-based.
* The Web UI loads an initial transfer snapshot and then stays synchronized through websocket updates.
* Changes to `video.recording` participate in hot config reloads. The background scheduler restarts and active transfers
  are  
  re-queued so they continue under the updated download configuration.
* RBAC integration is explicit:
  * `recording.read` allows opening DVR task, quota, library, and recurring-rule views.
  * `recording.create` allows requesting a new DVR recording.
  * `recording.manage` allows editing, cancelling and managing DVR tasks and rules.
  * `recording.delete` allows removing a recording from a library.
* The recording queue **fails closed**. It is not discarded on corruption: a damaged
  `recordings.db` is rebuilt from the recovery history, and a database that is ahead of every
  surviving history refuses to start rather than silently adopting a queue it cannot account
  for. Losing the recovery directory while the database survives is therefore a startup error,
  not an empty queue.

### 6.1 DVR Runtime Files

The DVR runtime keeps durable state under `storage_dir`:

* `recordings.db`: the recording queue, stored as a B+Tree.
* `recording_rules.json`: recurring recording rules and tombstones.

Recovery generations live under `backup_dir/recordings_recovery/`, deliberately away from
`storage_dir` so they can be pointed at a different filesystem. Each generation holds a
checkpoint and the journal that continues it; the current generation plus one verified
predecessor are retained. Every queue mutation is appended to the journal and made durable
*before* the B+Tree is touched, so a crash can only leave recovery ahead of the database — which
the next start repairs by rebuilding — and never the database ahead of its own history.

Records are stored as field-named JSON carrying their schema version, so a future change to the
record shape is migrated forward on restore rather than refused.

There is **no migration** from the previous `recordings_state.json`. An existing queue is not
carried across; the repository starts empty.

Live recordings use a partial-file lifecycle. The worker writes to `<filename>.partial` and renames it to the final
path only after ffmpeg exits successfully and the final path is still free.

> **See also:** the full [DVR Operator Reference](../operator/dvr.md) — configuration reference, directory layout,
> filename placeholders, lifecycle / restart, quota charge-by-state, disk admission, safe deletion, authorization
> matrix, identity-registry bootstrap, token refresh, the removed `/file/record`, REST + WebSocket surface, conflict
> preview, recurring-rule matching + DST + reconciliation, at-most-once notification protocol, migration checklist,
> and the 32-scenario acceptance sweep.

#### 6.1.1 Filename placeholders

The filename template supports these placeholders (filename only — **never** the directory):

| Placeholder         | Resolves to                                                       |
|---------------------|-------------------------------------------------------------------|
| `{channel}`         | Channel name                                                      |
| `{program_title}`   | Programme title (sanitized)                                       |
| `{start_time}`      | UTC `YYYY-MM-DDTHH-MM`                                            |
| `{end_time}`        | UTC `YYYY-MM-DDTHH-MM`                                            |
| `{episode}`         | Episode identifier extracted by `episode_pattern`                 |
| `{owner}`           | The owner principal id (`user:<id>` or `legacy:admin`)            |

> **Security:** `{owner}` is allowed **only** in the filename template, never in directory templates.
> Directory templates are resolved against the caller's identity, so the directory part is intrinsically owner-scoped.

#### 6.1.2 Authorization matrix

The DVR layer runs an additional authorization pass on top of `recording.read`, `recording.create`,
`recording.manage` and `recording.delete`.

| Visibility | Owner                  | Admin (`builtin:admin`) | Foreign user | Notes                                     |
|------------|------------------------|-------------------------|--------------|-------------------------------------------|
| `private`  | read + write + delete  | read + write + delete   | —            | Foreign reads return 404                  |
| `shared`   | —                      | read + write + delete   | read         | Only admins create shared recordings      |

#### 6.1.3 Identity bootstrap

If the `users` table is empty on first boot:

1. Tuliprox reads `TULIPROX_BOOTSTRAP_ADMIN` from the environment.
2. The built-in `builtin:admin` role is assigned to that user.
3. The bootstrap admin must use `POST /api/v1/auth/login` to obtain a JWT.
4. From that point on, the admin creates additional users via `POST /api/v1/users`.
5. If `TULIPROX_BOOTSTRAP_ADMIN` is unset and the table is empty, the server **fails closed** at boot.
6. The bootstrap admin cannot be deleted while it is the sole `builtin:admin` member.

#### 6.1.4 Token refresh on schema bump

When the JWT schema version is bumped (a new field is added), existing tokens are rejected with
`401 Unauthorized` and an `X-Token-Refresh: required` response header. The frontend automatically calls
`POST /api/v1/auth/refresh` to mint a new token. The wire code is `recording_token_refresh_required` so the
toastr surfaces a stable, translatable message. Operators upgrading across a schema-bump release do not need
to do anything manually.

#### 6.1.5 The removed `/file/record`

`POST /api/v1/file/record` and the `/api/v1/file/download/*` family have been
**removed**. There is no deprecated alias and no compatibility shim; a caller still using them gets a
`404`. Use `POST /api/v1/recording/requests`, which answers `204 No Content`
(see [REST API cookbook](../rest-api-cookbook.md#recordings)).

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

## 10. MCP Server (`mcp`)

Tuliprox can expose a [Model Context Protocol](https://modelcontextprotocol.io) endpoint (`POST {web_ui_path}/mcp`)
that lets AI assistants such as Claude Desktop or Cursor inspect a running instance through a fixed set of tools. Every
tool is an adapter over an existing `/api/v1` route, so the caller's Web UI token and RBAC permissions apply. The
endpoint is **off by default** and **read-only by default**.

```yaml
mcp:
  enabled: true
  read_only: true
  allowed_origins:
    - https://tuliprox.example.com
  max_response_bytes: 262144
```

| Parameter            |      Type       | Default  | Technical Impact                                                                                                                                                                                                                                                                                           |
|:---------------------|:---------------:|:---------|:-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `enabled`            |      Bool       | `false`  | Mounts the MCP endpoint at `{web_ui_path}/mcp`. When `false`, or when the block is absent, no route exists.                                                                                                                                                                                                |
| `read_only`          |      Bool       | `true`   | Hides and rejects tools that change state (`update_playlists`, `update_geoip`). Always forced to `true` when `web_ui.auth` is disabled, because no permission layer protects the write routes.                                                                                                             |
| `allowed_origins`    | List of Strings | `[]`     | Extra `Origin` values accepted besides the request `Host` (DNS-rebinding protection). Needed when a reverse proxy rewrites `Host`. Each entry must be an origin (`scheme://host[:port]`, custom schemes allowed) without path, query or fragment. Requests without an `Origin` header are always accepted. |
| `max_response_bytes` |       Int       | `262144` | Tool results larger than this are truncated with an explicit marker. Minimum `1024`.                                                                                                                                                                                                                       |

### Technical Background

* **Authentication:** with `web_ui.auth` enabled a missing, invalid or expired token is a real HTTP `401`. A valid
  user without the permission of the underlying REST route gets a tool result with `isError: true` (`HTTP 403`).
* **Stdio clients:** `tuliprox --mcp-stdio --mcp-url http://localhost:8901` bridges stdin/stdout to the endpoint of
  a running instance. Credentials come from `TULIPROX_MCP_USER` and `TULIPROX_MCP_PASSWORD`.
* **Applied at startup:** the endpoint is mounted when the server starts. Restart Tuliprox after changing the block.
* **Web UI:** the block can be edited on the **MCP** page of the Web UI config.

See the [MCP Server Operator Reference](../operator/mcp.md) for the tool catalog, client setup and troubleshooting.

---

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
* `hls_session_or_lease_expired.ts` (Shared-HLS session or access lease expired; restart stream)
* `maintenance.ts` (The input is in maintenance mode; see [Maintenance mode](../rest-api-cookbook.md#maintenance-mode))

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
