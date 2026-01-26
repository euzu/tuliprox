#!/bin/bash
set -euo pipefail

# Required environment variables (set by CI)
if [ -z "${REPO_OWNER:-}" ]; then
    echo "🧨 Error: REPO_OWNER env var is required"
    exit 1
fi

if [ -z "${GITHUB_IO_TOKEN:-}" ]; then
    echo "🧨 Error: GITHUB_IO_TOKEN env var is required"
    exit 1
fi

# -----------------------------
# Rust / Cargo Setup
# -----------------------------
export RUSTUP_NO_UPDATE_CHECK=1
export CARGO_NET_GIT_FETCH_WITH_CLI=true
export RUSTFLAGS="--remap-path-prefix $HOME=~"

REQUIRED_TARGETS=(
  "x86_64-unknown-linux-musl"
  "aarch64-unknown-linux-musl"
)

echo "🦀 Checking Rust toolchain..."

if ! rustup toolchain list | grep -q "^stable"; then
  echo "🔧 Installing stable toolchain..."
  rustup toolchain install stable --profile minimal
fi

rustup default stable

echo "🎯 Ensuring required targets are installed..."
for TARGET in "${REQUIRED_TARGETS[@]}"; do
  if ! rustup target list --installed | grep -q "^${TARGET}$"; then
    echo "➕ Installing target $TARGET"
    rustup target add "$TARGET"
  fi
done

WASM_TARGET="wasm32-unknown-unknown"

if ! rustup target list --installed | grep -q "^${WASM_TARGET}$"; then
  echo "➕ Installing wasm target for frontend: ${WASM_TARGET}"
  rustup target add "${WASM_TARGET}"
fi

# -----------------------------------------
# Function to print usage instructions
# -----------------------------------------
print_usage() {
    echo "Usage: $(basename "$0") <branch>"
    echo
    echo "Arguments:"
    echo "  branch    Git branch name (only 'master' and 'develop' are supported)"
    echo
    echo "Examples:"
    echo "  $(basename "$0") master    # Builds and pushes with :VERSION and :latest tags"
    echo "  $(basename "$0") develop   # Builds and pushes with :VERSION and :dev tags"
    exit 1
}

# Validate arguments
if [ $# -ne 1 ]; then
    echo "🧨 Error: Exactly one argument required"
    print_usage
fi

BRANCH="$1"

# Validate branch
case "$BRANCH" in
    master)
        TAG_SUFFIX="latest"
        ;;
    develop)
        TAG_SUFFIX="dev"
        ;;
    *)
        echo "🧨 Error: Branch '$BRANCH' is not supported. Only 'master' and 'develop' are allowed."
        exit 1
        ;;
esac

echo "🚀 Building Docker images for branch: $BRANCH (tag: $TAG_SUFFIX)"

# Set up directories
WORKING_DIR=$(pwd)
BIN_DIR="${WORKING_DIR}/bin"
RESOURCES_DIR="${WORKING_DIR}/resources"
DOCKER_DIR="${WORKING_DIR}/docker"
BACKEND_DIR="${WORKING_DIR}/backend"
FRONTEND_DIR="${WORKING_DIR}/frontend"
FRONTEND_BUILD_DIR="${FRONTEND_DIR}/dist"

# Define architectures and their corresponding builds
declare -A ARCHITECTURES=(
    [LINUX]=x86_64-unknown-linux-musl
    [AARCH64]=aarch64-unknown-linux-musl
)

# Images to build with multi-platform support
declare -A MULTI_PLATFORM_IMAGES=(
    [tuliprox]="scratch-final"
    [tuliprox-alpine]="alpine-final"
)

# Get version from Cargo.toml
VERSION=$(grep -Po '^version\s*=\s*"\K[0-9\.]+' "${BACKEND_DIR}/Cargo.toml")
if [ -z "${VERSION}" ]; then
    echo "🧨 Error: Failed to determine the version from Cargo.toml"
    exit 1
fi

echo "📦 Version: ${VERSION}"

# Build resources if needed (check if resources are already built)
# Note: Docker build handles resource creation with its own ffmpeg container
RESOURCES_BUILT=true
for resource in "channel_unavailable.ts" "user_connections_exhausted.ts" "provider_connections_exhausted.ts" "user_account_expired.ts" "panel_api_provisioning.ppm"; do
    if [ ! -f "${RESOURCES_DIR}/${resource}" ]; then
        RESOURCES_BUILT=false
        break
    fi
done

if [ "$RESOURCES_BUILT" = "false" ] && [ -f "${BIN_DIR}/build_resources.sh" ]; then
    echo "🛠️ Building resources..."
    if ! "${BIN_DIR}/build_resources.sh"; then
        echo "⚠️ Resource building failed, but Docker build will handle resource creation"
    fi
elif [ "$RESOURCES_BUILT" = "true" ]; then
    echo "🛠️ Resources already built, skipping..."
fi

# Build frontend (skip if cached)
if [ "${FRONTEND_CACHE_HIT:-false}" = "true" ] && [ -d "${FRONTEND_BUILD_DIR}" ]; then
    echo "🎨 Frontend build found in cache, skipping build..."
else
    echo "🎨 Building frontend..."
    rm -rf "${FRONTEND_BUILD_DIR}"
    cd "${FRONTEND_DIR}" && env RUSTFLAGS="--remap-path-prefix $HOME=~" trunk build --release

    # Check if the frontend build directory exists
    if [ ! -d "${FRONTEND_BUILD_DIR}" ]; then
        echo "🧨 Error: Frontend build directory '${FRONTEND_BUILD_DIR}' does not exist"
        exit 1
    fi
fi

cd "$WORKING_DIR"

# Build binaries for all architectures first
echo "🏗️ Building binaries for all architectures..."

# Avoid executing stale build artifacts when caches are missing or the runner image changes.
if [ "${TULIPROX_FORCE_CLEAN:-0}" = "1" ] || [ "${CARGO_DEPS_CACHE_HIT:-false}" != "true" ]; then
    cargo clean || true
fi

for PLATFORM in "${!ARCHITECTURES[@]}"; do
    ARCHITECTURE=${ARCHITECTURES[$PLATFORM]}
    
    echo "🔨 Building binary for architecture: $ARCHITECTURE"

    # Use incremental compilation and enable cache-friendly flags
    env RUSTFLAGS="--remap-path-prefix $HOME=~ -C incremental=/tmp/rust-incremental-${ARCHITECTURE}" \
        CARGO_INCREMENTAL=1 \
        cross build -p tuliprox --release --target "$ARCHITECTURE" --locked
    
    BINARY_PATH="${WORKING_DIR}/target/${ARCHITECTURE}/release/tuliprox"
    if [ ! -f "$BINARY_PATH" ]; then
        echo "🧨 Error: Binary $BINARY_PATH does not exist"
        exit 1
    fi
    
    # Copy binary with architecture suffix for multi-platform build
    mkdir -p "${DOCKER_DIR}/binaries"
    cp "$BINARY_PATH" "${DOCKER_DIR}/binaries/tuliprox-${ARCHITECTURE}"
done

# Prepare common Docker context
echo "📋 Preparing Docker context..."
rm -rf "${DOCKER_DIR}/web"
rm -rf "${DOCKER_DIR}/resources"
cp -r "${FRONTEND_BUILD_DIR}" "${DOCKER_DIR}/web"
cp -r "${RESOURCES_DIR}" "${DOCKER_DIR}/resources"

cd "${DOCKER_DIR}"

# Login to GitHub Container Registry (needed before buildx push)
echo "🔑 Logging into GitHub Container Registry..."
printf '%s' "${GITHUB_IO_TOKEN}" | docker login ghcr.io --username "${REPO_OWNER}" --password-stdin

declare -a BUILT_IMAGES=()

# Build multi-platform images
for IMAGE_NAME in "${!MULTI_PLATFORM_IMAGES[@]}"; do
    BUILD_TARGET="${MULTI_PLATFORM_IMAGES[$IMAGE_NAME]}"
    
    echo "🎯 Building multi-platform image: ${IMAGE_NAME} with target ${BUILD_TARGET}"
    
    # Prepare tags based on branch
    DOCKER_TAGS="-t ghcr.io/${REPO_OWNER}/${IMAGE_NAME}:${VERSION} -t ghcr.io/${REPO_OWNER}/${IMAGE_NAME}:${TAG_SUFFIX}"
    BUILT_IMAGES+=("ghcr.io/${REPO_OWNER}/${IMAGE_NAME}:${VERSION}")
    BUILT_IMAGES+=("ghcr.io/${REPO_OWNER}/${IMAGE_NAME}:${TAG_SUFFIX}")

    # Build and push multi-platform image directly with cache
    BUILDX_CACHE_ARGS=()
    [ -n "${BUILDX_CACHE_FROM:-}" ] && BUILDX_CACHE_ARGS+=(--cache-from "${BUILDX_CACHE_FROM}")
    [ -n "${BUILDX_CACHE_TO:-}" ] && BUILDX_CACHE_ARGS+=(--cache-to "${BUILDX_CACHE_TO}")

    # If you build on local, you need to activate buildx multiarch builds
    # >  docker buildx create --name multiarch --driver docker-container --use --bootstrap
    # > docker run --rm --privileged tonistiigi/binfmt:latest --install all

    docker buildx build -f Dockerfile-manual \
        ${DOCKER_TAGS} \
        --target "$BUILD_TARGET" \
        --platform "linux/amd64,linux/arm64" \
        "${BUILDX_CACHE_ARGS[@]}" \
        --push \
        .
done

# Clean up Docker context
echo "🗑️ Cleaning up binaries..."
rm -rf "${DOCKER_DIR}/binaries"

cd "$WORKING_DIR"

# Final cleanup
echo "🗑️ Cleaning up Docker context..."
rm -rf "${DOCKER_DIR}/web"
rm -f "${DOCKER_DIR}/tuliprox"
rm -rf "${DOCKER_DIR}/resources"

echo "🎉 Docker images for branch '$BRANCH' (version ${VERSION}) have been successfully built and pushed!"
echo "📋 Built images:"
for img in "${BUILT_IMAGES[@]}"; do
    echo "   - $img"
done
