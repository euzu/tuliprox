# **tuliprox** — Self-Hosted IPTV Proxy, Media Gateway & Playlist Processor with Web UI

![tuliprox logo](https://github.com/user-attachments/assets/8ef9ea79-62ff-4298-978f-22326c5c3d02)
`tuliprox` is a high-performance, self-hosted media gateway for bringing IPTV providers, media servers, playlists, EPG data,  
and local media libraries together behind one clean and controllable interface.

Instead of configuring every player against every upstream source, Tuliprox sits in the middle: it imports your authorized  
media sources, normalizes and enriches their metadata, filters and reorganizes the catalog, manages provider connections,  
and publishes the result in the formats your clients already understand.

**One service. Multiple sources. Multiple users. Multiple output formats. Full control — from a complete Web UI.**

Everything is configured in the browser: a setup wizard for the first start, editors for sources, targets, users,
mappings, failover groups, schedules, notifications, recordings and plugins, plus live dashboards, logs and stream
views. YAML files stay available for people who prefer them, but you never have to touch one.

> **Legal Notice**
>
> `tuliprox` does not provide, host, sell, or distribute media content or access credentials.
> Users are solely responsible for ensuring that they have the necessary rights and authorization to access, process,
> proxy, record, or redistribute any media sources configured with `tuliprox`.
>
> The software is intended for use with legally obtained and properly authorized content and services.

## ✨ Why Tuliprox?

Tuliprox is built for more than playlist conversion. It is designed to become the central media gateway for a self-hosted setup:

- **Configure everything in the browser** — a guided setup wizard and a full Web UI cover every feature, from sources and
  users to failover groups, plugins, recordings and monitoring. No YAML required.
- **Unify fragmented sources** — combine Xtream, M3U/M3U8, Stalker/Ministra, Plex, Emby, Jellyfin, Silo, local media,
  staged catalogs, plugin inputs such as YouTube, and EPG data in one place.
- **Publish once, consume everywhere** — expose the same processed catalog as M3U, Xtream Codes API, HDHomeRun, or STRM without  
  maintaining separate setups.
- **Protect provider connections** — share streams, enforce connection limits, prioritize users, recover from upstream problems,  
  and avoid wasting provider slots on unnecessary background work.
- **Never lose a channel** — failover groups merge the same channel from several providers or accounts and switch to
  another source while the viewer keeps watching (MPEG-TS and HLS).
- **Shape the catalog your way** — filter, rename, map, sort, merge, deduplicate, curate, and enrich content before it reaches users.
- **Operate multiple users safely** — assign targets, plans, output clusters, network restrictions, priorities, limits, and  
  server profiles per user.
- **Extend it safely** — sandboxed WebAssembly plugins add inputs, media servers, outputs, filters, EPG matchers,
  notification channels, Web UI panels and AI tools, installable from a built-in marketplace.
- **Let AI assistants help** — an optional MCP server gives Claude, Cursor and other assistants read access to status,
  streams, playlists and EPG diagnostics, under your permissions.
- **Run it like infrastructure** — health and readiness probes, durable notifications, stream history, QoS snapshots, quality  
  guards, hot reloads, audit events, and optional runtime watchdog support are built in.
- **Keep the stack small** — Rust, a single application, embedded storage, and no mandatory external database.
- **Stay in control of your data** — self-hosted configuration, metadata, user management, recordings, and operational state.
- **Integrate custom IPTV players** — dedicated player APIs give external apps source switching, personal DVR, stream management,
  and joinable stream discovery under user content filters.

## 🚀 At a Glance

| Area           | What Tuliprox gives you                                                                                      |
|:---------------|:-------------------------------------------------------------------------------------------------------------|
| **Web UI**     | Setup wizard, editors for every setting, dashboard, playlist explorer, live logs, stream and QoS views       |
| **Inputs**     | Xtream, M3U/M3U8, Stalker/Ministra, Plex, Emby, Jellyfin, Silo, local libraries, staged sources, plugins     |
| **Outputs**    | M3U/M3U8, Xtream Codes API, HDHomeRun, STRM                                                                  |
| **Processing** | Filtering, mapping DSL, sorting, templates, EPG matching, metadata enrichment, curation                      |
| **Streaming**  | Reverse proxy, redirects, shared MPEG-TS/HLS, HLS cache/prefetch, catchup proxying, live failover groups     |
| **Users**      | Multi-user gateway, plans, connection limits, priorities, output clusters, category access, network policies |
| **Player API** | User `/api/v1/me` routes, dedicated switches, source switching, split, progress, events, DVR, rules          |
| **Plugins**    | Sandboxed WebAssembly plugins, marketplace, SDK, custom inputs, media servers, outputs, UI panels, MCP tools |
| **AI**         | Optional MCP server for AI assistants, read-only by default, with the REST API's permissions                 |
| **Operations** | REST API, scheduler, health/readiness probes, notifications, QoS, stream history, DVR, watchdog              |
| **Storage**    | Embedded B+Tree engine, WAL protection, mmap scans, compression, compaction — no external DB required        |
| **Deployment** | Docker, Docker Compose templates, Raspberry Pi, NAS, VPS, x86 and ARM                                        |

## 🏆 Key Features

### 1. Web UI — Operate Tuliprox Without Living in YAML

Tuliprox ships a complete, responsive Web UI. Every feature described on this page can be set up, changed and
monitored in the browser; the YAML files are written for you.

#### Setup and configuration

- **Setup wizard** on the first start: main settings, sources, users, plugins and MCP in a few guided steps
- **Source Editor** — a visual block editor for M3U, Xtream, Stalker, Plex, Emby, Jellyfin, Silo, local-library,
  staged and plugin inputs and their targets, with drag & drop, block selection, batch editing and bouquet
  whitelist/blacklist editing
- One **media server** block: pick Plex, Emby, Jellyfin or Silo and only the fields for that server appear
- **Failover Groups editor** to review automatic groups, correct them and build your own
- Editors for every configuration area: main settings, reverse proxy and streaming, connection admission, schedules,
  messaging and notification templates, recordings, metadata and probing, HDHomeRun devices, logging, the Web UI
  itself, MCP and panel API
- **User Management** with plans, connection limits, priorities, category access, output clusters and network
  policies, plus **RBAC Administration** for users, groups and permission grants
- **Plugin Manager** with a marketplace, install review of the requested capabilities, updates, settings forms,
  per-plugin logs and plugin-provided pages and panels

#### Operations and monitoring

- **Dashboard** with backend status, active streams, system metrics, provider capacity and live sparklines
- **Health banner** with green/amber/red capacity and backend state
- **Maintenance card** that turns red while inputs are in maintenance, with a dialog to put inputs into or take them
  out of maintenance with a remark
- **Input-focused Update view** with per-input actions, affected-target selection, status, last update time and
  capability-aware Update/Refresh/Force Update/Rescan actions
- **Update Details** explain where data came from, which quality guards were applied, processing order, target
  results and technical versus quality failures; the status survives a browser reload
- **Playlist Explorer** with tree/gallery views, EPG timeline, text/regex and field-scoped search
- **Manual item overrides**: correct single channels, movies or series in the Playlist Explorer — for the whole input
  or for one target — without writing mapper rules, with validation, conflict checks and orphan cleanup
- **Playlist Stats** with per-target content counts, input-to-output ratios, largest groups, catch-up channels, EPG
  source health and unmapped live channel CSV export
- **Stream View** with bandwidth, transferred bytes, player information, comments and EPG details
- **Stream History & QoS** views for reliability analysis
- **Download & Recording Manager** with retries, queue controls, fairness and RBAC-protected actions
- **Live Logs** as a standalone view

#### UX and accessibility

- Multiple themes, including a color-vision-deficiency-friendly option
- Runtime-discovered UI languages with RTL support
- Resizable panes and personal, persisted table layouts
- Bookmarkable views via URL hash and working browser back/forward navigation
- Responsive design for desktop and mobile
- Guided empty states, recoverable per-view error boundaries and confirmation before destructive actions
- Keyboard-friendly dialogs and menus, screen-reader improvements, descriptive image alt text and reduced-motion
  support

**Benefit:** advanced functionality stays available to power users without forcing routine administration onto the
command line or into configuration files.

### 2. Fast, Lightweight, and Built for 24/7 Operation

Tuliprox is written in Rust and designed to keep the runtime footprint small even when the catalog and number of connected clients grow.

- Single native application — no Python, Node.js, Java, or mandatory database server
- Low CPU and memory overhead, including disk-based playlist processing for memory-constrained systems
- Runs comfortably on small servers and can operate with as little as 256 MB RAM depending on workload
- Tokio-based asynchronous I/O for highly concurrent streaming and background work
- Runs on Raspberry Pi, NAS, small VPS instances, and x86/ARM systems
- Hot configuration reloads reduce the need for service restarts
- Optional **runtime liveness watchdog** can detect a wedged async runtime and capture diagnostics; it can optionally  
  trigger a restart when explicitly enabled
- Dedicated `/healthcheck` and `/ready` endpoints separate process liveness from actual provider capacity

**Benefit:** Tuliprox can stay in the background as infrastructure instead of becoming another heavyweight service stack  
you constantly have to maintain.

### 3. Bring All Your Media Sources Together

Use different upstream technologies without forcing every downstream player to understand them.

- **Xtream Codes** — Live, VOD, Series, Catchup, EPG metadata, provider aliases, and CSV batch accounts
- **M3U/M3U8** — remote or local playlists with headers, credentials, EPG sources, and CSV batch inputs
- **Stalker & Ministra** — full portal catalogs with MAC, credential or combined authentication, configurable MAG profiles,  
  `create_link` playback resolution, resumable refreshes, and per-alias configuration
- **Plex, Emby, Jellyfin & Silo** — import selected movie and TV libraries, each as its own category, and resolve protected playback  
  and artwork through Tuliprox
- **Plugin inputs** — YouTube channels, playlists and live streams through the official `youtube-input` plugin, and any
  other media server or source through your own WebAssembly plugin
- **Local Media Library** — scan movies and series from disk and enrich them with NFO or TMDB metadata
- **Staged Sources** — overlay a prepared Live, VOD, or Series catalog while keeping the original provider responsible for  
  actual stream delivery

Tuliprox can also stream large provider catalogs incrementally instead of requiring the complete catalog to be buffered in  
memory before processing starts.

**Benefit:** change providers, combine sources, or migrate clients without rebuilding your whole media setup around one vendor-specific API.

### 4. WebAssembly Plugins — Extend Tuliprox Without Forking It

Plugins add features to Tuliprox without touching its code. They run sandboxed in WebAssembly inside the single
Tuliprox binary — no Python, Node.js or other runtime to install.

- **Marketplace** in the Web UI: browse by category, review the requested capabilities, install, update and configure
- **Input plugins** bring new sources, for example the official `youtube-input` plugin for YouTube channels, playlists
  and live streams
- **Media server plugins** connect any further media server: the plugin delivers the library, Tuliprox handles
  storage, series, outputs, playback with seeking and artwork exactly like for Plex, Emby, Jellyfin and Silo
- **Output plugins** render additional playlist formats
- **Pipeline hooks** for filtering, renaming, mapping, sorting, deduplication, channel numbering, metadata, EPG
  loading, matching and enrichment, stream headers and managed channels
- **Notification channels** such as Matrix, Discord, ntfy or Gotify as plugins (source examples in `plugins/examples/`)
- **Custom UI**: settings forms, source-editor tiles, dashboard cards, explorer actions and pages inside the Web UI
- **MCP tools**: plugins can offer their own tools to AI assistants
- **Scheduled tasks**, HTTP routes, a key-value store, an image cache and controlled network access
- **Safe by design**: capabilities need explicit approval; every call has memory, CPU and time limits; a plugin that
  hangs or crashes fails only its own work, never Tuliprox
- **Rust SDK** with typed hooks, settings with secrets, a mock host for unit tests and step-by-step guides

**Benefit:** special requirements become a plugin instead of a fork, and a broken plugin cannot take your gateway
down.

### 5. AI Assistants (MCP)

- Optional **MCP server** for Claude Desktop, Cursor and other MCP clients
- Inspect status, streams, stream history, QoS, configuration, stored playlists (paged and filtered), media-server
  libraries and per-target EPG diagnostics
- Read-only by default; write tools such as starting a playlist update must be enabled explicitly, and every call is
  checked against the same authentication and permissions as the REST API
- Structured results and tool annotations, so assistants know which calls only read
- Configurable in the Web UI and in the setup wizard

### 6. One Catalog, Four Output Formats

| Format               | Best for                                                                               |
|----------------------|----------------------------------------------------------------------------------------|
| **M3U/M3U8**         | IPTV players such as VLC, TiviMate, iMPlayer, and similar clients                      |
| **Xtream Codes API** | Clients expecting Live, VOD, Series, Catchup, EPG, categories, and account information |
| **HDHomeRun**        | Plex, Jellyfin, Emby, and compatible tuner-based integrations with SSDP discovery      |
| **STRM**             | Kodi, Jellyfin, and Emby libraries with multi-version support and quality tags         |

Generate several output types from the same processed source at the same time.

**Benefit:** the player no longer dictates how your upstream sources must be organized.

### 7. Powerful Processing Pipeline — Make the Catalog Yours

Tuliprox can transform provider data into the structure you actually want to use.

- Merge multiple inputs into a single target
- **Filter Engine** with complex boolean expressions, for example:

  ```text
  (Group ~ "^DE.*") AND NOT (Name ~ ".*XXX.*")
  ```

- Target filters can run during normal processing or at the final **persist** stage after EPG processing, mapping, merging,  
  deduplication, sorting, numbering, and counters
- **Mapper DSL** for advanced transformations:
  - regex renaming with capture groups and backreferences
  - variables, `if`/`else`, and `for_each`
  - functions such as `replace`, `pad`, `format`, `first`, `capitalize`, `lowercase`, `uppercase`, and `template`
  - counters with padding such as `001`, `002`, `003`
  - read-only metadata fields such as `@Input` and `@Type`
  - mapping stages including normal processing and `after_epg`
- **Reusable templates** — global, inline, list-based, and hot-reloaded
- **Sort Engine** — regex sequences, source-order preservation, filtered sorting, and named capture groups
- Accent-independent matching with `match_as_ascii`
- Unicode normalization for filters and value comparisons
- Output filters for the final playlist state
- Favorites/bouquet management through `add_favourite(group_name)`
- Target-specific bouquet whitelist/blacklist editing directly from the Web UI
- Target-wide Trakt and TMDB Trending curation, with optional Xtream bouquets over locally available content

**Benefit:** providers supply the raw catalog; Tuliprox decides what your users actually see and how it is organized.

### 8. Protect Provider Slots and Improve Stream Delivery

Tuliprox is not just a metadata processor — it actively manages stream delivery.

- **Reverse Proxy Mode** — hide provider URLs and keep client traffic behind Tuliprox
- **Redirect Mode** — avoid proxy overhead where direct delivery is preferred
- **Shared MPEG-TS Streams** — several viewers can reuse one upstream provider stream
- **Shared HLS Sessions** — reuse one server-side HLS session while retaining individual user leases and accounting
- **Failover Groups** — one channel per live program across inputs, with source selection and runtime failover for
  MPEG-TS and HLS; correct automatic groups and build your own in the Web UI editor or in `failover.yml` /
  `failover.d/` files with hot reload
- **HLS Segment Cache & Prefetch** — fetch manifests, initialization maps, and segments once and serve them locally
- **HLS Recovery** — optional manifest recovery, bounded segment repair, stale-origin recovery, and diagnostics for difficult upstream streams
- **Universal Catchup** — expose provider archive/catchup through regular M3U-compatible `.ts` URLs
- **Bandwidth Throttling** with flexible units
- **Channel-switch friendly reservations** to avoid unnecessary slot waits
- **Fallback videos** for unavailable channels, exhausted capacity, expired accounts, and similar conditions
- Configurable fallback error mode for reverse proxies that should handle failures themselves

#### Connection admission and priority

- Normal and soft user connection limits
- Dedicated soft priority for temporary overflow connections
- Provider and user priority handling
- Ordered admission strategies for deciding what happens when limits are reached:
  - evict oldest/newest connections from the same user
  - optionally scope eviction to the same IP
  - instant or held grace-period strategies
- Anti-ping-pong protection for aggressive reconnect loops
- Session-aware handling for HLS/catchup and socket-bound admission for regular TS/VOD/local playback

**Benefit:** limited provider capacity can be shared intentionally instead of being consumed unpredictably by clients,  
reconnects, probes, and background jobs.

### 9. Provider Resilience, Failover, and Safer Refreshes

Upstream providers are not always reliable. Tuliprox is designed to preserve usable service when they fail partially or temporarily.

- Automatic provider URL failover on timeouts and server errors
- **Failover groups with stream failover** collapse the same live channel from several providers or accounts into one
  channel; playback starts on the best reachable source and, for MPEG-TS and live HLS, switches to another source while
  the client keeps watching. A Web UI editor and YAML failover files (`failover.yml` or `failover.d/`, hot reloaded) let you
  correct automatic groups and create your own
- Choose whether a provider resumes from the last working URL or restarts from the first URL on future requests
- `provider://` references resolve to the currently active provider URL
- `playback_provider` binds a provider profile to inputs with plain `http(s)://` playlist URLs, so mapped `provider://` playback URLs fail over too
- Asynchronous DNS resolution and caching
- Persisted resolved IPs without overwriting source configuration
- HTTP, HTTPS, and SOCKS5 outbound proxy support for VPN gateways, corporate proxies, or Gluetun
- Effective public IPv4/IPv6 display in the Web UI to verify routing
- Provider aliases and CSV batch configuration for larger account pools
- **Per-cluster update quality guards** for Xtream, Stalker, and M3U inputs can reject suspicious Live/VOD/Series refreshes  
  and retain the last accepted data
- Quality rejections are reported separately from technical failures
- Independent inputs can update in parallel while optional sequential groups protect providers that must not be refreshed concurrently
- Automatic Xtream account-expiration refresh with throttling designed to reduce unnecessary provider requests
- Stalker capability knowledge is reused during a running client session to avoid repeatedly probing known-failing endpoint variants
- **Maintenance mode** takes an input out of service from the dashboard or the REST API: new streams get a
  dedicated maintenance video, the last playlist and EPG keep being served, and no subsystem contacts the provider until
  maintenance ends; running streams and recordings continue

**Benefit:** a broken refresh or failing alias does not have to destroy an otherwise usable catalog.

### 10. Metadata, EPG, and Stream Intelligence

Tuliprox can improve incomplete provider data and make large catalogs easier to consume.

- Multi-source EPG with priorities
- Automatic provider EPG URL generation
- ICS/iCalendar sources converted into XMLTV programmes
- Smart fuzzy matching between playlist channels and EPG channels
- Configurable XMLTV timeshift with timezone and DST support
- Configurable normalization and cleanup for better matching
- Optional clearing of invalid EPG IDs without removing the playlist entry itself
- EPG logo override and authenticated image proxying
- In-memory EPG cache for fast UI and API access
- Asynchronous EPG processing for large guides
- Automatic EPG title synchronization after playlist updates
- Extended programme metadata where available

#### Metadata resolution and probing

- Background metadata resolution and stream probing when provider capacity is available
- Configurable fairness between metadata resolution and probing
- FFprobe-based codec, resolution, HDR, and audio-channel detection
- Provider-slot-aware probing — skip work instead of stealing the last available connection
- TMDB lookup for missing IDs and release information
- IMDb ratings with `Rating` and `Votes` filters
- Persistent retry/cooldown state for failed metadata operations
- No-change cache to avoid repeating work for unchanged items
- Periodic live-stream probing
- Automatic quality tags such as `[2160p 4K HEVC HDR TrueHD 7.1]`
- Multi-version grouping for Jellyfin/Emby-style libraries

**Benefit:** clients receive a cleaner, richer catalog without each player having to solve metadata and EPG inconsistencies on its own.

### 11. Multi-User Gateway, Plans, and Access Control

Expose one Tuliprox instance to multiple users without giving everyone identical access.

- Assign each account to a processed target
- Advertise different LAN/public server profiles per user
- Choose reverse-proxy or redirect behavior per account
- Restrict Live, VOD, and Series independently through per-user output clusters
- Select allowed categories/bouquets
- Configure normal and soft connection limits
- Configure connection priority and soft priority
- Reusable **plans** for capability tiers, cluster access, connection limits, and enforced filters
- Store users in versionable configuration or the integrated user database
- Apply user and gateway configuration changes without restarting the service

#### RBAC

- 20 permissions across 9 domains
- Independent `.read` and `.write` grants
- Custom groups and multi-group membership
- Additive permission resolution
- Compact permission bitmask in JWTs
- Built-in `admin` group with protected full access

**Benefit:** one backend can serve different devices, households, environments, or access tiers without duplicating the provider and processing configuration.

### 12. Security Built Into the Control Plane

- Argon2 password hashing
- JWT authentication with password-version tracking
- Sign-in throttling by client address and username
- Persistent token revocation for one user or all users
- Auth audit events for successful, failed, throttled, and permission-denied activity
- Per-IP request rate limiting
- Configurable Content Security Policy
- Per-user IPv4/IPv6 CIDR restrictions
- Per-user GeoIP country restrictions with secure deny behavior when GeoIP data is unavailable by default
- Configurable SSL/TLS behavior
- Header stripping for selected upstream/request headers
- Mandatory rewrite secret for stable resource URLs in reverse-proxy mode
- Sensitive values masked in configuration API responses

**Benefit:** Tuliprox can be exposed as a real multi-user gateway without treating authentication and access policies as an afterthought.

### 13. Event-Based Notifications and Monitoring

Tuliprox uses a shared event backbone so operational events can feed the Web UI, notifications, diagnostics, and future integrations consistently.

Supported notification channels include:

- Telegram
- Discord
- Pushover
- REST webhooks
- ntfy
- Gotify
- Slack
- Local command execution with structured event JSON on stdin
- Plugins can add more channels. See `plugins/examples/` for source examples

Notification capabilities include:

- Event subscriptions using dotted event IDs and glob patterns
- Per-channel minimum severity
- Per-channel subscriptions and routing
- Quiet hours that defer notifications instead of silently dropping them
- Deduplication windows and per-hour rate caps
- Durable delivery with retry handling for transient failures
- Dead-letter tracking for notifications that permanently fail
- Provider `Retry-After` support where applicable
- HMAC-SHA256 signing for REST webhooks
- Per-channel Handlebars templates with `json_escape`, `bytes` and `round` helpers
- Preview/test endpoint so templates can be rendered without sending them
- Events for provider failures, exhausted pools, priority fallback, user connection denial, auth activity, config reload  
  failures, metadata failures, scheduled-task failures, recording lifecycle, disk alerts, and more
- Playlist change tracking: get told when channels in the groups you care about change, or when whole groups appear
  or disappear — for example a new event or PPV group — each filtered by your own group patterns
- Event statistics and a recent-event ring for troubleshooting
- Disk usage alerts with warning/critical thresholds and configurable repeat intervals

**Benefit:** failures that would otherwise live only in a log file can reach the operator through the channel that is actually monitored.

### 14. Stream History and QoS Visibility

Optional stream telemetry can turn individual playback failures into longer-term reliability information.

- Persist stream connects, disconnects, preemptions, session expiry, and startup failures
- Classify failure stages and provider error classes
- Track stable stream identity across runs
- Track shared-stream participation
- Aggregate rolling reliability snapshots in the background
- Optionally measure Shared HLS origin transfer quality separately from availability, with sample coverage and
  freshness, via `GET /api/v1/hls/origin-telemetry`
- Maintain 24-hour, 7-day, and 30-day QoS windows
- View QoS summaries/details in the Web UI or query them through the API
- Compact QoS storage periodically to reclaim expired data

**Benefit:** troubleshooting moves from “this channel sometimes fails” to data that can show where and how often it fails.

### 15. Digital Video Recorder (DVR)

Turn existing Tuliprox sources into a managed recording library without adding another DVR server and database stack.

- Record live channels directly through Tuliprox
- Scheduled and recurring recording rules
- Conflict preview before recordings start
- Provider-aware scheduling that respects available connection capacity
- Reliable queue operations: pause, resume, retry, edit, and cancel
- Crash-safe startup reconciliation
- Automatic retention by age, count, and disk-space thresholds
- Recording quota management
- Configurable recording container format
- Durable lifecycle notifications with per-channel retry handling
- Real-time Web UI updates for recordings and rules
- Protected media, thumbnail, and subtitle paths with filesystem containment checks
- Hot-reloadable DVR configuration
- Built-in recording subsystem health endpoint
- `dvr_doctor` diagnostics for support and troubleshooting

**Benefit:** recording uses the same sources, permissions, provider limits, monitoring, and management UI you already configured for playback.

### 16. Embedded Storage — No External Database Required

Tuliprox includes its own storage engine for playlist and runtime data.

- Versioned 4 KiB slotted-page format
- Checksummed headers, pages, cells, and overflow chains
- Adaptive LZ4 compression
- Mmap-backed streaming scans instead of loading the complete database into RAM
- Shared query snapshots for reused mappings and validated routes
- WAL-protected in-place updates with immediate and batch flush policies
- Atomic publication of verified replacements and compaction results
- Startup migration for recognized legacy database versions
- Sorted indexes for playlist-order streaming
- String interning for repeated playlist values
- Tombstone and free-page reuse plus explicit compaction

**Benefit:** you get durable local state without deploying and maintaining PostgreSQL, MySQL, MongoDB, or Redis just to run Tuliprox.

### 17. Scheduling and Automation

- Cron-based scheduling with optional target selection
- Scheduled local-library scans
- Optional playlist update on startup
- Configuration file watching and hot reload
- Playlist/provider caching with configurable duration
- Staged-input processing
- Panel API integration for provider account provisioning/renewal workflows
- Dependency-aware parallel input processing
- Management REST API for configuration, processing, transfers, recordings, status, stream history, QoS, and system information
- Optional MCP server for AI assistants (see [AI Assistants](#5-ai-assistants-mcp))
- JSON and CBOR responses on supported endpoints
- CLI one-shot processing for scripts and CI/CD

**Benefit:** common maintenance tasks can run unattended, while the same operations remain available through the UI, API,  
or CLI when manual control is needed.

### 18. Complete Xtream-Compatible Delivery

- Player API for Live, VOD, and Series
- Categories and icons
- VOD and Series detail endpoints
- Catchup/timeshift support with session tracking
- XMLTV/EPG endpoints
- Panel API integration
- GET and POST request handling
- Series/Catchup lookup with virtual IDs
- Connection and bandwidth information in user responses
- Custom server messages
- Multiple advertised server profiles with different protocols, hosts, ports, and paths

### 19. Local Media Library

- Recursive directory scanning
- Automatic Movie/Series classification
- NFO, TMDB, and filename-based metadata resolution
- Incremental scans for new or changed files
- Scheduled rescans
- Stable virtual IDs
- Episode artwork/background integration
- Publish local content through the same Xtream, M3U, and STRM outputs as provider content
- Rebuild selected targets automatically after successful library rescans

**Benefit:** local files and provider content can live in one catalog instead of separate client libraries and workflows.

### 20. Dedicated Player API & Client Integration

Bring advanced gateway capabilities directly to external IPTV players (Android TV, mobile, desktop) using personal
tokens with Xtream login credentials:

- **Caller stream management (`/api/v1/me/streams`)**: list active streams with protocol, uptime, and user agent; stop own
  playback on remote devices to instantly free connection slots.
- **Failover source switching (MPEG-TS & live HLS)**: query available failover group sources and manually switch running streams
  under the open response without reconnecting.
- **Joinable shared streams & free shared joins**: discover running shared streams to watch without consuming extra provider
  slots, including compatible single-source streams started by other targets; optionally permit free shared joins beyond user
  connection limits.
- **Dedicated stream split**: split an own shared MPEG-TS stream into a dedicated provider connection with a single call and
  a reconnect URL, without reserving capacity up front.
- **Fallback diagnostics & CORS exposure**: `X-Tuliprox-Fallback` response header and exposed CORS headers inform players of
  stream preemption, capacity limits, or channel unavailability.
- **Personal DVR for API users**: plan- and user-configurable DVR tiers (`none`, `read`, `private`, `shared`), personal quotas,
  concurrency limits, deduplicated captures over running shared streams, and virtual MPEG-TS clips with automatic compaction.
- **Recurring recording rules**: create, edit, and delete weekly timeslot rules with a per-user limit
  (`max_recording_rules`), scheduled and re-validated per occurrence.
- **Resume watching**: per-user watch progress for movies, episodes, and recordings with automatic `watched` detection and a
  small REST surface for client-side resume and history views.
- **Live event stream**: a personal JSON WebSocket with stream-ended, failover-switch, recording-finished, connection-denied,
  and resync events, authenticated by the player token.
- **Player AI tools (MCP)**: optional assistant tools (`mcp.player_tools`) for searching visible channels, checking EPG guides,
  hands-free recording scheduling, recurring rules, failover switching, and stream splitting.
- **Recordings in any Xtream player**: completed recordings appear as a `Recordings` VOD category and play through the normal
  movie URL with seeking, so players need no extra API.
- **Source quality**: failover source lists show a redacted health state, resolution, codec, bitrate and QoS score per source.
- **Developer specification**: a guide with examples ([Player & Integration API](docs/src/player-api.md)) and an OpenAPI 3.1
  file ([tuliprox-integration-api.openapi.yaml](docs/src/api/tuliprox-integration-api.openapi.yaml)) for building products on
  these interfaces.

**Benefit:** external IPTV players can offer premium gateway features — such as manual source selection and personal DVR — without
requiring full administrative privileges or exposing provider details.

### 21. Operations and Deployment

- Alpine and Scratch Docker images
- Docker Compose templates for Traefik, CrowdSec, Gluetun/SOCKS5, iptv-org/epg, and Tuliprox
- Zero-downtime configuration swaps with `ArcSwap<Config>`
- Disk-based processing for large playlists
- Server and CLI modes
- `/healthcheck` liveness endpoint
- `/ready` capacity-aware readiness endpoint
- `/api/v1/status` detailed status endpoint
- SSDP/HDHomeRun discovery
- Database inspection tooling
- `${env:VAR}` interpolation throughout configuration
- `.env` loading with host/container environment variables taking precedence
- Optional startup runtime-configuration report in YAML or JSON with sensitive values redacted
- External log shipping via UDP (syslog RFC 5424, RFC 3164, or NDJSON) with independent remote log level and non-blocking delivery
- Configurable default outbound User-Agent
- Dev Container/Codespaces environment with the Rust/WASM tooling needed for development

## 🔐 Configuration and Secrets

Tuliprox ships with demo placeholder configuration only — nothing in this repository is a real credential.

- All major config files support `${env:VAR}` interpolation, including `config.yml`, `source.yml`, `api-proxy.yml`, `mapping.yml`, and `template.yml`
- Tuliprox can load variables from a `.env` file at startup; existing process/Docker environment variables take precedence
- Provider credentials, webhook secrets, API keys, and tokens can therefore stay outside version-controlled configuration
- `config/user.txt` contains sample Argon2 hashes for demo accounts only; replace them with hashes generated locally using `tuliprox --genpwd`
- Pin `web_ui.auth.secret` across restarts so active logins can survive a service restart
- Configuration API responses mask sensitive messaging secrets rather than returning them in clear text
- See [Secrets & Environment Variables](docs/src/configuration/secrets.md) for the full guide and pre-publish checklist

## 🎯 Who Is Tuliprox For?

### IPTV Enthusiasts

- Merge several providers into one catalog
- Normalize inconsistent names, groups, logos, and EPG data
- Keep only the content you actually want
- Use the same cleaned catalog in different players
- Set everything up in the Web UI instead of editing configuration files
- Share provider streams and make better use of limited connection slots

### Self-Hosted and Homelab Users

- Run a complete gateway in one container without an external database stack
- Deploy on Raspberry Pi, NAS, VPS, or a larger home server
- Route upstream traffic through a VPN or SOCKS5 gateway
- Verify the effective public IP directly from the Web UI
- Monitor health, capacity, disk usage, streams, and notifications from one place
- Use Traefik, CrowdSec, Gluetun, and EPG templates as building blocks

### Multi-User Setups

- Publish several virtual IPTV endpoints from one Tuliprox instance
- Assign different targets, plans, categories, clusters, limits, and priorities per user
- Share provider capacity while retaining per-user accounting and admission control
- Restrict access by CIDR and GeoIP policy
- Use RBAC for administrative access to Tuliprox itself

### Developers and Power Users

- Mapper DSL for complex transformations
- Reusable templates and staged mapping
- Management REST API and MCP server
- WebAssembly plugin SDK with typed hooks and a mock host for tests
- Event-based automation and signed webhooks
- CLI mode for scripting and CI/CD
- Database viewers and diagnostics
- Reproducible Dev Container/Codespaces setup

## 🐋 Docker Container Templates

- [traefik](docker/container-templates/traefik/) template
- [crowdsec](docker/container-templates/crowdsec/) template
- [gluetun/socks5](docker/container-templates/gluetun/) template
- [iptv-org-epg](docker/container-templates/iptv-org/) template
- [tuliprox](docker/container-templates/tuliprox/) (including Traefik) template

All templates live in [`docker/container-templates`](docker/container-templates/).

## ⚡ Quick Start

Start the latest Docker image:

```yaml
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

Then open `http://<your-host>:8901` in your browser. The setup wizard guides you through the main settings, your
first sources, users and optional plugins; everything else can be configured in the Web UI afterwards.

## 🤝 Community

Questions, ideas, feedback, and contributions are welcome.

[Join the Tuliprox Discord community](https://discord.gg/gkzCmWw9Tf)

## 🧱 Project Layout

- `backend/` — backend crates; `app/` contains the main server and processing pipeline
- `frontend/` — Yew Web UI
- `shared/` — DTOs and shared logic
- `config/` — example configuration
- `docs/` — Markdown source for the project documentation

## 📚 Documentation

The detailed documentation lives in Markdown under `docs/` and is designed to be rendered as a static site.

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
- [MCP Server](docs/src/operator/mcp.md)
- [Troubleshooting & Resilience](docs/src/troubleshooting.md)
- [WebAssembly Plugins](docs/src/plugins/overview.md)
  - [Plugin Quickstart](docs/src/plugins/quickstart.md)
  - [Plugin Marketplace](docs/src/plugins/marketplace.md)
  - [Connecting a Media Server as a Plugin](docs/src/plugins/media-server-plugins.md)

## ❓ FAQ

**Does Tuliprox have a Web UI?**
Yes. Tuliprox includes a full Web UI with a setup wizard. Sources, targets, users, mappings, failover groups,
schedules, notifications, recordings, plugins and all other settings can be managed in the browser.

**Do I need a database server?**
No. Tuliprox stores everything in its own embedded storage engine; it runs as a single binary or container.

**Which IPTV players and media servers does it work with?**
Any player that understands M3U/M3U8 or the Xtream Codes API (for example TiviMate, iMPlayer or VLC), HDHomeRun
clients such as Plex, Jellyfin and Emby, and STRM-based libraries such as Kodi. Plex, Emby, Jellyfin and Silo can also
be used as sources.

**What happens when a provider fails during playback?**
Provider URLs fail over automatically, and failover groups switch a running live stream to another provider or
account (MPEG-TS and HLS) while the viewer keeps watching.

**Can I extend it?**
Yes, with sandboxed WebAssembly plugins from the built-in marketplace or written yourself with the Rust SDK.

**How much hardware does it need?**
Very little: it runs on a Raspberry Pi or NAS and can operate with as little as 256 MB RAM, depending on the workload.

## 📄 License

See [`LICENSE`](https://github.com/euzu/tuliprox/blob/develop/LICENSE).
