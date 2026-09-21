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
  jq \
  ffmpeg

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

RUST_VERSION="stable"
RUST_NIGHTLY_VERSION="nightly-2026-09-21"
CARGO_BINSTALL_VERSION="1.23.0"
TRUNK_VERSION="0.21.14"
WASM_BINDGEN_CLI_VERSION="0.2.127"
CROSS_VERSION="0.2.5"
CARGO_EDIT_VERSION="0.13.13"
MDBOOK_VERSION="0.5.4"
CARGO_WATCH_VERSION="8.5.3"
CARGO_LLVM_COV_VERSION="0.9.1"
CARGO_DENY_VERSION="0.20.2"
CARGO_MACHETE_VERSION="0.9.2"
MARKDOWNLINT_CLI2_VERSION="0.23.2"
rustup toolchain install "${RUST_VERSION}" --profile minimal --component clippy --component rustfmt
rustup default "${RUST_VERSION}"

rustup toolchain install "${RUST_NIGHTLY_VERSION}" --profile minimal --component rustfmt --component clippy

rustup target add \
  wasm32-unknown-unknown \
  x86_64-unknown-linux-musl \
  aarch64-unknown-linux-musl

if ! command -v cargo-binstall >/dev/null 2>&1; then
  echo "==> Installing cargo-binstall"
  cargo_binstall_arch="$(uname -m)"
  case "${cargo_binstall_arch}" in
    x86_64)
      cargo_binstall_asset="cargo-binstall-x86_64-unknown-linux-musl.tgz"
      ;;
    aarch64 | arm64)
      cargo_binstall_asset="cargo-binstall-aarch64-unknown-linux-gnu.tgz"
      ;;
    armv7l)
      cargo_binstall_asset="cargo-binstall-armv7-unknown-linux-gnueabihf.tgz"
      ;;
    *)
      echo "Unsupported architecture for cargo-binstall release asset: ${cargo_binstall_arch}" >&2
      exit 1
      ;;
  esac
  cargo_binstall_base_url="https://github.com/cargo-bins/cargo-binstall/releases/download/v${CARGO_BINSTALL_VERSION}"
  cargo_binstall_tmpdir="$(mktemp -d)"
  trap 'rm -rf "${cargo_binstall_tmpdir}"' EXIT

  curl -fsSL "${cargo_binstall_base_url}/${cargo_binstall_asset}" -o "${cargo_binstall_tmpdir}/${cargo_binstall_asset}"
  curl -fsSL "${cargo_binstall_base_url}/SHA256SUMS" -o "${cargo_binstall_tmpdir}/SHA256SUMS"

  (
    cd "${cargo_binstall_tmpdir}"
    grep " ${cargo_binstall_asset}\$" SHA256SUMS | sha256sum -c -
    tar -xzf "${cargo_binstall_asset}"
    install -m 0755 cargo-binstall "${CARGO_HOME:-$HOME/.cargo}/bin/cargo-binstall"
  )

  rm -rf "${cargo_binstall_tmpdir}"
  trap - EXIT
fi

echo "==> Installing Rust CLI tools (trunk, wasm-bindgen, cross, cargo-edit, mdbook, cargo-watch, cargo-llvm-cov, cargo-deny, cargo-machete)"
cargo binstall -y \
  "trunk@${TRUNK_VERSION}" \
  "wasm-bindgen-cli@${WASM_BINDGEN_CLI_VERSION}" \
  "cross@${CROSS_VERSION}" \
  "cargo-edit@${CARGO_EDIT_VERSION}" \
  "mdbook@${MDBOOK_VERSION}" \
  "cargo-watch@${CARGO_WATCH_VERSION}" \
  "cargo-llvm-cov@${CARGO_LLVM_COV_VERSION}" \
  "cargo-deny@${CARGO_DENY_VERSION}" \
  "cargo-machete@${CARGO_MACHETE_VERSION}"

echo "==> Installing wasm tools 128 (wasm-opt)"
chmod +x ./bin/install_wasm_tools.sh
WASM_TOOLS_BIN="$(./bin/install_wasm_tools.sh 128)"
sudo ln -sf "${WASM_TOOLS_BIN}/wasm-opt" /usr/local/bin/wasm-opt

echo "==> Installing markdownlint-cli2"
npm install -g "markdownlint-cli2@${MARKDOWNLINT_CLI2_VERSION}"

echo "==> Dev container ready."
echo "    Backend:  cargo run -p tuliprox            (http://localhost:8901)"
echo "    Frontend: cd frontend && trunk serve       (http://localhost:9899)"
