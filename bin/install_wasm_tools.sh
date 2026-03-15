#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
WORKING_DIR="$(cd -- "${SCRIPT_DIR}/.." && pwd -P)"

VERSION="${1:-${WASM_TOOLS_VERSION:-${BINARYEN_VERSION:-128}}}"
INSTALL_DIR="${WASM_TOOLS_INSTALL_DIR:-${WORKING_DIR}/.tools/wasm-tools/version_${VERSION}}"

die() {
  echo "🧨 Error: $*" >&2
  exit 1
}

detect_asset_name() {
  local os
  local arch

  os="$(uname -s)"
  arch="$(uname -m)"

  case "${os}" in
    Linux)
      case "${arch}" in
        x86_64) echo "binaryen-version_${VERSION}-x86_64-linux.tar.gz" ;;
        aarch64|arm64) echo "binaryen-version_${VERSION}-aarch64-linux.tar.gz" ;;
        *) die "Unsupported Linux architecture '${arch}' for wasm tool installer." ;;
      esac
      ;;
    Darwin)
      case "${arch}" in
        x86_64) echo "binaryen-version_${VERSION}-x86_64-macos.tar.gz" ;;
        aarch64|arm64) echo "binaryen-version_${VERSION}-arm64-macos.tar.gz" ;;
        *) die "Unsupported macOS architecture '${arch}' for wasm tool installer." ;;
      esac
      ;;
    *)
      die "Unsupported operating system '${os}' for wasm tool installer."
      ;;
  esac
}

asset_name="$(detect_asset_name)"
download_url="https://github.com/WebAssembly/binaryen/releases/download/version_${VERSION}/${asset_name}"
tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/tuliprox-wasm-tools.XXXXXX")"
archive_path="${tmp_dir}/${asset_name}"

cleanup() {
  rm -rf "${tmp_dir}"
}
trap cleanup EXIT

if [ -x "${INSTALL_DIR}/bin/wasm-opt" ]; then
  installed_version="$("${INSTALL_DIR}/bin/wasm-opt" --version 2>/dev/null || true)"
  if [ "${installed_version}" = "wasm-opt version ${VERSION}" ]; then
    printf '%s\n' "${INSTALL_DIR}/bin"
    exit 0
  fi
fi

mkdir -p "${INSTALL_DIR}"

echo "📦 Installing wasm tools ${VERSION} from ${download_url}"
curl -fsSL "${download_url}" -o "${archive_path}"
rm -rf "${INSTALL_DIR}"
mkdir -p "${INSTALL_DIR}"
tar -xzf "${archive_path}" -C "${INSTALL_DIR}" --strip-components=1

if [ ! -x "${INSTALL_DIR}/bin/wasm-opt" ]; then
  die "Installed wasm tools archive does not contain bin/wasm-opt."
fi

printf '%s\n' "${INSTALL_DIR}/bin"
