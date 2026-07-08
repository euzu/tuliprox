#!/usr/bin/env bash
set -e

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"

###############################
#
# Standard
# ./build.sh linux-musl
#
# # ARM without Frontend
# ./build.sh armv7 --no-frontend
#
# # Only Frontend
# ./build.sh linux-musl --frontend-only
#
# # Only Backend + clean
# ./build.sh windows --backend-only --clean
#
# # Backend with release optimizations but debug symbols
# ./build.sh linux-musl --backend-only --debug
#
# # Help
# ./build.sh --help
#
#####################################################

########################################
# Globals
########################################
SCRIPT_NAME="$(basename "$0")"
export RUSTFLAGS="--remap-path-prefix $HOME=~"

BUILD_BACKEND=true
BUILD_FRONTEND=true
DO_CLEAN=false
BUILD_DEBUG_SYMBOLS=false
TARGET=""

########################################
# Usage
########################################
usage() {
  cat <<EOF
Usage:
  $SCRIPT_NAME <target> [options]

Targets:
  linux-musl     x86_64-unknown-linux-musl        (cargo zigbuild on macOS, cross otherwise)
  linux-gnu      x86_64-unknown-linux-gnu         (cargo on Linux, cross otherwise)
  armv7          armv7-unknown-linux-musleabihf   (cross)
  aarch64        aarch64-unknown-linux-musl       (cross)
  macos          x86_64-apple-darwin              (cross)
  windows        x86_64-pc-windows-gnu            (cargo)

Options:
  --no-frontend      Skip frontend build
  --frontend-only    Build only frontend
  --backend-only     Build only backend
  --clean            cargo clean before build
  --debug            Keep debug symbols in release backend binary and disable strip
  -h, --help         Show this help

Examples:
  $SCRIPT_NAME linux-musl
  $SCRIPT_NAME armv7 --no-frontend
  $SCRIPT_NAME aarch64 --no-frontend
  $SCRIPT_NAME windows --backend-only
  $SCRIPT_NAME linux-musl --backend-only --debug
  $SCRIPT_NAME linux-gnu --clean
EOF
}

########################################
# Argument parsing
########################################
if [ $# -eq 0 ]; then
  echo "❌ No target specified"
  usage
  exit 1
fi

for arg in "$@"; do
  case "$arg" in
    linux-musl|linux-gnu|armv7|aarch64|macos|windows)
      TARGET="$arg"
      ;;
    --no-frontend)
      BUILD_FRONTEND=false
      ;;
    --frontend-only)
      BUILD_BACKEND=false
      BUILD_FRONTEND=true
      ;;
    --backend-only)
      BUILD_BACKEND=true
      BUILD_FRONTEND=false
      ;;
    --clean)
      DO_CLEAN=true
      ;;
    --debug)
      BUILD_DEBUG_SYMBOLS=true
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "❌ Unknown option: $arg"
      usage
      exit 1
      ;;
  esac
done

if [ -z "$TARGET" ]; then
  echo "❌ No valid target specified"
  usage
  exit 1
fi

########################################
# Clean
########################################
if [ "$DO_CLEAN" = true ]; then
  echo "==> Cleaning workspace"
  cargo clean
fi

########################################
# Backend build
########################################
if [ "$BUILD_BACKEND" = true ]; then
  echo "==> Building backend ($TARGET)"
  if [ "$BUILD_DEBUG_SYMBOLS" = true ]; then
    echo "==> Keeping release debug symbols and disabling strip"
    export CARGO_PROFILE_RELEASE_DEBUG=true
    export CARGO_PROFILE_RELEASE_STRIP=false
  fi

  case "$TARGET" in
    linux-musl)
      if [ "$(uname)" = "Darwin" ]; then
        if ! command -v cargo-zigbuild >/dev/null 2>&1; then
          echo "❌ cargo-zigbuild not found. Install with: cargo install cargo-zigbuild"
          exit 1
        fi
        if ! command -v zig >/dev/null 2>&1; then
          echo "❌ zig not found. Install with: brew install zig"
          exit 1
        fi
        rustup target add x86_64-unknown-linux-musl
        cargo zigbuild -p tuliprox --release --target x86_64-unknown-linux-musl
      else
        cross build -p tuliprox --release --target x86_64-unknown-linux-musl
      fi
      ;;
    linux-gnu)
      if [ "$(uname)" = "Linux" ]; then
        cargo build -p tuliprox --release
      else
        cross build -p tuliprox --release --target x86_64-unknown-linux-gnu
      fi
      ;;
    armv7)
      cross build -p tuliprox --release --target armv7-unknown-linux-musleabihf
      ;;
    aarch64)
      cross build -p tuliprox --release --target aarch64-unknown-linux-musl
      ;;
    macos)
      cross build -p tuliprox --release --target x86_64-apple-darwin
      ;;
    windows)
      cargo build -p tuliprox --release --target x86_64-pc-windows-gnu
      ;;
  esac
fi

########################################
# Frontend build
########################################
if [ "$BUILD_FRONTEND" = true ]; then
  echo "==> Building frontend (trunk)"
  "${SCRIPT_DIR}/build_fe.sh" release
fi

########################################
# Done
########################################
echo "✅ Build finished successfully"
