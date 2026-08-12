## **tuliprox** - A Powerful IPTV Proxy & Playlist Processor

`tuliprox` is a high-performance IPTV proxy and playlist processor written in Rust 🦀.
It unifies M3U/M3U8 playlists, Xtream providers, Stalker/Ministra portals, Plex, Emby, Jellyfin and local media,
reshapes them into clean outputs, and serves them to IPTV players and media clients from one place.

![tuliprox logo](https://github.com/user-attachments/assets/8ef9ea79-62ff-4298-978f-22326c5c3d02)

## Want to join the community

[Join us on Discord](https://discord.gg/gkzCmWw9Tf)

## License

See [`LICENSE`](https://github.com/euzu/tuliprox/blob/develop/LICENSE).

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

- Versioned 4 KiB Slotted Page format with checksummed headers, pages, cells, and overflow chains
- Adaptive LZ4 compression and overflow pages for compact variable-size values
- Mmap-backed streaming scans without loading the complete database into memory
- Shared query snapshots reuse mmap mappings, validated pages, and decoded internal routes
- WAL-protected in-place updates with `Immediate` and `Batch` flush policies
- Fully built replacements and compaction results are verified before atomic publication
- Typed startup migration upgrades recognized legacy v1/v2 databases to v3
- Identity- and generation-bound sorted indexes accelerate playlist-order streaming
- String interning (`Arc<str>`) for playlist entries reduces memory footprint
- Page-local updates, tombstone reuse, free-page reuse, and explicit compaction reclaim disk space

### 3. Input Sources — Bring Everything Together

- **Xtream Codes**: Import Live, VOD, Series, Catchup and EPG metadata through the native provider API. Alias pools and
  CSV batch inputs make large account sets manageable.
- **M3U/M3U8**: Load remote or local playlists with custom headers, credentials and EPG sources. CSV batch inputs cover
  providers that expose multiple playlist URLs.
- **Stalker & Ministra**: Process complete portal catalogs with MAC, credentials or combined authentication, configurable
  MAG profiles, reliable `create_link` playback resolution, resumable refreshes and per-alias CSV configuration.
- **Local Media Library**: Scan movies and series directly from disk, enrich them with NFO or TMDB metadata and publish
  them through the same M3U, Xtream and STRM outputs as provider content.
- **Plex, Emby & Jellyfin**: Import selected movie and TV libraries directly from an existing media server, refresh them
  manually or on a schedule and resolve protected playback and artwork through Tuliprox.
- **Staged Sources**: Overlay a prepared Live, VOD or Series catalog onto another provider while keeping the original
  provider responsible for stream delivery.

### 4. Four Output Formats — One Tool to Rule Them All

| Format               | Description                                                               |
|----------------------|---------------------------------------------------------------------------|
| **M3U/M3U8**         | For all IPTV players (VLC, Tivimate, iMPlayer, etc.)                      |
| **Xtream Codes API** | Full Xtream API with Live, VOD, Series, Catchup, EPG                      |
| **HDHomeRun**        | Emulation for Plex, Jellyfin, Emby — auto-discovery via SSDP              |
| **STRM**             | Kodi/Jellyfin/Emby compatible with multi-version support and quality tags |

Generate all four formats simultaneously from the same source — one setup, every platform covered.

### 5. Multi-Tenant IPTV Edge Gateway

- **Advertised Server Profiles**: Define multiple LAN or public reverse-proxy profiles with independent protocol, host,
  port, path, timezone and Xtream welcome message, then assign one profile to each user.
- **Target-Based User Access**: Bind every account to a curated target and expose it through M3U or the Xtream Codes API
- **Per-User Delivery Policies**: Choose reverse proxy or redirect behavior, Live/VOD/Series access, category selection,
  connection limits and priorities independently for every account
- **Multiple Deployment Profiles**: Advertise LAN and public HTTPS endpoints without duplicating playlists or provider
  configuration
- **Flexible User Storage**: Keep accounts in versionable YAML or migrate them to the integrated user database and
  manage them from the Web UI
- **Hot-Reloaded Gateway Configuration**: Apply server, user and access-policy changes without restarting the service

### 6. Reverse Proxy, Shared HLS & Stream Management — Enterprise-Grade

- **Reverse Proxy Mode**: Streams are proxied through Tuliprox — provider URLs stay invisible to end users
- **Redirect Mode**: Lightweight redirection for resource-efficient operation
- **Shared MPEG-TS Streams**: One provider stream shared across multiple users — saves valuable provider slots
- **Shared HLS Sessions**: Multiple viewers reuse one server-side HLS session while retaining individual access leases,
  user limits and accounting
- **HLS Segment Cache & Prefetch**: Fetch manifests, segments and initialization maps once, serve them locally and
  prefetch upcoming segments within configurable disk and concurrency budgets
- **HLS Recovery Tools**: Optional manifest recovery, bounded segment repair and corrupt-segment diagnostics for
  difficult upstream streams
- **HLS Session Management**: Short-lived provider reservations for HLS/Catchup without blocking real slots
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
- **Channel-Switch Friendly Reservations**: Instant takeover on channel switch — no TTL wait
- **Custom Stream Response Timeout**: Auto-stop fallback streams after configurable duration
- **Buffer Reuse**: Reusable serialization buffers minimize heap allocations during streaming
- **Universal Catchup**: Serve provider catchup/archive as plain M3U with `.ts` segments proxied through Tuliprox —
  any player, any timeline, no vendor lock-in
- **Smart Background Work**: Skip metadata resolution and probing for entries you don't need — save CPU and
  provider slots on huge providers that only need partial coverage

### 7. Provider Connectivity, Failover & VPN Routing

- **Provider URL Failover**: Automatic rotation on errors (5xx, timeout) — seamless switching, no viewer disruption
- **Configurable Provider URL Start Policy**: Per provider, choose whether new requests resume from the last working URL or  
  always restart from the first URL
- **`provider://` URL Scheme**: Reference providers by name — Tuliprox resolves to the active URL automatically
- **DNS-Aware Connection Routing**: Provider DNS resolved asynchronously and cached
- **Resolved DNS Persistence**: Resolved IPs persisted separately — no source config overwrite during hot reloads
- **Outgoing HTTP/HTTPS/SOCKS5 Proxy**: Route playlist downloads, metadata requests, probing and proxied streams through
  a corporate proxy, VPN gateway or Gluetun container
- **Public IP Verification**: Display the effective public IPv4/IPv6 in the Web UI to confirm that VPN and proxy routing
  are working as intended
- **Provider Aliases**: Manage multiple accounts from the same provider with different credentials
- **Batch Input**: Xtream, M3U and Stalker batch inputs via CSV files for mass provider management
- **Staged Cluster Source Routing**: Per-cluster decision whether Live/VOD/Series comes from staged input, main input,
  or is skipped entirely

### 8. Multi-Source Merging & Advanced Processing Pipeline

- Merge multiple input sources (M3U, Xtream, Stalker, Plex, Emby, Jellyfin, Local Library) into a single target
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
- **Auto-Curated Trakt Bouquets**: Drop in Trakt Charts and serve "Trending Now" and "Most Popular" as live Xtream
  categories — your playlist refreshes itself with what people are actually watching, no manual curation

### 9. Local Media Library — Integrate Your Own Movies & Series

- Recursive directory scanning for local video files
- Automatic classification (Movie vs. Series)
- Multi-source metadata: NFO files, TMDB API, filename parsing
- Incremental scanning — only new or changed files are processed
- Integration into Xtream API and M3U playlists — local content served like IPTV channels
- Metadata formats including NFO support
- Scheduled library scans — automatic updates via cron
- Episode backgrounds with direct TMDB image URLs
- Virtual ID management for stable assignment

### 10. Metadata Resolution & Stream Probing

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

### 11. Role-Based Access Control (RBAC) — Enterprise-Grade Security

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

### 12. Web UI — Full Control in the Browser

- **Dashboard**: System status, active streams, CPU usage, provider connections in real-time via WebSocket
- **Source Editor**: Dedicated forms for M3U, Xtream, Stalker, Plex, Emby, Jellyfin and local-library inputs, with
  drag & drop, block selection and batch mode
- **Playlist Explorer**: Tree and gallery view for channels with EPG timeline and search — text or regex, optionally
  scoped to specific fields (group, title, name, url)
- **Download & Recording Manager**: Provider-aware VOD downloads and live recordings with retries, fairness, and RBAC-controlled actions
- **Config Editor**: Direct editing of config.yml, source.yml, mapping.yml in the browser
- **User Management**: API users with category selection, priority, soft-priority, normal/soft connection limits, auto-generated credentials,
  and reusable **plans** (capability tiers with cluster access, connection limits, and admin-enforced content filters)
- **Network Access Policy UI**: API-user network restrictions can be configured with CIDR and GeoIP country rules, including
  the global GeoIP-unavailable `deny`/`allow` policy.
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

**UI/UX Highlights:**

- **Status Health Banner**: Single green/amber/red health indicator in the header aggregates realtime connection,
  backend, and provider capacity with hover breakdown and click-through to the Stats view
- **Live Metric Sparklines**: Stats cards show interactive time-series sparklines for CPU, memory, network, active
  users, and active user connections — hover shows cursor + tooltip, system metrics sampled every 2 s for responsive
  charts
- **Bookmarkable Views (Deep Linking)**: The active view is reflected in the URL hash (`#stats`,
  `#source_editor`) so views can be bookmarked, shared, and navigated directly; browser back/forward works as
  expected
- **At a glance**: Single green/amber/red health banner in the header aggregates every signal that matters
  (realtime connection, backend, provider capacity), with hover breakdown and click-through to detailed Stats.
  Live metric sparklines for CPU, memory, network, active users and connections — hover for cursor + tooltip,
  sampled every 2 s so the charts never feel stale.
- **A UI that feels right**: Micro-interactions (cards lift, buttons ripple, chevrons rotate), animated
  theme transitions, debounced filter input that stays responsive on huge expressions, shift+click range
  selection in the category editor, guided empty states that tell you what to do next, paged tables with
  localized empty messages. Every effect honors `prefers-reduced-motion: reduce`.
- **Works the way you work**: Bookmarkable views via URL hash (`#stats`, `#source_editor`) so you can share
  links and the browser back/forward buttons just work. Persisted sidebar state and table page size
  survive reloads. Sessions auto-logout cleanly when the JWT expires — no mysterious 401s.
- **For everyone**: UI languages with full RTL  support (drop a file in `assets/i18n` and a new language is live — no rebuild),
  a recoverable error boundary that keeps one misbehaving view from taking the app down, and confirmation dialogs before any
  destructive download/recording action.

### 13. EPG (Electronic Program Guide)

- **Multi-Source EPG**: Multiple EPG sources with priorities — best coverage through combination
- **ICS Calendar Sources**: Convert iCalendar events into XMLTV programmes, assign them through Smart Match and
  optionally fill schedule gaps with configurable dummy blocks
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

### 14. Notifications & Monitoring

- **Telegram**: Bot notifications with markdown support and thread support (`chat-id:thread-id`)
- **Discord**: Webhook notifications with Handlebars templates
- **Pushover**: Push notifications
- **REST Webhooks**: Custom HTTP methods, headers, Handlebars templating
- **Watch Notifications**: Real-time alerts on group changes in playlists
- **Processing Stats**: Automatic notification after playlist updates with statistics and processing duration
- **Per-Message Templates**: Individual Handlebars templates per message type (Info, Stats, Error, Watch) and channel
- **Template Loading**: Templates from files or HTTP/HTTPS URIs with automatic discovery
- **Typed Messaging Pipeline**: Strictly typed pipeline instead of raw JSON strings — robust and maintainable
- **Never Fill Up Unnoticed**: Disk-usage alerts through your existing Telegram/Discord/Pushover channels
  — opt-in, configurable thresholds, hourly re-arm so a stuck-at-95% disk keeps nagging until you actually
  fix it

### 15. Scheduling & Automation

- **Cron-based scheduler**: Multiple schedules with optional target selection
- **Scheduled library scans**: Automatic local library scans alongside playlist updates
- **Hot config reload**: Configuration changes detected and applied automatically — no restart needed
- **Config file watcher**: Monitors config.yml, source.yml, mapping.yml, api-proxy.yml, template files
- **Auto-update on boot**: Optional playlist update on startup
- **Staged inputs**: Side-loading — load metadata from staged input, serve streams from provider
- **Panel API integration**: Auto-renew expired provider accounts or provision new ones to maintain minimum valid
  accounts
- **Playlist caching**: Configurable cache duration for provider playlists (`60s`, `5m`, `12h`, `1d`)

### 16. Management REST API — Automate Everything

- **Configuration Management**: Read and update application, source and API-gateway configuration remotely
- **Processing Control**: Trigger playlist refreshes, target-scoped updates, library scans and GeoIP database updates
- **Playlist Inspection**: Preview Live, VOD and Series content before publishing it to users
- **Transfer Automation**: Queue, pause, resume, retry and cancel downloads or live recordings from external tools
- **Operational Visibility**: Query status, active streams, stream history, QoS snapshots and system information
- **Script-Friendly Formats**: Use JSON for broad compatibility or CBOR on supported endpoints for compact responses

### 17. Complete Xtream Codes API Implementation

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

### 18. Security

- **Argon2 password hashing**: Industry standard for password storage
- **JWT authentication**: Compact bitmask encoding with password-version tracking for automatic token invalidation
- **Rate limiting**: Per-IP rate limiting with configurable burst and period
- **Content Security Policy**: Configurable CSP headers
- **API User Network Access Restrictions**: Per-user `network_access` rules can restrict API proxy accounts by IPv4/IPv6
  CIDR ranges and/or GeoIP country codes. Matching any configured CIDR or country allows the request; otherwise the
  request is denied before it is forwarded upstream.
- **GeoIP-Unavailable Policy**: Country-based network restrictions default to `deny` when GeoIP is disabled, missing, or not
  loaded. Operators can explicitly accept that risk with `reverse_proxy.geoip.unavailable_policy: allow`; CIDR-only misses,
  unknown countries, and country mismatches still deny.
- **SSL/TLS support**: Configurable including `accept_insecure_ssl_certificates` option
- **Header stripping**: Configurable removal of referer, Cloudflare, and X-headers
- **Rewrite secret**: Mandatory secret for stable resource URLs in reverse proxy mode

### 19. Operations & Deployment

- **Docker**: Alpine and Scratch images — minimal image size
- **Docker Compose templates**: traefik, crowdsec, gluetun/socks5, iptv-org/epg templates ready to use
- **One-Click Dev Setup**: Open in GitHub Codespaces or VS Code Dev Containers and start hacking — Rust
  toolchain, WASM targets, and every build tool pre-configured
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
- Kodi/Jellyfin/Emby integration via STRM
- Plex integration via HDHomeRun

### For Self-Hosted & Homelab Users

- Single Docker container — no database stack needed
- Runs on Raspberry Pi and tiny VPS instances
- Minimal resource usage thanks to Rust and disk-based processing
- Route all upstream traffic through a VPN or SOCKS5 gateway and verify the public IP from the Web UI
- **Runs 24/7 for months with rock-solid stability and near-zero maintenance**
- Traefik/Crowdsec/Gluetun/IPTV-org-epg templates ready to deploy

### For Multi-User Operations

- Publish multiple virtual IPTV endpoints and plans from one Tuliprox instance
- User management with connection limits and priority levels
- RBAC with 14 granular permissions across 7 domains
- Provider slot sharing for live streams
- Custom fallback videos for professional support
- Panel API integration for automated account management

### For Developers & Power Users

- Mapper DSL for arbitrary transformations
- Template system for reusability
- Management REST API for configuration, processing, transfers and operational automation
- CLI mode for scripting and CI/CD
- Database viewer for debugging and analysis

## 🐋 Docker Container Templates

- [traefik](docker/container-templates/traefik/) template
- [crowdsec](docker/container-templates/crowdsec/) template
- [gluetun/socks5](docker/container-templates/gluetun/) template
- [iptv-org-epg](docker/container-templates/iptv-org/) template
- [tuliprox](docker/container-templates/tuliprox/) (incl. traefik) template

`> ./docker/container-templates`

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
