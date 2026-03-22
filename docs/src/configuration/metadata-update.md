# 📖 Metadata Update & FFprobe (`metadata_update`)

This chapter covers the `metadata_update` block inside `config.yml`, which determines how aggressively or gently Tuliprox manages background tasks for resolving metadata and technical stream properties.

Tuliprox utilizes three distinct mechanisms to ensure perfect library quality (especially for Plex/Jellyfin compatibility):
1. **Resolve (Xtream API):** Fetching missing VOD details (Cast, Director, Plot) directly from the provider's API.
2. **TMDB:** Supplementing missing Release Years and high-resolution Covers/Backdrops via The Movie Database.
3. **Probe (FFprobe):** Physically opening the stream to analyze the exact A/V codecs (HEVC, H264) and resolution.

## Top-level entries

```yaml
metadata_update:
  cache_path: metadata
  retry_delay: 2s
  worker_idle_timeout: 1m
  max_queue_size: 100000
  no_change_cache_ttl_secs: 3600
  probe_fairness_resolve_burst: 200
  log:
  resolve:
  probe:
  tmdb:
  ffprobe:
```

### Global Options (Flat Keys)

| Parameter | Type | Default | Technical Impact & Background |
| :--- | :--- | :--- | :--- |
| `cache_path` | String | `"metadata"`| Directory where TMDB cache files and metadata are stored. Relative paths are resolved against `storage_dir`. |
| `retry_delay` | String | `"2s"` | General minimum wait time when the worker encounters temporary runtime errors (e.g., socket timeout). |
| `worker_idle_timeout`| String | `"1m"` | Time of inactivity (empty queue) after which the background worker kills itself to free RAM/CPU. |
| `max_queue_size` | Int |`100000`| RAM Safety Limit: Maximum number of metadata tasks kept in memory per input simultaneously. |
| `no_change_cache_ttl_secs`| Int | `3600` | How long (seconds) a "No Change" status is cached to avoid unnecessary DB checks across subsequent playlist updates. |
| `probe_fairness_resolve_burst`| Int| `200` | After 200 consecutive Resolve tasks, 1 Probe task is forcibly prioritized so probes don't starve. |

---

## 1. Logging (`log`)

Controls background worker logging verbosity.

```yaml
metadata_update:
  log:
    queue_interval: 30s
    progress_interval: 15s
```

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `queue_interval` | String | `"30s"` | Interval to log the queue status (pending tasks). |
| `progress_interval`| String | `"15s"` | Interval for progress reports (successful/failed resolves). |

---

## 2. API Resolve Limits (`resolve`)

Controls API requests for pure metadata (Xtream Info / TMDB).

```yaml
metadata_update:
  resolve:
    max_retry_backoff: 1h
    min_retry_base: 5s
    max_attempts: 3
    exhaustion_reset_gap: 1h
```

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `max_retry_backoff`| String | `"1h"` | Maximum time limit for exponential wait between API failures. |
| `min_retry_base` | String | `"5s"` | Initial wait time on the very first failure. |
| `max_attempts` | Int (u8) | `3` | Max attempts per cycle before a resolve task is marked as "exhausted". |
| `exhaustion_reset_gap`| String| `"1h"` | Time window after a cycle completes before "exhausted" tasks are retried in the next run. |

---

## 3. FFprobe Retries & Limits (`probe`)

Controls technical stream probing retries via FFprobe.

```yaml
metadata_update:
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

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `cooldown` | String | `"7d"` | Hard lock time (cooldown) during which a broken stream is completely ignored to protect the provider. |
| `retry_load_retry_delay`| String| `"1m"` | Wait time if loading the internal `metadata_retry_state.db` fails. |
| `retry_backoff_step_1`| String | `"10m"`| Wait time after the 1st FFprobe failure. |
| `retry_backoff_step_2`| String | `"30m"`| Wait time after the 2nd FFprobe failure. |
| `retry_backoff_step_3`| String | `"1h"` | Wait time from the 3rd FFprobe failure onwards. |
| `max_attempts` | Int (u8) | `3` | Max failures to probe a stream before it enters global long-term cooldown. |
| `backoff_jitter_percent`| Int (u8)| `20` | Random time deviation in percent (Jitter) so hundreds of parallel retries don't hit the exact same second. |
| `user_priority` | Int (i8) | `127` | Priority of the probe task on the Unix Nice-Scale. `127` is the absolute lowest. A real user will instantly evict the probe task. |

---

## 4. TMDB API Integration (`tmdb`)

```yaml
metadata_update:
  tmdb:
    enabled: false
    api_key: "YOUR_KEY"
    rate_limit_ms: 250
    cache_duration_days: 30
    language: en-US
    cooldown: 7d
    match_threshold: 86
```

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `enabled` | Bool | `false` | Global master switch for TMDB resolution. |
| `api_key` | String | *(Internal)*| Your own TMDB API Key. If omitted, Tuliprox uses a built-in default key. |
| `rate_limit_ms` | Int (u64) | `250` | Throttling of TMDB API calls (in ms) to prevent TMDB IP bans. |
| `cache_duration_days`| Int (u32) | `30` | How long successful TMDB results are kept in the internal cache. |
| `language` | String | `"en-US"` | Preferred metadata language (e.g., `"de-DE"`). |
| `cooldown` | String | `"7d"` | Lock time for a movie if the TMDB search was *successful* but returned *no match* for the title. |
| `match_threshold` | Int (u16) | `86` | Minimum percentage score (Jaro-Winkler Distance) for a TMDB result to be accepted as a "Match". |

---

## 5. FFprobe Process Rules (`ffprobe`)

```yaml
metadata_update:
  ffprobe:
    enabled: true
    timeout: 60
    analyze_duration: 10s
    probe_size: 10MB
    live_analyze_duration: 5s
    live_probe_size: 5MB
```

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `enabled` | Bool | `false` | Global master switch for ALL stream probing. Must be `true` for input flags like `probe_vod` to work. |
| `timeout` | Int (u64) | `60` | Hard timeout (in seconds) for the OS FFprobe process. Prevents zombie processes. |
| `analyze_duration` | String | `"10s"` | Passes `-analyzeduration` to FFprobe for VODs/Series. *Warning: Requires an explicit suffix (`s`, `m`)!* |
| `probe_size` | String | `"10MB"`| Passes `-probesize` to FFprobe for VODs/Series (Data Limit). |
| `live_analyze_duration`| String| `"5s"` | Stricter time limit for Live-TV streams (minimizes latency). |
| `live_probe_size` | String | `"5MB"` | Stricter data limit for Live-TV streams. |

**Important Note on FFprobe split:** The split design between `ffprobe` (VOD) and `ffprobe.live_...` is essential. VODs reside statically on the server and can be analyzed generously. Live-TV streams, however, must respond quickly to avoid generating unnecessary traffic and occupying the provider slot uselessly.

----
&nbsp;

# Additional Information
Tuliprox is not just a proxy; it is a highly intelligent **Playlist Processing Engine**. A core part of this is the asynchronous update and metadata process that loads information from the provider, updates local databases, and fully automatically supplements missing metadata.

### 1. The Complete Processing Pipeline

When a playlist update starts (via Scheduler, API, or Boot), Tuliprox runs this pipeline:

1. **Download & Cache Check:** For each configured `input`, it checks if provider data needs re-downloading (controlled by `cache_duration`).
2. **Input Storage (B+Tree):** The raw data (M3U, Xtream categories) is written to a local, extremely fast B+Tree database (`input_name.db`). This drastically saves RAM.
3. **Target Processing:** For each defined `target` (output playlist), data is loaded from the input and routed through the pipeline (`processing_order`, e.g., Filter ➔ Rename ➔ Map).
4. **Metadata Resolve & Probe:** Tuliprox analyzes the filtered entries. If data is missing (e.g., TMDB IDs, Video Codecs), these are dispatched as "Jobs" to the `MetadataUpdateManager`.
5. **Target Storage & EPG:** The finished playlist is written to the Target databases. Only then is XMLTV EPG data matched and assigned.

---

### 2. The `MetadataUpdateManager` (Architecture)

The `MetadataUpdateManager` is an asynchronous background engine (if `resolve_background: true` is set on the input) that prevents blocking the main playlist update.

### Architecture & Logic
* **Per-Input Worker:** A dedicated, isolated *Tokio Task (Worker)* is started for each Provider-Input. This prevents a slow provider from blocking another.
* **Task-Merging:** If a stream requires both TMDB info and an FFprobe, they are merged into a single Task.
* **Rate-Limiting & Connection-Locks:** The manager strictly respects the `max_connections` of your input. An FFprobe (Stream Analysis) is *only* initiated if a provider connection is free. A probe task runs at the absolute lowest priority (`user_priority: 127`). If a real user starts streaming, the FFprobe process is **immediately aborted/preempted** to free the slot for the user!
* **Smart Retry & Cooldown:** If a fetch or probe fails (e.g., HTTP 502), an exponential backoff with Jitter (random deviation) kicks in. If the max attempts (`max_attempts`) are reached, the task enters a global cooldown (e.g., 7 days) to stop harassing the provider.
* **Persistence (Retry State):** The status of failed tasks is stored locally in `metadata_retry_state.db`. A server restart does not cause Tuliprox to immediately bombard the provider with requests for broken streams.
* **Cascading Updates:** Once a worker collects a batch of metadata, it saves it in the Input DB and immediately *cascades* (inherits) the updates into all Target DBs, without requiring a full playlist rebuild.

---

### 3. Metadata Collection Mechanisms

#### How is a stream queued for analysis?
Tuliprox checks every stream for completeness (`has_details()`). A task is queued if the switches in `inputs.options` are active **and** one of these conditions is met:

* **Info-Resolve (VOD/Series):** Missing provider info (Cast, Plot, Director) retrievable via Xtream API (`get_vod_info` / `get_series_info`).
* **TMDB/Date-Resolve:** Missing `tmdb_id` or `release_date`.
* **Probing (FFprobe):** 
  * VOD/Series: Missing technical A/V parameters (`video_codec`, `audio_codec`, `resolution`).
  * Live-TV: The `last_probed_timestamp` is older than `probe_live_interval_hours`.

#### Collection Engines

* **Release Year / Date (PTT):** 
  Tuliprox uses a highly optimized internal parser (`PTT` - Parse Torrent Title). It locally analyzes the stream name and extracts the year (e.g., from *"My Movie (2023)"*). If this fails, it queries the TMDB API.
* **TMDB Information:**
  Via TMDB API and a Jaro-Winkler distance comparison (similarity scoring), it fetches IDs, release years, covers, backdrops, genres, directors, and actors.
* **Video & Audio (FFprobe):**
  Tuliprox briefly opens the stream via `ffprobe`. It extracts and normalizes:
  * *Resolution:* SD, 720p, 1080p, 1440p, 4K, 8K
  * *Video:* Codec (H264, HEVC, AV1), Bit-Depth (8bit, 10bit), Dynamic Range (HDR10, Dolby Vision, HLG)
  * *Audio:* Codec (AAC, AC3, EAC3, DTS, TrueHD) and Channels (2.0, 5.1, 7.1)
  * Tuliprox uses these tags later for the `add_quality_to_filename` target feature (e.g., `My Movie [2160p 4K HEVC HDR].strm`).
* **Seasons & Episodes:**
  For series, the Xtream API delivers a structure of seasons and episodes. Tuliprox "flattens" these into individually playable streams (`PlaylistItemType::Series`). Each episode is treated **individually** during probing, as codecs and resolutions can change from episode to episode.