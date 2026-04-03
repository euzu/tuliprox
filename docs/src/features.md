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
- rename or normalize entries
- apply mappings and templates
- sort and regroup outputs
- merge multiple inputs into a curated target

## Runtime streaming

- reverse-proxy streams instead of redirecting them
- keep provider account affinity where clients need it
- share live streams across users
- enforce user connection limits
- prioritize higher-value sessions over lower-priority traffic
- serve custom fallback videos for failure cases
- persist optional stream history for connect/disconnect and startup-failure telemetry
- aggregate optional QoS snapshots from stream history for reliability analysis and failover planning

## Metadata and library

- resolve VOD and series metadata
- probe stream capabilities
- scan local media
- combine local library content with IPTV-oriented outputs

## Operational features

Tuliprox also includes:

- scheduled playlist refreshes
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
