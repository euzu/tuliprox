# 📂 Local Media Library

Tuliprox is not limited to external IPTV providers. Through the **Library** module, it can recursively scan, catalog,
and
seamlessly integrate local movie and TV show collections (much like Plex or Jellyfin do) directly into the Xtream/M3U
outputs
for your IPTV clients.

## Core Features

* **Recursive Scanning:** Traverses directories looking for supported video formats (`.mkv`, `.mp4`, `.mov`, `.m4v`,
  `.webm`, etc.).
* **Auto-Classification:** Automatically detects whether a file is a Movie or a Series episode (e.g.,
  `Breaking.Bad.S01E01.mkv`) using the internal PTT (Parse Torrent Title) engine.
* **Multi-Source Metadata:** Reads Kodi/Jellyfin/Emby/Plex-compatible `.nfo` files. If no `.nfo` is present, it
  automatically queries the TMDB API for covers, plots, and cast.
* **Stable Virtual IDs:** Generates stable, deterministic UUIDs for local files, ensuring that channel/stream IDs in
  your IPTV client remain constant across updates.
* **Incremental Scans:** Uses file modification timestamps to only process new or altered files, ensuring extremely fast
  updates.
* **Visuals:** Local series episode backgrounds in the Playlist Explorer use direct TMDB still-image URLs for a rich UI
  experience.

---

## Configuration (`config.yml`)

You enable the library globally in the `library` block of your `config.yml`. The metadata caches and resolved TMDB data
are
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
    - "mov"
    - "ts"
    - "m4v"
    - "webm"
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
  thumbnails:
    enabled: true
    width: 320
    height: 180
    quality: 75
```

### Thumbnail Extraction

When TMDB posters are unavailable, Tuliprox can automatically extract a thumbnail
from the video file itself using `ffmpeg`. The extracted frame is cached and served
via a local API endpoint.

**Requires:** `ffmpeg` must be installed and available in the system `PATH`.

| Parameter | Type   | Default | Description                           |
|:----------|:-------|:--------|:--------------------------------------|
| `enabled` | Bool   | `false` | Enable automatic thumbnail extraction |
| `width`   | Number | `320`   | Output width in pixels (16:9)         |
| `height`  | Number | `180`   | Output height in pixels               |
| `quality` | Number | `75`    | JPEG compression quality (1-100)      |

Thumbnails are extracted at ~10 seconds into the video (falls back to 0s for short clips).
They are re-extracted automatically when the source file changes. Orphaned thumbnails
are cleaned up during each library scan.

Remote URL thumbnail extraction via HTTP range requests is not implemented yet.
That remains a future feature.

### Library Configuration Parameters

| Block / Parameter          | Type   | Default          | Description                                                                   |
|:---------------------------|:-------|:-----------------|:------------------------------------------------------------------------------|
| `enabled`                  | Bool   | `false`          | Master switch to turn on the local media library feature.                     |
| **`scan_directories`**     | List   |                  | Folders to monitor for media files.                                           |
| ↳ `enabled`                | Bool   | `true`           | Allows temporarily disabling specific folders from being scanned.             |
| ↳ `path`                   | String |                  | The absolute or relative path to your media directory.                        |
| ↳ `content_type`           | Enum   | `auto`           | Classification mode. Options: `auto` (guess via PTT), `movie`, `series`.      |
| ↳ `recursive`              | Bool   | `true`           | If true, Tuliprox crawls all subdirectories within the specified path.        |
| `supported_extensions`     | List   | `[...]`          | Video extensions considered playable (e.g., `mp4`, `mkv`, `mov`, `webm`).     |
| **`metadata`**             | Object |                  | Configuration for metadata resolution and fallback logic.                     |
| ↳ `fallback_to_filename`   | Bool   | `true`           | Uses the parsed filename if NFO or TMDB metadata is unavailable.              |
| ↳ `read_existing.kodi`     | Bool   | `true`           | Reads Kodi-compatible `.nfo` files located alongside the media.               |
| ↳ `read_existing.plex`     | Bool   | `false`          | Attempts to read Plex-specific metadata formats.                              |
| ↳ `read_existing.jellyfin` | Bool   | `false`          | Attempts to read Jellyfin-specific metadata formats.                          |
| ↳ `formats`                | List   | `[]`             | List of metadata output formats (e.g., `nfo` to write Kodi-compatible files). |
| **`playlist`**             | Object |                  | Controls how library items appear in the resulting IPTV playlist.             |
| ↳ `movie_category`         | String | `Local Movies`   | The category name assigned to movies in M3U/Xtream outputs.                   |
| ↳ `series_category`        | String | `Local TV Shows` | The category name assigned to TV shows in M3U/Xtream outputs.                 |

*Note: For TMDB enrichments to work, `metadata_update.tmdb.enabled: true` must be set!*

---

### Integration as an Input (`source.yml`)

To make your local movies visible in your M3U/Xtream targets, you attach the library as a standard `input` of
`type: library`
in your `source.yml`:

```yaml
# In source.yml
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

In this setup, the target `mixed_target` now merges the Live-TV channels from your IPTV provider and your local `.mkv`
movies
into a single, clean Xtream API output for the client. The local files bypass the Reverse Proxy routing and stream
directly off
your disk when requested by the IPTV player!

---

&nbsp;

## Additional Information

### Content Classification

The MediaClassifier logic respects the `content_type` defined for configured directories.

* **auto** (default): Uses auto-detection to determine if a file is a movie or a series based on filename patterns (e.g.
  `S01E01`).
* **movie**: Forces all files to be classified as movies, overriding any auto-detection results.
* **series**: Forces classification as a series. If episode/season patterns are detected in the filename (e.g.
  `S02E05`), those values are used.  
  If no pattern is found, the file is still classified as a series with **season 1** and an **auto-incremented episode
  number**  
  (starting at 1, in file scan order).

### Triggering Scans (CLI & API)

By default, the library scan can be automated using standard Cron syntax under the `schedules:` block (
`type: LibraryScan`).
However, you can also force it manually.

**Manual Scans via CLI (Command Line):**

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

### Database Inspection (DBX/DBM/DBE)

If you need to verify the internal database content for troubleshooting:

```bash
./tuliprox --dbx /opt/tuliprox/data/all_channels/xtream/video.db
./tuliprox --dbm /opt/tuliprox/data/all_channels/m3u.db
./tuliprox --dbe /opt/tuliprox/data/all_channels/xtream/epg.db
