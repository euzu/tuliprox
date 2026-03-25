# 🚀 Installation (Docker & Binaries)

Tuliprox is designed to run flawlessly in modern DevOps infrastructures (Docker/Kubernetes) but can also be executed as a standalone binary on Linux,
Windows, or macOS for lightweight setups.

This guide covers the installation using precompiled artifacts. If you want to compile Tuliprox from source, create custom Docker images, or build
the documentation yourself, please refer to the [Build & Deploy (For Professionals)](build-and-deploy.md) chapter.

---

## 1. Running via Docker (Recommended)

The recommended way for production use is Docker. Tuliprox does not require external database containers (everything uses internal embedded `.db`
B+Tree files), making the setup extremely lightweight. You can pull the ready-to-use images directly from the GitHub Container Registry
(`ghcr.io`).

### The Ideal `docker-compose.yml`

```yaml
services:
  tuliprox:
    container_name: tuliprox
    image: ghcr.io/euzu/tuliprox-alpine:latest
    user: "133:144" # (Recommended) Run as non-root User (UID:GID of your host system)
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

### Volume Mapping (Technical Background)

The separation of volumes is critical for security and performance:

| Mount Point      | Explanation & Technical Background                                                                                                                                                                                                                                                                                  |
|:-----------------|:--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `/app/config`    | Must contain your YAML files (`config.yml`, `source.yml`, etc.). Monitored by Tuliprox for file system events (`config_hot_reload`).                                                                                                                                                                                |
| `/app/data`      | The `storage_dir`. Contains **Runtime Data**. Tuliprox recursively creates B+Tree databases (`*.db`), M3U caches, and TMDB metadata here. **THIS MUST BE PERSISTENT!** If lost during a restart, Tuliprox will attempt to redownload and FFprobe tens of thousands of streams, inevitably leading to provider bans. |
| `/app/backup`    | Destination folder for configuration backups triggered via the Web UI.                                                                                                                                                                                                                                              |
| `/app/downloads` | Destination folder for local video downloads initiated via the Web UI.                                                                                                                                                                                                                                              |
| `/app/cache`     | Destination folder for local image downloads initiated via the xtream codes or m3u api use.                                                                                                                                                                                                                         |

### Docker Image Variants (`scratch` vs. `alpine`)

Two distinct flavors are available in the container registry:

1. **`scratch-final` (`ghcr.io/euzu/tuliprox:latest`)**:
   This is an extremely hardened, minimalist image (`FROM scratch`). It contains *only* the statically linked Tuliprox binary, FFmpeg/FFprobe
   binaries, and CA certificates (for HTTPS/TMDB requests). There is no shell (`/bin/sh`), no package manager, and no system libraries.
   **Advantage:** Maximum security and minimal attack surface (DevSecOps Best Practice).
   **Disadvantage:** You cannot `docker exec` into the container for manual debugging.
2. **`alpine-final` (`ghcr.io/euzu/tuliprox-alpine:latest`)**:
   Based on Alpine Linux. Contains a shell, Tini (init system), and basic tools.
   **Advantage:** Ideal for debugging. You can `docker exec -it tuliprox sh` to inspect logs or run manual `curl`/`ffprobe` tests from within the
   container's network namespace to verify provider blocks.

---

## 2. Precompiled Standalone Binaries

If you prefer running Tuliprox bare-metal without Docker, you can download the precompiled binaries for your operating system (Linux, Windows,
macOS, ARM).

1. Go to the [Releases page on GitHub](https://github.com/euzu/tuliprox/releases).
2. Download the archive matching your OS and Architecture.
3. Extract the binary and place it in your desired home directory.
4. Run it via the CLI commands detailed below.

---

## 3. CLI Usage & Runtime Modes

Tuliprox can operate in two primary modes. Running the compiled binary directly is the best way to test configurations locally before deploying
them to production.

### Server Mode (Persistent Operation)

Run Tuliprox as a persistent IPTV proxy server:

```bash
./tuliprox -s -c config/config.yml -i config/source.yml
```

This mode enables:

* The Web UI Dashboard
* The active Reverse-Proxy Streaming Engine
* API endpoints (Xtream/M3U for players)
* Background Workers (Metadata Scanner, DNS Resolver, Scheduler)

### CLI Mode (One-Shot Processing)

Without the `-s` flag, Tuliprox runs as a one-time processor:

```bash
./tuliprox -c config/config.yml -i config/source.yml
```

It loads the configuration, downloads provider lists, applies all filters and mappings, writes the final database/M3U files to the `storage_dir`,
and gracefully exits.
*Useful for: Generating static playlists, debugging mappings, or running via external cronjobs.*

### CLI Arguments Reference

You can view all arguments using `./tuliprox --help`:

| Flag | Purpose |
| :--- | :--- |
| `-H, --home <HOME>` | Sets the home directory (base for config, storage, backup, downloads). Overrides all defaults. |
| `-p, --config-path <DIR>` | Path to the config directory. |
| `-c, --config <FILE>` | Specific path to the `config.yml`. |
| `-i, --source <FILE>` | Specific path to the `source.yml`. |
| `-m, --mapping <FILE>` | Specific path to the mapping file/directory. |
| `-T, --template <FILE>` | Specific path to the template file/directory. |
| `-t, --target <NAME>` | **Target Override:** Forces processing of the specified target *only*. Extremely useful to quickly re-render a broken list via shell without blocking the entire system, even if `enabled: false` in config. |
| `-a, --api-proxy <FILE>` | Specific path to the `api-proxy.yml`. |
| `-s, --server` | Run in continuous Server Mode. |
| `-l, --log-level <LEVEL>` | Override log level (e.g., `info`, `debug`, `trace`). |
| `--genpwd` | Interactively generate a secure `Argon2id` password hash for the `user.txt`. |
| `--healthcheck` | Checks the API over localhost. Returns Exit Code `0` if `{status: "ok"}`. Used by Docker. |
| `--scan-library` | Triggers an incremental scan of local media directories. |
| `--force-library-rescan` | Ignores modification timestamps and forces a full TMDB/PTT re-evaluation of local media. |
| `--dbx`, `--dbm`, `--dbe`, `--dbv`, `--dbms` | Opens internal database viewers (see *Operations & Debugging*). |

---

## 4. Advanced Ecosystem Integration

Tuliprox is built to be a team player. In the `docker/container-templates/` directory of the repository, you will find complete,
production-ready stack templates:

* **Traefik Integration:** Automated Let's Encrypt TLS, strict Content-Security-Policy headers, and ACME DNS-01 challenges.
* **Gluetun (VPN) Integration:** Route your upstream provider requests through WireGuard tunnels to hide your server IP, utilizing SOCKS5 proxy sidecars.
* **CrowdSec Integration:** Protect your Tuliprox instance against L7 AppSec attacks, path traversal, and brute-force attempts using Traefik Bouncers.

*(For an in-depth implementation guide on these templates, see the [Build & Deploy (For Professionals)](build-and-deploy.md) and
[Examples & Recipes](examples-recipes.md) chapters).
