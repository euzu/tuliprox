## **tuliprox** - A Powerful IPTV Proxy & Playlist Processor

`tuliprox` is a high-performance IPTV proxy and playlist processor written in Rust 🦀.
It ingests M3U/M3U8 playlists, Xtream sources and local media, reshapes them into clean outputs, and serves them to Plex,  
Jellyfin, Emby, Kodi and similar clients.

![tuliprox logo](https://github.com/user-attachments/assets/8ef9ea79-62ff-4298-978f-22326c5c3d02)

## 🔧 Core Features

- **Advanced Playlist Processing**: Filter, rename, map, and sort entries with ease.
- **Flexible Proxy Support**: Acts as a reverse/redirect proxy for EXTM3U, Xtream Codes, HDHomeRun, and STRM formats (Kodi, Plex, Emby, Jellyfin)
  with:
  - app-specific naming conventions
  - flat directory structure option (for compatibility reasons of some media scanners)
- **Local Media Library Management**: Scan and serve local video files with automatic metadata resolution:
  - Recursive directory scanning for movies and TV series
  - Multi-source metadata (NFO files, TMDB API, filename parsing)
  - Automatic classification (Movies vs Series)
  - Integration with Xtream API and M3U playlists
- **Multi-Source Handling**: Supports multiple input and output sources. Merge various playlists and generate custom outputs.
- **Scheduled Updates**: Keep playlists fresh with automatic updates in server mode.
- **Web Delivery**: Run as a CLI tool to create m3u playlist to serve with web servers like Nginx or Apache.
- **Template Reuse (DRY)**: Create and reuse templates using regular expressions and declarative logic.

## 🔍 Smart Filtering

Define complex filters using expressive logic, e.g.:
`(Group ~ "^FR.*") AND NOT (Group ~ ".*XXX.*" OR Group ~ ".*SERIES.*" OR Group ~ ".*MOVIES.*")`

## 📢 Monitoring & Alerts

- Send notifications via **Telegram**, **Pushover**, or custom **REST** endpoints when problems occur.
- Track group changes and get real-time alerts.

## 🔒 Role-Based Access Control

- Fine-grained permissions across 7 domains (config, source, user, playlist, library, system, epg).
- Manage users and groups via flat files (`user.txt`, `groups.txt`) or the Web UI.
- Permissions encoded as compact bitmask in JWT — zero file I/O on every request.
- Built-in `admin` group with full access; custom groups with any permission combination.
- Backward compatible — existing `user.txt` files work without changes.

## 📺 Stream Management

- Share live TV connections.
- Show a fallback video stream if a channel becomes unavailable.
- Integrate **HDHomeRun** devices with **Plex**, **Emby**, or **Jellyfin**.
- Use provider aliases to manage multiple lines from the same source.
- Per-stream bandwidth and transferred-bytes metrics (opt-in).

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
