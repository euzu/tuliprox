# 📂 Local Media Library

Tuliprox is not limited to external IPTV providers. Through the **Library** module, it can recursively scan, catalog, and
seamlessly integrate local movie and TV show collections (much like Plex or Jellyfin do) directly into the Xtream/M3U outputs
for your IPTV clients.

## Core Features

* **Recursive Scanning:** Traverses directories looking for supported video formats (`.mkv`, `.mp4`, etc.).
* **Auto-Classification:** Automatically detects whether a file is a Movie or a Series episode (e.g., `Breaking.Bad.S01E01.mkv`)
  using the internal PTT (Parse Torrent Title) engine.
* **Multi-Source Metadata:** Reads Kodi/Jellyfin/Emby compatible `.nfo` files. If no `.nfo` is present, it automatically queries
  the TMDB API for covers, plots, cast, and trailers.
* **Incremental Scans:** Uses file modification timestamps to only scan and process new or altered files, ensuring extremely fast
  updates.
* **Stable Virtual IDs:** Generates stable, deterministic UUIDs for local files, ensuring that channel/stream IDs in your IPTV
  client remain constant across updates.

---

## Configuration (`config.yml`)

You enable the library globally in the `library` block of your `config.yml`. The metadata caches and resolved TMDB data are
physically stored inside `metadata_update.cache_path/library` (relative to your `storage_dir`).

```yaml
library:
  enabled: true
  scan_directories:
    - enabled: true
      path: "/media/movies"
      content_type: movie  # Forces everything in this folder to be treated as a movie
      recursive: true
    - enabled: true
      path: "/media/shows"
      content_type: auto   # Uses PTT to guess S01E01 structures
      recursive: true
  supported_extensions:
    - "mp4"
    - "mkv"
    - "avi"
    - "ts"
  metadata:
    fallback_to_filename: true # Uses parsed filename details if TMDB/NFO fails
    read_existing:
      kodi: true
      plex: false
      jellyfin: false
    formats:
      - "nfo"
  playlist:
    movie_category: "Local Movies"
    series_category: "Local Shows"
```

### Library Configuration Parameters

| Block / Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `enabled` | Bool | `false` | Master switch to turn on the local media library feature. |
| **`scan_directories`** | List | | Folders to monitor. |
| ↳ `enabled` | Bool | `true` | Allows temporarily disabling specific folders. |
| ↳ `path` | String | | The absolute or relative path to your media directory. |
| ↳ `content_type` | Enum | `auto` | Forces classification. Options: `auto` (guess via filename), `movie`, `series`. |
| ↳ `recursive` | Bool | `true` | If true, Tuliprox crawls all subdirectories within `path`. |
| `supported_extensions` | List | `[mp4, mkv, avi, ts, ...]` | File extensions that Tuliprox considers as playable video files. |
| **`metadata`** | Object | | Instructions on how to fetch or fallback for movie details. |
| ↳ `fallback_to_filename` | Bool | `true` | If NFO or TMDB fails, uses the filename to construct basic metadata (Title, Year). |
| ↳ `read_existing.kodi` | Bool | `true` | Attempts to read Kodi-compatible `.nfo` files residing next to the media. |
| ↳ `read_existing.plex` | Bool | `false` | Attempts to read Plex metadata formats. |
| ↳ `read_existing.jellyfin` | Bool | `false` | Attempts to read Jellyfin metadata formats. |
| ↳ `formats` | List | `["nfo"]` | |

*Note: For TMDB enrichments to work on local files, `metadata_update.tmdb.enabled: true` must be set in your config! The library
utilizes the exact same API limits, API keys, and caches as the IPTV streams.*

---

## Integration as an Input (`source.yml`)

To make your local movies visible in your M3U/Xtream targets, you attach the library as a standard `input` of `type: library`
in your `source.yml`:

```yaml
inputs:
  - name: my_local_library
    type: library
    enabled: true

sources:
  - inputs:
      - my_local_library
      - my_iptv_provider
    targets:
      - name: mixed_target
        filter: 'Group ~ ".*"'
        output:
          - type: xtream
```

In this setup, the target `mixed_target` now merges the Live-TV channels from your IPTV provider and your local `.mkv` movies
into a single, clean Xtream API output for the client. The local files bypass the Reverse Proxy routing and stream directly off
your disk when requested by the IPTV player!

---

## Triggering Scans (CLI & API)

By default, the library scan can be automated using standard Cron syntax under the `schedules:` block (`type: LibraryScan`).
However, you can also force it manually.

**Via CLI (Command Line):**

```bash
# Incremental Delta-Scan (Only processes new/modified files)
./tuliprox --scan-library

# Ignores the cache and forces TMDB/PTT to re-evaluate ALL local files
./tuliprox --force-library-rescan
```

**Via REST API (e.g., triggered from a post-download script like Radarr/Sonarr):**

```http
POST /api/v1/library/scan
Content-Type: application/json
Authorization: Bearer <TULIPROX_API_TOKEN>

{"force_rescan": false}
```

**Status Polling:**

```http
GET /api/v1/library/status
```

Returns JSON information about the number of detected files, errors, and the progress of the background scan.
