# 🛠️ Build & Deploy (For Professionals)

This guide covers advanced topics for developers and professionals who want to compile Tuliprox from source code,
generate the documentation locally,
build custom Docker images, or deploy full ecosystem stacks.

## System Architecture

Tuliprox is built for extreme performance and modularity, consisting of three core components:

* **Rust Backend:** A high-performance, asynchronous engine handling stream brokering, metadata enrichment, and playlist
  processing.
* **Yew/WebAssembly Frontend:** A reactive management interface compiled to highly optimized WASM.
* **Static Assets:** Documentation and UI assets served directly from the configured web root.

The repository ships with various helper scripts located under `bin/` to simplify cross-platform toolchain setups:

* `bin/build_docs.sh`: Script to handle mdBook generation.
* `bin/build_fe.sh`: The shared entry point for building docs plus frontend assets.
* `bin/build_local.sh`: Compiles the backend locally.
* `bin/build_docker.sh`: Executes the multi-stage Docker build pipeline.
* `bin/release.sh`: Orchestrates a full production release build.

This project uses a `Makefile` to automate development tasks, tool installations, and build processes. This ensures a
consistent
environment across different machines.

**Tool installations:**

* `make install-tools`: Full Setup: Installs Rustup, Cross, Trunk, wasm-bindgen, cargo-edit, mdBook, and markdownlint.
* `make rustup`: Installs the Rust toolchain and Cargo.
* `make cross`: Installs cross for multi-platform compilation.
* `make trunk`: Installs trunk for managing frontend builds.
* `make wasm-bindgen`: Installs the CLI tool for WebAssembly bindings.
* `make cargo-set-version`: Installs cargo-edit for version management.
* `make mdbook`: Installs mdBook for documentation generation.
* `make markdownlint`: Installs markdownlint-cli2 (requires Node.js/npm).

**Testing & Linting**:

* `make test`: Runs all workspace tests using the Stable toolchain.
* `make lint`: Runs clippy to find common mistakes and improve code quality.
* `make lint-fix`: Automatically applies clippy suggestions (where possible).
* `make markdown-lint`: Checks all .md files for formatting consistency.

**Formatting**:

We use specific Nightly rules to ensure the code style remains consistent across the project.

* `make fmt`: Formats all code in the workspace.
* `make fmt-check`: Verifies if the code is correctly formatted (used in CI).

---

## 1. Documentation Delivery & Generation

Tuliprox embeds its own documentation directly into the Web UI. The documentation workflow is completely Markdown-based
to fit naturally into a Rust
project, avoiding the overhead of a full Node/React documentation stack.

**How it works:**

1. Write docs as Markdown in `docs/src`.
2. Generate static HTML with `mdBook` into `frontend/build/docs`.
3. Copy the generated site into `frontend/dist/static/docs` during the web build.

This gives you editable source files in Git without committing hand-written HTML.

### Main Build Commands (`make`)

Ensure you have `mdBook` installed (`cargo install mdbook`).

| Command           | Purpose                                                                                                                      |
|:------------------|:-----------------------------------------------------------------------------------------------------------------------------|
| `make docs`       | Builds only the static documentation via mdBook.                                                                             |
| `make docs-serve` | Serves the documentation locally on a dev port with live-reload.                                                             |
| `make web-dist`   | Builds the frontend WASM app (via Trunk) and the docs together. Copies the docs into the frontend dist folder automatically. |

---

## 2. Building the Frontend

### WASM Optimization (`wasm-opt`)

Wasm optimization is handled by Trunk during the build (configured via `data-wasm-opt` in `index.html`). Trunk requires
a compatible `wasm-opt`
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

If you need to run Tuliprox on specific hardware (e.g., Raspberry Pi) or want a fully static Linux binary without
`glibc` dependencies, follow these steps.

### Static Binary Builds (Linux MUSL)

For a portable Linux binary, the `musl` target is recommended. You can either use the `cross` toolchain (easiest) or
manually install the prerequisites.

#### Option A: Using `cross` (Recommended)

```bash
cargo install cross
cross build -p tuliprox --release --target x86_64-unknown-linux-musl
```

#### Option B: Manual compilation on Debian/Ubuntu

```bash
rustup update
sudo apt-get install pkg-config musl-tools libssl-dev
rustup target add x86_64-unknown-linux-musl

cargo build -p tuliprox --target x86_64-unknown-linux-musl --release
```

### Cross-Compilation (ARM / Windows)

To compile for architectures other than your host, use the `cross` tool or native mingw packages.

**For Raspberry Pi (ARMv7 via Cross):**

```bash
cargo install cross
env RUSTFLAGS="--remap-path-prefix $HOME=~" cross build -p tuliprox --release --target armv7-unknown-linux-musleabihf
```

**For Windows (via Linux Cross-Compiler):**

If you want to compile this project on Linux for Windows, install the `mingw` packages and the target toolchain:

```bash
sudo apt-get install gcc-mingw-w64
rustup target add x86_64-pc-windows-gnu
rustup toolchain install stable-x86_64-pc-windows-gnu

cargo build -p tuliprox --release --target x86_64-pc-windows-gnu
```

---

## 4. Custom Docker Builds

Tuliprox utilizes an advanced Multi-Stage Docker build to compile the Rust backend, the Yew frontend, and extract static
FFmpeg resources in a
single pipeline.

### Standard Build

To build the complete project and create a standard docker image based on your host architecture, run from the root
directory:

```bash
docker build --rm -f docker/Dockerfile -t tuliprox .
```

### Multi-Arch & Specific Targets

This setup distinguishes between the **Docker Stage** (image flavor) and the **Rust Compilation Target** (CPU
architecture).
You can strictly target specific environments using the `--target` and `--build-arg` flags:

* **`--target`**: Choose the image base. Use `scratch-final` for a hardened, minimal footprint or `alpine-final` if you
  need a shell for debugging.
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
```

### Running the Custom Image (`docker-compose.yml`)

When running your custom local image via `docker-compose`, ensure you change `image: ghcr.io/euzu/tuliprox:latest` to
`image: tuliprox`.
The configuration also includes a lightweight CLI-based healthcheck.

```yaml
version: '3'
services:
  tuliprox:
    container_name: tuliprox
    image: tuliprox
    user: "133:144"
    working_dir: /app
    volumes:
      - /opt/tuliprox/config:/app/config
      - /opt/tuliprox/data:/app/data
      - /opt/tuliprox/backup:/app/backup
      - /opt/tuliprox/downloads:/app/downloads
    environment:
      - TZ=Europe/Paris
    ports:
      - "8901:8901"
    restart: unless-stopped
    healthcheck:
      test: [ "CMD", "/app/tuliprox", "-p", "/app/config", "--healthcheck" ]
      interval: 30s
      timeout: 10s
      retries: 3
      start_period: 10s
```

---

## 5. Bare-Metal Deployments (LXC & System Services)

If you prefer running Tuliprox outside of Docker, for instance in a Proxmox LXC container, you can compile and register
it as a native system service.

### Installing in an Alpine Linux LXC Container

To bootstrap a fresh Alpine environment (e.g., Alpine 3.19), install the necessary build tools, clone the repository,
and compile it:

```bash
apk update
apk add nano git yarn bash cargo perl-local-lib perl-module-build make
cd /opt
git clone https://github.com/euzu/tuliprox.git

# Build Backend
cd /opt/tuliprox/bin
./build_local.sh
ln -s /opt/tuliprox/target/release/tuliprox /bin/tuliprox

# Build Frontend
cd /opt/tuliprox/frontend
yarn
yarn build

# Create Symlinks and Directories
ln -s /opt/tuliprox/frontend/build /web
ln -s /opt/tuliprox/config /config
mkdir /data
mkdir /backup
```

### Creating an OpenRC Service

To run Tuliprox as a background daemon on Alpine, create the file `/etc/init.d/tuliprox`:

```bash
#!/sbin/openrc-run
name=tuliprox
command="/bin/tuliprox"
command_args="-p /config -s"
command_user="root"
command_background="yes"
output_log="/var/log/tuliprox/tuliprox.log"
error_log="/var/log/tuliprox/tuliprox.log"
supervisor="supervise-daemon"

depend() {
    need net
}

start_pre() {
    checkpath --directory --owner $command_user:$command_user --mode 0775 \
           /run/tuliprox /var/log/tuliprox
}
```

Make the script executable and add it to the default boot runlevel:

```bash
chmod +x /etc/init.d/tuliprox
rc-update add tuliprox default
rc-service tuliprox start
```

---

## 6. Docker Container Templates — Deployment Guide

The Tuliprox repository contains ready-to-use Docker Compose templates for a secure reverse proxy stack with VPN egress
and CrowdSec protection.

**Location:** `docker/container-templates/`
**Software baseline:** Traefik v3.5, a current Rust toolchain, and a current Docker/Compose setup.

### Legend & Port Overview

| Template     | Folder      | Purpose                                                                                                                        | Notable Ports (Internal)                                                                                                                        |
|:-------------|:------------|:-------------------------------------------------------------------------------------------------------------------------------|:------------------------------------------------------------------------------------------------------------------------------------------------|
| **Traefik**  | `traefik/`  | Reverse proxy & TLS (ACME/DNS), dashboard, dynamic security middlewares, optional CrowdSec bouncer.                            | 80 `web`, 443 `websecure`                                                                                                                       |
| **Gluetun**  | `gluetun/`  | VPN egress via WireGuard; sidecars provide **SOCKS5**, **HTTP**, and **Shadowsocks** proxies bound to Gluetun’s network stack. | 1080/tcp (HTTP)<br>1388/tcp+udp (SOCKS5)<br>9388/tcp+udp (Shadowsocks)                                                                          |
| **CrowdSec** | `crowdsec/` | LAPI + bouncers (Traefik & firewall) to protect services from brute-force and L7 attacks.                                      | LAPI on 127.0.0.1:8080. AppSec must bind to 0.0.0.0:7422; binding to localhost will make AppSec unreachable from Traefik and break enforcement. |
| **Tuliprox** | `tuliprox/` | Example application container with Traefik labels and `expose: 8901` for reverse proxying.                                     | 8901 (internal)                                                                                                                                 |

### Wiring up the Stack

1. **Networks:** Create external Docker networks first:

   ```bash
   docker network create proxy-net
   docker network create crowdsec-net
   ```

2. **Gluetun (VPN & Proxy Sidecar):**

   Provide your Wireguard details in `gluetun-01/.env.wg-01` and set a user/pass in `.env.socks5-proxy`. Once started (
   `docker-compose up -d`), it
   securely routes all traffic attached to its network through the VPN.

3. **Tuliprox Integration:**

   In your Tuliprox `config.yml`, point the outgoing proxy to the SOCKS5 sidecar:

   ```yaml
   proxy:
     url: socks5://socks5-01:1388
     username: "<socks5-proxy-user>"
     password: "<socks5-proxy-password>"
   ```

   Ensure Tuliprox is in the `proxy-net` network to reach the sidecar. All Provider-API, TMDB, and Stream-Proxy traffic
   will now strictly egress
   through the WireGuard tunnel!
