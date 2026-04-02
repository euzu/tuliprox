# 🛠️ Operations & Debugging (CLI & DB Dumps)

Tuliprox is designed as a "Fire & Forget" stream broker. However, when streams stutter, provider connection limits block
your users, or EPG data and
TMDB covers do not match, the engine provides deep, low-level insights under the hood.

This chapter covers the Command Line Interface (CLI), Logging architecture, and the internal Database Viewers.

## 1. Command Line Arguments (CLI Flags)

While Tuliprox is usually run via Docker, understanding the CLI flags is crucial for debugging and manual interventions.

| Flag                     | Purpose & Technical Background                                                                                                                                                                                                                                                  |
|:-------------------------|:--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `-s, --server`           | Starts continuous Server Mode (API, Web UI, Background Workers). Without this flag, Tuliprox acts as a "One-Shot" playlist generator that downloads, processes, and immediately exits.                                                                                          |
| `-H, --home <DIR>`       | Sets the Home Directory. All relative paths in the configuration are resolved against this directory. If not set, resolves via `TULIPROX_HOME` env variable, or finally the binary's directory.                                                                                 |
| `-c, -i, -a, -m, -T`     | Overrides specific config paths (e.g., `-c /etc/tuliprox/config.yml`). Useful for testing experimental configurations without altering the production setup.                                                                                                                    |
| `-t, --target <NAME>`    | **Targeted Processing:** Forces processing of the specified target *only*. **Crucial:** This bypasses the `enabled: false` state in the config! Extremely useful to quickly re-render a broken list via cron/shell without blocking the entire system with other heavy targets. |
| `--genpwd`               | Interactively generates a secure `Argon2id` password hash for the `user.txt` file. Never store plaintext passwords!                                                                                                                                                             |
| `--healthcheck`          | Docker Support: Pings the API over localhost. Returns Exit Code `0` if the server responds with `{"status": "ok"}`.                                                                                                                                                             |
| `--sh <QUERY>`           | **Stream History Viewer:** Dumps and filters stream history records from binary archive files. Accepts inline JSON or `@file.json`. See [Stream History Viewer](#5-stream-history-viewer) below.                                                                                |
| `--scan-library`         | Triggers an incremental scan of the local media directory (if configured).                                                                                                                                                                                                      |
| `--force-library-rescan` | Ignores modification timestamps and forces a full TMDB/PTT re-evaluation of all local media files.                                                                                                                                                                              |

---

## 2. Logging Levels and Module Filtering

Tuliprox utilizes the powerful Rust `env_logger` crate. The log verbosity can be controlled at an extremely granular
level via `config.yml`
(`log.log_level`), the environment variable `TULIPROX_LOG`, or the CLI flag `-l`.

The evaluation hierarchy is: **CLI Argument > Env-Var > config.yml > Default (`info`)**.

Available levels: `trace`, `debug`, `info`, `warn`, `error`.

**The Magic of Module Filtering:**
Often, you do not want to set the entire system to `trace` (which would flood your console and disk), but rather
investigate a specific algorithm.
You can pass comma-separated module paths:

```bash
# Everything on Info, but the internal Mapper on Trace
# (Useful to make print() commands from the DSL visible!):
./tuliprox -s -l "info,tuliprox::foundation::mapper=trace"

# Show me all low-level HTTP-Connection errors from the Hyper crate:
./tuliprox -s -l "info,hyper_util::client::legacy::connect=error"
```

*Note: If `log.sanitize_sensitive_info` is set to `true` in the config (default), Tuliprox masks passwords, provider
URLs, and external client IPs in
the logs with `***`. This is strongly recommended so you can safely share logs on GitHub or Discord!*

---

## 3. Database Dumps (B+Tree Analysis)

Tuliprox is built to be extremely resource-efficient. It does not keep massive playlists (often > 200,000 entries)
permanently in RAM. Instead, it
stores all parsed metadata, enriched by FFprobe and TMDB, in highly optimized local **B+Tree Database files** (`.db`).

Sometimes you need to know *exactly* what Tuliprox has discovered in the background about a specific stream. Using the
built-in dump flags, you can
output these binary files in clean JSON format to your console (or pipe them into a file).

You must point the flag directly at the corresponding `.db` file inside your `storage_dir` (e.g., `/app/data/`):

| Flag & Example                                                                       | Usage & Purpose                                                                                                                                                                                                                                                                                              |
|:-------------------------------------------------------------------------------------|:-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **`--dbx <PATH>`**<br>`./tuliprox --dbx ./data/input_name/xtream/video.db`           | **Xtream DB:** Reads the metadata derived from the Xtream API. Shows you the final JSON payloads with resolved TMDB IDs, extracted video codecs (e.g., H264), and bitrates.                                                                                                                                  |
| **`--dbm <PATH>`**<br>`./tuliprox --dbm ./data/input_name/m3u.db`                    | **M3U Playlist DB:** Reads the raw M3U entries. Ideal for seeing how the fallback logic for `Tvg-ID` or `Virtual_ID` reacted to messy provider tags.                                                                                                                                                         |
| **`--dbe <PATH>`**<br>`./tuliprox --dbe ./data/input_name/xtream/epg.db`             | **EPG DB:** Prints the fully matched XMLTV grid. You see a list of all programmes with their correct Unix timestamps.                                                                                                                                                                                        |
| **`--dbms <PATH>`**<br>`./tuliprox --dbms ./data/input_name/metadata_retry_state.db` | **Metadata Retry Status (Cooldowns):** Extremely important! Shows you the asynchronous backoff state. If TMDB finds no info for a stream, it lands in a cooldown here. Shows `attempts: 3`, `last_error: "404 Not Found"`, `cooldown_until_ts: 1740000000`. This explains *why* a movie isn't being updated. |
| **`--dbv <PATH>`**<br>`./tuliprox --dbv ./data/target_name/id_mapping.db`            | **Target-ID Mapping:** Tracks the stability of stream UUIDs across updates. Shows which original Provider-ID points to which internal Virtual-ID.                                                                                                                                                            |
| **`--dbq <PATH>`**<br>`./tuliprox --dbq ./data/qos_snapshot.db`                      | **QoS Snapshot DB:** Dumps the aggregated QoS snapshots used as the future failover input. Shows per-stream identity, daily buckets, and the current `24h/7d/30d` score/confidence windows.                                                                                                                  |

### Example output via `--dbms`

```json
{
  "Stream_ID_4242": {
    "resolve": {
      "attempts": 3,
      "next_allowed_at_ts": 1718000000,
      "cooldown_until_ts": 1718604800,
      "last_error": "TMDB lookup completed without matching result",
      "tmdb": null,
      "updated_at_ts": 1718604910
    }
  }
}
```

**Diagnosis:** This dump immediately tells you: Tuliprox tried three times to find the movie on TMDB, failed every time,
and has now paused this
movie until `cooldown_until_ts` (e.g., 7 days in the future) to save API traffic and prevent rate-limiting.

---

## 4. Reading QoS Snapshots

The QoS snapshot DB is the condensed operational view built from raw stream history. It is intended to answer questions
like:

* Which stream is currently the most reliable?
* Is a stream failing before startup, during first byte, or later during playback?
* Is the problem likely provider instability, capacity pressure, or transient churn?

### What a Snapshot Represents

Each snapshot is keyed by a stable stream identity and stores rolling windows:

* `24h` — recent behavior, best for spotting active incidents or current degradations
* `7d` — medium-term stability, useful to smooth out one-off noise
* `30d` — long-term baseline, useful to judge whether a stream is generally trustworthy

Each window contains:

* `score` — compact reliability score (`0-100`)
* `confidence` — how much data the score is based on
* startup counters
* runtime abort counters
* provider-close counters
* reconnect burden
* latency and session-duration averages

### How To Interpret `score`

The score is a weighted operational quality estimate:

* **High score (`80-100`)**: recent connects are successful, disconnects are rare, first-byte failures are low, and
  reconnect burden is low
* **Medium score (`50-79`)**: stream is usable, but shows noticeable instability or intermittent provider issues
* **Low score (`0-49`)**: stream is currently risky; repeated startup failures, runtime aborts, or provider-side churn
  are dominating

The exact weighting is intentionally pragmatic, not academic. It is designed to provide a stable operational signal for
later failover ranking, not a mathematically "perfect" SLA model.

### How To Interpret `confidence`

`confidence` answers: "How much should I trust this score?"

* **High confidence**: enough events were seen in the window to make the score meaningful
* **Low confidence**: too little recent traffic; the score may be technically correct but statistically weak

Operationally:

* treat **high score + high confidence** as a strong candidate
* treat **high score + low confidence** as promising but not yet proven
* treat **low score + high confidence** as a real reliability warning

### Common Diagnosis Patterns

**1. Low `score`, high `connect_failed_count`**

Startup path is unstable. Look at:

* `startup_capacity_failure_count`
* `provider_open_failure_count`

Interpretation:

* high capacity failures => provider/user capacity is too tight
* high provider-open failures => upstream is unstable before streaming even starts

**2. Good startup, but high `first_byte_failure_count`**

Tuliprox could open the session, but the provider never became stream-ready in time. This often points to bad upstream
responsiveness or unstable pre-stream behavior.

**3. Good startup, but high `runtime_abort_count` / `provider_closed_count`**

The stream starts, then dies later. This is the classic "provider instability during playback" case and is usually more
relevant for later failover ordering than pure startup success.

**4. High `avg_provider_reconnect_count`**

The stream survives, but only because Tuliprox has to reconnect repeatedly behind the scenes. This is a reliability
warning even if users do not immediately see hard failures.

**5. High score in `30d`, bad score in `24h`**

Usually indicates a current incident rather than a historically bad stream. Treat as a recent degradation, not a
permanently bad source.

**6. Strong `24h` score, weak `30d` score**

Usually indicates recent recovery. Good sign, but wait for confidence to grow before treating it as fully stable again.

### QoS Snapshots vs Raw Stream History

Use **QoS snapshots** when you want a compact answer:

* "Which streams are strong?"
* "Which providers are degrading?"
* "Is this a capacity problem or a runtime stability problem?"

Use **raw stream history** when you need event-level truth:

* exact disconnect reasons
* exact timestamps
* per-session provider metadata
* startup vs disconnect timeline reconstruction

The intended workflow is:

1. Look at QoS snapshots first for ranking and trend detection.
2. Drill into raw stream history when a snapshot looks suspicious or degraded.

### Practical Workflow

```bash
# Dump the QoS snapshot DB
./tuliprox --dbq ./data/qos_snapshot.db

# Then inspect raw history for one affected day
./tuliprox --sh '{"from":"2026-04-02","filter":{"provider_name":"acme"}}'
```

### Important Limitation

QoS snapshots are intentionally **operational summaries**, not a complete event archive:

* they are ideal for ranking and triage
* they are not a replacement for raw stream history
* they are also not yet the failover engine itself

The future failover feature is expected to consume these snapshots, but the snapshots already stand on their own as a
debugging and reliability-analysis tool.

---

## 5. Stream History Viewer

When stream history is enabled (`stream_history` in `reverse_proxy` config), Tuliprox persists connect/disconnect
records to daily binary files.
The `--sh` CLI flag lets you query and filter these records offline without starting the server.

### Query Format

The `--sh` flag accepts a JSON query, either inline or via `@file.json`:

```bash
# Inline query: all records from a single day
./tuliprox --sh '{"from":"2026-03-22"}'

# Date range with filter
./tuliprox --sh '{"from":"2026-03-20","to":"2026-03-22","filter":{"api_username":"alice"}}'

# Query from file
./tuliprox --sh @query.json
```

### Query Fields

| Field    | Type     | Description                                                                                                                 |
|:---------|:---------|:----------------------------------------------------------------------------------------------------------------------------|
| `from`   | `string` | Start date/datetime. Formats: `YYYY-MM-DD`, `YYYY-MM-DD HH:MM`, `YYYY-MM-DD HH:MM:SS`. At least `from` or `to` is required. |
| `to`     | `string` | End date/datetime. Same formats as `from`. Date-only values expand to end of day (23:59:59).                                |
| `path`   | `string` | Stream history directory. Defaults to `data/stream_history`.                                                                |
| `filter` | `object` | Key-value filters applied per record. See filter syntax below.                                                              |

When only `from` or `to` is provided, the query expands to the full UTC day. All timestamps are interpreted as UTC.

### Filter Syntax

Filters are key-value pairs where the key is a record field name:

| Syntax               | Meaning                        | Example                       |
|:---------------------|:-------------------------------|:------------------------------|
| `"field": "value"`   | Exact match (case-insensitive) | `"api_username": "alice"`     |
| `"field": "~regex"`  | Regex match (prefix with `~`)  | `"provider_name": "~^acme.*"` |
| `"session_id": "42"` | Numeric exact match            | `"session_id": "42"`          |

**Filterable fields:** `event_type`, `api_username`, `provider_name`, `provider_username`, `item_type`, `title`,
`group`,  
`country`, `source_addr`, `disconnect_reason`, `session_id`.

### Output

Output is a streaming JSON array to stdout. Warnings and errors go to stderr.

```json
[
  {
    "schema_version": 1,
    "event_type": "connect",
    "event_ts_utc": 1742601600,
    ...
  },
  {
    "schema_version": 1,
    "event_type": "disconnect",
    "event_ts_utc": 1742605200,
    ...
  }
]
```

### Examples

```bash
# All disconnects with provider errors on March 22
./tuliprox --sh '{"from":"2026-03-22","filter":{"event_type":"disconnect","disconnect_reason":"provider_error"}}'

# All activity for user "bob" in a date range, piped to jq
./tuliprox --sh '{"from":"2026-03-20","to":"2026-03-25","filter":{"api_username":"bob"}}' | jq '.[] | {ts: .event_ts_utc, type: .event_type}'

# Query with regex filter for provider names starting with "acme"
./tuliprox --sh '{"from":"2026-03-22","filter":{"provider_name":"~^acme"}}'
```

---

## 6. Hot Reloading Caveats

Tuliprox supports hot-reloading for specific files (`mapping.yml`, `api-proxy.yml`) if `config_hot_reload: true` is set
in `config.yml`.

**Important Note for Docker Bind Mounts:**
If you edit a file on your host system that is bind-mounted into the container (e.g.,
`nano /home/user/tuliprox/config/mapping.yml`), the file
watcher might report the inotify event using the *original host path* instead of the container's mount point
`/app/config/mapping.yml`.
Tuliprox attempts to resolve this, but depending on your host OS (Windows/WSL vs Linux), filesystem events can be flaky.
If hot-reload fails to
trigger, a container restart (`docker restart tuliprox`) is the safest fallback.

---
