#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
WORKING_DIR="$(cd -- "${SCRIPT_DIR}/.." && pwd -P)"
cd "${WORKING_DIR}"

if ! command -v mdbook >/dev/null 2>&1; then
  echo "🧨 Error: 'mdbook' is required. Install it with 'make mdbook'." >&2
  exit 1
fi

echo "📚 Building documentation with mdBook..."
mdbook build
echo "✅ Documentation built into frontend/build/docs"
