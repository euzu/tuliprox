## **tuliprox** - A Powerful IPTV Proxy & Playlist Processor

`tuliprox` is a high-performance IPTV proxy and playlist processor written in Rust 🦀.
It ingests M3U/M3U8 playlists, Xtream sources and local media, reshapes them into clean outputs, and serves them to
Plex,  
Jellyfin, Emby, Kodi and similar clients.

![tuliprox logo](https://github.com/user-attachments/assets/8ef9ea79-62ff-4298-978f-22326c5c3d02)

## 🏆 Key Features

### 1. Written in Rust — Maximum Performance, Minimal Footprint

- Single binary — no Python, no Node.js, no Java, no runtime overhead
- **Extremely low CPU usage** — even with hundreds of concurrent streams
- **Very low RAM consumption** — runs comfortably on 256 MB, disk-based processing mode for even less
- Native async I/O with Tokio — thousands of concurrent streams without breaking a sweat
- **Rock-solid stability** — designed to run 24/7 for months without memory leaks, restarts, or degradation
- Runs on Raspberry Pi, tiny VPS, NAS, or any x86/ARM system
- No external database required — everything embedded

### 2. Custom B+Tree Storage Engine — No External Database Needed

- Purpose-built B+Tree with Slotted Page architecture
- Adaptive LZ4 compression for minimal disk footprint
- Zero-copy scans at up to 96,000 ops/sec
- Batch upsert for massive throughput during playlist updates
- Atomic I/O with file locking — no corrupt data, ever
- Configurable flush policy (Immediate, Batch, None)
- String interning (`Arc<str>`) for playlist entries reduces memory footprint
- B+Tree compaction to reclaim disk space
- Persistent value caching with thread-safe access
- Packed block update optimization — direct disk writes for same-size updates, bypassing expensive
  read-scan-modify-write cycles

### 3. Four Output Formats — One Tool to Rule Them All

| Format               | Description                                                               |
|----------------------|---------------------------------------------------------------------------|
| **M3U/M3U8**         | For all IPTV players (VLC, Tivimate, iMPlayer, etc.)                      |
| **Xtream Codes API** | Full Xtream API with Live, VOD, Series, Catchup, EPG                      |
| **HDHomeRun**        | Emulation for Plex, Jellyfin, Emby — auto-discovery via SSDP              |
| **STRM**             | Kodi/Plex/Jellyfin compatible with multi-version support and quality tags |

Generate all four formats simultaneously from the same source — one setup, every platform covered.

### 4. Reverse Proxy & Stream Management — Enterprise-Grade

- **Reverse Proxy Mode**: Streams are proxied through Tuliprox — provider URLs stay invisible to end users
- **Redirect Mode**: Lightweight redirection for resource-efficient operation
- **Shared Live Streams**: One provider stream shared across multiple users — saves valuable provider slots
- **User Connection Priority**: Higher-priority users evict lower-priority connections when all provider slots are full
- **Soft Connections & Soft Priority**: Users can temporarily exceed their normal slot limit with preemptible soft slots;  
  a dedicated `soft_priority` applies only while a connection is on a soft slot and automatically switches back to the normal  
  priority when a regular slot becomes free again
- **Grace Period**: Configurable transition window during connection handovers — no abrupt drops
- **Bandwidth Throttling**: With flexible units (KB/s, MB/s, kbps, mbps)
- **Per-Stream Metrics**: Bandwidth and transferred bytes per stream in the Web UI (opt-in)
- **Stream History**: Optional persisted stream lifecycle telemetry for connects, disconnects, preemptions, and startup failures
- **QoS Aggregation**: Optional background reliability snapshots built from stream history for long-term stream quality analysis
- **Custom Fallback Videos**: User-defined video files for channel unavailable, connections exhausted, account expired,
  etc.
- **HLS Session Management**: Short-lived provider reservations for HLS/Catchup without blocking real slots
- **Channel-Switch Friendly Reservations**: Instant takeover on channel switch — no TTL wait
- **Custom Stream Response Timeout**: Auto-stop fallback streams after configurable duration
- **Buffer Reuse**: Reusable serialization buffers minimize heap allocations during streaming

### 5. Provider Failover & DNS Rotation — Maximum Availability

- **Provider URL Failover**: Automatic rotation on errors (5xx, timeout) — seamless switching, no viewer disruption
- **`provider://` URL Scheme**: Reference providers by name — Tuliprox resolves to the active URL automatically
- **DNS-Aware Connection Routing**: Provider DNS resolved asynchronously and cached
- **Resolved DNS Persistence**: Resolved IPs persisted separately — no source config overwrite during hot reloads
- **Provider Aliases**: Manage multiple accounts from the same provider with different credentials
- **Batch Input**: Xtream and M3U batch inputs via CSV files for mass provider management
- **Staged Cluster Source Routing**: Per-cluster decision whether Live/VOD/Series comes from staged input, main input,
  or is skipped entirely

### 6. Multi-Source Merging & Advanced Processing Pipeline

- Merge multiple input sources (M3U, Xtream, Local Library) into a single target
- **Filter Engine**: Complex boolean expressions — `(Group ~ "^DE.*") AND NOT (Name ~ ".*XXX.*")`
- **Mapper DSL**: A custom Domain-Specific Language for powerful transformations
  - Regex-based renaming with capture groups and backreferences
  - Variables, if/else blocks, loops (`for_each`)
  - Built-in functions: `replace`, `pad`, `format`, `first`, `capitalize`, `lowercase`, `uppercase`, `template`
  - Counters with padding (e.g. `001`, `002`)
  - Transform operations directly in mappings
- **Template System**: Centralized, reusable pattern collection
  - Global templates shared across sources and mappings
  - Inline templates for backward compatibility
  - List templates for sequences
  - Hot-reload on template changes
- **Sort Engine**:
  - Regex sequences for groups and channels
  - `order: none` to preserve source order
  - Filter-based sorting — sort only specific entries
  - Named capture groups for multi-level sorting (`c1`, `c2`, `c3`)
- **Accent-Independent Matching**: `match_as_ascii` — "Cinema" matches "Cinéma"
- **Deunicoding**: On-the-fly Unicode normalization in filters and value comparisons
- **Output Filters**: Apply filters to the final playlist state after all transformations
- **Favorites System**: Explicit `add_favourite(group_name)` script function for bouquet management

### 7. Local Media Library — Integrate Your Own Movies & Series

- Recursive directory scanning for local video files
- Automatic classification (Movie vs. Series)
- Multi-source metadata: NFO files, TMDB API, filename parsing
- Incremental scanning — only new or changed files are processed
- Integration into Xtream API and M3U playlists — local content served like IPTV channels
- Metadata formats including NFO support
- Scheduled library scans — automatic updates via cron
- Episode backgrounds with direct TMDB image URLs
- Virtual ID management for stable assignment

### 8. Metadata Resolution & Stream Probing

- **Background Metadata Queue**: Metadata resolution and stream analysis run in the background when provider
  connections  
  are idle — prevents "No Connections" errors for active users
- **Metadata Fairness**: Configurable ratio between resolve and probe tasks — no probe starvation
- **FFprobe Integration**: Automatic detection of codec, resolution, HDR (HDR10/HLG/Dolby Vision), audio channels
- **FFprobe respects provider limits**: If no slot is available, the item is skipped — zero risk of provider bans
- **TMDB Integration**: Automatic lookup of missing TMDB IDs and release dates
- **TMDB No-Match Cooldown**: Prevents infinite loops on items with no TMDB match
- **Quality Tagging**: Automatic STRM tags like `[2160p 4K HEVC HDR TrueHD 7.1]`
- **Flat Grouping**: Multi-version merge (e.g. 4K + 1080p) in a single folder — compatible with Jellyfin/Emby
  multi-version feature
- **Probe Priority**: Configurable priority for probe tasks — provider slots stay free for real users
- **Metadata Retry State**: Persistent retry/cooldown state per item in a dedicated database
- **No-Change Cache**: Deduplication cache prevents unnecessary re-resolution of unchanged items
- **Live Stream Probing**: Periodic re-probing of live streams with configurable interval

### 9. Role-Based Access Control (RBAC) — Enterprise-Grade Security

- **14 permissions** across 7 domains (config, source, user, playlist, library, system, epg)
- Each permission with independent `.read` and `.write` grants
- Custom groups via `groups.txt` — define your own roles (e.g. `viewer`, `source_manager`)
- User-to-group assignment — one user can belong to multiple groups
- Union-based permission resolution — group permissions stack additively
- Compact bitmask (`u16`) in JWT — zero file I/O per request, single-instruction bitwise checks
- Password-version tracking in JWT — automatic token invalidation on password change
- RBAC admin panel in the Web UI with tabbed user/group management and permission checkbox grid
- Built-in `admin` group — always full permissions, cannot be deleted
- Backward compatible — existing `user.txt` files work without changes

### 10. Web UI — Full Control in the Browser

- **Dashboard**: System status, active streams, CPU usage, provider connections in real-time via WebSocket
- **Source Editor**: Global input management with drag & drop, block selection, batch mode, scroll wheel support
- **Playlist Explorer**: Tree and gallery view for channels with EPG timeline and search
- **Download & Recording Manager**: Provider-aware VOD downloads and live recordings with retries, fairness, and RBAC-controlled actions
- **Config Editor**: Direct editing of config.yml, source.yml, mapping.yml in the browser
- **User Management**: API users with category selection, priority, soft-priority, normal/soft connection limits, auto-generated credentials
- **RBAC Admin Panel**: Tabbed user/group management, permission checkbox grid, write-without-read warnings
- **Stream Table**: Real-time stream monitoring with copy-to-clipboard, bandwidth metrics, episode titles
- **EPG View**: Timeline with channels, now-line, program details
- **Messaging Config View**: Discord, Telegram, Pushover, REST webhook configuration with template editor
- **Multiple Themes**: Dark/bright and additional themes available
- **GeoIP Country Flags**: Country flags displayed when GeoIP is active
- **Playlist User Login**: Playlist users can select their own groups/bouquets
- **Resource Proxy**: Channel logos and images loaded via authenticated same-origin endpoints — HTTP upstream assets
  render behind HTTPS frontends
- **Mobile-friendly**: Responsive design for all screen sizes

### 11. EPG (Electronic Program Guide)

- **Multi-Source EPG**: Multiple EPG sources with priorities — best coverage through combination
- **Auto-EPG**: Automatic EPG URL generation from providers
- **Smart Match**: Fuzzy matching with configurable threshold for automatic channel-to-EPG assignment
- **XMLTV Timeshift**: Full timezone support (`Europe/Paris`, `America/New_York`, `-2:30`, `+0:15`, etc.) with automatic
  DST handling
- **EPG Memory Cache**: In-memory cache for fast Web UI and short-EPG lookups — reduces disk access
- **Logo Override**: EPG logos can override channel logos
- **Async Processing**: EPG streamed and processed asynchronously — minimal memory overhead even with large guides
- **EPG Icon Proxy**: HTTP upstream assets rendered through HTTPS frontend
- **Strip & Normalize**: Configurable terms and regex for better channel matching
- **EPG Title Synchronization**: Automatic sync after playlist updates

### 12. Notifications & Monitoring

- **Telegram**: Bot notifications with markdown support and thread support (`chat-id:thread-id`)
- **Discord**: Webhook notifications with Handlebars templates
- **Pushover**: Push notifications
- **REST Webhooks**: Custom HTTP methods, headers, Handlebars templating
- **Watch Notifications**: Real-time alerts on group changes in playlists
- **Processing Stats**: Automatic notification after playlist updates with statistics and processing duration
- **Per-Message Templates**: Individual Handlebars templates per message type (Info, Stats, Error, Watch) and channel
- **Template Loading**: Templates from files or HTTP/HTTPS URIs with automatic discovery
- **Typed Messaging Pipeline**: Strictly typed pipeline instead of raw JSON strings — robust and maintainable

### 13. Scheduling & Automation

- **Cron-based scheduler**: Multiple schedules with optional target selection
- **Scheduled library scans**: Automatic local library scans alongside playlist updates
- **Hot config reload**: Configuration changes detected and applied automatically — no restart needed
- **Config file watcher**: Monitors config.yml, source.yml, mapping.yml, api-proxy.yml, template files
- **Auto-update on boot**: Optional playlist update on startup
- **Staged inputs**: Side-loading — load metadata from staged input, serve streams from provider
- **Panel API integration**: Auto-renew expired provider accounts or provision new ones to maintain minimum valid
  accounts
- **Playlist caching**: Configurable cache duration for provider playlists (`60s`, `5m`, `12h`, `1d`)

### 14. Complete Xtream Codes API Implementation

- Player API (Live, VOD, Series streams)
- Category listings with icons
- VOD Info & Series Info with episodes and metadata
- Catchup / Timeshift API with session tracking
- XMLTV / EPG API
- Panel API for account management
- POST and GET request support
- Series/Catchup lookup with virtual ID support
- Bandwidth and connection info in user info response
- Custom server message support
- Multi-server configuration with different protocols and ports

### 15. Security

- **Argon2 password hashing**: Industry standard for password storage
- **JWT authentication**: Compact bitmask encoding with password-version tracking for automatic token invalidation
- **Rate limiting**: Per-IP rate limiting with configurable burst and period
- **Content Security Policy**: Configurable CSP headers
- **SSL/TLS support**: Configurable including `accept_insecure_ssl_certificates` option
- **Proxy support**: HTTP, HTTPS, SOCKS5 proxies for all outgoing requests
- **Header stripping**: Configurable removal of referer, Cloudflare, and X-headers
- **Rewrite secret**: Mandatory secret for stable resource URLs in reverse proxy mode

### 16. Operations & Deployment

- **Docker**: Alpine and Scratch images — minimal image size
- **Docker Compose templates**: traefik, crowdsec, gluetun/socks5 templates ready to use
- **Zero-downtime config reload**: `ArcSwap<Config>` for atomic configuration swaps without interruption
- **Disk-based processing**: Playlist processing from disk instead of RAM — massively reduced memory consumption
- **CLI mode**: One-shot processing without a server — ideal for scripting and CI/CD
- **Server mode**: Long-running HTTP server with background tasks
- **Healthcheck endpoint**: `/api/v1/status` for Docker/uptime monitoring
- **SSDP discovery**: HDHomeRun auto-discovery via SSDP and proprietary UDP protocol (port 65001)
- **Database viewer**: CLI flags to inspect internal databases
- **Environment variables**: `${env:VAR}` interpolation in all config files
- **Default User-Agent**: Configurable default user-agent for all outgoing requests

## 🎯 Target Audiences

### For IPTV Enthusiasts

- Merge multiple providers into one unified playlist
- Filter, rename, sort channels — exactly the way you want
- Automatic EPG assignment with fuzzy matching
- Kodi/Plex/Jellyfin integration via STRM or HDHomeRun

### For Self-Hosted & Homelab Users

- Single Docker container — no database stack needed
- Runs on Raspberry Pi and tiny VPS instances
- Minimal resource usage thanks to Rust and disk-based processing
- **Runs 24/7 for months with rock-solid stability and near-zero maintenance**
- Traefik/Crowdsec/Gluetun templates ready to deploy

### For Multi-User Operations

- User management with connection limits and priority levels
- RBAC with 14 granular permissions across 7 domains
- Provider slot sharing for live streams
- Custom fallback videos for professional support
- Panel API integration for automated account management

### For Developers & Power Users

- Mapper DSL for arbitrary transformations
- Template system for reusability
- REST API for automation
- CLI mode for scripting and CI/CD
- Database viewer for debugging and analysis

## 🐋 Docker Container Templates

- traefik template
- crowdsec template
- gluetun/socks5 template
- tuliprox (incl. traefik) template

`> ./docker/container-templates`

## Want to join the community

[Join us on Discord](https://discord.gg/gkzCmWw9Tf)

## License

See [`LICENSE`](https://github.com/euzu/tuliprox/blob/develop/LICENSE).

## Quick start

Install with docker the latest image.

```docker
services:
  tuliprox:
    container_name: tuliprox
    image: ghcr.io/euzu/tuliprox-alpine:latest
    working_dir: /app
    volumes:
      - /home/tuliprox/config:/app/config
      - /home/tuliprox/data:/app/data
      - /home/tuliprox/cache:/app/cache
    environment:
      - TZ=Europe/Paris
    ports:
      - "8901:8901"
    restart: unless-stopped
```

Open the Browser and continue setup.

## Project layout

- `backend/`: main server and processing pipeline
- `frontend/`: Yew Web UI
- `shared/`: DTOs and shared logic
- `config/`: example configuration
- `docs/`: Markdown source for the project documentation

## Documentation

The detailed documentation lives in Markdown under `docs/` and is meant to be rendered as a static site.

- Docs source: [`docs/src/index.md`](docs/src/index.md)
- Build static docs: `make docs`
- Serve generated docs with the Web UI build at `/static/docs/`

Main entry points:

- **[Getting Started](docs/src/getting-started.md)**
- [Core Features](docs/src/features.md)
- [Build & Deploy](docs/src/build-and-deploy.md)
- **[Installation](docs/src/installation.md)**
- **[Configuration Overview](docs/src/configuration/overview.md)**
  - [Main Config](docs/src/configuration/config.md)
  - [Sources & Targets](docs/src/configuration/source.md)
  - [API Proxy](docs/src/configuration/api-proxy.md)
  - [Streaming & Proxy Behavior](docs/src/configuration/reverse-proxy.md)
  - [Mapping & Templates](docs/src/configuration/template.md)
- [Examples & Recipes](docs/src/examples-recipes.md)
- [Operations & Debugging](docs/src/operations-debugging.md)
- [Troubleshooting & Resilience](docs/src/troubleshooting.md)

## Documentation strategy

The recommended format is:

- source in Markdown
- generated as static HTML
- shipped together with the frontend/web root

For this repository, `mdBook` is the best fit:

- Markdown stays easy to edit in Git
- static HTML output is simple to host
- it fits a Rust project better than a Node-heavy doc stack
- navigation and search come out of the box
