# 🛠️ Build & Deploy (For Professionals)

This guide covers advanced topics for developers and professionals who want to compile Tuliprox from source code, generate the documentation locally,
build custom Docker images, or deploy full ecosystem stacks.

## System Architecture

Tuliprox is built for extreme performance and modularity, consisting of three core components:

* **Rust Backend:** A high-performance, asynchronous engine handling stream brokering, metadata enrichment, and playlist processing.
* **Yew/WebAssembly Frontend:** A reactive management interface compiled to highly optimized WASM.
* **Static Assets:** Documentation and UI assets served directly from the configured web root.

The repository ships with various helper scripts located under `bin/` to simplify cross-platform toolchain setups:

* `bin/build_docs.sh`: Script to handle mdBook generation.
* `bin/build_fe.sh`: The shared entry point for building docs plus frontend assets.
* `bin/build_local.sh`: Compiles the backend locally.
* `bin/build_docker.sh`: Executes the multi-stage Docker build pipeline.
* `bin/release.sh`: Orchestrates a full production release build.

---

## 1. Documentation Delivery & Generation

Tuliprox embeds its own documentation directly into the Web UI. The documentation workflow is completely Markdown-based to fit naturally into a Rust
project, avoiding the overhead of a full Node/React documentation stack.

**How it works:**

1. Write docs as Markdown in `docs/src`.
2. Generate static HTML with `mdBook` into `frontend/build/docs`.
3. Copy the generated site into `frontend/dist/static/docs` during the web build.

This gives you editable source files in Git without committing hand-written HTML.

### Main Build Commands (`make`)

Ensure you have `mdBook` installed (`cargo install mdbook`).

| Command | Purpose |
| :--- | :--- |
| `make docs` | Builds only the static documentation via mdBook. |
| `make docs-serve` | Serves the documentation locally on a dev port with live-reload. |
| `make web-dist` | Builds the frontend WASM app (via Trunk) and the docs together. Copies the docs into the frontend dist folder automatically. |

---

## 2. Building the Frontend

### WASM Optimization (`wasm-opt`)

Wasm optimization is handled by Trunk during the build (configured via `data-wasm-opt` in `index.html`). Trunk requires a compatible `wasm-opt`
binary in your system `PATH`.

**Recommended local setup script sequence:**

```bash
# Installs a specific version of wasm-tools locally in the repo
./bin/install_wasm_tools.sh 128

# Add it to the PATH
export PATH="$PWD/.tools/wasm-tools/version_128/bin:$PATH"

# Build the optimized release frontend
./bin/build_fe.sh release
```

---

## 3. Building from Source & Cross-Compilation

If you need to run Tuliprox on specific hardware (e.g., Raspberry Pi) or want a fully static Linux binary without `glibc` dependencies, follow these steps.

### Static Binary Builds (Linux MUSL)

For a portable Linux binary, the `musl` target is recommended.

**Prerequisite install on Debian/Ubuntu:**

```bash
rustup update
sudo apt-get install pkg-config musl-tools libssl-dev
rustup target add x86_64-unknown-linux-musl
```

**Build:**

```bash
cargo build -p tuliprox --release --target x86_64-unknown-linux-musl
```

### Cross-Compilation (ARM / Windows)

To compile for architectures other than your host, use the `cross` tool.

**For Raspberry Pi (ARMv7):**

```bash
cargo install cross
env RUSTFLAGS="--remap-path-prefix $HOME=~" cross build -p tuliprox --release --target armv7-unknown-linux-musleabihf
```

**For Windows (via Linux Cross-Compiler):**

```bash
rustup target add x86_64-pc-windows-gnu
sudo apt-get install gcc-mingw-w64
cargo build -p tuliprox --release --target x86_64-pc-windows-gnu
```

---

## 4. Custom Docker Builds (Multi-Arch)

Tuliprox utilizes an advanced Multi-Stage Docker build to compile the Rust backend, the Yew frontend, and extract static FFmpeg resources in a
single pipeline. This setup distinguishes between the **Docker Stage** (image flavor) and the **Rust Compilation Target** (CPU architecture).

### Full Pipeline Build

You can strictly target specific environments using the `--target` and `--build-arg` flags. These refer to the **destination system** where the
container will run, regardless of your local build machine's architecture:

* **`--target`**: Choose the image base. Use `scratch-final` for a hardened, minimal footprint or `alpine-final` if you need a shell for debugging.
* **`--build-arg RUST_TARGET`**: Defines the Instruction Set Architecture (ISA) for the binary.

```bash
# Build for x86_64 Linux (Standard Cloud/Desktop)
docker build --rm -f docker/Dockerfile -t tuliprox \
  --target scratch-final \
  --build-arg RUST_TARGET=x86_64-unknown-linux-musl .

# Build for ARM 64-bit (Apple Silicon / Raspberry Pi 4 & 5)
docker build --rm -f docker/Dockerfile -t tuliprox \
  --target scratch-final \
  --build-arg RUST_TARGET=aarch64-unknown-linux-musl .

# Build for ARMv7 (Raspberry Pi 3 / Older IoT)
docker build --rm -f docker/Dockerfile -t tuliprox \
  --target scratch-final \
  --build-arg RUST_TARGET=armv7-unknown-linux-musleabihf .

# Build for macOS (Darwin)
docker build --rm -f docker/Dockerfile -t tuliprox \
  --target scratch-final \
  --build-arg RUST_TARGET=x86_64-apple-darwin .
```

### Manual Docker Image

If you want to build the binary and web folder manually on your host and only package them into an image:

1. Compile the static binary (`bin/build_lin_static.sh`).
2. Compile the frontend (`yarn build`).
3. Change into the `docker` directory and copy the required files.
4. Run the manual Dockerfile build:

   ```bash
   docker build -f Dockerfile-manual -t tuliprox .
   ```

*(Note: When running your custom local image via docker-compose, ensure you change `image: ghcr.io/euzu/tuliprox:latest` to `image: tuliprox` in
your `docker-compose.yml`, and set your timezone appropriately (`TZ=${TZ:-Europe/Paris}`).)*

---

## 5. Docker Container Templates — Deployment Guide

The Tuliprox repository contains ready-to-use Docker Compose templates for a secure reverse proxy stack with VPN egress and CrowdSec protection.

**Location:** `docker/container-templates/`
**Software baseline:** Traefik v3.5, a current Rust toolchain, and a current Docker/Compose setup.

### Legend & Port Overview

| Template | Folder | Purpose | Notable Ports (Internal) |
| :--- | :--- | :--- | :--- |
| **Traefik** | `traefik/` | Reverse proxy & TLS (ACME/DNS), dashboard, dynamic security middlewares, optional CrowdSec bouncer. | 80 `web`, 443 `websecure` |
| **Gluetun** | `gluetun/` | VPN egress via WireGuard; sidecars provide **SOCKS5**, **HTTP**, and **Shadowsocks** proxies bound to Gluetun’s network stack. | 1080/tcp (HTTP)<br>1388/tcp+udp (SOCKS5)<br>9388/tcp+udp (Shadowsocks) |
| **CrowdSec** | `crowdsec/` | LAPI + bouncers (Traefik & firewall) to protect services from brute-force and L7 attacks. | LAPI on `127.0.0.1:8080` (host) |
| **Tuliprox** | `tuliprox/` | Example application container with Traefik labels and `expose: 8901` for reverse proxying. | 8901 (internal) |

### Wiring up the Stack

1. **Networks:** Create external Docker networks first:

   ```bash
   docker network create proxy-net
   docker network create crowdsec-net
   ```

2. **Gluetun (VPN & Proxy Sidecar):**

   Provide your Wireguard details in `gluetun-01/.env.wg-01` and set a user/pass in `.env.socks5-proxy`. Once started (`docker-compose up -d`), it
   securely routes all traffic attached to its network through the VPN.

3. **Tuliprox Integration:**

   In your Tuliprox `config.yml`, point the outgoing proxy to the SOCKS5 sidecar:

   ```yaml
   proxy:
     url: socks5://socks5-01:1388
     username: "<socks5-proxy-user>"
     password: "<socks5-proxy-password>"
   ```

   Ensure Tuliprox is in the `proxy-net` network to reach the sidecar. All Provider-API, TMDB, and Stream-Proxy traffic will now strictly egress
   through the WireGuard tunnel!

---
