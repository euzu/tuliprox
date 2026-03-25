# 🔌 Pillar 2: `source.yml` (Inputs, Panel API & Targets)
The `source.yml` serves as the central orchestration hub for all data flows within Tuliprox. It defines the lifecycle of a stream—from the upstream provider to the end-user device—through three primary architectural layers:

* **`providers` (Resilience Layer):** Defines backend endpoints and failover logic. Use this to implement intelligent URL rotation and ensure high availability across multiple mirrors.
* **`inputs` (Ingestion Layer):** Manages upstream data sources. This layer handles credential management, connection pooling via **Aliases**, and automated account lifecycle management through **Panel API** integration.
* **`sources` & `targets` (Egress Layer):** The final mapping stage where ingested data is filtered, transformed, and routed to specific **Targets** (M3U, Xtream, Strm or HDHomeRun) for consumption by end devices.
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

### 1.1 DNS Resolved IP Persistence
Resolved IPs are persisted to `{storage_dir}/provider_dns_resolved.json` (not to `source.yml`). This file is written atomically after each DNS refresh cycle and read at startup to seed DNS caches before the background resolver completes its first cycle. On config hot-reloads, DNS caches are carried over from previous provider instances so that resolved IPs are available immediately.

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
    aliases: []
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
| `cache_duration` | String | No | `0` | **Crucial:** Determines how often Tuliprox actually downloads the raw list from the provider. At `1d` (1 day), Tuliprox serves from its local `.db` for 24 hours, even if you trigger hourly updates. This heavily protects against provider bans! Supported units are `s`, `m`, `h`, and `d`. If `cache_duration` is set, the cached provider playlist stored on disk is reused for subsequent updates instead of downloading it again. |
| `persist` | String | No | | Optional path template (e.g., `./playlist_{}.m3u`) to permanently store the downloaded raw provider list locally on your disk. The `{}` in the filename is filled with the current timestamp. For `m3u` use a full filename. For `xtream` use a prefix like `./playlist_`. |
| `method` | Enum | No | `GET` | HTTP Request method for playlist downloads (`GET` or `POST`). |
| `exp_date` | Mixed | No | | Expiration date as `"YYYY-MM-DD HH:MM:SS"` or Unix timestamp. Used for status tracking and Panel API logic. |
| `headers` | Dict | No | | Custom HTTP headers for the download (e.g., `User-Agent: My-Player`). |
| `epg` | Object | No | | Allows mapping of external XMLTV files (see [below](#input-subsections-object-keys)). |
| `aliases` | List | No | | Connection pooling / Sub-accounts (see [below](#input-subsections-object-keys)). |
| `staged` | Object | No | | Hybrid architecture feature (see [below](#input-subsections-object-keys)). |
| `panel_api` | Object | No | | Automated reseller account generation (see [below](#input-subsections-object-keys)). |


#### Input URL Schemes (`inputs[].url`)

Tuliprox utilizes a flexible URI-based system to define where input data originates. Depending on the prefix used, the engine switches between remote downloads, local file access, or internal failover logic.

| Scheme | Target Type | Technical Impact & Background |
| :--- | :--- | :--- |
| **`http(s)://`** | Remote Server | Standard method for downloading playlists from provider endpoints. |
| **`file://`** | Local Storage | Reads a playlist directly from the host filesystem. Useful for manual backups or pre-processed files. |
| **`provider://`** | Failover System | Resolves the URL via internal `provider` definitions. **Pro-Tip:** Use this to implement automatic rotation or failover between multiple mirrors/gateways of the same provider. |
| **`batch://`** | CSV File | Dedicated scheme for bulk alias management. Points to a local `;` separated CSV file (e.g., `batch://./aliases.csv`). |

**Additional Notes**

* **Automatic Type Conversion:** If the input `type` is set to `m3u` or `xtream` but the `url` starts with the `batch://` prefix, Tuliprox automatically upgrades the input to `m3u_batch` or `xtream_batch` respectively.
* **Batch Constraints:** For `m3u_batch` and `xtream_batch`, only **local** CSV sources are permitted. You must use either the `batch://` scheme or a plain absolute/relative filesystem path.
* **Protocol Restrictions:** To ensure stability in batch processing, URI schemes such as `provider://`, `http(s)://`, or `file://` are strictly rejected when used within a batch context.

---

### Input Subsections (Object Keys)

| Block | Description | Link |
| :--- | :--- | :--- |
| `headers` | Custom HTTP request headers for playlist and EPG downloads. | [See Headers](#21-headers-headers) |
| `options` | Behavior controls for metadata resolution, stream probing, and skip logic. | [See Options](#22-input-options-options) |
| `epg` | XMLTV source management and Smart Match fuzzy logic settings. | [See EPG](#23-epg-assignment--smart-match-epg) |
| `aliases` | Connection pooling for multiple subscriptions from the same provider. | [See Aliases](#24-provider-aliases-aliases--batch) |
| `staged` | Hybrid architecture for sideloading external playlists into an input. | [See Staged](#25-staged-sources-staged) |
| `panel_api` | Automated reseller panel integration (provisioning/renewal). | [See Panel API](#26-provider-panel-api-panel_api) |
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

> **Note:** For `resolve_vod` and `resolve_series`, data is cached per input and only new or changed entries are updated.

---

### 2.3 EPG Assignment & Smart Match (`epg`)

Tuliprox can load external XMLTV files and map them intelligently using advanced fuzzy matching to streams that are missing a valid EPG ID. Within the `epg` block, you can define multiple XMLTV providers. Tuliprox aggregates these sources and assigns EPG data based on priority and matching rules.

#### Example Configuration
```yaml
epg:
  sources:
    - url: "auto"           # Automatically generated provider URL
      priority: -2          # High priority
      logo_override: true   # Replaces provider logos with EPG icons
    - url: "http://localhost:3001/xmltv.php?epg_id=1"
      priority: -1
    - url: "http://localhost:3001/xmltv.php?epg_id=2"
      priority: 3
    - url: "http://localhost:3001/xmltv.php?epg_id=3"
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
#### EPG Source Parameters (`sources`)
| Parameter | Type | Required | Default | Technical Impact & Background |
| :--- | :--- | :---: | :--- | :--- |
| **`url`** | String | Yes | | The XMLTV endpoint. Use **`auto`** for Xtream inputs to automatically generate the native XMLTV URL using your credentials. Supports local paths and `http(s)` links. |
| **`priority`** | Int | No | `0` | Determines the lookup order. **Lower numbers have higher priority.** For example, `-2` is processed before `0`. Use negative numbers for primary sources. |
| **`logo_override`**| Bool | No | `false` | If set to `true`, channel logos from the provider are replaced by the icons found in the XMLTV file. |
#### Smart Match Parameters (`smart_match`)
The fuzzy matching logic attempts to "guess" the EPG ID by generating search keys based on the channel name.

| Parameter | Type | Default | Technical Impact |
| :--- | :--- | :--- | :--- |
| `enabled` | Bool | `false` | Activates the Smart Match engine for streams without a fixed `tvg-id`. |
| `fuzzy_matching` | Bool | `false` | Fallback to phonetic and Jaro-Winkler similarity matching if exact ID match fails. |
| `match_threshold` | Int | `80` | Minimum similarity score (10-100) required to accept a fuzzy match. |
| `best_match_threshold`| Int | `99` | Score at which Tuliprox stops searching and immediately accepts the EPG assignment. |
| `name_prefix` | Object | `ignore` | Options: `ignore`, `suffix`, `prefix`. For `suffix`/`prefix`, a concat string (e.g., `{ suffix: "." }`) is required. |
| `name_prefix_separator`| List | `[':', '\|', '-']` | Characters used by providers to delimit country codes (e.g., `US:`, `FR\|`). |
| `strip` | List | *(HD/4K tags)* | Default: `["3840p", "uhd", "fhd", "hd", "sd", "4k", "plus", "raw"]`. Terms stripped before matching. |
| `normalize_regex` | String | `[^a-zA-Z0-9\-]` | Default pattern to strip non-alphanumeric characters (except dashes) for cleaner matching. |

#### How Smart-Matching works
If a stream is missing the `tvg-id`, Tuliprox performs the following steps:
1. **Normalization:** The channel name (e.g., `US: HBO HD 4K`) is processed.
2. **Prefix Extraction:** Using `name_prefix_separator`, Tuliprox identifies `:` and splits the name. It recognizes `US` as the country prefix.
3. **Cleaning:** It strips terms defined in `strip` ("4K", "HD") and applies the `normalize_regex`. The core name becomes `hbo`.
4. **Reconstruction:** Using `name_prefix.suffix` (`.`), the country code is appended to the name. The target search key becomes `hbo.us`.
5. **Phonetic Matching:** The engine uses **Double Metaphone** phonetic encoding to search for the ID `hbo.us` in the aggregated XMLTV database.

> **Note:** Lower `match_threshold` values increase the chance of EPG assignment but may lead to incorrect matches for channels with very similar names.
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
#### Field Specifications

| Parameter | Technical Impact & Details |
| :--- | :--- |
| **`name`** | **Crucial:** The first alias is automatically renamed with the `name` from the input definition (e.g., `my_provider_1` gets `my_provider`). This is necessary for stable playlist UUID generation and consistent channel numbering across updates. |
| **`max_connections`**| Defines allowed concurrent streams. Default in CSV is **1**. |
| **`priority`** | Lower numbers = higher priority. `0` is higher than `1`. Negative numbers (e.g., `-1`) are allowed for top-tier priority. Items with the lowest values are processed first. |
| **`exp_date`** | Account expiration. Supports "YYYY-MM-DD HH:MM:SS" (e.g., `2028-11-30 12:00:00`) or Unix timestamps (seconds). Used for auto-cleanup or Panel API sync. |

---
### 2.5 Staged Sources (`staged`)

The **"Staged Source"** acts as a structural template for an input's playlist. Instead of manually mapping and sorting channels within Tuliprox, this feature allows you to "inject" a pre-configured external playlist (e.g., from a GitHub repository or a third-party playlist editor) to define the layout while keeping the actual delivery linked to your main provider.

**The Hybrid Mechanism:**
* **Structure Provider (Staged):** The external source dictates the channel order, selection, and group naming during the update process. It is used **temporarily** only for updating the playlist structure.
* **Data Provider (Main Input):** Regular queries (streaming, authentication, EPG mapping, and metadata fetching) still go through the main provider defined in the root of the input. 

**Use Case:**
If you have a perfectly maintained playlist in another online tool or want to bypass Tuliprox's internal mapping for a specific provider, you can "plug in" that playlist as a staged source. It won’t replace your main provider; it simply acts as a blueprint for the update cycle.

#### Configuration Example (Xtream Main + Staged Xtream)

In this setup, Tuliprox uses an external provider's structure for Live-TV, but keeps the local provider's data for VOD and ignores the series section entirely.

```yaml
inputs:
  - name: provider_main
    type: xtream
    url: [http://provider-a.example:8080](http://provider-a.example:8080)
    username: main_user
    password: main_pass
    options:
      xtream_skip_live: false
      xtream_skip_vod: false
      xtream_skip_series: false
    staged:
      enabled: true
      type: xtream
      url: [http://provider-b.example:8080](http://provider-b.example:8080)
      username: staged_user
      password: staged_pass
      live_source: staged
      vod_source: input
      series_source: skip
```
#### Parameters

| Parameter | Type | Required | Default | Technical Impact & Background |
| :--- | :--- | :---: | :--- | :--- |
| `enabled` | Bool | No | `false` | Master switch for the hybrid staged architecture. |
| `type` | Enum | No | `m3u` | Format of the staged source. Allowed: `m3u`, `xtream`. |
| `url` | String | Yes | | Download URL (HTTP/HTTPS) or local file path (can be gzip). For `xtream`, use the base hostname:port. |
| `username` / `password` | String | Yes | | Mandatory only if staged `type` is `xtream`. |
| `method` | Enum | No | `GET` | HTTP request method (`GET` or `POST`). |
| `headers` | Dict | No | | Custom HTTP headers for the staged download. |
| `live_source` | Enum | No | `staged` | Source for the Live-TV section: `staged`, `input`, or `skip`. |
| `vod_source` | Enum | No | *(Varies)*| Source for the VOD section: `staged`, `input`, or `skip`. |
| `series_source` | Enum | No | *(Varies)*| Source for the Series section: `staged`, `input`, or `skip`. |

#### Staged Cluster Source Behavior & Logic

The selection of data sources for different clusters (Live, VOD, Series) follows specific validation and priority rules:

1.  **Global Skip Priority:** If `input.options.xtream_skip_live|vod|series` is set to `true`, that section is skipped entirely, regardless of the `staged` settings.
2.  **Xtream Main Inputs:** * At least one cluster (`live`, `vod`, or `series`) must be set to `staged`. If all effective values resolve to `input` or `skip`, the configuration is considered invalid.
    * **Default Behavior:** If staged `type` is `xtream`, all clusters default to `staged`.
3.  **M3U Staged Sources:**
    * M3U sources cannot provide structured VOD or Series metadata for Xtream clusters. Therefore, `vod_source: staged` and `series_source: staged` are **invalid** if the staged type is `m3u`.
    * **Default Behavior:** If staged `type` is `m3u`, it defaults to `live_source: staged`, `vod_source: input`, and `series_source: input`.
4.  **M3U Main Inputs:** If the primary input `type` is `m3u`, the cluster source fields (`live_source`, etc.) are ignored.

#### File Persistence (`persist`)
When using `persist` to save the staged data, follow these filename conventions:
* **For `m3u`:** Use a full filename template like `./staged_playlist_{}.m3u`.
* **For `xtream`:** Use a prefix template like `./staged_playlist_`.

---
### 2.6 Provider Panel API (`panel_api`)

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
| ↳ `size.min` | Mixed | `1` | Min accounts to keep. `number` or `auto`. If `auto`, it uses the count of enabled Tuliprox users (Active/Trial, not expired) mapped to this input's targets. |
| ↳ `size.max` | Mixed | `1` | Upper bound for aliases. If `auto`, checks are triggered upon user add/update. |
| ↳ `remove_expired`| Bool | `false` | If `true`, removes expired accounts from `source.yml` or batch CSVs during boot/update. (The root input is never removed). |
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

---
## 3. Routing & Targets (`sources`)

This block links your inputs to one or more output targets and defines how Tuliprox transforms, filters, sorts, and exports the resulting playlist.  
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
        rename: []
        mapping: []
        sort: {}
        options:
          ignore_logo: false
          share_live_streams: false
          remove_duplicates: false
        output:
          - type: xtream
        favourites: []
        watch: []
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

| Parameter | Type | Required | Default | Technical Impact & Background |
| :--- | :--- | :---: | :--- | :--- |
| `enabled` | Bool | No | `true` | If set to `false`, Tuliprox skips building this target during normal processing. This reduces CPU, disk, and upstream workload, but the target can still be selected explicitly via CLI target execution if matched by `-t`. |
| `name` | String | No | `default` | Logical target name. If not `default`, it must be unique. Unique names are important for selective execution (`-t <target_name>`) and for clearly separating output identities in Tuliprox's processing pipeline. |
| `processing_order` | Enum | No | `frm` | Defines execution order for **F**ilter, **R**ename, and **M**ap. This directly changes which intermediate state downstream steps operate on and can therefore materially alter the final playlist result. |
| `filter` | String | Yes | | Global filter DSL expression for the target. This determines which entries survive into the final target after the selected processing order has been applied. |
| `rename` | List | No | | Regex-based transformations applied to selected fields. This is commonly used to normalize channel/group labels before sorting, mapping, or export. |
| `mapping` | List | No | | References mapping IDs from `mapping.yml` for advanced transformation logic. This is where deep structural rewriting and metadata normalization can be applied. |
| `sort` | Object | No | | Defines ordering for groups and channels after transformations. This affects the final playlist structure seen by clients and can significantly improve navigation quality in IPTV players. |
| `options` | Object | No | | Target-level behavior switches such as logo suppression, duplicate removal, and shared live-stream handling. These options influence memory usage, playlist cleanliness, and reverse-proxy behavior. |
| `output` | List | Yes | | Mandatory list of output formats. A single target can generate multiple output representations (e.g., `xtream`, `m3u`, `strm`, `hdhomerun`) from the same transformed result set. |
| `favourites` | List | No | | Duplicates final transformed channels into dedicated favorite groups after processing is complete. This adds curated views without changing the original group structure. |
| `watch` | List | No | | Defines watched group patterns. If matching groups change during updates, Tuliprox emits Messaging events so operational changes become observable automatically. |
| `use_memory_cache` | Bool | No | `false` | If enabled, the final compiled playlist is cached in RAM. This reduces disk access and improves delivery speed, especially for M3U downloads, but increases memory consumption. |

---

### 3.2.1 `processing_order`

The processing order defines how Tuliprox applies:

- **F**ilter
- **R**ename
- **M**ap

Valid values are:

- `frm` (default)
- `fmr`
- `rfm`
- `rmf`
- `mfr`
- `mrf`

> **Note:** The selected processing order can change the final result significantly. For example, if renaming occurs before filtering, the filter must match the renamed state rather than the original source value.

---

> **Note:** The following sections are intended to be inserted **between** `### 3.2.4 Output Formats (\`output\`)` and the existing `Favourites` / `Watch` subsections.
> If you keep strict numbering, the existing `Favourites` and `Watch` sections should be renumbered accordingly.

### 3.2.2 `filter`

The target-level `filter` is a string-based expression using Tuliprox's filter DSL.  
It defines which entries remain in the final target after the selected processing stages have been applied.

You can define complex strings or regex patterns exactly once in [template.yml](./configuration/template.md) and call them by wrapping the template name in exclamation marks: `!MACRO_NAME!`. For less verbose expression definitions, inline filter definitions are also supported.

Tuliprox supports the following filter expression types:

- Use `NOT` for exclusion logic
- Use `AND` / `OR` for boolean combinations
- Type Comparison: `Type = vod` or `Type = live` or `Type = series`
- Regular expression comparison: `([fieldanme]) ~ "regexp"` <br>
  The [fieldanme] can be `Group`, `Title`, `Name`, `Caption`, `Url`, `Genre`, `Input` or `Type`.

>**Note:** 
* If you use special characters like `+ | [ ] ( )` inside the filter expression you must escape them correctly with backslashes.
* When testing expressions externally, e.g. [regex101.com](https://regex101.com/), select the **Rust** flavor. This helps avoid mismatches between development-time testing and Tuliprox runtime behavior.

> **⚠️ Warning:** Filter expressions are evaluated using Rust-style regex behavior. Unsupported features such as lookarounds and backreferences are not available, so patterns copied from PCRE-based tools may need adjustment.


#### Example Filter

```yaml
targets:
  - name: regional_mix
    filter: '((Group ~ "^DE.*") AND (NOT Title ~ ".*Shopping.*")) OR (Group ~ "^AU.*")'
    output:
      - type: m3u
```

This example keeps:

- entries from groups starting with `DE`, except titles containing `Shopping`
- all entries from groups starting with `AU`

---

### 3.2.3 `rename`

The `rename` block is a list of rename rules applied to selected fields.  
Each rule performs regex-based search and replace using capture groups where needed.
#### Rename Parameters

| Parameter | Type | Required | Default | Technical Impact & Background |
| :--- | :--- | :---: | :--- | :--- |
| `field` | Enum | Yes | | Field to transform, can be  `group`, `title`, `name`, `caption`  or `url`. This determines which part of the playlist entry Tuliprox rewrites before later stages such as sorting or final export. |
| `pattern` | String (Regex) | Yes | | Regular expression used to match the current value of the selected field. This enables structural normalization of inconsistent source naming schemes. |
| `new_name` | String | Yes | | Replacement string. It can reference regex capture groups via `$1`, `$2`, and so on. This allows Tuliprox to preserve selected original content while reformatting labels. |

#### Rename Example
Eample:
```yaml
rename:
  - field: group
    pattern: '^DE(.*)'
    new_name: '1. DE$1'
```

In above example, every group beginning with `DE` is renamed to start with `1.`, for example:

- `DE Sports` → `1. DE Sports`
- `DE Movies` → `1. DE Movies`

This can be useful for players that ignore provider order and perform their own alphabetical sorting.

> **Note:** The effective value that `rename` sees depends on `processing_order`. If mapping runs before renaming, your rename pattern must match the already mapped value rather than the original source value.

---

### 3.2.4 `mapping`

The `mapping` block references a list of mapping identifiers (IDs) defined in your [mapping files](./mapping-dsl.md) (default: `mapping.yml`).

```yaml
mapping:
  - map_cleanup
  - map_regional_groups
  - map_vod_enrichment
```
#### Mapping Parameters

| Parameter | Type | Required | Default | Technical Impact & Background |
| :--- | :--- | :---: | :--- | :--- |
| `mapping` | List of Strings | No | | Ordered list of mapping IDs to apply. Each referenced mapping can perform deep transformations on the playlist structure, metadata, grouping, or labels, making this one of the most powerful target-level processing stages in Tuliprox. |

To define a new mapping IDs see details in chapter [Mapper DSL & Logic](./mapping-dsl.md).
### 3.2.5 `sort`

The `sort` block defines ordering rules for groups and channels.

It has the following top-level attributes:

- `match_as_ascii` _optional_, default `false`
- `rules`

#### Sort Parameters

| Parameter | Type | Required | Default | Technical Impact & Background |
| :--- | :--- | :---: | :--- | :--- |
| `match_as_ascii` | Bool | No | `false` | If enabled, Tuliprox normalizes accented characters during sorting comparisons. This improves deterministic ordering across multilingual playlists without modifying the original visible channel names. |
| `rules` | List | Yes | | Ordered list of sort rules. Each rule is evaluated against the playlist after transformation, and directly shapes the browsing order clients see in the final target. |

#### `rules`

Each sort rule supports the following entries:

| Parameter | Type | Required | Default | Technical Impact & Background |
| :--- | :--- | :---: | :--- | :--- |
| `target` | Enum | Yes | | Defines whether the rule sorts `group` or `channel` entries. This changes whether Tuliprox reorders category containers or items within those categories. |
| `field` | String | Yes | | Sort field. For `channel`: `title`, `name`, `caption`, or `url`. For `group`: `group`. This determines which final-state value Tuliprox uses for ordering. |
| `filter` | String | Yes | | Filter expression defining which entries the rule applies to. This makes it possible to sort only selected subsets of the playlist instead of the entire target uniformly. |
| `order` | Enum | Yes | | `asc`, `desc`, or `none`. `none` preserves source order for matched entries and is useful when provider order should remain untouched. |
| `sequence` | List | No | | Ordered regex list used for index-based sorting. When present, Tuliprox prioritizes regex sequence position over `order`, enabling explicit semantic ordering such as quality tiers or curated group precedence. |

>**Note:** Sort rules must be written with the configured `processing_order` in mind, because sorting operates on the transformed state that exists at that point in the pipeline.
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
>**Note:** 
>* The numeric suffix defines priority. c1 > c2 > c3

This allows Tuliprox to perform structured multi-level sorting based on extracted fragments of a channel title or label.

In the example above:
- groups are ordered according to the explicit `sequence`
- Channels within the `Freetv` group are first sorted by `quality` (as matched by the regexp sequence), and then by the `captured prefix`.

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
      share_live_streams: true
      remove_duplicates: false
```

#### Target Option Parameters

| Parameter | Type | Required | Default | Technical Impact & Background |
| :--- | :--- | :---: | :--- | :--- |
| `ignore_logo` | Bool | No | `false` | Ignores `tvg-logo` and `tvg-logo-small` attributes. This reduces downstream device-side logo caching and can keep generated M3U playlists leaner for clients with limited storage or poor cache invalidation behavior. |
| `share_live_streams` | Bool | No | `false` | Allows Tuliprox to share live stream connections in reverse proxy mode. This can reduce upstream provider connection usage when multiple clients watch the same channel, but it increases memory usage per shared channel. |
| `remove_duplicates` | Bool | No | `false` | Attempts to remove duplicate entries by `url`. This improves playlist cleanliness and reduces confusing duplicates in the client-facing output. |
| `force_redirect` | Bool | No | `false` | Optional redirect-related behavior switch. This influences how Tuliprox serves final stream delivery where redirect-style output handling is required by the deployment model. |

> **⚠️ Warning:** When `share_live_streams` is enabled, each shared channel consumes at least **12 MB** of memory, regardless of the number of connected clients.
> If the reverse-proxy buffer size is increased above `1024`, memory usage increases accordingly.  
> Example: with a buffer size of `2024`, each shared channel consumes at least **24 MB**.

---

### 3.2.7 Output Formats (`output`)

A target can be exported to multiple formats simultaneously. The target-level filter, rename, mapping, and sort logic are applied first, and each output then formats the result differently.

> **Note:** Output-specific filters are applied **after all transformations have completed**. Therefore, any filter inside an individual output block must refer to the **final playlist state**.

#### Output Block Parameters

Every output block contains at least:

| Parameter | Type | Required | Default | Technical Impact & Background |
| :--- | :--- | :---: | :--- | :--- |
| `type` | Enum | Yes | | Output format type. Supported values include `xtream`, `m3u`, `strm`, and `hdhomerun`. This determines how Tuliprox serializes and serves the final playlist to downstream consumers. |
| `filter` | String | No | | Optional output-level filter applied after all target transformations. This allows Tuliprox to derive specialized output subsets from the same target without duplicating upstream processing logic. |

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

| Parameter | Type | Required | Default | Technical Impact & Background |
| :--- | :--- | :---: | :--- | :--- |
| `type` | Enum | Yes | | Must be `xtream`. Generates an Xtream-compatible API output backed by Tuliprox's processed data model. |
| `skip_live_direct_source` | Bool | No | `true` | If `true`, Tuliprox ignores provider `direct_source` values for live content. This keeps playback under Tuliprox's delivery logic and avoids client behavior differences caused by bypass URLs. |
| `skip_video_direct_source` | Bool | No | `true` | If `true`, Tuliprox ignores provider `direct_source` values for movies/VOD. This improves consistency across clients that otherwise may bypass Tuliprox for video playback. |
| `skip_series_direct_source` | Bool | No | `true` | If `true`, Tuliprox ignores provider `direct_source` values for series entries. This ensures Tuliprox stays in control of series playback URL generation and proxy behavior. |
| `update_strategy` | Enum | No | `instant` | `instant` writes changes immediately, while `bundled` batches write operations. This directly trades off freshness versus disk I/O load during background metadata enrichment and output maintenance. |
| `trakt` | Object | No | | Trakt.tv integration block. Tuliprox can fetch Trakt lists, fuzzy-match them against playlist entries, and inject matched VOD or series entries into generated virtual categories. |
| `filter` | String | No | | Optional output-level filter for the Xtream export only. Useful when the same target should expose different subsets to different output formats. |

>**Note:** IPTV players vary in how they resolve streams: some use the direct-source attribute, while others reconstruct URLs  from server metadata. To ensure Tuliprox maintains control over the stream routing (Proxy/Redirect), the Direct Source Handling (skip_*_direct_source) attributes default to true.
> **⚠️ Warning:** Setting `skip_*_direct_source` to `false` forces the player to use the provider's original `direct-source` URL. This effectively **bypasses Tuliprox**, which will disable internal features like connection tracking, IP masking, and failover logic for those streams.

#### `trakt` Object in Xtream Output

Trakt.tv is an online platform for tracking, organizing, and discovering movies and TV shows.  
Tuliprox can query Trakt lists and match playlist entries using Jaro-Winkler-style fuzzy matching. Matching entries are then added to new virtual categories inside the Xtream output.

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
```

This configuration creates two additional virtual categories populated with matched entries from the configured Trakt lists.

##### Trakt Parameters

| Parameter | Type | Required | Default | Technical Impact & Background |
| :--- | :--- | :---: | :--- | :--- |
| `api.api_key` | String | Yes | | Trakt API key used for authenticated access. Without a valid key, Tuliprox cannot fetch remote list content. |
| `api.version` | String | No | `"2"` | API version header value. This ensures Tuliprox formats requests against the correct Trakt API version. |
| `api.url` | String | No | `https://api.trakt.tv` | Base API URL for Trakt requests. This defines the remote endpoint Tuliprox queries for list data. |
| `api.user_agent` | String | No | | Optional `User-Agent` used for Trakt API requests. This can help satisfy API gateway expectations or deployment-specific request policies. |
| `lists[].user` | String | Yes | | Trakt username owning the list. This identifies which account namespace Tuliprox fetches list data from. |
| `lists[].list_slug` | String | Yes | | Trakt list slug. Combined with `user`, this uniquely identifies the remote list to load. |
| `lists[].category_name` | String | Yes | | Name of the generated virtual category inside Tuliprox's Xtream output. This controls where matched entries appear to clients. |
| `lists[].content_type` | Enum | Yes | | `vod` or `series`. This determines which class of playlist entries Tuliprox will attempt to match and inject into the generated category. |
| `lists[].fuzzy_match_threshold` | Integer | No | | Fuzzy matching threshold for title matching. Higher values reduce false positives but may miss loosely matching items. |

---
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

| Parameter | Type | Required | Default | Technical Impact & Background |
| :--- | :--- | :---: | :--- | :--- |
| `type` | Enum | Yes | | Must be `m3u`. Generates a traditional playlist file suitable for IPTV players and related clients. |
| `filename` | String | No | | Optional custom output filename. This affects how Tuliprox writes or exposes the generated playlist artifact. |
| `include_type_in_url` | Bool | No | `false` | If enabled, Tuliprox adds the stream type (`live`, `movie`, `series`) into generated stream URLs. This can improve downstream routing clarity and compatibility with clients that distinguish path structure by media type. |
| `mask_redirect_url` | Bool | No | `false` | If enabled, Tuliprox uses URLs from `api_proxy.yml` for users operating in `redirect` proxy mode. This is important for multi-provider failover or cycling setups where exposing the provider URL directly would bypass Tuliprox's routing logic too early. |
| `filter` | String | No | | Optional M3U-only post-transformation filter. This allows M3U consumers to receive a narrower subset than other output formats derived from the same target. |

> **Note:** `mask_redirect_url` should be enabled if you use multiple providers and want Tuliprox to preserve redirect-mode routing and cycling behavior without exposing the direct upstream endpoint in the initial playlist URL.

---

### 3. Type `strm`

```yaml
output:
  - type: strm
    directory: /media/strm
    username: local_user
    style: plex
    flat: true
    cleanup: false
    underscore_whitespace: false
    add_quality_to_filename: true
    strm_props:
      - "#KODIPROP:seekable=true"
      - "#KODIPROP:inputstream=inputstream.ffmpeg"
    filter: 'Type = vod'
```

Generates local `.strm` files for Plex, Emby, Jellyfin, or Kodi-based library ingestion.

#### `strm` Parameters

| Parameter | Type | Required | Default | Technical Impact & Background |
| :--- | :--- | :---: | :--- | :--- |
| `type` | Enum | Yes | | Must be `strm`. Generates filesystem-based `.strm` references instead of a network playlist format. |
| `directory` | String | Yes | | Target directory where `.strm` files are written. This is the root Tuliprox manages for exported media stubs and must be chosen carefully to avoid overlap with real media directories. |
| `username` | String | No | | Optional username context used when generating stream references. This affects which user-specific URL or access context Tuliprox embeds into the exported `.strm` files. |
| `underscore_whitespace` | Bool | No | `false` | Replaces whitespace with `_` in paths and filenames. This improves compatibility with environments or scrapers that prefer filesystem-safe, normalized naming. |
| `cleanup` | Bool | No | `false` | If enabled, Tuliprox removes orphaned output files from the STRM directory. This keeps the export directory synchronized with the target, but can delete files if the directory points to an existing media folder. |
| `style` | Enum | Yes | | Naming convention for the output structure. Supported values: `kodi`, `plex`, `emby`, `jellyfin`. This affects scraper compatibility and how downstream media servers identify titles. |
| `flat` | Bool | No | `false` | If enabled, Tuliprox creates a flatter directory structure. This changes how categories and group information are represented on disk and can simplify some media-server imports. |
| `strm_props` | List | No | | Stream property lines inserted into `.strm` files, mainly for Kodi player behavior. This allows low-level playback hints to be embedded directly into generated files. |
| `add_quality_to_filename` | Bool | No | `false` | Appends detected media quality tags such as `[1080p 4K HEVC HDR]` to the filename. This improves visibility in library UIs but depends on prior probing/enrichment data being available. |
| `filter` | String | No | | Optional STRM-only output filter. Useful when only a subset of the target should be materialized as filesystem entries. |

#### Supported `style` Conventions

- **Kodi:** `Movie Name (Year) {tmdb=ID}/Movie Name (Year).strm`
- **Plex:** `Movie Name (Year) {tmdb-ID}/Movie Name (Year).strm`
- **Emby:** `Movie Name (Year) [tmdbid=ID]/Movie Name (Year).strm`
- **Jellyfin:** `Movie Name (Year) [tmdbid-ID]/Movie Name (Year).strm`

##### Kodi-Specific Behavior

If `style: kodi` is selected:

- `#KODIPROP:seekable=true|false` is added automatically
- if `strm_props` is not specified, Tuliprox additionally sets:
  - `#KODIPROP:inputstream=inputstream.ffmpeg`
  - `#KODIPROP:http-reconnect=true`

> **⚠️ Warning:** If `cleanup` is enabled, do **not** point `directory` at a real media library folder. Tuliprox may delete files that are no longer part of the generated target.

---

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

| Parameter | Type | Required | Default | Technical Impact & Background |
| :--- | :--- | :---: | :--- | :--- |
| `type` | Enum | Yes | | Must be `hdhomerun`. Exposes the target through Tuliprox's HDHomeRun emulation layer for tuner-style discovery by clients such as Plex or Jellyfin. |
| `device` | String | Yes | | Must match a device name defined in `config.yml`. This links the playlist target to a specific emulated tuner endpoint. |
| `username` | String | Yes | | Must match a user from `api-proxy.yml`. This determines which account context, access restrictions, and connection limits apply when clients consume the lineup through the tuner interface. |
| `use_output` | Enum | No | | Selects whether the HDHomeRun stream URLs are based on `m3u` or `xtream` output behavior. This affects how playback URLs are generated and which delivery semantics back the tuner lineup. |

---
### 3.2.8 Favourites (`favourites`)

`favourites` lets you duplicate final transformed channels into dedicated favorite groups **after** filtering, renaming, mapping, and other transformations are complete.

```yaml
favourites:
  - cluster: series
    group: "My Favourites"
    filter: 'Name ~ "Cinema"'
    match_as_ascii: true
```

#### `favourites` Parameters

| Parameter | Type | Required | Default | Technical Impact & Background |
| :--- | :--- | :---: | :--- | :--- |
| `cluster` | String | No | | Optional logical cluster, for example `series`. This influences how Tuliprox groups the duplicated entries internally for output generation. |
| `group` | String | Yes | | Name of the favorite group created in the final playlist. This adds a curated access path without removing the original group membership. |
| `filter` | String | Yes | | Filter expression selecting which final entries should be duplicated into the favorites group. This operates on the transformed end state rather than the original raw input. |
| `match_as_ascii` | Bool | No | `false` | If enabled, Tuliprox normalizes accented characters during matching. This improves filter matching robustness across multilingual names while preserving the original visible title in output. |

---

### 3.2.9 Watch (`watch`)
For each target with a _unique name_, you can define watched groups. It is a list of group patterns Tuliprox monitors for content changes during updates.

If matching groups gain or lose channels, Tuliprox emits a Messaging event such as:

- channels added
- channels removed

```yaml
watch:
  - group: '^Sports'
  - group: '^Movies'
```

#### `watch` Parameters

| Parameter | Type | Required | Default | Technical Impact & Background |
| :--- | :--- | :---: | :--- | :--- |
| `group` | String (Regex) | Yes | | Regex pattern matched against final group names. This allows Tuliprox to detect meaningful content changes in selected areas of the playlist and notify operators automatically through the configured messaging backends. |

> **Note:** `watch` is especially useful for monitoring premium groups, VOD collections, or unstable provider segments where additions and removals should generate operational alerts.