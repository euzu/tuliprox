# Getting Started

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
  --dbe
  --dbv
```

## Important files

- `config/config.yml`: application and server configuration
- `config/source.yml`: inputs, providers, targets
- `config/api-proxy.yml`: users and published server URLs
- `mapping.yml` / `template.yml`: optional mapping and template rules

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

All relative paths in the configuration are resolved against that home directory.

## Quick Docker start

```yaml
services:
  tuliprox:
    container_name: tuliprox
    image: ghcr.io/euzu/tuliprox-alpine:latest
    working_dir: /app
    volumes:
      - /home/tuliprox2/tuliprox:/app/tuliprox
      - /home/tuliprox2/config:/app/config
      - /home/tuliprox2/data:/app/data
      - /home/tuliprox2/cache:/app/cache
    environment:
      - TZ=Europe/Paris
    ports:
      - "8901:8901"
    restart: unless-stopped
```

Open the Web UI afterwards and continue with the configuration.

## Good first milestone

For a new setup, the usual first goal is:

1. add one working input
2. create one target
3. confirm playlist output
4. confirm one stream works
5. only then add mapping, filtering, reverse proxy and metadata features

That keeps failures local and makes provider-specific issues much easier to diagnose.

## Recommended reading order

1. [Config Reference](configuration/main-config.md)
2. [Sources And Targets](configuration/sources-and-targets.md)
3. [API Proxy](configuration/api-proxy.md)
4. [Streaming And Proxy](streaming-and-proxy.md)
5. [Mapping And Templates](mapping-and-templates.md)
