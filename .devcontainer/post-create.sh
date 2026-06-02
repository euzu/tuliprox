#!/usr/bin/env bash

set -euxo pipefail

echo "==> Provisioning tuliprox dev container"

# Ensure cargo-installed binaries are on PATH for this session.
export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:${PATH}"

# Mounted cache volumes are created root-owned; hand them to the dev user.
for dir in "${CARGO_HOME:-/usr/local/cargo}/registry" "${PWD}/target" "${HOME}/.config/gh"; do
  if [ -d "${dir}" ]; then
    sudo chown -R "$(id -u):$(id -g)" "${dir}"
  fi
done

sudo apt-get update
sudo apt-get install -y --no-install-recommends \
  musl-tools \
  pkg-config \
  libssl-dev \
  curl \
  gnupg \
  git \
  build-essential \
  clang \
  lld \
  mold \
  jq

if ! command -v gh >/dev/null 2>&1; then
  echo "==> Installing GitHub CLI"
  curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
    | sudo dd of=/usr/share/keyrings/githubcli-archive-keyring.gpg
  sudo chmod go+r /usr/share/keyrings/githubcli-archive-keyring.gpg
  echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
    | sudo tee /etc/apt/sources.list.d/github-cli.list >/dev/null
  sudo apt-get update
  sudo apt-get install -y --no-install-recommends gh
fi

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

echo "==> Installing Rust CLI tools (trunk, wasm-bindgen, cross, cargo-edit, mdbook, cargo-watch, cargo-llvm-cov, cargo-deny, cargo-machete)"
cargo binstall -y \
  trunk \
  wasm-bindgen-cli \
  cross \
  cargo-edit \
  mdbook \
  cargo-watch \
  cargo-llvm-cov \
  cargo-deny \
  cargo-machete

echo "==> Installing markdownlint-cli2"
npm install -g markdownlint-cli2

echo "==> Dev container ready."
echo "    Backend:  cargo run -p tuliprox            (http://localhost:8901)"
echo "    Frontend: cd frontend && trunk serve       (http://localhost:9899)"
