#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
WORKING_DIR="$(cd -- "${SCRIPT_DIR}/.." && pwd -P)"
cd "${WORKING_DIR}"

die() {
  echo "🧨 Error: $*" >&2
  exit 1
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
