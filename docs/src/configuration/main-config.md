# Config Reference

`config.yml` controls the application runtime, storage, reverse proxy behavior and optional subsystems.

## Top-level entries

Common top-level fields are:

- `api`
- `storage_dir`
- `default_user_agent`
- `process_parallel`
- `messaging`
- `video`
- `metadata_update`
- `schedules`
- `backup_dir`
- `mapping_path`
- `template_path`
- `update_on_boot`
- `web_ui`
- `reverse_proxy`
- `log`
- `user_access_control`
- `connect_timeout_secs`
- `custom_stream_response_path`
- `custom_stream_response_timeout_secs`
- `hdhomerun`
- `proxy`
- `ipcheck`
- `config_hot_reload`
- `sleep_timer_mins`
- `accept_unsecure_ssl_certificates`
- `disk_based_processing`
- `library`

Relative paths are resolved against the tuliprox home directory:
`--home` -> `TULIPROX_HOME` -> directory of the `tuliprox` binary.

## Core runtime fields

### `process_parallel`

Enable this on multi-core systems if you want parallel processing.
Be aware that multiple worker threads can also consume multiple provider connections during updates.

### `api`

`api` contains the server-mode settings. Example:

```yaml
api:
  host: localhost
  port: 8901
  web_root: ./web
```

`web_root` is resolved relative to the tuliprox home directory when it is not absolute.

### `storage_dir`

Storage location for persisted playlists, databases and metadata.
Example:

```yaml
storage_dir: ./data
```

### `mapping_path` and `template_path`

Tuliprox can load mappings and templates centrally from files or directories.

- `mapping_path`: single file or directory
- `template_path`: single file or directory

Defaults:

- mappings: `mapping.yml`
- templates: `template.yml`

If a path points to a directory, all `*.yml` files are loaded in alphanumeric order and merged.
Template names must remain globally unique.

## Provider failover and rotation

Tuliprox supports provider failover and DNS-aware rotation.
Providers can expose multiple URLs, and Tuliprox can rotate to the next candidate on supported failure conditions.

### `provider://` scheme

Configurations can reference providers via `provider://<provider_name>/...`.
Tuliprox resolves that to the current active provider URL or resolved IP.

### Automatic failover triggers

Failover is triggered for:

- network timeouts
- HTTP `408`
- HTTP `500`, `502`, `503`, `504`
- HTTP `404`, `410`, `429`

It does not trigger for `401` or `403`.

### Provider DNS resolution

Each provider can optionally enable a `dns` block with:

- `enabled`
- `refresh_secs`
- `prefer`: `system` | `ipv4` | `ipv6`
- `max_addrs`
- `schemes`
- `keep_vhost`
- `overrides`
- `on_resolve_error`
- `on_connect_error`

Resolved IPs are persisted in `provider_dns_resolved.json` below `storage_dir`.

Example:

```yaml
provider:
  - name: my_provider
    urls:
      - http://provider-a.example
      - http://provider-b.example
    dns:
      enabled: true
      refresh_secs: 300
      prefer: ipv4
      schemes: [http, https]
      keep_vhost: true
      max_addrs: 2
      on_resolve_error: keep_last_good
      on_connect_error: try_next_ip
```

## `messaging`

Optional notification system for `telegram`, `discord`, `rest` and `pushover`.

`notify_on` can include:

- `info`
- `stats`
- `error`
- `watch`

Templates can be raw strings or `file://` / `http(s)://` URIs.
Handlebars variables include `message`, `kind`, `timestamp`, `stats`, `watch` and `processing`.

Example:

```yaml
messaging:
  notify_on:
    - info
    - stats
    - error
  telegram:
    markdown: true
    bot_token: "<telegram bot token>"
    chat_ids:
      - "<chat id>"
      - "<chat id>:<thread id>"
```

## `video`

Optional video-related behavior:

- `extensions`: file extensions treated as video content
- `download`: download integration for the Web UI
- `web_search`: URL template for media searches

Example:

```yaml
video:
  web_search: "https://www.imdb.com/search/title/?title={}"
  extensions: [mkv, mp4, avi]
  download:
    directory: /tmp
    organize_into_directories: true
    episode_pattern: '.*(?P<episode>[Ss]\d{1,2}(.*?)[Ee]\d{1,2}).*'
```

## `metadata_update`

Controls metadata resolve, TMDB, probing and FFprobe behavior.

Important groups:

- `log`
- `resolve`
- `probe`
- `tmdb`
- `ffprobe`
- `retry_delay`
- `worker_idle_timeout`
- `max_queue_size`
- `no_change_cache_ttl_secs`
- `probe_fairness_resolve_burst`

Key fields:

- `resolve.max_retry_backoff`
- `resolve.min_retry_base`
- `resolve.max_attempts`
- `resolve.exhaustion_reset_gap`
- `probe.cooldown`
- `probe.max_attempts`
- `probe.retry_backoff_step_1`
- `probe.retry_backoff_step_2`
- `probe.retry_backoff_step_3`
- `probe.retry_load_retry_delay`
- `probe.backoff_jitter_percent`
- `probe.user_priority`
- `tmdb.enabled`
- `tmdb.api_key`
- `tmdb.rate_limit_ms`
- `tmdb.cache_duration_days`
- `tmdb.language`
- `tmdb.cooldown`
- `tmdb.match_threshold`
- `ffprobe.enabled`
- `ffprobe.timeout`
- `ffprobe.analyze_duration`
- `ffprobe.probe_size`
- `ffprobe.live_analyze_duration`
- `ffprobe.live_probe_size`

Example:

```yaml
metadata_update:
  cache_path: metadata
  log:
    queue_interval: 30s
    progress_interval: 15s
  resolve:
    max_retry_backoff: 1h
    min_retry_base: 5s
    max_attempts: 3
    exhaustion_reset_gap: 1h
  probe:
    cooldown: 7d
    retry_load_retry_delay: 1m
    retry_backoff_step_1: 10m
    retry_backoff_step_2: 30m
    retry_backoff_step_3: 1h
    max_attempts: 3
    backoff_jitter_percent: 20
    user_priority: 127
```

Duration fields support `s`, `m`, `h`, `d` or plain seconds.
Size fields support `B`, `KB`, `MB`, `GB`, `TB` or plain bytes.

## `schedules`

Tuliprox schedules use cron expressions with a leading seconds field.

Example:

```yaml
schedules:
  - schedule: "0  0  8  *  *  *  *"
    type: PlaylistUpdate
    targets:
      - m3u
  - schedule: "0  0  20  *  *  *  *"
    type: LibraryScan
```

Supported schedule types:

- `PlaylistUpdate`
- `LibraryScan`
- `GeoIpUpdate`

## `reverse_proxy`

This block controls stream delivery, buffering, caching, headers, rate limiting and resource retries.
The detailed stream behavior lives in [Streaming And Proxy](../streaming-and-proxy.md).

## `backup_dir`

Location for configuration backups written from the Web UI.

## `update_on_boot`

If `true`, a playlist update starts automatically at application start.

## `log`

Supported fields:

- `sanitize_sensitive_info`
- `log_active_user`
- `log_level`

`log_level` can be a plain level such as `debug` or a module list such as:

```yaml
log:
  log_level: hyper_util::client::legacy::connect=error,tuliprox=debug
```

## `web_ui`

Controls the Web UI itself:

- `enabled`
- `user_ui_enabled`
- `content_security_policy`
- `path`
- `player_server`
- `kick_secs`
- `combine_views_stats_streams`
- `auth`

`auth` supports:

- `enabled`
- `issuer`
- `secret`
- `token_ttl_mins`
- `userfile`

Example:

```yaml
web_ui:
  enabled: true
  user_ui_enabled: true
  auth:
    enabled: true
    issuer: tuliprox
    secret: "<jwt secret>"
    userfile: user.txt
```

The `userfile` format is `username:password_hash` per line.
Password hashes can be generated with `tuliprox --genpwd`.

## Access control and stream fallback

### `user_access_control`

If enabled, Tuliprox checks provider or configured user status, expiration date and max-connections data.

### `connect_timeout_secs`

Defines the connect-phase timeout for provider requests.
`0` disables the connect timeout.

### `custom_stream_response_path`

Directory that contains fallback transport streams:

- `channel_unavailable.ts`
- `user_connections_exhausted.ts`
- `provider_connections_exhausted.ts`
- `low_priority_preempted.ts`
- `user_account_expired.ts`
- `panel_api_provisioning.ts`

`custom_stream_response_timeout_secs` can cap the playback duration of those fallback streams.

## Other optional sections

### `user_config_dir`

Storage location for user-specific configuration such as bouquets.

### `hdhomerun`

Enables HDHomeRun emulation for Plex, Jellyfin, Emby or TVHeadend integration.
It supports multiple virtual devices, optional lineup auth and SSDP plus SiliconDust discovery.

### `proxy`

Global outgoing proxy for upstream requests.
Supported schemes include `http`, `https` and `socks5`.

### `ipcheck`

Defines endpoints and optional regexes for public IP detection:

- `url`
- `url_ipv4`
- `url_ipv6`
- `pattern_ipv4`
- `pattern_ipv6`

### `config_hot_reload`

If enabled, mapping files and API-proxy configuration are hot reloaded.

### `library`

Local media library integration:

- recursive directory scanning
- movie vs series classification
- NFO reading and writing
- TMDB enrichment
- incremental scans
- stable virtual IDs

Example:

```yaml
library:
  enabled: true
  scan_directories:
    - enabled: true
      path: "/projects/media"
      content_type: auto
      recursive: true
  supported_extensions: ["mp4", "mkv", "avi", "mov", "ts", "m4v", "webm"]
```
