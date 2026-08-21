# 🔌 Pillar 2: `source.yml` (Inputs, Panel API & Targets)

The `source.yml` serves as the central orchestration hub for all data flows within Tuliprox.
It defines the lifecycle of a stream—from the upstream provider to the end-user device—through three primary
architectural layers:

* **`providers` (Resilience Layer):** Defines backend endpoints and failover logic.
  Use this to implement intelligent URL rotation and ensure high availability across multiple mirrors.
* **`inputs` (Ingestion Layer):** Manages upstream data sources. This layer handles credential management, connection
  pooling via **Aliases**,
  and automated account lifecycle management through **Panel API** integration.
* **`sources` & `targets` (Egress Layer):** The final mapping stage where ingested data is filtered, transformed,
  and routed to specific **Targets** (M3U, Xtream, Strm or HDHomeRun) for consumption by end devices.

## Top-level entries

```yaml
templates:
provider:
inputs:
sources:
```

| Block       | Description                                                           | Link                                                       |
|:------------|:----------------------------------------------------------------------|:-----------------------------------------------------------|
| `templates` | *(Legacy)* Inline templates for filter macros. Prefer `template.yml`. |                                                            |
| `provider`  | Provider Failover & DNS Rotation definitions.                         | [See section](#1-provider-failover--dns-rotation-provider) |
| `inputs`    | Data Sources (Providers, Files, Batches, Library).                    | [See section](#2-inputs-data-sources-inputs)               |
| `sources`   | Routing logic combining inputs to output targets.                     | [See section](#3-routing--targets-sources)                 |

---

## 1. Provider Failover & DNS Rotation (`provider`)

Tuliprox includes a robust failover engine for unstable IPTV providers. You can define backup URLs and intelligent IP
rotation.

Define a `provider` block globally in `source.yml` to specify multiple backup URLs:

```yaml
provider:
  - name: my_failover_provider
    urls:
      - http://primary.example.com
      - http://backup.example.com
    provider_url_selection_policy: resume_last_working  # or restart_from_first
    dns:
      enabled: true
      refresh_secs: 300
      prefer: ipv4  # system, ipv4, ipv6
      schemes: [ http, https ]
      keep_vhost: true
      max_addrs: 2
      on_resolve_error: keep_last_good  # or fallback_to_hostname
      on_connect_error: try_next_ip     # or rotate_provider_url
      overrides:
        "primary.example.com":
          - 203.0.113.10
```

### Provider Parameters (`provider[]`)

| Parameter                       | Type   | Default               | Technical Impact                                                                                                                                                                                             |
|:--------------------------------|:-------|:----------------------|:-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `name`                          | String | required              | Internal provider identifier referenced by `provider://<name>` URLs. Must be unique within the provider list.                                                                                                |
| `urls`                          | List   | required              | Ordered failover URL list for this provider. Tuliprox rotates through these URLs within a request when failover is triggered.                                                                                |
| `provider_url_selection_policy` | Enum   | `resume_last_working` | Controls how a new request chooses its starting URL. `resume_last_working` starts at the last successful URL. `restart_from_first` always begins again at `urls[0]` and only fails over within that request. |
| `dns`                           | Object | unset                 | Optional DNS/IP rotation settings for the provider. See the table below.                                                                                                                                     |

### DNS Rotation Parameters (`provider.dns`)

| Parameter          | Type | Default          | Technical Impact                                                                                                                                                                              |
|:-------------------|:-----|:-----------------|:----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `refresh_secs`     | Int  | `300`            | The interval in seconds the background task resolves the hostnames. (Minimum effective value is 10).                                                                                          |
| `prefer`           | Enum | `system`         | Which IP protocol to prefer during DNS resolution. Options: `system`, `ipv4`, `ipv6`.                                                                                                         |
| `max_addrs`        | Int  | `None`           | Hard limit on the number of resolved IPs to retain per host.                                                                                                                                  |
| `schemes`          | List | `[http, https]`  | The HTTP schemes that IP connection rotation applies to.                                                                                                                                      |
| `keep_vhost`       | Bool | `false`          | If `true`, the `Host` header retains the original `hostname[:port]`. If `false`, it uses `IP[:port]`. Essential for reverse proxies upstream!                                                 |
| `on_resolve_error` | Enum | `keep_last_good` | Policy on DNS resolution failure. Options: `keep_last_good` (uses cached IPs), `fallback_to_hostname` (clears cache, forcing host lookup).                                                    |
| `on_connect_error` | Enum | `try_next_ip`    | Policy on TCP connection failure. Options: `try_next_ip` (cycles to the next resolved IP for the same host), `rotate_provider_url` (instantly fails over to the next URL in the `urls` list). |

### 1.1 DNS Resolved IP Persistence

Resolved IPs are persisted to `{storage_dir}/provider_dns_resolved.json` (not to `source.yml`).
This file is written atomically after each DNS refresh cycle and read at startup to seed DNS caches before the
background resolver
completes its first cycle.
On config hot-reloads, DNS caches are carried over from previous provider instances so that resolved IPs are available
immediately.

### Failover Triggers

Tuliprox automatically switches URLs or DNS IPs on failure.
Failover **DOES** occur on:

* Network Timeouts
* HTTP 5xx errors (500, 502, 503, 504)
* HTTP 404 / 410 / 429

Failover **DOES NOT** trigger on:

* HTTP 401 / 403 (Authentication errors, to avoid rotating due to a banned account).

## 2. Inputs (Data Sources) (`inputs`)

An `input` represents an upstream provider or a local media library.

```yaml
inputs:
  - name: my_provider
    type: xtream
    url: provider://my_failover_provider
    username: my_user
    password: my_password
    enabled: true
    sequential_group: 1
    cache_duration: 1d
    persist: playlist_{}.m3u
    method: GET
    exp_date: "2028-11-30 12:34:12"
    headers: { }
    options: { }
    epg: { }
    aliases: [ ]
    panel_api: { }
```

### Input Base Parameters

| Parameter               | Type   | Required | Default | Technical Impact & Background                                                                                                                                                                                                                                                                                                                                                                                                             |
|:------------------------|:-------|:--------:|:--------|:------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `name`                  | String |   Yes    |         | Internal reference ID for Tuliprox. Must be strictly unique. Critical for persistent UUID generation!                                                                                                                                                                                                                                                                                                                                     |
| `type`                  | Enum   |    No    | `m3u`   | Allowed: `m3u`, `xtream`, `stalker`, `library`, `staged`, `emby`, `jellyfin`, `plex`, and `m3u_batch` / `xtream_batch` / `stalker_batch` (CSV offloading). Stalker inputs use the portal handshake/catalog flow instead of a plain playlist download.                                                                                                                                                                                     |
| `url`                   | String |   Yes    |         | The Provider URL. Tuliprox supports magic scheme prefixes: `http(s)://`, `file://`, `batch://`, and **`provider://my_failover_provider`** (for the Failover System above).                                                                                                                                                                                                                                                                |
| `username` / `password` | String |  Often   |         | Mandatory for `xtream` and for Stalker inputs using `credentials_only` or `mac_plus_credentials`. Stalker inputs using `mac_only` do not need them.                                                                                                                                                                                                                                                                                       |
| `enabled`               | Bool   |    No    | `true`  | If `false`, this input is completely ignored in all processing.                                                                                                                                                                                                                                                                                                                                                                           |
| `sequential_group`      | Int    |    No    |         | Optional non-zero process-wide group ID. With `process_parallel: true`, complete refreshes of inputs sharing an ID run one after another, including all Stalker page slices. Different groups and ungrouped inputs may overlap. Not valid for `staged` inputs; a staged overlay inherits its provider job.                                                                                                                                |
| `cache_duration`        | String |    No    | `0`     | **Crucial:** Determines how often Tuliprox actually downloads the raw list from the provider. At `1d` (1 day), Tuliprox serves from its local `.db` for 24 hours, even if you trigger hourly updates. This heavily protects against provider bans! Supported units are `s`, `m`, `h`, and `d`. If `cache_duration` is set, the cached provider playlist stored on disk is reused for subsequent updates instead of downloading it again.  |
| `persist`               | String |    No    |         | Optional path template (e.g., `./playlist_{}.m3u`) to permanently store the downloaded raw provider list locally on your disk. The `{}` in the filename is filled with the current timestamp. For `m3u` use a full filename. For `xtream` use a prefix like `./playlist_`.                                                                                                                                                                |
| `method`                | Enum   |    No    | `GET`   | HTTP Request method for playlist downloads (`GET` or `POST`).                                                                                                                                                                                                                                                                                                                                                                             |
| `exp_date`              | Mixed  |    No    |         | Expiration date as `"YYYY-MM-DD HH:MM:SS"` or Unix timestamp. In server mode, Tuliprox refreshes missing or soon-expiring Xtream account dates through the account's `player_api.php` credentials; see [Automatic Xtream Expiration Refresh](#automatic-xtream-expiration-refresh).                                                                                                                                                       |
| `headers`               | Dict   |    No    |         | Custom HTTP headers for the download (e.g., `User-Agent: My-Player`).                                                                                                                                                                                                                                                                                                                                                                     |
| `epg`                   | Object |    No    |         | Allows mapping of external XMLTV files (see [below](#input-subsections-object-keys)).                                                                                                                                                                                                                                                                                                                                                     |
| `aliases`               | List   |    No    |         | Connection pooling / Sub-accounts (see [below](#input-subsections-object-keys)).                                                                                                                                                                                                                                                                                                                                                          |
| `staged`                | Object |    No    |         | Staged overlay settings. Only valid when `type: staged` (see [below](#input-subsections-object-keys)).                                                                                                                                                                                                                                                                                                                                    |
| `panel_api`             | Object |    No    |         | Automated reseller account generation (see [below](#input-subsections-object-keys)).                                                                                                                                                                                                                                                                                                                                                      |

#### Minimal Stalker Input Example

```yaml
inputs:
  - name: stalker_main
    type: stalker
    url: http://portal.example.com/c/
    stalker:
      auth_mode: mac_only
      mag_preset: generic_safe
      endpoint_preference: auto
      device:
        mac_address: '00:1A:79:12:34:56'
    enabled: true
    options:
      stalker_pre_resolve_playback: false
      stalker_runtime_resolve_playback: true
```

Stalker refreshes write pages into an unpublished generation. Live, VOD, series, and EPG selected for one refresh
become active together only after the complete selection is durable. Until then, Tuliprox continues serving the
previous complete snapshot; a first import exposes no partial catalog. A saved checkpoint resumes after restart.

When `process_parallel` is enabled, progress messages include the input name. Targets wait for every enabled input in
their source and begin as soon as that source is ready, without waiting for unrelated sources.

Use `stalker_pre_resolve_playback: true` if you want Tuliprox to materialize playback URLs during refresh whenever the portal already grants them.
Keep `stalker_runtime_resolve_playback: true` when the portal uses expiring or session-bound temp links that may need a fresh `create_link`
call later during playback.

Stalker supports four authentication modes:

* `auto`: requires either a MAC address or a complete username/password pair.
* `mac_only`: requires `stalker.device.mac_address`; username/password are ignored.
* `credentials_only`: requires username and password; no MAC address is required.
* `mac_plus_credentials`: requires both a MAC address and username/password.

#### Input URL Schemes (`inputs[].url`)

Tuliprox utilizes a flexible URI-based system to define where input data originates.
Depending on the prefix used, the engine switches between remote downloads, local file access, or internal failover
logic.

| Scheme            | Target Type     | Technical Impact & Background                                                                                                                                                                                                                                                                                                 |
|:------------------|:----------------|:------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **`http(s)://`**  | Remote Server   | Standard method for downloading playlists from provider endpoints.                                                                                                                                                                                                                                                            |
| **`file://`**     | Local Storage   | Reads a playlist directly from the host filesystem. Useful for manual backups or pre-processed files.                                                                                                                                                                                                                         |
| **`provider://`** | Failover System | Resolves the URL via internal `provider` definitions. **Pro-Tip:** Use this to implement automatic rotation or failover between multiple mirrors/gateways of the same provider. New requests honor `provider_url_selection_policy`, so they can either resume from the last healthy URL or always restart from the first URL. |
| **`batch://`**    | CSV File        | Dedicated scheme for bulk alias management. Points to a local `;` separated CSV file (e.g., `batch://./aliases.csv`).                                                                                                                                                                                                         |

### Additional Notes

* **Automatic Type Conversion:** If the input `type` is set to `m3u` or `xtream` but the `url` starts with the
  `batch://` prefix,
  Tuliprox automatically upgrades the input to `m3u_batch` or `xtream_batch` respectively.
* **Batch Constraints:** For `m3u_batch` and `xtream_batch`, only **local** CSV sources are permitted.
  You must use either the `batch://` scheme or a plain absolute/relative filesystem path.
* **Protocol Restrictions:** To ensure stability in batch processing, URI schemes such as `provider://`, `http(s)://`,
  or `file://` are strictly rejected when used within a batch context.

### Input Subsections (Object Keys)

| Block       | Description                                                                | Link                                               |
|:------------|:---------------------------------------------------------------------------|:---------------------------------------------------|
| `headers`   | Custom HTTP request headers for playlist and EPG downloads.                | [See Headers](#21-headers-headers)                 |
| `options`   | Behavior controls for metadata resolution, stream probing, and skip logic. | [See Options](#22-input-options-options)           |
| `epg`       | XMLTV source management and Smart Match fuzzy logic settings.              | [See EPG](#23-epg-assignment--smart-match-epg)     |
| `aliases`   | Connection pooling for multiple subscriptions from the same provider.      | [See Aliases](#24-provider-aliases-aliases--batch) |
| `staged`    | Overlay settings for first-class staged inputs.                            | [See Staged](#25-staged-sources-staged)            |
| `panel_api` | Automated reseller panel integration (provisioning/renewal).               | [See Panel API](#26-provider-panel-api-panel_api)  |

---

### 2.1 Headers (`headers`)

Allows the injection of custom HTTP headers into outgoing requests for this specific provider.
This is often required for providers that enforce `User-Agent` whitelisting or specific authorization tokens.

| Parameter       | Type   | Technical Impact & Background                                        |
|:----------------|:-------|:---------------------------------------------------------------------|
| `User-Agent`    | String | Mimics a specific player or browser to prevent 403 Forbidden errors. |
| `Authorization` | String | Manual token injection if required by the upstream API.              |
| `Referer`       | String | Can be used to bypass basic hotlink protections.                     |

```yaml
headers:
  User-Agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) Tuliprox/3.0"
  X-Custom-Auth: "my-secret-token"
```

---

### 2.2 Input Options (`options`)

Controls the behavior during download and asynchronous metadata resolution (see the *Metadata Update* chapter) for this
specific provider.

| Parameter                                  | Type     | Default | Technical Impact & Background                                                                                                                                                                                                          |
|:-------------------------------------------|:---------|:--------|:---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `skip_live` / `skip_vod` / `skip_series`   | Bool     | `false` | Immediately ignores entire categories during Xtream or Stalker ingestion. Saves massive amounts of RAM and runtime if you only want specific clusters from a provider.                                                                 |
| `xtream_live_stream_without_extension`     | Bool     | `false` | Strips `.ts` from generated stream URLs.                                                                                                                                                                                               |
| `xtream_live_stream_use_prefix`            | Bool     | `true`  | Injects the `/live/` prefix into URLs.                                                                                                                                                                                                 |
| `disable_hls_streaming`                    | Bool     | `false` | Rewrites live `.m3u8` requests to `.ts` and bypasses Tuliprox HLS handling.                                                                                                                                                            |
| `resolve_tmdb`                             | Bool     | `false` | Enables TMDB queries for this specific input based on parsed titles to fill missing posters and release years.                                                                                                                         |
| `probe_stream`                             | Bool     | `false` | Uses FFprobe to read A/V details (HDR, 4K). Respects `max_connections`.                                                                                                                                                                |
| `resolve_background`                       | Bool     | `true`  | Metadata scans run asynchronously in the background so the general playlist update (which blocks clients) finishes instantly.                                                                                                          |
| `resolve_series` / `resolve_vod`           | Bool     | `false` | Fetches missing details like Plot or Cast via the Provider's API (`get_vod_info` / `get_series_info`).                                                                                                                                 |
| `probe_series` / `probe_vod`               | Bool     | `false` | Allows explicit FFprobe analysis of movies or entire TV show seasons.                                                                                                                                                                  |
| `probe_live`                               | Bool     | `false` | Allows FFprobe to periodically tap into Live-TV streams in the background.                                                                                                                                                             |
| `probe_live_interval_hours`                | Int      | `120`   | Interval after which a Live stream is re-analyzed (Important as backup streams often change resolutions).                                                                                                                              |
| `resolve_delay` / `probe_delay`            | Int      | `2`     | **Ban Protection:** Hard wait time (in seconds) between API or Probe requests to the *same* provider! Prevents API spamming.                                                                                                           |
| `resolve_filter`                           | String   | -       | Filter expression to selectively resolve only entries matching the condition. Uses the same Filter syntax.                                                                                                                             |
| `probe_filter`                             | String   | -       | Filter expression to selectively probe only entries matching the condition. Uses the same Filter syntax.                                                                                                                               |
| `stalker_pre_resolve_playback`             | Bool     | `false` | Stalker-only: resolves `create_link` during playlist processing and persists the returned playback URL when the portal already grants one. Useful when you want the playlist/export step to materialize playable stream URLs up front. |
| `stalker_runtime_resolve_playback`         | Bool     | `false` | Stalker-only: lets the reverse-proxy retry `create_link` during playback when a persisted Stalker URL has gone stale or is rejected by the portal. This is the recovery path for temp links and expired session-bound stream URLs.     |

> **Note:** For `resolve_vod` and `resolve_series`, data is cached per input and only new or changed entries are
> updated.

#### Minimal Xtream MPEG-TS Example

```yaml
inputs:
  - name: ts-capable-provider
    type: xtream
    url: http://provider.example
    username: user
    password: pass
    options:
      disable_hls_streaming: true
```

#### Stalker playback notes

* Stalker playlist preview in the Web UI now works through the same protected playlist endpoints used for other input types.
* `stalker_pre_resolve_playback` and `stalker_runtime_resolve_playback` are complementary:
  * `stalker_pre_resolve_playback: true` tries to turn portal `cmd` values into concrete playback URLs during refresh.
  * `stalker_runtime_resolve_playback: true` retries `create_link` later if the stored playback URL is stale, temp-link based, or rejected after processing.
* Temp-link variants (`nginx_secure_link`, `flussonic_tmp_link`, `wowza_tmp_link`) are persisted as explicit playback modes
  and reused during runtime refresh, instead of being flattened into a generic direct-URL path.
* When pre-resolve does not materialize a URL, Tuliprox keeps the Stalker item metadata and playback descriptor but does
  not leak the raw `cmd` into the exported playlist URL field.
* If pre-resolve is disabled or the portal refuses to resolve a specific item during refresh, the item can still remain playable
  later through runtime resolution, assuming the reverse-proxy path is used and runtime resolve is enabled.
* Runtime refresh reuses a cached Stalker client per input configuration and treats the session TTL as a soft re-handshake boundary.
  If refresh still cannot resolve a playable URL, Tuliprox invalidates the stale persisted URL instead of continuing to serve it indefinitely.
* Stalker EPG import now also consumes the portal bulk-EPG endpoint during processing when Stalker playback pre-resolve is enabled.
* The bulk-EPG path is streamed and batch-persisted to reduce peak memory pressure on large portals, but portal-specific
  tuning for pathological datasets is still a separate follow-up topic.
* Supported Stalker playback transports are currently `http` and `https` only. `rtmp://` / `rtsp://` commands are rejected
  explicitly because Tuliprox's reverse-proxy path does not relay those schemes.
* Fresh temp-link resolution is implemented. The still-open edge case is whether a specific portal also requires extra forwarded
  cookies or headers on the final media request after temp-link resolution.

---

### 2.3 EPG Assignment & Smart Match (`epg`)

Tuliprox can load external EPG sources and map them intelligently using advanced fuzzy matching to streams that are
missing a valid EPG ID.

The `epg.sources` list supports the following source types:

* **XMLTV** sources, which provide complete XMLTV channel and programme data.
* **ICS calendar** sources, which import iCalendar events and convert them into a virtual XMLTV-compatible EPG channel.

Tuliprox aggregates all configured EPG sources and assigns EPG data based on priority and matching rules.

#### Example Configuration

```yaml
epg:
  sources:
    - url: "auto" # Automatically generated provider XMLTV URL
      priority: -2 # High priority
      logo_override: true # Replaces provider logos with EPG icons

    - url: "http://localhost:3001/xmltv.php?epg_id=1"
      priority: -1

    - url: "http://localhost:3001/xmltv.php?epg_id=2"
      priority: 3

    - type: ics
      url: "https://files-f1.motorsportcalendars.com/f1-calendar_p1_p2_p3_qualifying_sprint_gp.ics"
      channel_id: "f1.calendar"
      channel_title: "Formula 1"
      priority: -10
      ics:
        timezone: "Europe/Budapest"
        event:
          title: "{summary}"
          description: "{description}"
          include_location: true
          include_categories: true
        dummy:
          enabled: true
          title: "No Formula 1 session"
          description: "There is currently no scheduled Formula 1 session."
          days_past: 1
          days_future: 30
          block_hours: 4
          min_gap_minutes: 1

  smart_match:
    enabled: true
    fuzzy_matching: true
    match_threshold: 80
    best_match_threshold: 99
    name_prefix: { suffix: "." }
    name_prefix_separator: [":", "|", "-"]
    strip: ["3840p", "uhd", "fhd", "hd", "sd", "4k", "plus", "raw"]
    normalize_regex: '[^a-zA-Z0-9\-]'
```

#### EPG Source Parameters (`sources`)

| Parameter           | Type   | Required | Default | Technical Impact & Background                                                                                                                                                                                          |
| :------------------ | :----- | :------: | :------ | :--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **`type`**          | Enum   |    No    | `xmltv` | Defines the source format. Supported values are `xmltv` and `ics`. Existing configurations without `type` are treated as `xmltv`.                                                                                      |
| **`url`**           | String |   Yes    |         | The EPG source URL or local path. For XMLTV sources, use **`auto`** with Xtream inputs to automatically generate the native XMLTV URL using your credentials. For ICS sources, this points to an `.ics` calendar file. |
| **`priority`**      | Int    |    No    | `0`     | Determines the lookup order. **Lower numbers have higher priority.** For example, `-2` is processed before `0`. Use negative numbers for primary sources.                                                              |
| **`logo_override`** | Bool   |    No    | `false` | If set to `true`, channel logos from the provider are replaced by icons found in the EPG source. This mainly applies to XMLTV sources.                                                                                 |
| **`channel_id`**    | String | ICS only |         | Required for `type: ics`. Defines the generated XMLTV channel ID for the imported calendar. Playlist entries can reference this value through their EPG ID or receive it through Smart Match.                          |
| **`channel_title`** | String |    No    |         | Optional display name for the generated ICS EPG channel. If omitted, `channel_id` is used as fallback. This value is also used as a Smart Match candidate.                                                             |
| **`match_names`**   | List   |    No    | `[]`    | Optional additional names for matching the generated ICS channel to playlist entries. Useful when the playlist channel name differs from the calendar title, for example `F1`, `Formula One`, or `Formel 1`.           |
| **`ics`**           | Object | ICS only |         | Additional configuration for `type: ics`. See [ICS Calendar Source Parameters](#ics-calendar-source-parameters-ics).                                                                                                   |

#### XMLTV Sources

XMLTV is the default source type. Existing configurations remain valid and do not need to be changed.

```yaml
epg:
  sources:
    - url: "auto"
      priority: -2
      logo_override: true

    - url: "https://example.org/xmltv.xml"
      priority: 0
```

The following configuration is equivalent:

```yaml
epg:
  sources:
    - type: xmltv
      url: "https://example.org/xmltv.xml"
      priority: 0
```

For Xtream inputs, `url: "auto"` automatically generates the provider's native XMLTV endpoint from the configured
input URL, username, and password.

The ICS-only fields `channel_id`, `channel_title`, `match_names`, and `ics` are rejected on `type: xmltv` sources. This
keeps accidental calendar settings from changing XMLTV download or runtime behavior.

#### ICS Calendar Sources

ICS sources import iCalendar files and convert their `VEVENT` entries into XMLTV-compatible programme entries.
Recurring events using `RRULE`, `RDATE`, or `EXDATE` are detected but not expanded yet; Tuliprox imports the base
`DTSTART`/`DTEND` occurrence and logs one aggregated warning per parsed source when such entries are present.

An ICS source creates exactly one virtual EPG channel. It does **not** create a playlist channel or stream. The generated
`channel_id` must therefore be assigned to an existing playlist entry by one of the following mechanisms:

* the playlist entry already uses the same EPG ID,
* a mapper rule assigns the EPG ID,
* Smart Match matches the playlist channel name against `channel_id`, `channel_title`, or `match_names`.

The generated programmes are written into the regular XMLTV output. M3U and Xtream use the same internal
`epg_channel_id` assignment, so the same `channel_id` works for both input types. If an Xtream provider supplies a valid
but unwanted EPG ID, use a mapping rule to overwrite the stream's EPG ID with the ICS `channel_id`.

Example using the public Formula 1 calendar:

```yaml
epg:
  sources:
    - type: ics
      url: "https://files-f1.motorsportcalendars.com/f1-calendar_p1_p2_p3_qualifying_sprint_gp.ics"
      channel_id: "f1.calendar"
      channel_title: "Formula 1"
      priority: -10
      match_names:
        - "F1"
        - "Formula One"
        - "Formel 1"
      ics:
        timezone: "Europe/Budapest"
        event:
          title: "{summary}"
          description: "{description}"
          include_location: true
          include_categories: true
        dummy:
          enabled: true
          title: "No Formula 1 session"
          description: "There is currently no scheduled Formula 1 session."
          days_past: 1
          days_future: 30
          block_hours: 4
          min_gap_minutes: 1
```

A playlist channel can then be linked explicitly by using the generated EPG channel ID:

```m3u
#EXTINF:-1 tvg-id="f1.calendar" tvg-name="Formula 1",Formula 1
http://example.org/live/f1/index.m3u8
```

Alternatively, Smart Match can match playlist names such as `Formula 1`, `F1`, or `Formel 1` when the corresponding
values are configured through `channel_title` or `match_names`.

#### ICS Calendar Source Parameters (`ics`)

| Parameter                    | Type   | Required | Default    | Technical Impact & Background                                                                                                                                    |
| :--------------------------- | :----- | :------: | :--------- | :--------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **`timezone`**               | String |    No    | `UTC`      | Fallback timezone for floating ICS timestamps and the local dummy block calculation. Use an IANA zone such as `Europe/Budapest`, `UTC`, or `America/New_York`.   |
| **`event`**                  | Object |    No    |            | Controls how calendar events are mapped to programme title and description. See [ICS Event Mapping](#ics-event-mapping-event).                                   |
| **`dummy`**                  | Object |    No    |            | Controls optional gap filling with generated dummy programme entries. See [ICS Dummy Gap Filling](#ics-dummy-gap-filling-dummy).                                 |
| **`include_cancelled`**      | Bool   |    No    | `false`    | If set to `true`, calendar events with `STATUS:CANCELLED` are imported. By default, cancelled events are skipped.                                                |
| **`max_events`**             | Int    |    No    | `50000`    | Safety budget for encountered `VEVENT` blocks, including invalid or skipped events. Parsing stops when the budget is exhausted. Hard cap: `200000`.              |
| **`max_download_bytes`**     | Int    |    No    | `10485760` | Maximum downloaded ICS size in bytes. The default is 10 MiB. Hard cap: `52428800` (50 MiB).                                                                      |
| **`max_decompressed_bytes`** | Int    |    No    | `20971520` | Maximum decompressed ICS size in bytes. The default is 20 MiB. Hard cap: `104857600` (100 MiB).                                                                  |

`timezone` must be a valid IANA timezone known to Tuliprox, for example `UTC`, `Europe/Budapest`, or
`America/New_York`.

#### ICS Event Mapping (`event`)

The `event` block controls how ICS `VEVENT` properties are converted into EPG programme fields.

```yaml
ics:
  event:
    title: "{summary}"
    description: "{description}"
    include_location: true
    include_categories: true
```

| Parameter                | Type   | Required | Default         | Technical Impact & Background                                                           |
| :----------------------- | :----- | :------: | :-------------- | :-------------------------------------------------------------------------------------- |
| **`title`**              | String |    No    | `{summary}`     | Template used for the generated programme title.                                        |
| **`description`**        | String |    No    | `{description}` | Template used for the generated programme description.                                  |
| **`include_location`**   | Bool   |    No    | `false`         | Appends the ICS `LOCATION` value to the generated programme description when available. |
| **`include_categories`** | Bool   |    No    | `false`         | Appends ICS `CATEGORIES` to the generated programme description when available.         |

Supported template variables:

| Variable            | Description                                       |
| :------------------ | :------------------------------------------------ |
| **`{summary}`**     | The ICS `SUMMARY` value. Usually the event title. |
| **`{description}`** | The ICS `DESCRIPTION` value.                      |
| **`{location}`**    | The ICS `LOCATION` value.                         |
| **`{categories}`**  | The ICS `CATEGORIES` value.                       |
| **`{uid}`**         | The ICS `UID` value.                              |
| **`{start}`**       | The localized event start time.                   |
| **`{end}`**         | The localized event end time.                     |

Example:

```yaml
ics:
  event:
    title: "Formula 1: {summary}"
    description: "{description}"
    include_location: true
    include_categories: true
```

#### ICS Time Handling

Tuliprox converts all imported calendar events into the internal EPG time format.

Supported ICS time forms:

| ICS Time Form                                  | Behavior                                                                 |
| :--------------------------------------------- | :----------------------------------------------------------------------- |
| `DTSTART:20260306T123000Z`                     | Treated as UTC.                                                          |
| `DTSTART;TZID=Europe/Berlin:20260306T1230`     | Interpreted in the specified `TZID` timezone and converted internally.   |
| `DTSTART:20260306T123000`                      | Treated as a floating timestamp and interpreted using `ics.timezone`.    |
| `DTSTART;VALUE=DATE:20260306`                  | All-day events. These are ignored.                                       |

For regular programme entries, an ICS event must provide a valid start and end time. `DTEND` is preferred. If `DTEND`
is missing, Tuliprox may use `DURATION` when present. Events without a usable end time are skipped. For template
variables, `{start}` and `{end}` are formatted consistently; when `DURATION` supplies the end time, `{end}` uses the
same display timezone as `DTSTART`.

#### ICS Dummy Gap Filling (`dummy`)

ICS calendars often only contain real events, for example race sessions, meetings, or special broadcasts. This may leave
large gaps in the EPG. The optional dummy gap filler can create placeholder programmes for these gaps.

Dummy entries are generated in local day blocks. By default, the block size is 4 hours:

```text
00:00 - 04:00
04:00 - 08:00
08:00 - 12:00
12:00 - 16:00
16:00 - 20:00
20:00 - 24:00
```

Example:

```yaml
ics:
  timezone: "UTC"
  dummy:
    enabled: true
    title: "No Formula 1 session"
    description: "There is currently no scheduled Formula 1 session."
    days_past: 1
    days_future: 30
    block_hours: 4
    min_gap_minutes: 1
```

| Parameter             | Type   | Required | Default              | Technical Impact & Background                                                                                                                                      |
| :-------------------- | :----- | :------: | :------------------- | :----------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **`enabled`**         | Bool   |    No    | `false`              | Enables generation of dummy programme entries for gaps without real events.                                                                                        |
| **`title`**           | String |    No    | `No programme entry` | Programme title for generated dummy entries.                                                                                                                       |
| **`description`**     | String |    No    | empty                | Programme description for generated dummy entries.                                                                                                                 |
| **`days_past`**       | Int    |    No    | `1`                  | Number of days before the current day for which dummy entries are generated.                                                                                       |
| **`days_future`**     | Int    |    No    | `14`                 | Number of days after the current day for which dummy entries are generated.                                                                                        |
| **`block_hours`**     | Int    |    No    | `4`                  | Size of the local dummy blocks in hours. The value must divide 24 evenly. For example: `1`, `2`, `3`, `4`, `6`, `8`, or `12`.                                      |
| **`min_gap_minutes`** | Int    |    No    | `1`                  | Minimum gap length required to generate a dummy entry. Smaller gaps are ignored to avoid tiny placeholder programmes caused by second-level timestamp differences. |

Dummy entries never replace real programme entries. They only fill uncovered time ranges.

`days_past` is capped at `30`; `days_future` is capped at `366`. Dummy titles use the same length cap as event summaries
and dummy descriptions use the same length cap as event descriptions.

Example with one real event from `03:30` to `05:00`:

```text
00:00 - 03:30  No Formula 1 session
03:30 - 05:00  Real calendar event
05:00 - 08:00  No Formula 1 session
08:00 - 12:00  No Formula 1 session
...
```

Example with an event aligned to a block boundary:

```text
00:00 - 04:00  No Formula 1 session
04:00 - 06:00  Real calendar event
06:00 - 08:00  No Formula 1 session
08:00 - 12:00  No Formula 1 session
...
```

If two events are adjacent, no dummy programme is inserted between them:

```text
10:00 - 11:00  Real calendar event
11:00 - 12:00  Real calendar event
12:00 - 16:00  No Formula 1 session
```

Dummy block boundaries are calculated in `ics.timezone`. Around daylight saving time changes, the local block labels
remain stable, for example `00:00 - 04:00`, `04:00 - 08:00`, and so on, while the corresponding UTC duration may differ.
If a local boundary does not exist during a spring-forward transition, it maps to the first real instant after the gap.
If a boundary is ambiguous during a fall-back transition, the earlier occurrence is used. Any resulting zero-length or
negative UTC interval is omitted, so generated dummy programmes remain ordered, non-overlapping, and continuous over
the real local day.

#### ICS Download and Security Notes

ICS sources use the same download infrastructure as playlist and XMLTV EPG downloads. This means that input-level
headers, disabled headers, default user-agent handling, cache handling, retry behavior, provider failover, and sanitized
error logging are applied consistently.

Supported URL forms for ICS sources:

| URL Form         | Description                                                               |
| :--------------- | :------------------------------------------------------------------------ |
| `https://...`    | Recommended for remote calendar files.                                    |
| `http://...`     | Supported when explicitly needed.                                         |
| `webcal://...`   | Normalized internally to `https://...`.                                   |
| `provider://...` | Uses a configured Tuliprox provider failover definition.                  |
| `file://...`     | Reads a local calendar file from the host filesystem.                     |
| local path       | Reads a local relative or absolute file path, subject to path validation. |

Unsupported or unsafe schemes such as `data:`, `ftp:`, `gopher:`, `javascript:`, `mailto:`, or `ssh:` are rejected.

Tuliprox does not load external resources referenced inside the ICS file. Fields such as `ATTACH`, `URL`, `ORGANIZER`,
`ATTENDEE`, or `IMAGE` are treated as metadata only and must not trigger additional network requests.

To protect the server, ICS imports are bounded by safety limits such as maximum download size and maximum event count.
Sensitive request data, such as credentials and authorization headers, is sanitized in logs and error messages.

Additional parser limits are enforced regardless of the configured values:

| Limit | Hard Cap |
| :---- | :------- |
| Physical or unfolded ICS line length | 128 KiB |
| Properties per `VEVENT` | 256 |
| Imported `SUMMARY` text | 4 KiB, truncated at a UTF-8 boundary |
| Imported `DESCRIPTION` text | 64 KiB, truncated at a UTF-8 boundary |

If the file itself is unreadable, too large, not valid UTF-8, violates the line-length limit, or does not contain one
complete, correctly nested `VCALENDAR` envelope, the source fails. A correctly wrapped calendar without events is
valid. If a single `VEVENT` inside that envelope is malformed, has too many properties, has an unknown event `TZID`, or
lacks a usable
`DTSTART`/`DTEND`/`DURATION`, that event is skipped and the remaining events continue to be imported. The Web UI EPG
preview uses the same merged output behavior as the client XMLTV output, including ICS dummy gap filling. If a cached
`.ics` file cannot be parsed in preview, Tuliprox ignores the cached file and downloads the source again through the
normal EPG download path.

The graphical source editor currently preserves existing ICS fields when editing an input, but full creation and editing
of every ICS-specific field is YAML-first. Configure new ICS sources in `source.yml`.

#### Smart Match Parameters (`smart_match`)

The fuzzy matching logic attempts to "guess" the EPG ID by generating search keys based on the channel name.

For XMLTV sources, Tuliprox uses the channel IDs and display names from the XMLTV file.

For ICS sources, Tuliprox uses the generated virtual channel metadata:

* `channel_id`
* `channel_title`
* `match_names`

| Parameter               | Type   | Default            | Technical Impact                                                                                                     |
| :---------------------- | :----- | :----------------- | :------------------------------------------------------------------------------------------------------------------- |
| `enabled`               | Bool   | `false`            | Activates the Smart Match engine for streams without a fixed `tvg-id`.                                               |
| `fuzzy_matching`        | Bool   | `false`            | Fallback to phonetic and Jaro-Winkler similarity matching if exact ID match fails.                                   |
| `match_threshold`       | Int    | `80`               | Minimum similarity score (10-100) required to accept a fuzzy match.                                                  |
| `best_match_threshold`  | Int    | `95`               | Minimum score for the strict fallback used when phonetic keys differ.                                                |
| `name_prefix`           | Object | `ignore`           | Options: `ignore`, `suffix`, `prefix`. For `suffix`/`prefix`, a concat string (e.g., `{ suffix: "." }`) is required. |
| `name_prefix_separator` | List   | `[':', '\|', '-']` | Characters used by providers to delimit country codes (e.g., `US:`, `FR\|`).                                         |
| `strip`                 | List   | *(quality tags)*   | Resolution, codec and frame-rate markers stripped as complete terms before matching.                                 |
| `normalize_regex`       | String | `[^a-zA-Z0-9._\-]` | Default pattern preserving the separators commonly found in XMLTV channel IDs.                                       |

When upgrading, an explicitly configured legacy pattern such as `[^a-zA-Z0-9\-]` remains unchanged and continues to
remove periods and underscores. Remove that override or set `[^a-zA-Z0-9._\-]` to adopt the new default behavior.

#### How Smart-Matching works

If a stream is missing the `tvg-id`, Tuliprox performs the following steps:

1. **Normalization:** The channel name (e.g., `US: HBO HD 4K`) is processed.
2. **Prefix Extraction:** Using `name_prefix_separator`, Tuliprox identifies `:` and splits the name. It recognizes `US`
   as the country prefix.
3. **Cleaning:** It strips terms defined in `strip` ("4K", "HD") and applies the `normalize_regex`. The core name
   becomes `hbo`.
4. **Reconstruction:** Using `name_prefix.suffix` (`.`), the country code is appended to the name. The target search key
   becomes `hbo.us`.
5. **Exact Matching:** Normalized XMLTV IDs and display names are checked first. Source priority resolves duplicate
   exact candidates. Quality variants sharing one normalized name can reuse a populated direct ID.
6. **Fuzzy Matching:** When enabled, the engine scores every candidate in the matching **Double Metaphone** bucket and
   keeps the globally best result rather than the first acceptable result. If the phonetic lookup yields nothing, a
   same-initial fallback is allowed only at `best_match_threshold`.
7. **Safety Checks:** Numeric signatures must agree (`TF1` cannot match `TF1+1`), tied candidates are rejected, and
   low-confidence candidates must beat the runner-up by a minimum margin. Explicit XMLTV ID country suffixes prevent
   cross-country matches, and decorative playlist separators are ignored.
8. **Programme Validation:** A channel declaration without programmes is not treated as a successful guide match.
   Tuliprox keeps ranked alternatives while parsing and selects the best candidate that actually contributes programme
   data. Existing IDs with no programmes can therefore be replaced by a populated, semantically compatible candidate.
   ICS channels with an enabled dummy policy also count as populated.

At `debug` level, each processed input emits one `Smart EPG summary` with the number of live channels whose original ID
was valid, whose ID was assigned by Smart Match, or which remained unresolved.

For an ICS source, the generated EPG channel participates in the same matching process. For example, this configuration:

```yaml
epg:
  sources:
    - type: ics
      url: "https://files-f1.motorsportcalendars.com/f1-calendar_p1_p2_p3_qualifying_sprint_gp.ics"
      channel_id: "f1.calendar"
      channel_title: "Formula 1"
      match_names:
        - "F1"
        - "Formel 1"
```

can match playlist channel names such as:

```text
Formula 1
F1
Formel 1
```

> **Note:** Lower `match_threshold` values increase the chance of EPG assignment but may lead to incorrect matches for
> channels with very similar names.

---

### 2.4 Provider Aliases (`aliases` & `batch://`)

Tuliprox allows you to pool multiple subscriptions from the same provider into a single logical source.
By merging these "aliases," Tuliprox tracks connection availability across all accounts, ensuring that if one
subscription is at its limit,
the next available connection from the pool is used.

#### Defining Aliases in YAML

Aliases are ideal for a small number of fixed accounts. Note that in YAML, `max_connections: 0` signifies "unlimited,"
which is the default setting.

```yaml
inputs:
  - type: xtream
    name: my_provider # Mandatory: Used for stable UUID generation
    url: 'http://provider.net'
    username: sub_1
    password: pw1
    max_connections: 1
    aliases:
      - name: my_provider_2
        url: 'http://provider.net'
        username: sub_2
        password: pw2
        priority: 1
        max_connections: 2
        exp_date: "2028-11-30 12:00:00"
        enabled: true
```

**Result:** Tuliprox treats this as a single provider source with a total pool of 1 + 2 = **3** concurrent connections.

#### YAML Alias Fields

Alias entries use the same effective connection attributes as a normal input account. The input-level `headers`,
`epg`, `options`, `persist`, `method`, `panel_api`, `cache_duration`, and provider failover settings remain inherited
from the parent input; the alias fields below override the concrete account/connection identity.

| Parameter             | Type   | Required | Default | Technical Impact & Details                                                                                                                                                                                                                         |
|:----------------------|:-------|:--------:|:--------|:---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `id`                  | Int    |    No    |         | Internal/generated alias ID. Normally omit this; Tuliprox assigns IDs from the input/alias order during config preparation.                                                                                                                        |
| `name`                | String |   Yes    |         | Unique alias name. Used for stable playlist UUID generation and consistent channel numbering across updates.                                                                                                                                       |
| `url`                 | String |   Yes    |         | Provider base URL, playlist URL, or `provider://<name>` reference. For M3U aliases, credentials can also be extracted from the URL query parameters.                                                                                               |
| `username`            | String |  Xtream  |         | Account username. Mandatory for regular Xtream YAML aliases. Optional for M3U aliases when credentials are embedded in the playlist URL.                                                                                                           |
| `password`            | String |  Xtream  |         | Account password. Mandatory for regular Xtream YAML aliases. Optional for M3U aliases when credentials are embedded in the playlist URL.                                                                                                           |
| `priority`            | Int    |    No    | `0`     | Connection selection priority for this alias. Lower numbers have higher priority; negative numbers are allowed.                                                                                                                                    |
| `max_connections`     | Int    |    No    | `0`     | Allowed concurrent streams for this alias. In YAML, `0` means unlimited/no explicit limit.                                                                                                                                                         |
| `exp_date`            | Mixed  |    No    |         | Account expiration. Supports `"YYYY-MM-DD HH:MM:SS"` interpreted as UTC or Unix timestamps in seconds. Xtream dates participate in the automatic refresh described below.                                                                          |
| `enabled`             | Bool   |    No    | `true`  | If `false`, this alias is ignored when Tuliprox builds the usable connection pool.                                                                                                                                                                 |

#### Automatic Xtream Expiration Refresh

In server mode, Tuliprox runs an autonomous task that refreshes expiration dates for enabled Xtream inputs and aliases.
It calls the account's standard `player_api.php` endpoint with the configured URL, username, and password. This does not
require a reseller Panel API key and continues to work when `panel_api` provisioning is absent or disabled.

To limit provider traffic and avoid bans, the task uses these fixed rules:

* Accounts without an expiration date, or whose expiration is at most three days away, are eligible for refresh.
* An eligible account is queried at most once every 24 hours.
* Requests to accounts belonging to the same configured panel, including aliases, are spaced at least five minutes
  apart.
* Transport errors, HTTP 403/429 responses, and server errors put the complete panel into a six-hour cooldown.

Successful non-expired updates are collected for up to 15 minutes and then persisted as a batch. An expiration date at
or before the current time is persisted immediately and the account is set to `enabled: false`. Disabled accounts are
not queried and are not re-enabled automatically.

Tuliprox updates the main source YAML and local `xtream_batch` alias CSV files in place. Before changing either file it
creates a timestamped copy in `backup_dir`; CSV comments, column order, unknown columns, and environment placeholders
are preserved. The in-memory source configuration is refreshed once per persisted batch, without triggering a second
reload for Tuliprox's own file write.

Throttle and pending-batch state is stored in `{storage_dir}/xtream_expiry_state.json`, so restarts do not reset request
limits or discard already fetched expiration dates. The intervals above are currently fixed and have no configuration
keys.

---

#### Batch CSV Offloading (`batch://`)

For managing dozens or hundreds of accounts, Tuliprox supports offloading alias definitions to local CSV files using the
`batch://` scheme.

| Scheme                   | Description                    |
|:-------------------------|:-------------------------------|
| `batch://./file.csv`     | Relative path to the CSV file. |
| `batch:///path/file.csv` | Absolute path to the CSV file. |

> **Note:** Batch inputs only support local filesystem paths. Schemes like `http(s)://`, `file://`, or `provider://`
> are rejected for batch URL definitions. If an input `url` starts with `batch://`,
> Tuliprox automatically sets the corresponding batch type for `xtream`, `m3u`, or `stalker` inputs.

#### Batch CSV Formats

Batch files use a semicolon (`;`) as a separator. Unlike standard YAML config, the default for `max_connections` in CSV
files is **1**.

##### `XtreamBatch`

Used for Xtream Codes API accounts.

```yaml
inputs:
  - type: xtream_batch
    name: my_provider
    url: 'batch://./xtream_aliases.csv'
```

**CSV Structure:**

```csv
#name;username;password;url;max_connections;priority;exp_date;enabled
my_provider_1;user1;password1;http://p1.com:80;1;0;2028-11-23 12:34:23;true
my_provider_2;user2;password2;http://p2.com:8080;1;1;1732365263;true
```

##### `M3uBatch`

Used for plain M3U playlist URLs.

```yaml
inputs:
  - type: m3u_batch
    name: m3u_pool
    url: 'batch:///etc/tuliprox/m3u_aliases.csv'
```

**CSV Structure:**

```csv
#url;max_connections;priority;enabled
http://p1.com/get.php?username=u1&password=p1;1;0;true
http://p2.com/get.php?username=u2&password=p2;1;5;true
```

##### `StalkerBatch`

Used for Stalker/Ministra portals. The complete portal path is retained, so URLs such as `/c/` must be included in
the CSV. The following credentials-based example uses [`config/stalker_aliases.csv`](../../../config/stalker_aliases.csv):

```yaml
inputs:
  - type: stalker_batch
    name: stalker_pool
    url: 'batch://./config/stalker_aliases.csv'
    stalker:
      catalog_max_pages: 1000
```

**CSV Structure:**

```csv
#name;url;username;password;mac_address;auth_mode;mag_preset;endpoint_preference;max_connections;priority;exp_date;enabled
portal_primary;http://portal.example/c/;account1;secret1;00:1A:79:12:34:56;mac_plus_credentials;mag254_strict;portal;1;0;;true
portal_backup;https://backup.example/stalker_portal/c/;account2;secret2;;credentials_only;generic_safe;auto;1;10;;true
```

Columns are selected by the header and may appear in any order. `mac_address`, `auth_mode`, `mag_preset`, and
`endpoint_preference` are optional per-alias Stalker fields. An alias with no Stalker-specific values inherits the
complete parent configuration; otherwise empty enum fields use their normal defaults. Other device fields, size caps,
and the catalog page limit are configured on the parent input in the Web UI or YAML and inherited by every CSV alias.

#### Field Specifications

| Parameter                 | Technical Impact & Details                                                                                                                                                                                                                          |
|:--------------------------|:----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **`url`**                 | Provider base URL or full M3U playlist URL. Required in CSV rows.                                                                                                                                                                                   |
| **`name`**                | **Crucial:** The first alias is automatically renamed with the `name` from the input definition (e.g., `my_provider_1` gets `my_provider`). This is necessary for stable playlist UUID generation and consistent channel numbering across updates.  |
| **`username`**            | Xtream or Stalker account username. For M3U CSV rows, Tuliprox can also extract credentials from the URL query parameters.                                                                                                                          |
| **`password`**            | Xtream or Stalker account password. For M3U CSV rows, Tuliprox can also extract credentials from the URL query parameters.                                                                                                                          |
| **`mac_address`**         | Stalker alias MAC address. Required by `mac_only` and `mac_plus_credentials`; optional for credentials-only authentication.                                                                                                                         |
| **`auth_mode`**           | Stalker alias authentication mode: `auto`, `mac_only`, `credentials_only`, or `mac_plus_credentials`.                                                                                                                                               |
| **`mag_preset`**          | Stalker alias MAG profile: `generic_safe`, `mag250_legacy`, `mag254_strict`, or `ministra_modern`.                                                                                                                                                  |
| **`endpoint_preference`** | Stalker portal endpoint selection: `auto`, `server_load`, or `portal`.                                                                                                                                                                              |
| **`max_connections`**     | Defines allowed concurrent streams. Default in CSV is **1**.                                                                                                                                                                                        |
| **`priority`**            | Lower numbers = higher priority. `0` is higher than `1`. Negative numbers (e.g., `-1`) are allowed for top-tier priority. Items with the lowest values are processed first.                                                                         |
| **`exp_date`**            | Account expiration. Supports `"YYYY-MM-DD HH:MM:SS"` interpreted as UTC or Unix timestamps in seconds. Used for auto-cleanup or Panel API sync.                                                                                                     |
| **`enabled`**             | Enables/disables a CSV alias row. Empty values, `1`, `t`, and `true` are treated as enabled; `0`, `f`, or `false` disable the alias.                                                                                                                |

---

### 2.5 Staged Sources (`staged`)

The **staged input** is a first-class input type for pre-formatted playlists. Tuliprox reads the selected playlist
clusters from the staged source, then stores the merged result in the linked provider input. Stream delivery and API
requests continue to use that provider input.

This is useful when an external playlist editor already has the desired channel order, groups, and original stream IDs.
For example, an IPTV editor can provide the Live playlist layout while the actual streams are still opened against the
Xtream or M3U provider.

**Data flow:**

* `staged input -> provider input`: the staged input is an overlay for that provider. Clusters listed in
  `staged.clusters` are loaded from the staged input; the remaining clusters are loaded from the provider input itself.
  The merged playlist is persisted under the provider input, and streaming/API requests still target the provider input.
* `staged input -> target` is not supported. Use a normal `m3u` or `xtream` input if the source should be connected
  directly to a target.

#### Configuration Example (Provider With Staged Live Overlay)

In this setup, Live-TV comes from the external staged playlist, while VOD and Series come from the original Xtream
provider.

```yaml
inputs:
  - name: provider_main
    type: xtream
    url: http://provider-a.example:8080
    username: main_user
    password: main_pass
  - name: provider_main_editor_live
    type: staged
    url: http://editor.example/provider-main-live.m3u
    staged:
      for_input: provider_main
      clusters: [live]
```

#### Parameters

| Parameter               | Type   | Required | Default | Technical Impact & Background                                                                         |
|:------------------------|:-------|:--------:|:--------|:------------------------------------------------------------------------------------------------------|
| `type`                  | Enum   |   Yes    |         | Must be `staged`.                                                                                     |
| `staged_type`           | Enum   |    No    | `m3u`   | Format of the staged source. Allowed: `m3u`, `xtream`.                                                |
| `url`                   | String |   Yes    |         | Download URL (HTTP/HTTPS) or local file path. For `staged_type: xtream`, use the base hostname:port.  |
| `username` / `password` | String |   Yes    |         | Mandatory only if `staged_type: xtream`. Not inherited from the provider input.                       |
| `method`                | Enum   |    No    | `GET`   | HTTP request method (`GET` or `POST`). Not inherited from the provider input.                         |
| `headers`               | Dict   |    No    |         | Custom HTTP headers for the staged download. Not inherited from the provider input.                   |
| `staged.for_input`      | String |   Yes    |         | Provider input name. Must reference a non-staged `m3u` or `xtream` input.                             |
| `staged.clusters`       | List   |    No    | all     | Clusters loaded from the staged input: `live`, `vod`, `series`.                                       |

#### Staged Cluster Behavior & Validation

`staged.clusters` is the group of clusters loaded from the staged input.

* The referenced provider supplies all clusters not listed in `staged.clusters`.
* `staged.for_input` must reference an existing non-staged `m3u` or `xtream` input.
* Each provider input can have at most one staged overlay.
* `staged.clusters` must not be empty.
* Source definitions must reference the provider input, not the staged input.
* Staged inputs do not use `priority`, `max_connections`, or `cache_duration`. The linked provider input controls
  stream limits and refresh cadence. If the provider input is still cached, Tuliprox does not query the staged input.

#### File Persistence (`persist`)

When using `persist` to save the staged data, follow these filename conventions:

* **For `m3u`:** Use a full filename template like `./staged_playlist_{}.m3u`.
* **For `xtream`:** Use a prefix template like `./staged_playlist_`.

---

### 2.6 Provider Panel API (`panel_api`)

Tuliprox can optionally interface with a provider's reseller panel API to automate account lifecycle management.
This allows the system to fetch credit balances, sync expiration dates, and automatically provision or renew alias
accounts based on demand.

> **Important:** Panel API accounts are managed as individual connections/aliases.
> Tuliprox does **not** assume unlimited provider access; each alias consumes a slot or credit according to your
> provider's rules.

```yaml
    panel_api:
      url: '[https://panel.provider.com/api.php](https://panel.provider.com/api.php)'
      api_key: 'YOUR_ADMIN_KEY'
      credits: "0.0" # Persisted credit balance, updated via account_info
      provisioning:
        timeout_sec: 65
        method: GET           # Probe method (HEAD, GET, or POST)
        probe_interval_sec: 10
        cooldown_sec: 120     # Wait time after successful probe for DB finalization
        offset: 12h           # Pre-expiry window (e.g., 15m, 5h, 1d)
      alias_pool:
        size: { min: auto, max: auto }
        remove_expired: true
      query_parameter:
        account_info: # Executed on boot/update to fetch credits
          - { key: action, value: account_info }
          - { key: api_key, value: auto }
        client_info: # Mandatory for syncing exp_date
          - { key: action, value: client_info }
          - { key: username, value: auto }
          - { key: password, value: auto }
          - { key: api_key, value: auto }
        client_new: # Create new account (type: m3u only)
          - { key: action, value: new }
          - { key: type, value: m3u }
          - { key: sub, value: '1' }
          - { key: api_key, value: auto }
        client_renew: # Renew existing account (type: m3u only)
          - { key: action, value: renew }
          - { key: type, value: m3u }
          - { key: username, value: auto }
          - { key: password, value: auto }
          - { key: sub, value: '1' }
          - { key: api_key, value: auto }
        client_adult_content: # Optional: Unlock adult content after new/renew
          - { key: action, value: adult_content }
          - { key: username, value: auto }
          - { key: password, value: auto }
          - { key: api_key, value: auto }
```

---

#### Configuration Parameters

| Block / Parameter  | Type   | Default | Technical Impact & Background                                                                                                                                |
|:-------------------|:-------|:--------|:-------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `url`              | String |         | The base endpoint for the provider's reseller API.                                                                                                           |
| `api_key`          | String |         | Your reseller administrative key.                                                                                                                            |
| **`alias_pool`**   | Object |         | Controls the lifecycle of active aliases.                                                                                                                    |
| ↳ `size.min`       | Mixed  | `1`     | Min accounts to keep. `number` or `auto`. If `auto`, it uses the count of enabled Tuliprox users (Active/Trial, not expired) mapped to this input's targets. |
| ↳ `size.max`       | Mixed  | `1`     | Upper bound for aliases. If `auto`, checks are triggered upon user add/update.                                                                               |
| ↳ `remove_expired` | Bool   | `false` | If `true`, removes expired accounts from `source.yml` or batch CSVs during boot/update. (The root input is never removed).                                   |
| **`provisioning`** | Object |         | Verification and renewal logic.                                                                                                                              |
| ↳ `offset`         | String | `None`  | Pre-expiry window. If `now + offset > exp_date`, Tuliprox fires `client_renew` (falls back to `client_new`).                                                 |
| ↳ `timeout_sec`    | Int    | `65`    | Max wait time for probing a new account before continuing boot/update.                                                                                       |
| ↳ `method`         | Enum   | `HEAD`  | HTTP method for probes (`HEAD`, `GET`, `POST`).                                                                                                              |
| ↳ `cooldown_sec`   | Int    | `0`     | Extra wait time after a successful probe to mitigate 5XX errors during provider provisioning.                                                                |

---

#### Runtime Logic & Dynamic Values (`auto`)

The keyword `auto` acts as a placeholder for Tuliprox to inject runtime values dynamically into query parameters:

* **`api_key: auto`**: Replaced by `panel_api.api_key`.
* **`username / password: auto`**: Replaced by the specific credentials of the account being queried, renewed, or
  probed.

#### Response Evaluation & Fallback Logic

Tuliprox processes all Panel API responses as JSON and strictly requires `status: true`.

* **`account_info`**: Extracts the `credits` field and persists it. Uses root input credentials if `auto` is specified.
* **`client_info`**: Syncs the `expire` field, normalizing the timestamp/date to UTC.
* **`client_new`**: Attempts to extract `username` and `password` directly.
  * **Fallback:** If fields are missing, Tuliprox parses a `url` field within the JSON response to extract credentials
    from the query string.
  * Failure to derive credentials results in a failed operation and no alias persistence.
* **`client_renew`**: Updates the expiration date without modifying existing credentials.
* **`client_adult_content`**: Optionally executed after `client_new` or `client_renew` to toggle adult content settings
  on the provider side. Requires `status: true` for success.

---

## 3. Routing & Targets (`sources`)

This block links your inputs to one or more output targets and defines how Tuliprox transforms, filters, sorts, and
exports the resulting playlist.
Under `sources:`, you connect `inputs` from the `inputs` section of `source.yml` with one or more `targets`.

```yaml
sources:
  - inputs:
      - my_provider
    targets:
      - name: my_target
        filter: 'Group ~ ".*"'
        output:
          - type: m3u
```

### 3.1 `inputs`

`inputs` is a list of input names referencing entries defined in the `inputs` section of `source.yml`.

```yaml
sources:
  - inputs:
      - my_input_a
      - my_input_b
```

> **Note:** The `inputs` list only references previously defined input names. It does not define input behavior itself.

### 3.2 `targets`

A `target` defines the final transformed playlist that clients consume.
Tuliprox supports multiple targets per source, and each target can export to multiple output formats simultaneously.

```yaml
sources:
  - inputs:
      - my_provider
    targets:
      - name: my_target
        enabled: true
        processing_order: frm
        filter: 'Group ~ ".*"'
        rename: [ ]
        mapping: [ ]
        sort: { }
        options:
          ignore_logo: false
          epg_output:
            lowercase_ids: false
            lowercase_xmltv_display_names: false
          share_live_streams:
            hls: false
            mpeg_ts: false
          remove_duplicates: false
        output:
          - type: xtream
        favourites: [ ]
        watch: [ ]
        use_memory_cache: false

  targets:
    - name: my_target
      processing_order: rmf
      filter: 'Group ~ "Sports.*"'
      rename:
        - field: group
          pattern: '^UK '
          new_name: ''
      mapping:
        - sports_map
      output:
        - type: m3u
```

#### Target Parameters

| Parameter          | Type   | Required | Default   | Technical Impact & Background                                                                                                                                                                                                |
|:-------------------|:-------|:--------:|:----------|:-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `enabled`          | Bool   |    No    | `true`    | If set to `false`, Tuliprox skips building this target during normal processing. This reduces CPU, disk, and upstream workload, but the target can still be selected explicitly via CLI target execution if matched by `-t`. |
| `name`             | String |    No    | `default` | Logical target name. If not `default`, it must be unique. Unique names are important for selective execution (`-t <target_name>`) and for clearly separating output identities in Tuliprox's processing pipeline.            |
| `processing_order` | Enum   |    No    | `frm`     | Defines execution order for **F**ilter, **R**ename, and **M**ap. This directly changes which intermediate state downstream steps operate on and can therefore materially alter the final playlist result.                    |
| `filter`           | String |   Yes    |           | Global filter DSL expression for the target. This determines which entries survive into the final target after the selected processing order has been applied.                                                               |
| `rename`           | List   |    No    |           | Regex-based transformations applied to selected fields. This is commonly used to normalize channel/group labels before sorting, mapping, or export.                                                                          |
| `mapping`          | List   |    No    |           | References mapping IDs from `mapping.yml` for advanced transformation logic. This is where deep structural rewriting and metadata normalization can be applied.                                                              |
| `sort`             | Object |    No    |           | Defines ordering for groups and channels after transformations. This affects the final playlist structure seen by clients and can significantly improve navigation quality in IPTV players.                                  |
| `options`          | Object |    No    |           | Target-level behavior switches such as logo suppression, duplicate removal, and shared live-stream handling. These options influence memory usage, playlist cleanliness, and reverse-proxy behavior.                         |
| `output`           | List   |   Yes    |           | Mandatory list of output formats. A single target can generate multiple output representations (e.g., `xtream`, `m3u`, `strm`, `hdhomerun`) from the same transformed result set.                                            |
| `favourites`       | List   |    No    |           | Duplicates final transformed channels into dedicated favorite groups after processing is complete. This adds curated views without changing the original group structure.                                                    |
| `watch`            | List   |    No    |           | Defines watched group patterns. If matching groups change during updates, Tuliprox emits Messaging events so operational changes become observable automatically.                                                            |
| `use_memory_cache` | Bool   |    No    | `false`   | If enabled, the final compiled playlist is cached in RAM. This reduces disk access and improves delivery speed, especially for M3U downloads, but increases memory consumption.                                              |

---

### 3.2.1 `processing_order`

The processing order defines how Tuliprox applies:

* **F**ilter
* **R**ename
* **M**ap

Valid values are:

* `frm` (default)
* `fmr`
* `rfm`
* `rmf`
* `mfr`
* `mrf`

> **Note:** The selected processing order can change the final result significantly.
> For example, if renaming occurs before filtering, the filter must match the renamed state rather than the original
> source value.

`processing_order` only arranges the `processing`-stage mapping blocks around filter and rename. Mapping blocks that
opt into `stage: after_epg` always run once EPG enrichment has completed, regardless of `processing_order`.

---

### 3.2.2 `filter`

The target-level `filter` is a string-based expression using Tuliprox's filter DSL.
It defines which entries remain in the final target after the selected processing stages have been applied.

You can define complex strings or regex patterns exactly once in [template.yml](./template.md)
and call them by wrapping the template name in exclamation marks: `!MACRO_NAME!`.
For less verbose expression definitions, inline filter definitions are also supported.

Tuliprox supports the following filter expression types:

* Use `NOT` for exclusion logic
* Use `AND` / `OR` for boolean combinations
* Type Comparison: `Type = vod` or `Type = live` or `Type = series`
* Regular expression comparison: `([fieldname]) ~ "regexp"` <br>
  The `[fieldname]` can be `Group`, `Title`, `Name`, `Caption`, `Url`, `Genre`, `Input`, `EpgId` or `Type`.
* String comparison (case-insensitive, no regex needed):
  * Exact: `Group = "Sports"` / negated: `Group != "Sports"`
  * Substring: `Title CONTAINS "HD"`
  * Prefix: `Caption STARTSWITH "DE:"`
  * Case-insensitivity is ASCII-only: ASCII letters match regardless of case, non-ASCII characters must match
    exactly. `Title CONTAINS "cinéma"` matches `Cinéma` but not `CINÉMA`.
* Set membership (case-insensitive exact match against a list): `Group IN ["Sports", "News"]`
* Numeric comparison on the channel number: `Chno = 5`, `Chno != 5`, `Chno > 100`, `Chno >= 100`, `Chno < 200`, `Chno <= 200`
* Numeric comparison on the detected quality tier: `Quality >= 3` <br>
  The tier is derived from quality tokens in the caption: `5` = 4K/UHD/2160p, `4` = QHD/1440p, `3` = FHD/1080p,
  `2` = HD/720p, `1` = SD/480p/576p, `0` = no recognized quality token.
* Filters don't have operator precedence, so please use parentheses
* You can apply Morgan’s Law `NOT (A) AND NOT (B)`is the same as `NOT( A OR B)`

> **Note:**
>
> * If you use special characters like `+ | [ ] ( )` inside the filter expression you must escape them correctly with
    backslashes.
> * When testing expressions externally, e.g. [regex101.com](https://regex101.com/), select the **Rust** flavor.
    > This helps avoid mismatches between development-time testing and Tuliprox runtime behavior.
>
> > **⚠️ Warning:** Filter expressions are evaluated using Rust-style regex behavior.
> Unsupported features such as lookarounds and backreferences are not available, so patterns copied from PCRE-based
> > tools may need adjustment.

#### Example Filter

```yaml
targets:
  - name: regional_mix
    filter: '((Group ~ "^DE.*") AND (NOT Title ~ ".*Shopping.*")) OR (Group ~ "^AU.*")'
    output:
      - type: m3u
```

This example keeps:

* entries from groups starting with `DE`, except titles containing `Shopping`
* all entries from groups starting with `AU`

---

### 3.2.3 `rename`

The `rename` block is a list of rename rules applied to selected fields.
Each rule performs regex-based search and replace using capture groups where needed.

#### Rename Parameters

| Parameter  | Type           | Required | Default | Technical Impact & Background                                                                                                                                                                    |
|:-----------|:---------------|:--------:|:--------|:-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `field`    | Enum           |   Yes    |         | Field to transform, can be `group`, `title`, `name`, `caption` or `url`. This determines which part of the playlist entry Tuliprox rewrites before later stages such as sorting or final export. |
| `pattern`  | String (Regex) |   Yes    |         | Regular expression used to match the current value of the selected field. This enables structural normalization of inconsistent source naming schemes.                                           |
| `new_name` | String         |   Yes    |         | Replacement string. It can reference regex capture groups via `$1`, `$2`, and so on. This allows Tuliprox to preserve selected original content while reformatting labels.                       |

#### Rename Example

Example:

```yaml
rename:
  - field: group
    pattern: '^DE(.*)'
    new_name: '1. DE$1'
```

In above example, every group beginning with `DE` is renamed to start with `1.`, for example:

* `DE Sports` → `1. DE Sports`
* `DE Movies` → `1. DE Movies`

This can be useful for players that ignore provider order and perform their own alphabetical sorting.

> **Note:** The effective value that `rename` sees depends on `processing_order`.
> If mapping runs before renaming, your rename pattern must match the already mapped value rather than the original
> source value.

---

### 3.2.4 `mapping`

The `mapping` block references a list of mapping identifiers (IDs) defined in your [mapping files](./mapping-dsl.md) (
default: `mapping.yml`).

```yaml
mapping:
  - map_cleanup
  - map_regional_groups
  - map_vod_enrichment
```

#### Mapping Parameters

| Parameter | Type            | Required | Default | Technical Impact & Background                                                                                                                                                                                                             |
|:----------|:----------------|:--------:|:--------|:------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `mapping` | List of Strings |    No    |         | Ordered list of mapping IDs to apply. Each referenced mapping can perform deep transformations on the playlist structure, metadata, grouping, or labels, making this one of the most powerful target-level processing stages in Tuliprox. |

To define a new mapping IDs see details in chapter [Mapper DSL & Logic](./mapping-dsl.md).

---

### 3.2.5 `sort`

The `sort` block defines ordering rules for groups and channels.

It has the following top-level attributes:

* `match_as_ascii` *optional*, default `false`
* `rules`

#### Sort Parameters

| Parameter        | Type | Required | Default | Technical Impact & Background                                                                                                                                                                            |
|:-----------------|:-----|:--------:|:--------|:---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `match_as_ascii` | Bool |    No    | `false` | If enabled, Tuliprox normalizes accented characters during sorting comparisons. This improves deterministic ordering across multilingual playlists without modifying the original visible channel names. |
| `rules`          | List |   Yes    |         | Ordered list of sort rules. Each rule is evaluated against the playlist after transformation, and directly shapes the browsing order clients see in the final target.                                    |

#### `rules`

Each sort rule supports the following entries:

| Parameter  | Type   | Required | Default | Technical Impact & Background                                                                                                                                                                                               |
|:-----------|:-------|:--------:|:--------|:----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `target`   | Enum   |   Yes    |         | Defines whether the rule sorts `group` or `channel` entries. This changes whether Tuliprox reorders category containers or items within those categories.                                                                   |
| `field`    | String |   Yes    |         | Sort field. For `channel`: `title`, `name`, `caption`, `url`, or `quality` (detected quality tier, best used with `order: desc`). For `group`: `group`. This determines which final-state value Tuliprox uses for ordering. |
| `filter`   | String |   Yes    |         | Filter expression defining which entries the rule applies to. This makes it possible to sort only selected subsets of the playlist instead of the entire target uniformly.                                                  |
| `order`    | Enum   |   Yes    |         | `asc`, `desc`, or `none`. `none` preserves source order for matched entries and is useful when provider order should remain untouched.                                                                                      |
| `natural`  | Bool   |    No    | `false` | Natural sort: numbers embedded in values compare numerically instead of lexicographically, so `Channel 2` sorts before `Channel 10`. Applies to the rule's value and sequence capture comparisons.                          |
| `sequence` | List   |    No    |         | Ordered regex list used for index-based sorting. When present, Tuliprox prioritizes regex sequence position over `order`, enabling explicit semantic ordering such as quality tiers or curated group precedence.            |

> **Note:** Sort rules must be written with the configured `processing_order` in mind,
> because sorting operates on the transformed state that exists at that point in the pipeline.
>
> **Multi-field sorting:** rules are applied in the order they are declared. When a rule compares
> equal, the next rule decides — so a `channel` rule on `group` followed by one on `caption`
> produces group-then-caption ordering.

#### Sort Example

```yaml
sort:
  match_as_ascii: false
  rules:
    - target: group
      order: asc
      filter: 'Group ~ ".*"'
      field: group
      sequence:
        - '^Freetv'
        - '^Shopping'
        - '^Entertainment'
        - '^Sunrise'
    - target: channel
      order: asc
      filter: 'Group ~ ".*"'
      field: title
      sequence:
        - '(?P<c1>.*?)\bUHD\b'
        - '(?P<c1>.*?)\bFHD\b'
        - '(?P<c1>.*?)\bHD\b'
        - '(?P<c1>.*?)\bSD\b'
```

**Named Capture Groups** in `sequence`

To sort by specific parts of a value, use named capture groups such as:

1. `c1`
2. `c2`
3. `c3`

> **Note:**
>
> * The numeric suffix defines priority. c1 > c2 > c3

This allows Tuliprox to perform structured multi-level sorting based on extracted fragments of a channel title or label.

In the example above:

* groups are ordered according to the explicit `sequence`
* Channels within the `Freetv` group are first sorted by `quality` (as matched by the regexp sequence), and then by the
  `captured prefix`.

---

### 3.2.6 `options`

Target-level `options` control behavior of the final playlist independent of output type.

```yaml
targets:
  - name: xc_m3u
    output:
      - type: xtream
        skip_live_direct_source: true
        skip_video_direct_source: true
      - type: m3u
      - type: strm
        directory: /tmp/kodi
      - type: hdhomerun
        username: hdhruser
        device: hdhr1
        use_output: xtream
    options:
      ignore_logo: false
      epg_output:
        lowercase_ids: true
        lowercase_xmltv_display_names: false
      share_live_streams:
        hls: true
        mpeg_ts: true
      remove_duplicates: false
      deduplicate:
        match_by: caption
        keep: best_quality
        match_as_ascii: false
```

#### Target Option Parameters

| Parameter                                  | Type | Required | Default | Technical Impact & Background                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
|:-------------------------------------------|:-----|:--------:|:--------|:--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `ignore_logo`                              | Bool |    No    | `false` | Ignores `tvg-logo` and `tvg-logo-small` attributes. This reduces downstream device-side logo caching and can keep generated M3U playlists leaner for clients with limited storage or poor cache invalidation behavior.                                                                                                                                                                                                                                                                                                                                                                            |
| `share_live_streams.hls`                   | Bool |    No    | `false` | Enables HLS live sharing for the new HLS cache proxy path. This is a configuration switch for the HLS cache feature and is independent from MPEG-TS stream sharing.                                                                                                                                                                                                                                                                                                                                                                                                                               |
| `share_live_streams.mpeg_ts`               | Bool |    No    | `false` | Allows Tuliprox to share MPEG-TS live stream connections in reverse proxy mode. This can reduce upstream provider connection usage when multiple clients watch the same channel, but it increases memory usage per shared channel.                                                                                                                                                                                                                                                                                                                                                                |
| `remove_duplicates`                        | Bool |    No    | `false` | Attempts to remove duplicate entries by `url`. This improves playlist cleanliness and reduces confusing duplicates in the client-facing output.                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| `deduplicate`                              | Map  |    No    | -       | Quality-aware duplicate removal. Channels whose match value is identical after stripping quality tokens (`4K`, `UHD`, `2160p`, `QHD`, `1440p`, `FHD`, `1080p`, `HD`, `720p`, `SD`, `480p`, `576p`) collapse to a single entry. Sub-keys: `match_by` (`caption` (default), `name`, `title`), `keep` (`best_quality` (default) keeps the highest quality tier, `first` keeps the first occurrence) and `match_as_ascii` (default `false`, normalizes accented characters in match keys so `Café HD` matches `Cafe FHD`). Matching is per cluster across all groups; ties keep the first occurrence. |
| `epg_output.lowercase_ids`                 | Bool |    No    | `false` | Canonicalizes visible technical EPG IDs with ASCII lowercase across M3U `tvg-id`, Xtream `epg_channel_id`, XMLTV channel/programme references, and EPG API responses. Changing this option requires a full target refresh.                                                                                                                                                                                                                                                                                                                                                                        |
| `epg_output.lowercase_xmltv_display_names` | Bool |    No    | `false` | Applies Unicode lowercase exclusively to XMLTV `<display-name>` values during serialization. Playlist names, Xtream names, programme titles, and programme descriptions remain unchanged; persisted target data does not require rebuilding.                                                                                                                                                                                                                                                                                                                                                      |
| `force_redirect`                           | Bool |    No    | `false` | Optional redirect-related behavior switch. This influences how Tuliprox serves final stream delivery where redirect-style output handling is required by the deployment model.                                                                                                                                                                                                                                                                                                                                                                                                                    |

> **Shared HLS:** `share_live_streams.hls` requires `reverse_proxy.hls_cache` in `config.yml`.
> Start with [Shared HLS Sessions](./shared-hls-sessions.md) for the feature overview and
> [Shared HLS Configuration](./shared-hls-configuration.md) for the full setup checklist.
>
> Use the object form shown above. The old boolean style `share_live_streams: true` is not valid for this configuration,
> because HLS sharing and MPEG-TS sharing are independent switches.
>
> **⚠️ Warning:** When `share_live_streams.mpeg_ts` is enabled, each shared channel consumes at least **12 MB** of memory,
> regardless of the number of connected clients.
> If the reverse-proxy buffer size is increased above `1024`, memory usage increases accordingly.
> Example: with a buffer size of `2048`, each shared channel consumes at least **24 MB**.

#### Quality-Aware Deduplication Example

Keep only the best-quality copy of every channel, collapsing entries like `News HD`, `News FHD`, and `NEWS [4K]`
into the single `NEWS [4K]` entry:

```yaml
targets:
  - name: clean_target
    filter: 'Group ~ ".*"'
    options:
      deduplicate:
        match_by: caption      # caption (default) | name | title
        keep: best_quality     # best_quality (default) | first
        match_as_ascii: false  # true: "Café HD" matches "Cafe FHD"
    output:
      - type: m3u
```

* Matching compares the selected field with quality tokens stripped and remaining words lowercased,
  so unrelated channels never collapse.
* `keep: first` keeps the first occurrence in playlist order instead of the highest quality tier
  (useful when provider ordering already encodes your preference).
* Deduplication runs after group merging and before sorting; groups left empty are removed.

#### EPG Output Normalization

`epg_output` applies to the entire target so every output format uses the same EPG identity space. Both options
default to `false`; when they are omitted, Tuliprox preserves the source casing and existing output behavior.

```yaml
targets:
  - name: sample_target
    options:
      epg_output:
        lowercase_ids: true
        lowercase_xmltv_display_names: true
    output:
      - type: m3u
      - type: xtream
```

With both options enabled, neutral input values such as `Example.Channel` and `Sample Network` produce a canonical
technical ID of `example.channel` and the following XMLTV output:

```xml
<channel id="example.channel">
  <display-name>sample network</display-name>
</channel>
<programme channel="example.channel">
  <title>Example Programme</title>
</programme>
```

`lowercase_ids` uses ASCII lowercase for technical identifiers. Non-ASCII characters in those IDs remain unchanged.
The canonical visible ID is used consistently for M3U `tvg-id`, Xtream `epg_channel_id`, XMLTV `<channel id>` and
`<programme channel>`, and Short EPG / Stream EPG responses. Target EPG database keys and API lookup keys use the same
target output casing: source case is preserved while the option is disabled, and ASCII lowercase is used after the
option is enabled and the target is refreshed. This keeps existing mixed-case target databases compatible while the
two XMLTV attributes continue to use exactly the same visible value.

`lowercase_xmltv_display_names` uses Unicode lowercase only while serializing XMLTV `<display-name>` values. It takes
effect for subsequent XMLTV responses and normally does not require rebuilding persisted target data. It does not
change playlist or Xtream names, programme titles or descriptions, input data, parser values, caches, or Smart Match
behavior.

Changing `lowercase_ids` requires a full target refresh so persisted playlist and EPG artifacts use the same visible
IDs; a configuration hot reload alone is insufficient. Clients may need to re-index their EPG data once after the
visible IDs change.

---

### 3.2.7 Output Formats (`output`)

A target can be exported to multiple formats simultaneously. The target-level filter, rename, mapping, and sort
logic are applied first, and each output then formats the result differently.

> **Note:** Output-specific filters are applied **after all transformations have completed**.
> Therefore, any filter inside an individual output block must refer to the **final playlist state**.

#### Output Block Parameters

Every output block contains at least:

| Parameter | Type   | Required | Default | Technical Impact & Background                                                                                                                                                                        |
|:----------|:-------|:--------:|:--------|:-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `type`    | Enum   |   Yes    |         | Output format type. Supported values include `xtream`, `m3u`, `strm`, and `hdhomerun`. This determines how Tuliprox serializes and serves the final playlist to downstream consumers.                |
| `filter`  | String |    No    |         | Optional output-level filter applied after all target transformations. This allows Tuliprox to derive specialized output subsets from the same target without duplicating upstream processing logic. |

**Specific Output Properties** are defined for each type:

### 1. Type `xtream`

```yaml
output:
  - type: xtream
    skip_live_direct_source: true
    skip_video_direct_source: true
    skip_series_direct_source: true
    update_strategy: instant
    trakt:
      api:
        api_key: "YOUR_API_KEY"
        version: "2"
        url: "https://api.trakt.tv"
        user_agent: "Mozilla/5.0"
      lists:
        - user: "gary"
          list_slug: "latest-tv"
          category_name: "Trending TV"
          content_type: series
          fuzzy_match_threshold: 80
```

#### `xtream` Parameters

| Parameter                   | Type   | Required | Default   | Technical Impact & Background                                                                                                                                                                         |
|:----------------------------|:-------|:--------:|:----------|:------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `type`                      | Enum   |   Yes    |           | Must be `xtream`. Generates an Xtream-compatible API output backed by Tuliprox's processed data model.                                                                                                |
| `skip_live_direct_source`   | Bool   |    No    | `true`    | If `true`, Tuliprox ignores provider `direct_source` values for live content. This keeps playback under Tuliprox's delivery logic and avoids client behavior differences caused by bypass URLs.       |
| `skip_video_direct_source`  | Bool   |    No    | `true`    | If `true`, Tuliprox ignores provider `direct_source` values for movies/VOD. This improves consistency across clients that otherwise may bypass Tuliprox for video playback.                           |
| `skip_series_direct_source` | Bool   |    No    | `true`    | If `true`, Tuliprox ignores provider `direct_source` values for series entries. This ensures Tuliprox stays in control of series playback URL generation and proxy behavior.                          |
| `update_strategy`           | Enum   |    No    | `instant` | `instant` writes changes immediately, while `bundled` batches write operations. This directly trades off freshness versus disk I/O load during background metadata enrichment and output maintenance. |
| `trakt`                     | Object |    No    |           | Trakt.tv integration block. Tuliprox can fetch Trakt lists, fuzzy-match them against playlist entries, and inject matched VOD or series entries into generated virtual categories.                    |
| `filter`                    | String |    No    |           | Optional output-level filter for the Xtream export only. Useful when the same target should expose different subsets to different output formats.                                                     |

> **Note:** IPTV players vary in how they resolve streams: some use the direct-source attribute, while others
> reconstruct URLs
> from server metadata. To ensure Tuliprox maintains control over the stream routing (Proxy/Redirect),
> the Direct Source Handling (skip_*_direct_source) attributes default to true.
>
> **⚠️ Warning:** Setting `skip_*_direct_source` to `false` forces the player to use the provider's original
`direct-source` URL.
> This effectively **bypasses Tuliprox**, which will disable internal features like connection tracking,
> IP masking, and failover logic for those streams.

#### `trakt` Object in Xtream Output

Trakt.tv is an online platform for tracking, organizing, and discovering movies and TV shows.
Tuliprox can query Trakt lists and match playlist entries using Jaro-Winkler-style fuzzy matching.
Matching entries are then added to new virtual categories inside the Xtream output.

You can define a `Trakt` config like

```yaml
inputs:
  - name: my_xtream_input
    type: xtream
    options:
      resolve_series: false
      resolve_vod: false

sources:
  - inputs:
      - my_xtream_input
    targets:
      - name: iptv-trakt-example
        filter: 'Group ~ ".*"'
        output:
          - type: xtream
            skip_live_direct_source: true
            skip_video_direct_source: true
            skip_series_direct_source: true
            trakt:
              api:
                api_key: "YOUR_API_KEY"
                version: "2"
                url: "https://api.trakt.tv"
                user_agent: "Mozilla/5.0"
              lists:
                - user: "linaspurinis"
                  list_slug: "top-watched-movies-of-the-week"
                  category_name: "📈 Top Weekly Movies"
                  content_type: vod
                  fuzzy_match_threshold: 80
                - user: "garycrawfordgc"
                  list_slug: "latest-tv-shows"
                  category_name: "📺 Latest TV Shows"
                  content_type: series
                  fuzzy_match_threshold: 80
              charts:
                - kind: movies
                  chart: trending
                  category_name: "🔥 Trending Movies"
                  tmdb_only: true
                - kind: shows
                  chart: popular
                  category_name: "⭐ Popular Shows"
                  tmdb_only: true
```

This configuration creates additional virtual categories populated with matched entries from the configured Trakt user
lists and public Trakt charts.

##### Trakt Parameters

| Parameter                        | Type    | Required | Default                | Technical Impact & Background                                                                                                              |
| :------------------------------- | :------ | :------: | :--------------------- | :----------------------------------------------------------------------------------------------------------------------------------------- |
| `api.api_key`                    | String  | Yes      |                        | Trakt API key used for authenticated access. Without a valid key, Tuliprox cannot fetch remote list content.                               |
| `api.version`                    | String  | No       | `"2"`                  | API version header value. This ensures Tuliprox formats requests against the correct Trakt API version.                                    |
| `api.url`                        | String  | No       | `https://api.trakt.tv` | Base API URL for Trakt requests. This defines the remote endpoint Tuliprox queries for list data.                                          |
| `api.user_agent`                 | String  | No       |                        | Optional `User-Agent` used for Trakt API requests. This can help satisfy API gateway expectations or deployment-specific request policies. |
| `lists[].user`                   | String  | Yes      |                        | Trakt username owning the list. This identifies which account namespace Tuliprox fetches list data from.                                   |
| `lists[].list_slug`              | String  | Yes      |                        | Trakt list slug. Combined with `user`, this uniquely identifies the remote list to load.                                                   |
| `lists[].category_name`          | String  | Yes      |                        | Name of the generated virtual category inside Tuliprox's Xtream output. This controls where matched entries appear to clients.             |
| `lists[].content_type`           | Enum    | Yes      |                        | `vod` or `series`. This determines which class of playlist entries Tuliprox will attempt to match and inject into the generated category.  |
| `lists[].tmdb_only`              | Bool    | No       | `false`                | If `true`, only exact TMDB-id matches are accepted for this list, disabling title/year fuzzy fallback and reducing false positives.        |
| `lists[].fuzzy_match_threshold`  | Integer | No       |                        | Fuzzy matching threshold for title matching. Higher values reduce false positives but may miss loosely matching items.                     |
| `charts[]`                       | List    | No       | `[]`                   | Public Trakt chart definitions. Unlike `lists[]`, these are system charts and do not have a user/list owner.                               |
| `charts[].kind`                  | Enum    | Yes      |                        | `movies` or `shows`. Aliases such as `movie`, `vod`, `show`, `series`, and `tvshows` are accepted.                                         |
| `charts[].chart`                 | Enum    | Yes      |                        | Public chart to fetch. MVP supports `trending` and `popular`.                                                                              |
| `charts[].category_name`         | String  | Yes      |                        | Name of the generated virtual category inside Tuliprox's Xtream output.                                                                    |
| `charts[].tmdb_only`             | Bool    | No       | `false`                | If `true`, only exact TMDB-id matches are accepted. This is recommended for dynamic charts to avoid fuzzy false positives.                 |
| `charts[].fuzzy_match_threshold` | Integer | No       |                        | Fuzzy matching threshold for chart title matching when `tmdb_only` is not enabled.                                                         |

The `charts[]` MVP intentionally supports only public, non-OAuth Trakt charts. User-specific recommendations and
account-scoped history feeds are not fetched by this block.

### 2. Type `m3u`

```yaml
output:
  - type: m3u
    filename: custom_playlist.m3u
    include_type_in_url: false
    mask_redirect_url: false
    filter: 'Type = live'
```

#### `m3u` Parameters

| Parameter             | Type   | Required | Default | Technical Impact & Background                                                                                                                                                                                                                               |
|:----------------------|:-------|:--------:|:--------|:------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `type`                | Enum   |   Yes    |         | Must be `m3u`. Generates a traditional playlist file suitable for IPTV players and related clients.                                                                                                                                                         |
| `filename`            | String |    No    |         | Optional custom output filename. This affects how Tuliprox writes or exposes the generated playlist artifact.                                                                                                                                               |
| `include_type_in_url` | Bool   |    No    | `false` | If enabled, Tuliprox adds the stream type (`live`, `movie`, `series`) into generated stream URLs. This can improve downstream routing clarity and compatibility with clients that distinguish path structure by media type.                                 |
| `mask_redirect_url`   | Bool   |    No    | `false` | If enabled, Tuliprox uses URLs from `api-proxy.yml` for users operating in `redirect` proxy mode. This is important for multi-provider failover or cycling setups where exposing the provider URL directly would bypass Tuliprox's routing logic too early. |
| `filter`              | String |    No    |         | Optional M3U-only post-transformation filter. This allows M3U consumers to receive a narrower subset than other output formats derived from the same target.                                                                                                |

> **Note:** `mask_redirect_url` should be enabled if you use multiple providers and want Tuliprox to preserve
> redirect-mode
> routing and cycling behavior without exposing the direct upstream endpoint in the initial playlist URL.

#### M3U Catchup & Archive Support

Tuliprox preserves the following catchup and archive attributes when it imports and exports M3U playlists:

* `catchup`
* `catchup-days`
* `catchup-source`
* `catchup-time`
* `catchup-correction`
* `catchup-type`
* unknown attributes whose names start with `catchup-`

When an output keeps direct provider URLs, the catchup metadata is written unchanged. For reverse-proxied outputs,
Tuliprox replaces supported provider templates with authenticated local URLs. The provider URL and its credentials are
not included in the generated catchup URL.

The template modes `default`, `append`, `shift`, `xc`, `fs`, and `vod` are supported. An unknown mode can still be
proxied when it supplies an explicit source template. Without a usable template, Tuliprox preserves the metadata but
cannot create a local catchup URL.

Native Flussonic playback is available for both HLS and MPEG-TS:

* HLS uses `.m3u8` archive, relative-timeshift, and absolute-timeshift paths.
* MPEG-TS uses absolute-timeshift `.ts` paths.
* Flat archive requests generated by TiviMate and nested Flussonic archive paths are accepted.
* A live item with a `timeshift` value but no catchup block is treated as Flussonic-style catchup.

For HLS archives, Tuliprox carries the archive start parameter into same-origin child playlists when the child does not
already contain one. It does not add archive parameters to media segments, encryption keys, or initialization files.

When API server information is available, the generated `#EXTM3U` header contains `url-tvg` and `x-tvg-url`. Both point
to the authenticated Tuliprox XMLTV endpoint for the playlist user; a provider-supplied `url-tvg` value is not forwarded.

Tuliprox also reads a per-channel `#EXTVLCOPT:http-user-agent` directive. It writes the directive back when the output
contains the direct provider URL. For rewritten URLs, the value is kept internal and applied to the upstream HLS or
MPEG-TS request instead of being exposed in the playlist. Disabling the `User-Agent` header through the reverse-proxy
header settings takes precedence.

### 3. Type `strm`

```yaml
output:
  - type: strm
    directory: /media/strm
    username: local_user
    style: jellyfin
    flat: true
    cleanup: false
    underscore_whitespace: false
    add_quality_to_filename: true
    use_metadata: false
    strm_props:
      - "#KODIPROP:seekable=true"
      - "#KODIPROP:inputstream=inputstream.ffmpeg"
    filter: 'Type = vod'
```

Generates local `.strm` files for Emby, Jellyfin, or Kodi-based library ingestion.

> **Upgrade note:** `style: plex` is no longer supported. Existing STRM targets must switch to `kodi`,
> `emby`, or `jellyfin`, for example `style: plex` -> `style: jellyfin`. For Plex use cases, use the
> `hdhomerun` integration instead. Leaving `style: plex` in the configuration will fail validation.

#### `strm` Parameters

| Parameter                 | Type   | Required | Default | Technical Impact & Background                                                                                                                                                                                                             |
|:--------------------------|:-------|:--------:|:--------|:------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `type`                    | Enum   |   Yes    |         | Must be `strm`. Generates filesystem-based `.strm` references instead of a network playlist format.                                                                                                                                       |
| `directory`               | String |   Yes    |         | Target directory where `.strm` files are written. This is the root Tuliprox manages for exported media stubs and must be chosen carefully to avoid overlap with real media directories.                                                   |
| `username`                | String |    No    |         | Optional username context used when generating stream references. This affects which user-specific URL or access context Tuliprox embeds into the exported `.strm` files.                                                                 |
| `underscore_whitespace`   | Bool   |    No    | `false` | Replaces whitespace with `_` in paths and filenames. This improves compatibility with environments or scrapers that prefer filesystem-safe, normalized naming.                                                                            |
| `cleanup`                 | Bool   |    No    | `false` | If enabled, Tuliprox removes orphaned output files from the STRM directory. This keeps the export directory synchronized with the target, but can delete files if the directory points to an existing media folder.                       |
| `style`                   | Enum   |   Yes    |         | Naming convention for the output structure. Supported values: `kodi`, `emby`, `jellyfin`. This affects scraper compatibility and how downstream media servers identify titles.                                                            |
| `flat`                    | Bool   |    No    | `false` | If enabled, Tuliprox creates a flatter directory structure. This changes how categories and group information are represented on disk and can simplify some media-server imports.                                                         |
| `strm_props`              | List   |    No    |         | Stream property lines inserted into `.strm` files, mainly for Kodi player behavior. This allows low-level playback hints to be embedded directly into generated files.                                                                    |
| `add_quality_to_filename` | Bool   |    No    | `false` | Appends detected media quality tags such as `[1080p 4K HEVC HDR]` to the filename. This improves visibility in library UIs but depends on prior probing/enrichment data being available.                                                  |
| `use_metadata`            | Bool   |    No    | `false` | Uses the media metadata name for STRM filenames and folders. By default, the target's processed title is used, so rename and mapping rules affect the generated paths. If metadata has no name, the processed title remains the fallback. |
| `filter`                  | String |    No    |         | Optional STRM-only output filter. Useful when only a subset of the target should be materialized as filesystem entries.                                                                                                                   |

#### Supported `style` Conventions

* **Kodi:** `Movie Name (Year) {tmdb=ID}/Movie Name (Year).strm`
* **Emby:** `Movie Name (Year) [tmdbid=ID]/Movie Name (Year).strm`
* **Jellyfin:** `Movie Name (Year) [tmdbid-ID]/Movie Name (Year).strm`

##### Kodi-Specific Behavior

If `style: kodi` is selected:

* `#KODIPROP:seekable=true|false` is added automatically
* if `strm_props` is not specified, Tuliprox additionally sets:
  * `#KODIPROP:inputstream=inputstream.ffmpeg`
  * `#KODIPROP:http-reconnect=true`

> **⚠️ Warning:** If `cleanup` is enabled, do **not** point `directory` at a real media library folder.
> Tuliprox may delete files that are no longer part of the generated target.

### 4. Type `hdhomerun`

```yaml
output:
  - type: hdhomerun
    device: hdhr1
    username: local_user
    use_output: xtream
```

This binds the target to a configured HDHomeRun virtual tuner device from `config.yml`.

#### `hdhomerun` Parameters

| Parameter    | Type   | Required | Default | Technical Impact & Background                                                                                                                                                                |
|:-------------|:-------|:--------:|:--------|:---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `type`       | Enum   |   Yes    |         | Must be `hdhomerun`. Exposes the target through Tuliprox's HDHomeRun emulation layer for tuner-style discovery by clients such as Plex or Jellyfin.                                          |
| `device`     | String |   Yes    |         | Must match a device name defined in `config.yml`. This links the playlist target to a specific emulated tuner endpoint.                                                                      |
| `username`   | String |   Yes    |         | Must match a user from `api-proxy.yml`. This determines which account context, access restrictions, and connection limits apply when clients consume the lineup through the tuner interface. |
| `use_output` | Enum   |    No    |         | Selects whether the HDHomeRun stream URLs are based on `m3u` or `xtream` output behavior. This affects how playback URLs are generated and which delivery semantics back the tuner lineup.   |

---

### 3.2.8 Favourites (`favourites`)

`favourites` lets you duplicate final transformed channels into dedicated favorite groups **after**
filtering, renaming, mapping, and other transformations are complete.

```yaml
favourites:
  - cluster: series
    group: "My Favourites"
    filter: 'Name ~ "Cinema"'
    match_as_ascii: true
```

#### `favourites` Parameters

| Parameter        | Type   | Required | Default | Technical Impact & Background                                                                                                                                                                  |
|:-----------------|:-------|:--------:|:--------|:-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `cluster`        | String |    No    |         | Optional logical cluster, for example `series`. This influences how Tuliprox groups the duplicated entries internally for output generation.                                                   |
| `group`          | String |   Yes    |         | Name of the favorite group created in the final playlist. This adds a curated access path without removing the original group membership.                                                      |
| `filter`         | String |   Yes    |         | Filter expression selecting which final entries should be duplicated into the favorites group. This operates on the transformed end state rather than the original raw input.                  |
| `match_as_ascii` | Bool   |    No    | `false` | If enabled, Tuliprox normalizes accented characters during matching. This improves filter matching robustness across multilingual names while preserving the original visible title in output. |

---

### 3.2.9 Watch (`watch`)

For each target with a *unique name*, you can define watched groups.
It is a list of group patterns Tuliprox monitors for content changes during updates.

If matching groups gain or lose channels, Tuliprox emits a Messaging event such as:

* channels added
* channels removed

```yaml
watch:
  - group: '^Sports'
  - group: '^Movies'
```

#### `watch` Parameters

| Parameter | Type           | Required | Default | Technical Impact & Background                                                                                                                                                                                              |
|:----------|:---------------|:--------:|:--------|:---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `group`   | String (Regex) |   Yes    |         | Regex pattern matched against final group names. This allows Tuliprox to detect meaningful content changes in selected areas of the playlist and notify operators automatically through the configured messaging backends. |

> **Note:** `watch` is especially useful for monitoring premium groups, VOD collections,
> or unstable provider segments where additions and removals should generate operational alerts.
