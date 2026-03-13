#!/usr/bin/env bash
set -euo pipefail

WORKING_DIR=$(pwd)

ROOT_TOML="${WORKING_DIR}/Cargo.toml"

# Read [workspace.package].version from Cargo.toml
OLD_VERSION="$(
  awk '
    /^\[workspace\.package\][[:space:]]*$/ { in_ws_pkg=1; next }
    /^\[[^]]+\][[:space:]]*$/ { if (in_ws_pkg) exit }
    in_ws_pkg && /^[[:space:]]*version[[:space:]]*=/ {
      if (match($0, /"[^"]+"/)) {
        print substr($0, RSTART + 1, RLENGTH - 2)
        exit
      }
    }
  ' "${ROOT_TOML}" || true
)"
if [ -z "${OLD_VERSION}" ]; then
    echo "🧨 Failed to read [workspace.package].version from '${ROOT_TOML}'."
    exit 1
fi

# Remove pre-release and build metadata (e.g., 1.0.0-dev → 1.0.0)
CLEAN_VERSION="${OLD_VERSION%%-*}"
CLEAN_VERSION="${CLEAN_VERSION%%+*}"
IFS='.' read -r major minor patch <<< "$CLEAN_VERSION"

case "$1" in
  k)
    ;;
  m) # Major bump
     major=$((major + 1))
     minor=0
     patch=0
     ;;
  p) # Minor bump
     minor=$((minor + 1))
     patch=0
     ;;
  *) # Patch bump (default)
     patch=$((patch + 1))
     ;;
esac

NEW_VERSION="${major}.${minor}.${patch}"

# Update only [workspace.package].version in Cargo.toml
TMP_FILE="$(mktemp)"
if awk -v new_version="${NEW_VERSION}" '
    BEGIN { in_ws_pkg=0; version_updated=0 }
    /^\[workspace\.package\][[:space:]]*$/ { in_ws_pkg=1; print; next }
    /^\[[^]]+\][[:space:]]*$/ {
      if (in_ws_pkg && !version_updated) {
        in_ws_pkg=0
      }
    }
    {
      if (in_ws_pkg && !version_updated && /^[[:space:]]*version[[:space:]]*=/) {
        sub(/[[:space:]]*=[[:space:]]*"[^"]*"/, " = \"" new_version "\"")
        version_updated=1
      }
      print
    }
    END {
      if (!version_updated) {
        exit 2
      }
    }
  ' "${ROOT_TOML}" > "${TMP_FILE}"; then
  mv "${TMP_FILE}" "${ROOT_TOML}"
else
  rm -f "${TMP_FILE}"
  echo "🧨 Failed to update [workspace.package].version in '${ROOT_TOML}'."
  exit 1
fi

VERSION=v$NEW_VERSION
echo "🛠️ Set version $VERSION"
echo 
