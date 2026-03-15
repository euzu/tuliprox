#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
WORKING_DIR="$(cd -- "${SCRIPT_DIR}/.." && pwd -P)"
cd "${WORKING_DIR}"

die() {
  echo "🧨 Error: $*" >&2
  exit 1
}

maybe_optimize_wasm() {
  local wasm_opt_version
  local wasm_file
  local tmp_file
  local -a wasm_files

  if ! command -v wasm-opt >/dev/null 2>&1; then
    die "'wasm-opt' is required for frontend builds. Install the wasm tools with './bin/install_wasm_tools.sh 128' and add the returned bin path to PATH."
  fi

  wasm_opt_version="$(wasm-opt --version 2>/dev/null || echo "unknown")"

  shopt -s nullglob
  wasm_files=(dist/*_bg.wasm)
  shopt -u nullglob

  if [ "${#wasm_files[@]}" -eq 0 ]; then
    die "No frontend wasm artifact found for wasm-opt."
  fi

  for wasm_file in "${wasm_files[@]}"; do
    tmp_file="$(mktemp "${TMPDIR:-/tmp}/tuliprox-wasm-opt.XXXXXX.wasm")"
    if wasm-opt -O --all-features --output="${tmp_file}" "${wasm_file}"; then
      if mv "${tmp_file}" "${wasm_file}"; then
        echo "✅ Optimized $(basename "${wasm_file}") with ${wasm_opt_version}"
        continue
      fi

      rm -f "${tmp_file}"
      die "Failed to replace $(basename "${wasm_file}") with the optimized wasm artifact."
    fi

    rm -f "${tmp_file}"
    die "wasm-opt failed for $(basename "${wasm_file}") with ${wasm_opt_version}. Install the wasm tools with './bin/install_wasm_tools.sh 128' and add the returned bin path to PATH."
  done
}

MODE="${1:-release}"
case "${MODE}" in
  release)
    TRUNK_ARGS=(build --release)
    ;;
  debug)
    TRUNK_ARGS=(build)
    ;;
  *)
    die "Unsupported build mode '${MODE}'. Use 'release' or 'debug'."
    ;;
esac

"${SCRIPT_DIR}/build_docs.sh"

if ! command -v trunk >/dev/null 2>&1; then
  die "'trunk' is required. Install it with 'make trunk'."
fi

cd "${WORKING_DIR}/frontend"
rm -rf dist
env -u NO_COLOR RUSTFLAGS="--remap-path-prefix $HOME=~" trunk "${TRUNK_ARGS[@]}"
maybe_optimize_wasm

if [ ! -d build/docs ]; then
  die "Documentation output directory 'frontend/build/docs' does not exist."
fi

if [ -z "$(find build/docs -mindepth 1 -maxdepth 1 -print -quit)" ]; then
  die "Documentation output directory 'frontend/build/docs' is empty."
fi

mkdir -p dist/static
rm -rf dist/static/docs
cp -R build/docs dist/static/docs

echo "✅ Web assets built into frontend/dist with docs at frontend/dist/static/docs"
