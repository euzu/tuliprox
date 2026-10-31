#!/bin/bash
set -euo pipefail

# -----------------------------
# Rust / Cargo Setup (Assuming toolchain is handled by CI)
# -----------------------------
export RUSTUP_NO_UPDATE_CHECK=1
export CARGO_NET_GIT_FETCH_WITH_CLI=true
export RUSTFLAGS="--remap-path-prefix $HOME=~"

# Validate arguments
if [ $# -lt 1 ] || [ $# -gt 2 ]; then
    echo "Usage: $(basename "$0") <branch> [build|publish|all]"
    exit 1
fi

BRANCH="$1"
case "$BRANCH" in
    master)  TAG_SUFFIX="latest" ;;
    develop) TAG_SUFFIX="dev"    ;;
    experimental) TAG_SUFFIX="experimental" ;;
    *) echo "🧨 Error: Branch '$BRANCH' not supported"; exit 1 ;;
esac

PHASE="${2:-all}"
case "$PHASE" in
    build|publish|all) ;;
    *) echo "🧨 Error: Phase '$PHASE' not supported"; exit 1 ;;
esac

if [ "$PHASE" != "build" ] && { [ -z "${REPO_OWNER:-}" ] || [ -z "${GITHUB_IO_TOKEN:-}" ]; }; then
    echo "🧨 Error: REPO_OWNER and GITHUB_IO_TOKEN env vars are required for publishing"
    exit 1
fi

echo "🚀 Building for branch: $BRANCH (tag: $TAG_SUFFIX)"

# Directories
WORKING_DIR=$(pwd)
DOCKER_DIR="${WORKING_DIR}/docker"
#BACKEND_DIR="${WORKING_DIR}/backend"
FRONTEND_DIR="${WORKING_DIR}/frontend"
FRONTEND_BUILD_DIR="${FRONTEND_DIR}/dist"
ARTIFACT_DIR="${WORKING_DIR}/artifacts"

# Config
declare -A ARCHITECTURES=([LINUX]=x86_64-unknown-linux-musl [AARCH64]=aarch64-unknown-linux-musl)
declare -A MULTI_PLATFORM_IMAGES=([tuliprox]="scratch-final" [tuliprox-alpine]="alpine-final")
# Master releases also ship bundles for platforms without a Docker image.
RELEASE_ONLY_TARGETS=()
if [ "$BRANCH" = "master" ]; then
    RELEASE_ONLY_TARGETS=(armv7-unknown-linux-musleabihf x86_64-pc-windows-gnu)
fi

# Version detection
VERSION=$(grep -Po '^version\s*=\s*"\K[0-9\.]+' "${WORKING_DIR}/Cargo.toml")
echo "📦 Version: ${VERSION}"

write_checksum() {
    local file_path="$1"
    local file_dir
    local file_name
    file_dir="$(dirname "${file_path}")"
    file_name="$(basename "${file_path}")"
    if command -v sha256sum >/dev/null 2>&1; then
        (cd "${file_dir}" && sha256sum "${file_name}") > "${file_path}.sha256"
    else
        (cd "${file_dir}" && shasum -a 256 "${file_name}") > "${file_path}.sha256"
    fi
}

create_release_bundle() {
    local architecture="$1"
    local binary_path="$2"
    local bundle_root=""
    local bundle_name
    local bundle_path

    bundle_root="$(mktemp -d)"
    trap 'if [ -n "${bundle_root:-}" ] && [ -d "${bundle_root}" ]; then rm -rf "${bundle_root}"; fi' RETURN ERR INT
    mkdir -p "${bundle_root}/tuliprox"

    cp "${binary_path}" "${bundle_root}/tuliprox/"
    chmod +x "${bundle_root}/tuliprox/$(basename "${binary_path}")"
    cp -r "${FRONTEND_BUILD_DIR}" "${bundle_root}/tuliprox/web"
    cp -r "${WORKING_DIR}/resources" "${bundle_root}/tuliprox/resources"

    if [[ "${architecture}" == *-windows-* ]]; then
        bundle_name="tuliprox-v${VERSION}-${architecture}.zip"
        bundle_path="${ARTIFACT_DIR}/${bundle_name}"
        (cd "${bundle_root}" && zip -qr "${bundle_path}" tuliprox)
    else
        bundle_name="tuliprox-v${VERSION}-${architecture}.tar.gz"
        bundle_path="${ARTIFACT_DIR}/${bundle_name}"
        tar -C "${bundle_root}" -czf "${bundle_path}" tuliprox
    fi
    write_checksum "${bundle_path}"
}

# -----------------------------------------
# 1. Documentation + Frontend Build
# -----------------------------------------
if [ "$PHASE" != "publish" ]; then
    # Clean only when building; publishing reuses the validated build outputs.
    rm -rf "${ARTIFACT_DIR}"
    mkdir -p "${ARTIFACT_DIR}"

    echo "🎨 Building frontend..."
    "${WORKING_DIR}/bin/build_fe.sh" release

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

        SOURCE_BIN_PATH="target/${ARCHITECTURE}/release/tuliprox"
        cp "${SOURCE_BIN_PATH}" "${DOCKER_DIR}/binaries/tuliprox-${ARCHITECTURE}"

        create_release_bundle "${ARCHITECTURE}" "${SOURCE_BIN_PATH}"
    done

    for ARCHITECTURE in "${RELEASE_ONLY_TARGETS[@]}"; do
        echo "🔨 Building release bundle for $ARCHITECTURE"
        cross build -p tuliprox --release --target "$ARCHITECTURE" --locked

        BIN_NAME="tuliprox"
        if [[ "${ARCHITECTURE}" == *-windows-* ]]; then
            BIN_NAME="tuliprox.exe"
        fi
        create_release_bundle "${ARCHITECTURE}" "target/${ARCHITECTURE}/release/${BIN_NAME}"
    done

    echo "📦 Release bundles prepared in ${ARTIFACT_DIR}"

    echo "📋 Preparing Docker context..."
    # Remove a context left by an earlier build-only run; cp -r would nest into it.
    rm -rf "${DOCKER_DIR}/web" "${DOCKER_DIR}/resources"
    cp -r "${FRONTEND_BUILD_DIR}" "${DOCKER_DIR}/web"
    cp -r "resources" "${DOCKER_DIR}/resources"
fi

if [ "$PHASE" == "build" ]; then
    exit 0
fi

for ARCHITECTURE in "${ARCHITECTURES[@]}"; do
    test -f "${DOCKER_DIR}/binaries/tuliprox-${ARCHITECTURE}"
done
test -d "${DOCKER_DIR}/web"
test -d "${DOCKER_DIR}/resources"

# -----------------------------------------
# 3. Docker Build & Push (Optimized Caching)
# -----------------------------------------
cd "${DOCKER_DIR}"

# Login
echo "🔑 Logging into GHCR..."
echo "${GITHUB_IO_TOKEN}" | docker login ghcr.io -u "${REPO_OWNER}" --password-stdin

REPO_OWNER_LC="${REPO_OWNER,,}"

for IMAGE_NAME in "${!MULTI_PLATFORM_IMAGES[@]}"; do
    BUILD_TARGET="${MULTI_PLATFORM_IMAGES[$IMAGE_NAME]}"
    TAG_BRANCH="ghcr.io/${REPO_OWNER_LC}/${IMAGE_NAME}:${TAG_SUFFIX}"
    if [ "$BRANCH" = "experimental" ]; then
        # Experimental images must not overwrite official release version tags
        TAG_VERSION="ghcr.io/${REPO_OWNER_LC}/${IMAGE_NAME}:experimental-${VERSION}"
    else
        TAG_VERSION="ghcr.io/${REPO_OWNER_LC}/${IMAGE_NAME}:${VERSION}"
    fi

    echo "🎯 Building multi-platform image: ${IMAGE_NAME}"

    # THE FIX: Using type=gha for automatic GitHub Actions cache management.
    # Scoped per branch/tag_suffix to avoid cache pollution between develop and experimental.
    docker buildx build -f Dockerfile.manual \
        -t "${TAG_VERSION}" \
        -t "${TAG_BRANCH}" \
        --target "$BUILD_TARGET" \
        --platform "linux/amd64,linux/arm64" \
        --cache-from "type=gha,scope=${IMAGE_NAME}-${TAG_SUFFIX}" \
        --cache-to "type=gha,mode=max,scope=${IMAGE_NAME}-${TAG_SUFFIX}" \
        --push \
        .
done

# -----------------------------------------
# Cleanup
# -----------------------------------------
echo "🗑️ Final cleanup..."
rm -rf "${DOCKER_DIR}/binaries" "${DOCKER_DIR}/web" "${DOCKER_DIR}/resources"
