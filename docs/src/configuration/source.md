# 🔌 Pillar 2: `source.yml` (Inputs, Panel API & Targets)

The `source.yml` is the central hub for data flows. Here you define your upstream providers (`inputs`), pool them using aliases,
configure automated reseller provisioning (`panel_api`), and define the output channels for your end devices (`targets`).

## Top-level entries

```yaml
templates:
provider:
inputs:
sources:
```

| Block | Description | Link |
| :--- | :--- | :--- |
| `templates` | *(Legacy)* Inline templates for filter macros. Prefer `template.yml`. | |
| `provider` | Provider Failover & DNS Rotation definitions. | [See section](#1-provider-failover--dns-rotation-provider) |
| `inputs` | Data Sources (Providers, Files, Batches, Library). | [See section](#2-inputs-data-sources-inputs) |
| `sources` | Routing logic combining inputs to output targets. | [See section](#3-routing--targets-sources) |

---

## 1. Provider Failover & DNS Rotation (`provider`)

Tuliprox includes a robust failover engine for unstable IPTV providers. You can define backup URLs and intelligent IP rotation.

Define a `provider` block globally in `source.yml` to specify multiple backup URLs:

```yaml
provider:
  - name: my_failover_provider
    urls:
      - http://primary.example.com
      - http://backup.example.com
    dns:
      enabled: true
      refresh_secs: 300
      prefer: ipv4  # system, ipv4, ipv6
      schemes: [http, https]
      keep_vhost: true
      max_addrs: 2
      on_resolve_error: keep_last_good  # or fallback_to_hostname
      on_connect_error: try_next_ip     # or rotate_provider_url
      overrides:
        "primary.example.com":
          - 203.0.113.10
```

### DNS Rotation Parameters (`provider.dns`)

| Parameter | Type | Default | Technical Impact |
| :--- | :--- | :--- | :--- |
| `refresh_secs` | Int | `300` | The interval in seconds the background task resolves the hostnames. (Minimum effective value is 10). |
| `prefer` | Enum | `system` | Which IP protocol to prefer during DNS resolution. Options: `system`, `ipv4`, `ipv6`. |
| `max_addrs` | Int | `None` | Hard limit on the number of resolved IPs to retain per host. |
| `schemes` | List | `[http, https]` | The HTTP schemes that IP connection rotation applies to. |
| `keep_vhost` | Bool | `false` | If `true`, the `Host` header retains the original `hostname[:port]`. If `false`, it uses `IP[:port]`. Essential for reverse proxies upstream! |
| `on_resolve_error` | Enum | `keep_last_good` | Policy on DNS resolution failure. Options: `keep_last_good` (uses cached IPs), `fallback_to_hostname` (clears cache, forcing host lookup). |
| `on_connect_error` | Enum | `try_next_ip` | Policy on TCP connection failure. Options: `try_next_ip` (cycles to the next resolved IP for the same host), `rotate_provider_url` (instantly fails over to the next URL in the `urls` list). |

### Failover Triggers

Tuliprox automatically switches URLs or DNS IPs on failure.
Failover **DOES** occur on:

* Network Timeouts
* HTTP 5xx errors (500, 502, 503, 504)
* HTTP 404 / 410 / 429

Failover **DOES NOT** trigger on:

* HTTP 401 / 403 (Authentication errors, to avoid rotating due to a banned account).

---
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
    cache_duration: 1d
    persist: playlist_{}.m3u
    method: GET
    exp_date: "2028-11-30 12:34:12"
    headers: {}
    options: {}
    epg: {}
    aliases:[]
    staged: {}
    panel_api: {}
```

### Input Base Parameters

| Parameter | Type | Required | Default | Technical Impact & Background |
| :--- | :--- | :---: | :--- | :--- |
| `name` | String | Yes | | Internal reference ID for Tuliprox. Must be strictly unique. Critical for persistent UUID generation! |
| `type` | Enum | No | `m3u` | Allowed: `m3u`, `xtream`, `library` (Local files) and `m3u_batch`, `xtream_batch` (CSV offloading). |
| `url` | String | Yes | | The Provider URL. Tuliprox supports magic scheme prefixes: `http(s)://`, `file://`, `batch://`, and **`provider://my_failover_provider`** (for the Failover System above). |
| `username` / `password` | String | Often | | Mandatory if `type` = `xtream`. |
| `enabled` | Bool | No | `true` | If `false`, this input is completely ignored in all processing. |
| `cache_duration` | String | No | `0` | **Crucial:** Determines how often Tuliprox actually downloads the raw list from the provider. At `1d` (1 day), Tuliprox serves from its local `.db` for 24 hours, even if you trigger hourly updates. This heavily protects against provider bans! Supports suffixes `s`, `m`, `h`, `d`. |
| `persist` | String | No | | Optional path template (e.g., `./playlist_{}.m3u`) to permanently store the downloaded raw provider list locally on your disk. |
| `method` | Enum | No | `GET` | HTTP Request method for playlist downloads (`GET` or `POST`). |
| `exp_date` | Mixed | No | | Expiration date as `"YYYY-MM-DD HH:MM:SS"` or Unix timestamp. Used for status tracking and Panel API logic. |
| `headers` | Dict | No | | Custom HTTP headers for the download (e.g., `User-Agent: My-Player`). |
| `epg` | Object | No | | Allows mapping of external XMLTV files (see below). |
| `aliases` | List | No | | Connection pooling / Sub-accounts (see below). |
| `staged` | Object | No | | Hybrid architecture feature (see below). |
| `panel_api` | Object | No | | Automated reseller account generation (see below). |

---

### Input Subsections (Object Keys)

| Block | Description | Link |
| :--- | :--- | :--- |
| `headers` | Custom HTTP request headers for playlist and EPG downloads. | [See Headers](#21-headers-headers) |
| `options` | Behavior controls for metadata resolution, stream probing, and skip logic. | [See Options](#input-options-options) |
| `epg` | XMLTV source management and Smart Match fuzzy logic settings. | [See EPG](#21-epg-assignment--smart-match-epg) |
| `aliases` | Connection pooling for multiple subscriptions from the same provider. | [See Aliases](#22-provider-aliases-aliases--batch) |
| `staged` | Hybrid architecture for sideloading external playlists into an input. | [See Staged](#22-staged-sources-staged) |
| `panel_api` | Automated reseller panel integration (provisioning/renewal). | [See Panel API](#provider-panel-api-panel_api) |
---

#### 2.1 Headers (`headers`)
Allows the injection of custom HTTP headers into outgoing requests for this specific provider. This is often required for providers that enforce `User-Agent` whitelisting or specific authorization tokens.

| Parameter | Type | Technical Impact & Background |
| :--- | :--- | :--- |
| `User-Agent` | String | Mimics a specific player or browser to prevent 403 Forbidden errors. |
| `Authorization`| String | Manual token injection if required by the upstream API. |
| `Referer` | String | Can be used to bypass basic hotlink protections. |

```yaml
headers:
  User-Agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) Tuliprox/3.0"
  X-Custom-Auth: "my-secret-token"
```

### 2.2 Input Options (`options`)
Controls the behavior during download and asynchronous metadata resolution (see the *Metadata Update* chapter) for this specific provider.

| Parameter | Type | Default | Technical Impact & Background |
| :--- | :--- | :--- | :--- |
| `xtream_skip_live` / `vod` / `series` | Bool | `false` | Immediately ignores entire categories during the Xtream API download. Saves massive amounts of RAM and runtime if you only want Live-TV from a specific provider, for instance. |
| `xtream_live_stream_without_extension` | Bool | `false` | Strips `.ts` from generated stream URLs. |
| `xtream_live_stream_use_prefix` | Bool | `true` | Injects the `/live/` prefix into URLs. |
| `disable_hls_streaming` | Bool | `false` | Forces Tuliprox to play Live-TV as a raw MPEG-TS (`.ts`) stream, skipping HLS (`.m3u8`) reverse-proxy handling, and forcing direct TS endpoints. |
| `resolve_tmdb` | Bool | `false` | Enables TMDB queries for this specific input based on parsed titles to fill missing posters and release years. |
| `probe_stream` | Bool | `false` |Uses FFprobe to read A/V details (HDR, 4K). Respects `max_connections`. |
| `resolve_background` | Bool | `true` | Metadata scans run asynchronously in the background so the general playlist update (which blocks clients) finishes instantly. |
| `resolve_series` / `resolve_vod` | Bool | `false` | Fetches missing details like Plot or Cast via the Provider's API (`get_vod_info` / `get_series_info`). |
| `probe_series` / `probe_vod` | Bool | `false` | Allows explicit FFprobe analysis of movies or entire TV show seasons. |
| `probe_live` | Bool | `false` | Allows FFprobe to periodically tap into Live-TV streams in the background. |
| `probe_live_interval_hours` | Int | `120` | Interval after which a Live stream is re-analyzed (Important as backup streams often change resolutions). |
| `resolve_delay` / `probe_delay` | Int | `2` | **Ban Protection:** Hard wait time (in seconds) between API or Probe requests to the *same* provider! Prevents API spamming. |

---

### 2.3 EPG Assignment & Smart Match (`epg`)
Tuliprox can load external XMLTV files and map them extremely intelligently (Fuzzy-Matching) to streams missing a valid EPG-ID.

Within the `epg` block, you can define multiple XMLTV providers. Tuliprox aggregates these sources and assigns EPG data based on the priority and matching rules defined.

```yaml
epg:
  sources:
    - url: auto           # Auto generates XMLTV URL from Xtream credentials
      priority: -2        # Lower numbers = higher priority
      logo_override: true # Replaces channel logos with EPG logos
    - url: http://localhost/custom.xml
      priority: 0
  smart_match:
    enabled: true
    fuzzy_matching: true
    match_threshold: 80
    best_match_threshold: 99
    name_prefix: { suffix: "." }
    name_prefix_separator: [':', '|', '-']
    strip: ["3840p", "uhd", "fhd", "hd", "sd", "4k", "plus", "raw"]
    normalize_regex: '[^a-zA-Z0-9\-]'
```
**Sources Parameters:**

| Parameter | Type | Default | Technical Impact & Background |
| :--- | :--- | :--- | :--- |
| **`url`** | String | | The XMLTV endpoint. Use **`auto`** for Xtream inputs to automatically generate the provider's native XMLTV URL using your credentials. Supports local paths and remote `http(s)` links. |
| **`priority`** | Int | `0` | Determines the lookup order. **Lower numbers have higher priority.** For example, `-2` is processed before `0`, and `0` before `1`. Useful for prioritizing local or high-quality EPG data over generic provider data. |
| **`logo_override`**| Bool | `false` | If `true`, Tuliprox replaces the channel logos provided by the M3U/Xtream list with the icons found in the XMLTV file for that specific channel. |

**Smart Match Parameters:**

| Parameter | Type | Default | Technical Impact |
| :--- | :--- | :--- | :--- |
| `fuzzy_matching` | Bool | `false` | Fallback to phonetic and Jaro-Winkler similarity matching if exact ID match fails. |
| `match_threshold` | Int | `80` | Minimum percentage score (10-100) to accept a fuzzy match. |
| `best_match_threshold` | Int | `99` | Score at which Tuliprox stops searching for better matches and immediately accepts the EPG assignment. |
| `name_prefix` | Enum | `Ignore` | How to treat extracted country prefixes (`US`, `FR`). Options: `Ignore`, `Suffix` (appends to end), `Prefix` (appends to start). Example: `{ suffix: "." }` turns `US: HBO` into `hbo.us`. |
| `name_prefix_separator` | List | `[':', '\|', '-']` | Characters used by the provider to delimit the country prefix from the channel name. |
| `strip` | List | *(HD/4K tags)* | Terms aggressively stripped from the channel name before attempting to match against the XMLTV database. |
| `normalize_regex` | String | `[^a-zA-Z0-9\-]` | Regex pattern used to clean names. Default strips all non-alphanumeric characters (except dashes). |

**How Smart-Matching works:**
If a stream is missing the `tvg-id`, Tuliprox tries to map the channel name to the XMLTV file.
If a channel is named `US: HBO HD 4K`, Tuliprox uses the `name_prefix_separator` logic. It splits at `:`, recognizes `US`
as a country code, strips "4K" and "HD", cleans the string to "hbo", and appends the `name_prefix.suffix` (`.`) ➔ The EPG
Fuzzy-Matching (using Double Metaphone phonetic encoding) now actively searches for the ID `hbo.us` in the XMLTV file!

---

### 2.4 Provider Aliases (`aliases` & `batch://`)

Tuliprox allows you to pool multiple subscriptions from the same provider into a single logical source. By merging these "aliases," Tuliprox tracks connection availability across all accounts, ensuring that if one subscription is at its limit, the next available connection from the pool is used.

#### Defining Aliases in YAML
Aliases are ideal for a small number of fixed accounts. Note that in YAML, `max_connections: 0` signifies "unlimited," which is the default setting.

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
        max_connections: 2
```
**Result:** Tuliprox treats this as a single provider source with a total pool of 1 + 2 = **3** concurrent connections.

---

#### Batch CSV Offloading (`batch://`)
For managing dozens or hundreds of accounts, Tuliprox supports offloading alias definitions to local CSV files using the `batch://` scheme.

| Scheme | Description |
| :--- | :--- |
| `batch://./file.csv` | Relative path to the CSV file. |
| `batch:///path/file.csv` | Absolute path to the CSV file. |

> **Note:** Batch inputs only support local filesystem paths. Schemes like `http(s)://`, `file://`, or `provider://` are rejected for batch URL definitions. If an input `url` starts with `batch://`, Tuliprox automatically sets the type to `xtream_batch` or `m3u_batch`.



---

#### Batch CSV Formats

Batch files use a semicolon (`;`) as a separator. Unlike standard YAML config, the default for `max_connections` in CSV files is **1**.

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
#name;username;password;url;max_connections;priority;exp_date
my_provider_1;user1;password1;[http://p1.com:80](http://p1.com:80);1;0;2028-11-23 12:34:23
my_provider_2;user2;password2;[http://p2.com:8080](http://p2.com:8080);1;1;1732365263
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
#url;max_connections;priority
[http://p1.com/get.php?username=u1&password=p1;1;0](http://p1.com/get.php?username=u1&password=p1;1;0)
[http://p2.com/get.php?username=u2&password=p2;1;5](http://p2.com/get.php?username=u2&password=p2;1;5)
```

---

#### Field Specifications

| Parameter | Technical Impact & Details |
| :--- | :--- |
| **`name`** | **Crucial:** The first valid CSV row is automatically renamed to the `name` defined in the YAML `input` block. This ensures stable Playlist UUIDs and consistent channel numbering across updates. |
| **`max_connections`**| Defines allowed concurrent streams. Default in CSV is **1**. |
| **`priority`** | Lower numbers = higher priority. `0` is higher than `1`. Negative numbers (e.g., `-1`) are allowed for top-tier priority. Items with the lowest values are processed first. |
| **`exp_date`** | Account expiration. Supports "YYYY-MM-DD HH:MM:SS" (e.g., `2028-11-30 12:00:00`) or Unix timestamps (seconds). Used for auto-cleanup or Panel API sync. |

---

#### Supported URL Schemes (General Inputs)
Beyond batches, the standard `inputs[].url` field supports:
* `http(s)://`: Remote provider endpoints.
* `file://`: Local playlist files.
* `provider://<name>/`: Resolves via internal `provider` definitions (Failover/Rotation).

---
### 2.3 Provider Panel API (`panel_api`)

Tuliprox can optionally interface with a provider's reseller panel API to automate account lifecycle management. This allows the system to fetch credit balances, sync expiration dates, and automatically provision or renew alias accounts based on demand.

> **Important:** Panel API accounts are managed as individual connections/aliases. Tuliprox does **not** assume unlimited provider access; each alias consumes a slot or credit according to your provider's rules.

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
        account_info:         # Executed on boot/update to fetch credits
          - { key: action, value: account_info }
          - { key: api_key, value: auto }
        client_info:          # Mandatory for syncing exp_date
          - { key: action, value: client_info }
          - { key: username, value: auto }
          - { key: password, value: auto }
          - { key: api_key, value: auto }
        client_new:           # Create new account (type: m3u only)
          - { key: action, value: new }
          - { key: type, value: m3u }
          - { key: sub, value: '1' }
          - { key: api_key, value: auto }
        client_renew:         # Renew existing account (type: m3u only)
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

| Block / Parameter | Type | Default | Technical Impact & Background |
| :--- | :--- | :--- | :--- |
| `url` | String | | The base endpoint for the provider's reseller API. |
| `api_key` | String | | Your reseller administrative key. |
| **`alias_pool`** | Object | | Controls the lifecycle of active aliases. |
| ↳ `size.min` | Mixed | `1` | Min accounts to keep. `auto` uses the count of enabled Tuliprox users (Active/Trial, not expired) mapped to this input's targets. |
| ↳ `size.max` | Mixed | `1` | Upper bound for aliases. If `auto`, checks are triggered upon user add/update. |
| ↳ `remove_expired`| Bool | `false` | If `true`, removes expired aliases from `source.yml` or CSV. Root input is never removed. |
| **`provisioning`** | Object | | Verification and renewal logic. |
| ↳ `offset` | String | `None` | Pre-expiry window. If `now + offset > exp_date`, Tuliprox fires `client_renew` (falls back to `client_new`). |
| ↳ `timeout_sec` | Int | `65` | Max wait time for probing a new account before continuing boot/update. |
| ↳ `method` | Enum | `HEAD` | HTTP method for probes (`HEAD`, `GET`, `POST`). |
| ↳ `cooldown_sec` | Int | `0` | Extra wait time after a successful probe to mitigate 5XX errors during provider provisioning. |

---
#### Runtime Logic & Dynamic Values (`auto`)

The keyword `auto` acts as a placeholder for Tuliprox to inject runtime values dynamically into query parameters:
* **`api_key: auto`**: Replaced by `panel_api.api_key`.
* **`username / password: auto`**: Replaced by the specific credentials of the account being queried, renewed, or probed.

#### Response Evaluation & Fallback Logic
Tuliprox processes all Panel API responses as JSON and strictly requires `status: true`.

* **`account_info`**: Extracts the `credits` field and persists it. Uses root input credentials if `auto` is specified.
* **`client_info`**: Syncs the `expire` field, normalizing the timestamp/date to UTC.
* **`client_new`**: Attempts to extract `username` and `password` directly. 
    * **Fallback:** If fields are missing, Tuliprox parses a `url` field within the JSON response to extract credentials from the query string.
    * Failure to derive credentials results in a failed operation and no alias persistence.
* **`client_renew`**: Updates the expiration date without modifying existing credentials.
* **`client_adult_content`**: Optionally executed after `client_new` or `client_renew` to toggle adult content settings on the provider side. Requires `status: true` for success.
### Staged Inputs (`staged`)

Merge a perfectly maintained M3U file (e.g. from GitHub) for Live-TV with your Xtream Provider for VOD into a *single provider*
in Tuliprox!

**Background:** You buy a Premium Xtream account for VODs. However, the Live-TV section of this provider is terribly sorted.
But you have a perfectly maintained M3U file (e.g. found on a GitHub repository) for Live-TV.
With `staged`, you can logically merge these physical sources into a *single provider* in Tuliprox!

**Example:**

```yaml
    staged:
      enabled: true
      type: m3u
      url: https://github.com/m3u_list...
      live_source: staged
      vod_source: input
      series_source: skip
```
**Cluster Selection:** You can decide per section (`live`, `vod`, `series`) where the data originates.
In this example, Tuliprox pulls `live` from the m3u file on Github url and uses it for Live  (Staged source), but continues to use your Xtream input for VOD.

| Parameter | Options | Default | Description |
| :--- | :--- | :--- | :--- |
| `enabled` | Bool | `false` | Enables the hybrid staged architecture. |
| `live_source` | `staged` / `input` / `skip` | `staged` | Source for the Live-TV section. |
| `vod_source` | `staged` / `input` / `skip` | `staged` | Source for the VOD section. |
| `series_source`| `staged` / `input` / `skip` | `staged` | Source for the Series section. |

> **Note:** If the staged `type` is `m3u`, only `live_source: staged` is valid. M3U sources cannot provide Xtream-structured VOD or Series clusters.

---


## 3. Routing & Targets (`sources`)

This block links your inputs to specific output targets and applies transformation filters.
The Target defines the final list your clients download. Under `sources:` you link Targets with one or multiple `inputs`.

```yaml
sources:
  - inputs:
      - my_provider
    targets:
      - name: my_target
        output: []
        filter: 'Group ~ ".*"'
        rename:[]
        sort: {}
        mapping: []
        favourites: []
        watch:[]
```

### Target Parameters

| Parameter | Type | Required | Default | Technical Impact & Background |
| :--- | :--- | :---: | :--- | :--- |
| `name` | String | Yes | | Unique Target name, appears in the delivery URL (e.g., `http://host/get.php?username=X&password=Y` delivers the target assigned to this user). |
| `enabled` | Bool | No | `true` | Skips this target during building. |
| `filter` | String | Yes | | Your global filter DSL. Allows operators like `NOT`, `AND`, `OR`. Example: `(!TEMPLATE_TRASH!) AND Type = live`. |
| `processing_order` | Enum | No | `frm` | Execution order: **F**ilter, **R**ename, **M**ap. With `rmf`, it renames first, then maps, then filters. |
| `rename` | List | No | | Simple Regex Search & Replace on specific fields (e.g., `@Group`). |
| `mapping` | List | No | | References IDs from `mapping.yml` for deep DSL logic. |
| `sort` | Object | No | | Sorting logic with Regex Sequences and Orders (`asc`, `desc`). |
| `favourites` | List | No | | Duplicates final channels into a named Fav-group after all transformations. |
| `watch` | List | No | | Regex on group names. If channels in these groups change during an update, Tuliprox generates a Messaging-Event ("Channels added/removed"). |
| `use_memory_cache` | Bool | No | `false` | Puts the entire compiled target playlist into RAM. Extreme speed advantages during M3U download by clients, but costs system memory. |

---

### Output Formats (`output`)

A Target can be exported to multiple formats simultaneously. Filter logic applies globally, but each output formats the result
differently.

**1. `xtream`:**

```yaml
output:
  - type: xtream
    skip_live_direct_source: true
    update_strategy: instant
    trakt:
      api: { api_key: "XXX", version: "2", url: "https://api.trakt.tv" }
      lists:
        - { user: "gary", list_slug: "latest-tv", category_name: "Trending TV", content_type: series, fuzzy_match_threshold: 80 }
```

* `skip_live_direct_source`: Forces players to use Tuliprox's Xtream logic (Reverse Proxy/Redirect) instead of calling the
  provider's direct bypass URL.
* `update_strategy`: `instant` writes changes to disk immediately. `bundled` queues updates to reduce Disk I/O.
* `trakt`: **Deep-Dive:** Tuliprox queries lists from Trakt.tv and searches your playlist for matching movies using Jaro-Winkler
  fuzzy logic. If it finds hits, it creates a virtual VOD category in Xtream (e.g., "Trending TV") and copies the movies there!

**2. `m3u`:**

```yaml
output:
  - type: m3u
    filename: custom_playlist.m3u
    include_type_in_url: false
    mask_redirect_url: false
```

* `include_type_in_url`: If true, adds the stream type (`live`, `movie`, `series`) to the URL.
* `mask_redirect_url`: If true, uses URLs from `api_proxy.yml` for users in `redirect` proxy mode. Necessary if you have
  multiple providers and want to cycle/failover in redirect mode without exposing the direct provider IP initially.

**3. `strm`:**

```yaml
output:
  - type: strm
    directory: /media/strm
    style: plex
    flat: true
    add_quality_to_filename: true
    cleanup: true
    strm_props:["#KODIPROP:seekable=true", "#KODIPROP:inputstream=inputstream.ffmpeg"]
```

Generates local `.strm` files for Emby, Plex, or Jellyfin.

| Parameter | Type | Default | Technical Impact |
| :--- | :--- | :--- | :--- |
| `directory` | String | | **Mandatory.** The output folder on your local disk where `.strm` files will be written. |
| `style` | Enum | `kodi` | Naming convention styles for scrapers. Options: `kodi`, `plex`, `emby`, `jellyfin`. (E.g., Plex style outputs: `Movie Name (Year) {tmdb-ID}/Movie Name (Year).strm`). |
| `flat` | Bool | `false` | If true, creates a flat directory structure, skipping category/group subfolders. |
| `cleanup` | Bool | `false` | **Warning:** Deletes orphaned files from the directory that have been removed from the Target. Do not point this directly at your actual media files folder! |
| `underscore_whitespace` | Bool | `false` | Replaces all whitespaces in paths and filenames with `_`. |
| `add_quality_to_filename` | Bool | `false` | Appends tags like `[2160p 4K HEVC HDR]` to the filename. (Requires `ffprobe` probing enabled on the Input!). |
| `strm_props` | List | | Properties injected into `.strm` files to configure Kodi's internal player (e.g., `#KODIPROP:seekable=true`). |

**4. `hdhomerun`:**

```yaml
output:
  - type: hdhomerun
    device: hdhr1 # Must match a device name from config.yml
    username: local_user # Must match a user from api-proxy.yml
    use_output: xtream # m3u or xtream
```

Physically binds this Target to the simulated hardware tuner from `config.yml`. The `username` dictates which user's connection
limits and reverse proxy rules apply when Plex streams from the virtual antenna.

#### Favourites (`favourites`)

You can duplicate final, transformed channels into dedicated Favorite groups *after* all filtering and mapping is complete.

```yaml
favourites:
  - cluster: series
    group: "My Favourites"
    filter: 'Name ~ "Cinema"'
    match_as_ascii: true
```

* **`match_as_ascii`**: (Bool) Normalizes accented characters during the filter match (allowing "Cinema" to match "Cinéma").
  The final output channel name retains its original accents.

#### Watch (`watch`)

Regex on group names. If channels in these groups change during an update, Tuliprox generates a Messaging-Event ("Channels
added/removed").
