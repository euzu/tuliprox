#!/bin/bash
set -euo pipefail

# Required environment variables (set by CI)
if [ -z "${REPO_OWNER:-}" ] || [ -z "${GITHUB_IO_TOKEN:-}" ]; then
    echo "🧨 Error: REPO_OWNER and GITHUB_IO_TOKEN env vars are required"
    exit 1
fi

# -----------------------------
# Rust / Cargo Setup (Assuming toolchain is handled by CI)
# -----------------------------
export RUSTUP_NO_UPDATE_CHECK=1
export CARGO_NET_GIT_FETCH_WITH_CLI=true
export RUSTFLAGS="--remap-path-prefix $HOME=~"

# Validate arguments
if [ $# -ne 1 ]; then
    echo "Usage: $(basename "$0") <branch>"
    exit 1
fi

BRANCH="$1"
case "$BRANCH" in
    master)  TAG_SUFFIX="latest" ;;
    develop) TAG_SUFFIX="dev"    ;;
    *) echo "🧨 Error: Branch '$BRANCH' not supported"; exit 1 ;;
esac

echo "🚀 Building for branch: $BRANCH (tag: $TAG_SUFFIX)"

# Directories
WORKING_DIR=$(pwd)
DOCKER_DIR="${WORKING_DIR}/docker"
BACKEND_DIR="${WORKING_DIR}/backend"
FRONTEND_DIR="${WORKING_DIR}/frontend"
FRONTEND_BUILD_DIR="${FRONTEND_DIR}/dist"

# Config
declare -A ARCHITECTURES=([LINUX]=x86_64-unknown-linux-musl [AARCH64]=aarch64-unknown-linux-musl)
declare -A MULTI_PLATFORM_IMAGES=([tuliprox]="scratch-final" [tuliprox-alpine]="alpine-final")

# Version detection
VERSION=$(grep -Po '^version\s*=\s*"\K[0-9\.]+' "${BACKEND_DIR}/Cargo.toml")
echo "📦 Version: ${VERSION}"

# -----------------------------------------
# 1. Frontend Build
# -----------------------------------------
if [ -d "${FRONTEND_BUILD_DIR}" ]; then
    echo "🎨 Frontend build found, skipping build..."
else
    echo "🎨 Building frontend..."
    cd "${FRONTEND_DIR}" && trunk build --release
fi

cd "$WORKING_DIR"

# -----------------------------------------
# 2. Binary Compilation (Multi-Arch)
# -----------------------------------------
echo "🏗️ Building binaries..."
mkdir -p "${DOCKER_DIR}/binaries"

for PLATFORM in "${!ARCHITECTURES[@]}"; do
    ARCHITECTURE=${ARCHITECTURES[$PLATFORM]}
    echo "🔨 Building for $ARCHITECTURE"

    # Using cross for compilation
    cross build -p tuliprox --release --target "$ARCHITECTURE" --locked

    cp "target/${ARCHITECTURE}/release/tuliprox" "${DOCKER_DIR}/binaries/tuliprox-${ARCHITECTURE}"
done

# -----------------------------------------
# 3. Docker Build & Push (Optimized Caching)
# -----------------------------------------
echo "📋 Preparing Docker context..."
cp -r "${FRONTEND_BUILD_DIR}" "${DOCKER_DIR}/web"
cp -r "resources" "${DOCKER_DIR}/resources"

cd "${DOCKER_DIR}"

# Login
echo "🔑 Logging into GHCR..."
echo "${GITHUB_IO_TOKEN}" | docker login ghcr.io -u "${REPO_OWNER}" --password-stdin

REPO_OWNER_LC="${REPO_OWNER,,}"

for IMAGE_NAME in "${!MULTI_PLATFORM_IMAGES[@]}"; do
    BUILD_TARGET="${MULTI_PLATFORM_IMAGES[$IMAGE_NAME]}"
    TAG_VERSION="ghcr.io/${REPO_OWNER_LC}/${IMAGE_NAME}:${VERSION}"
    TAG_BRANCH="ghcr.io/${REPO_OWNER_LC}/${IMAGE_NAME}:${TAG_SUFFIX}"

    echo "🎯 Building multi-platform image: ${IMAGE_NAME}"

    # THE FIX: Using type=gha for automatic GitHub Actions cache management.
    # No more local files, no more leftover artifacts.
    docker buildx build -f Dockerfile-manual \
        -t "${TAG_VERSION}" \
        -t "${TAG_BRANCH}" \
        --target "$BUILD_TARGET" \
        --platform "linux/amd64,linux/arm64" \
        --cache-from "type=gha,scope=${IMAGE_NAME}" \
        --cache-to "type=gha,mode=max,scope=${IMAGE_NAME}" \
        --push \
        .
done

# -----------------------------------------
# Cleanup
# -----------------------------------------
echo "🗑️ Final cleanup..."
rm -rf "${DOCKER_DIR}/binaries" "${DOCKER_DIR}/web" "${DOCKER_DIR}/resources"
