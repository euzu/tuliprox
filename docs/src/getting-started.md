# Getting Started

## Recommended reading order

1. [Installation](installation.md)
2. [Configuration (Core System)](configuration/config.md)
3. [Add Sources & Targets](configuration/source.md)
4. [API Proxy](configuration/api-proxy.md)
5. [Streaming & Proxy](configuration/reverse-proxy.md)
6. [Templates](configuration/template.md)
7. [Mappings](configuration/mapping-dsl.md)

## Run Tuliprox via docker compose

```yaml
services:
  tuliprox:
    container_name: tuliprox
    image: ghcr.io/euzu/tuliprox-alpine:latest
    working_dir: /app
    volumes:
      - /opt/tuliprox/config:/app/config
      - /opt/tuliprox/data:/app/data
      - /opt/tuliprox/backup:/app/backup
      - /opt/tuliprox/downloads:/app/downloads
      - /opt/tuliprox/cache:/app/cache
    environment:
      - TZ=Europe/Berlin
    ports:
      - "8901:8901"
    restart: unless-stopped
    healthcheck:
      test: ["CMD", "/app/tuliprox", "-p", "/app/config", "--healthcheck"]
      interval: 30s
      timeout: 10s
      retries: 3
      start_period: 10s
```

Open the Web UI afterward and continue with the configuration.

### First configuration steps

For a new setup, the usual first goal is:

1. add one working input
2. create one target
3. confirm playlist output
4. confirm one stream works
5. only then add mapping, filtering, reverse proxy and metadata features

That keeps failures local and makes provider-specific issues much easier to diagnose.

## Run modes

Tuliprox has two main modes:

- CLI mode: process playlists once and exit
- Server mode: run the API, background tasks and Web UI integration

## Main commands

Run once:

```bash
cargo run --bin tuliprox -- -c config/config.yml -i config/source.yml
```

Run as server:

```bash
cargo run --bin tuliprox -- -s -c config/config.yml -i config/source.yml
```

Generate a UI password hash:

```bash
cargo run --bin tuliprox -- --genpwd
```

## CLI arguments

```text
Usage: tuliprox [OPTIONS]

Options:
  -H, --home <HOME>
  -p, --config-path <CONFIG_PATH>
  -c, --config <CONFIG_FILE>
  -i, --source <SOURCE_FILE>
  -m, --mapping <MAPPING_FILE>
  -T, --template <TEMPLATE_FILE>
  -t, --target <TARGET>
  -a, --api-proxy <API_PROXY>
  -s, --server
  -l, --log-level <LOG_LEVEL>
  --genpwd
  --healthcheck
  --scan-library
  --force-library-rescan
  --dbx
  --dbm
  --dbms
  --dbe
  --dbv
```

`--dbx`, `--dbm`, `--dbe`, `--dbv` and `--dbms` open the internal database viewers for Xtream, M3U, EPG, target-id mapping and metadata retry status files.

## Important files

- `config/config.yml`: application and server configuration
- `config/source.yml`: inputs, providers, targets
- `config/api-proxy.yml`: users and published server URLs
- `config/mapping.yml` / `config/template.yml`: optional mapping and template rules
- `config/user.txt`: Web UI login credentials (`username:hash[:groups]`). If `:groups` is omitted, the user falls back to  
   the legacy `admin` assignment. Examples: `admin:$argon2id$...` and `editor:$argon2id$...:operators`
- `config/groups.txt`: RBAC permission group definitions (optional)

## Default project layout

Tuliprox resolves its home directory in this order:

1. `--home`
2. `TULIPROX_HOME`
3. directory of the `tuliprox` binary

Typical directories below that home:

- `config/`
- `data/`
- `data/backup/`
- `downloads/`
- `web/`
- `cache/`

All relative paths in the configuration are resolved against that home directory.
