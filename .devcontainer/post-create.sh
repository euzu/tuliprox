#!/usr/bin/env bash

set -euxo pipefail

echo "==> Provisioning tuliprox dev container"

# Ensure cargo-installed binaries are on PATH for this session.
export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:${PATH}"

sudo apt-get update
sudo apt-get install -y --no-install-recommends \
  musl-tools \
  pkg-config \
  libssl-dev
sudo rm -rf /var/lib/apt/lists/*

RUST_VERSION="1.89.0"
rustup toolchain install "${RUST_VERSION}" --profile minimal --component clippy --component rustfmt
rustup default "${RUST_VERSION}"

rustup toolchain install nightly --profile minimal --component rustfmt

rustup target add \
  wasm32-unknown-unknown \
  x86_64-unknown-linux-musl \
  aarch64-unknown-linux-musl

if ! command -v cargo-binstall >/dev/null 2>&1; then
  echo "==> Installing cargo-binstall"
  curl -L --proto '=https' --tlsv1.2 -sSf \
    https://raw.githubusercontent.com/cargo-bins/cargo-binstall/main/install-from-binstall-release.sh | bash
fi

echo "==> Installing Rust CLI tools (trunk, wasm-bindgen, cross, cargo-edit, mdbook)"
cargo binstall -y \
  trunk \
  wasm-bindgen-cli \
  cross \
  cargo-edit \
  mdbook

echo "==> Installing markdownlint-cli2"
npm install -g markdownlint-cli2

echo "==> Dev container ready."
echo "    Backend:  cargo run -p tuliprox            (http://localhost:8901)"
echo "    Frontend: cd frontend && trunk serve       (http://localhost:9899)"
