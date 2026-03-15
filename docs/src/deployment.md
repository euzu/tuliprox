# Deployment

## Backend and frontend

Tuliprox consists of:

- a Rust backend
- a Yew/WebAssembly frontend
- static assets served from the configured web root

## Documentation delivery

The recommended documentation workflow is:

1. write docs as Markdown in `docs/src`
2. generate static HTML with `mdBook` into `frontend/build/docs`
3. copy the generated site into `frontend/dist/static/docs` during the web build

That gives you editable source files in Git without committing hand-written HTML.

## Why `mdBook`

For this repository, `mdBook` is the most pragmatic choice because it:

- keeps source in Markdown
- generates static HTML
- fits naturally into a Rust-based project
- avoids the overhead of a full Node/React documentation stack

## Main build commands

Build docs only:

```bash
make docs
```

Preview docs locally:

```bash
make docs-serve
```

Build frontend plus docs together:

```bash
make web-dist
```

This does:

1. `mdbook build` -> `frontend/build/docs`
2. `trunk build` -> `frontend/dist`
3. copy docs -> `frontend/dist/static/docs`

## Docker build

Build the project image manually:

```bash
docker build --rm -f docker/Dockerfile -t tuliprox .
```

For the local development image, adapt your compose file to point to the locally built image.

## Static binary builds

Recommended musl build:

```bash
cross build -p tuliprox --release --target x86_64-unknown-linux-musl
```

Manual prerequisite install on Debian or Ubuntu:

```bash
rustup update
sudo apt-get install pkg-config musl-tools libssl-dev
rustup target add x86_64-unknown-linux-musl
```

Then:

```bash
cargo build -p tuliprox --release --target x86_64-unknown-linux-musl
```

## Multi-platform helper scripts

The repository ships helper scripts under `bin/`:

- `bin/build_docs.sh`
- `bin/build_fe.sh`
- `bin/build_local.sh`
- `bin/build_docker.sh`
- `bin/release.sh`

`bin/build_fe.sh` is the shared entry point for docs plus frontend assets.

Wasm optimization is always part of the frontend build.
`bin/build_fe.sh` expects a compatible `wasm-opt` in `PATH`.

Recommended local setup:

```bash
./bin/install_wasm_tools.sh 128
export PATH="$PWD/.tools/wasm-tools/version_128/bin:$PATH"
./bin/build_fe.sh release
```

## Healthcheck

Tuliprox supports:

```bash
tuliprox --healthcheck
```

This can be wired into Docker healthchecks.

## Cross-compilation notes

Windows:

```bash
rustup target add x86_64-pc-windows-gnu
cargo build -p tuliprox --release --target x86_64-pc-windows-gnu
```

Raspberry Pi / armv7:

```bash
cross build -p tuliprox --release --target armv7-unknown-linux-musleabihf
```
