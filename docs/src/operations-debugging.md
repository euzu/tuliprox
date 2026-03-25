# 🛠️ Operations & Debugging (CLI & DB Dumps)

Tuliprox is designed as a "Fire & Forget" stream broker. However, when streams stutter, provider connection limits block your users, or EPG data and
TMDB covers do not match, the engine provides deep, low-level insights under the hood.

This chapter covers the Command Line Interface (CLI), Logging architecture, and the internal Database Viewers.

## 1. Command Line Arguments (CLI Flags)

While Tuliprox is usually run via Docker, understanding the CLI flags is crucial for debugging and manual interventions.

| Flag | Purpose & Technical Background |
| :--- | :--- |
| `-s, --server` | Starts continuous Server Mode (API, Web UI, Background Workers). Without this flag, Tuliprox acts as a "One-Shot" playlist generator that downloads, processes, and immediately exits. |
| `-H, --home <DIR>` | Sets the Home Directory. All relative paths in the configuration are resolved against this directory. If not set, resolves via `TULIPROX_HOME` env variable, or finally the binary's directory. |
| `-c, -i, -a, -m, -T` | Overrides specific config paths (e.g., `-c /etc/tuliprox/config.yml`). Useful for testing experimental configurations without altering the production setup. |
| `-t, --target <NAME>` | **Targeted Processing:** Forces processing of the specified target *only*. **Crucial:** This bypasses the `enabled: false` state in the config! Extremely useful to quickly re-render a broken list via cron/shell without blocking the entire system with other heavy targets. |
| `--genpwd` | Interactively generates a secure `Argon2id` password hash for the `user.txt` file. Never store plaintext passwords! |
| `--healthcheck` | Docker Support: Pings the API over localhost. Returns Exit Code `0` if the server responds with `{"status": "ok"}`. |
| `--scan-library` | Triggers an incremental scan of the local media directory (if configured). |
| `--force-library-rescan` | Ignores modification timestamps and forces a full TMDB/PTT re-evaluation of all local media files. |

---

## 2. Logging Levels and Module Filtering

Tuliprox utilizes the powerful Rust `env_logger` crate. The log verbosity can be controlled at an extremely granular level via `config.yml`
(`log.log_level`), the environment variable `TULIPROX_LOG`, or the CLI flag `-l`.

The evaluation hierarchy is: **CLI Argument > Env-Var > config.yml > Default (`info`)**.

Available levels: `trace`, `debug`, `info`, `warn`, `error`.

**The Magic of Module Filtering:**
Often, you do not want to set the entire system to `trace` (which would flood your console and disk), but rather investigate a specific algorithm.
You can pass comma-separated module paths:

```bash
# Everything on Info, but the internal Mapper on Trace
# (Useful to make print() commands from the DSL visible!):
./tuliprox -s -l "info,tuliprox::foundation::mapper=trace"

# Show me all low-level HTTP-Connection errors from the Hyper crate:
./tuliprox -s -l "info,hyper_util::client::legacy::connect=error"
```

*Note: If `log.sanitize_sensitive_info` is set to `true` in the config (default), Tuliprox masks passwords, provider URLs, and external client IPs in
the logs with `***`. This is strongly recommended so you can safely share logs on GitHub or Discord!*

---

## 3. Database Dumps (B+Tree Analysis)

Tuliprox is built to be extremely resource-efficient. It does not keep massive playlists (often > 200,000 entries) permanently in RAM. Instead, it
stores all parsed metadata, enriched by FFprobe and TMDB, in highly optimized local **B+Tree Database files** (`.db`).

Sometimes you need to know *exactly* what Tuliprox has discovered in the background about a specific stream. Using the built-in dump flags, you can
output these binary files in clean JSON format to your console (or pipe them into a file).

You must point the flag directly at the corresponding `.db` file inside your `storage_dir` (e.g., `/app/data/`):

| Flag & Example | Usage & Purpose |
| :--- | :--- |
| **`--dbx <PATH>`**<br>`./tuliprox --dbx ./data/input_name/xtream/video.db` | **Xtream DB:** Reads the metadata derived from the Xtream API. Shows you the final JSON payloads with resolved TMDB IDs, extracted video codecs (e.g., H264), and bitrates. |
| **`--dbm <PATH>`**<br>`./tuliprox --dbm ./data/input_name/m3u.db` | **M3U Playlist DB:** Reads the raw M3U entries. Ideal for seeing how the fallback logic for `Tvg-ID` or `Virtual_ID` reacted to messy provider tags. |
| **`--dbe <PATH>`**<br>`./tuliprox --dbe ./data/input_name/xtream/epg.db` | **EPG DB:** Prints the fully matched XMLTV grid. You see a list of all programmes with their correct Unix timestamps. |
| **`--dbms <PATH>`**<br>`./tuliprox --dbms ./data/input_name/metadata_retry_state.db` | **Metadata Retry Status (Cooldowns):** Extremely important! Shows you the asynchronous backoff state. If TMDB finds no info for a stream, it lands in a cooldown here. Shows `attempts: 3`, `last_error: "404 Not Found"`, `cooldown_until_ts: 1740000000`. This explains *why* a movie isn't being updated. |
| **`--dbv <PATH>`**<br>`./tuliprox --dbv ./data/target_name/id_mapping.db` | **Target-ID Mapping:** Tracks the stability of stream UUIDs across updates. Shows which original Provider-ID points to which internal Virtual-ID. |

### Example output via `--dbms`

```json
{
  "Stream_ID_4242": {
    "resolve": {
      "attempts": 3,
      "next_allowed_at_ts": 1718000000,
      "cooldown_until_ts": 1718604800,
      "last_error": "TMDB lookup completed without matching result",
      "tmdb":null,
      "updated_at_ts":1718604910
    }
  }
}
```

**Diagnosis:** This dump immediately tells you: Tuliprox tried three times to find the movie on TMDB, failed every time, and has now paused this
movie until `cooldown_until_ts` (e.g., 7 days in the future) to save API traffic and prevent rate-limiting.

---

## 4. Hot Reloading Caveats

Tuliprox supports hot-reloading for specific files (`mapping.yml`, `api-proxy.yml`) if `config_hot_reload: true` is set in `config.yml`.

**Important Note for Docker Bind Mounts:**
If you edit a file on your host system that is bind-mounted into the container (e.g., `nano /home/user/tuliprox/config/mapping.yml`), the file
watcher might report the inotify event using the *original host path* instead of the container's mount point `/app/config/mapping.yml`.
Tuliprox attempts to resolve this, but depending on your host OS (Windows/WSL vs Linux), filesystem events can be flaky. If hot-reload fails to
trigger, a container restart (`docker restart tuliprox`) is the safest fallback.

---
