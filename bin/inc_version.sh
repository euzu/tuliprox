#!/usr/bin/env bash
set -eo pipefail

WORKING_DIR=$(pwd)
BACKEND_DIR="${WORKING_DIR}/backend"

if ! command -v cargo-set-version &> /dev/null
then
    echo "🧨 cargo-set-version could not be found. Install it with 'cargo install cargo-edit'"
    exit 1
fi


# Read current version from Cargo.toml
OLD_VERSION="$(grep '^version' "${BACKEND_DIR}/Cargo.toml" | head -n1 | cut -d'"' -f2 || true)"
if [ -z "${OLD_VERSION}" ]; then
    echo "🧨 Failed to read version from '${BACKEND_DIR}/Cargo.toml' (expected a line like: version = \"x.y.z\")."
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


cargo set-version "$NEW_VERSION"

VERSION=v$NEW_VERSION
echo "🛠️ Set version $VERSION"
echo 
