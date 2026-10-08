# Core Features

## Input side

Tuliprox can ingest:

- M3U / M3U8 playlists
- Xtream inputs
- local library content

## Output side

Tuliprox can publish:

- M3U
- Xtream-style outputs
- HDHomeRun
- STRM Files

That makes it usable both for IPTV players and for media-server-oriented workflows.

## Playlist processing

- filter channels and groups
- manage target-specific group bouquets in whitelist or blacklist mode from the Source Editor
- rename or normalize entries
- apply mappings and templates
- sort and regroup outputs
- merge multiple inputs into a curated target

## Runtime streaming

- reverse-proxy streams instead of redirecting them
- proxy M3U catchup and archive playback for template-based and native Flussonic HLS or MPEG-TS URLs
- keep provider account affinity where clients need it
- share live streams across users
- enforce user connection limits
- prioritize higher-value sessions over lower-priority traffic
- serve custom fallback videos for failure cases
- persist optional stream history for per-variant connect/disconnect, startup-failure, and reconnect telemetry
- aggregate optional QoS snapshots from stream history to rank equivalent channel variants by reliability

## Metadata and library

- resolve VOD and series metadata
- probe stream capabilities
- scan local media
- combine local library content with IPTV-oriented outputs

## Operational features

Tuliprox also includes:

- scheduled playlist refreshes
- retain the last usable input and target playlists when an update unexpectedly produces no items
- hot config reload support
- provider failover and DNS-aware connection rotation
- integrated download and recording manager with provider-aware fairness, retries, and RBAC
- notifications and monitoring hooks
- **Web UI** with monitoring and web-based configuration ability

## Access control

Tuliprox supports role-based access control (RBAC) for the Web UI:

- fine-grained permissions across 7 domains (config, source, user, playlist, library, system, epg)
- custom groups with configurable permission sets
- user-to-group assignments with union-based permission resolution
- compact bitmask encoding in JWT claims for low-overhead permission checks
- backward-compatible user file format
- Web UI admin panel for user and group management

## Personal table layouts

The **Columns** strip at the inline end of each Web UI table opens an overlay panel.
Select columns with the checkboxes and reorder them with the handles using mouse or touch.
For keyboard control, focus a handle, press Space or Enter, move with Up/Down, and press Space or Enter again to drop it.
Escape cancels an active move; otherwise it closes the panel. Required fields remain visible.

**Save** stores the layout for your account. **Cancel** discards the draft.
**Restore defaults** resets the draft; save it to remove that table's stored overrides.
Changes made in another browser are detected when saving. The panel keeps your draft and lets you explicitly apply it again.
A token refresh preserves your layout; switching accounts clears the previous account's settings.
Without Web UI authentication, the installation uses shared settings and labels them accordingly.

Settings live under `<config_path>/user_settings/`, with separate `builtin`, `web`, `api`, and `local` directories.
Back up this directory with the configuration volume. Ordinary lowercase names stay readable; special bytes are percent-encoded,
and reserved or long names use a readable prefix plus a complete BLAKE3 hash.
Explicit account deletion removes that account's settings. Configuration reloads do not run a settings garbage collector.
After manually deleting and recreating an account while the server is stopped, remove its previous settings file yourself.
Edit settings files while the server is stopped; invalid files and unsupported versions are never overwritten by the UI.

Accounts CSV layouts are scoped to their header schema. Duplicate headers are distinguished by occurrence;
without semantic header IDs, exchanging two identically named headers cannot preserve their individual identity.
Ragged rows retain extra fields and show empty cells for missing values.
CSV files wider than 256 columns remain readable with the default layout and explain why saving a layout is unavailable.
Each settings file supports at most 64 stored tables and 256 KiB. Limit failures leave existing layouts unchanged.
