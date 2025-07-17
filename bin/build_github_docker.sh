#!/bin/bash
set -euo pipefail
source "${HOME}/.ghcr.io"

WORKING_DIR=$(pwd)
BIN_DIR="${WORKING_DIR}/bin"
RESOURCES_DIR="${WORKING_DIR}/resources"
DOCKER_DIR="${WORKING_DIR}/docker"
BACKEND_DIR="${WORKING_DIR}/backend"
FRONTEND_DIR="${WORKING_DIR}/frontend"
declare -A ARCHITECTURES=(
    [LINUX]=x86_64-unknown-linux-musl
    [AARCH64]=aarch64-unknown-linux-musl
)

declare -A BUILDS=(
    [LINUX]="tuliprox:scratch-final tuliprox-alpine:alpine-final"
    [AARCH64]="tuliprox-aarch64:scratch-final"
)

cd "$FRONTEND_DIR" && rm -rf build && yarn  && yarn build
cd "$WORKING_DIR"

if [ ! -f "${BIN_DIR}/build_resources.sh" ]; then
  "${BIN_DIR}/build_resources.sh"
fi

# Check if the frontend build directory exists
if [ ! -d "$FRONTEND_DIR/build" ]; then
    echo "🧨 Error: Web directory '$FRONTEND_DIR/build' does not exist."
    exit 1
fi

cd "$WORKING_DIR"


VERSION=$(grep -Po '^version\s*=\s*"\K[0-9\.]+' "${BACKEND_DIR}/Cargo.toml")
if [ -z "${VERSION}" ]; then
    echo "🧨 Error: Failed to determine the version."
    exit 1
fi

declare -a BUILT_IMAGES=()

# Start build loop per platform
for PLATFORM in "${!ARCHITECTURES[@]}"; do
  ARCHITECTURE=${ARCHITECTURES[$PLATFORM]}

  echo "🛠️ Building binary for architecture: $ARCHITECTURE"

  cargo clean || true
  env RUSTFLAGS="--remap-path-prefix $HOME=~" cross build -p tuliprox --release --target "$ARCHITECTURE"

  BINARY_PATH="${WORKING_DIR}/target/${ARCHITECTURE}/release/tuliprox"
  if [ ! -f "$BINARY_PATH" ]; then
      echo "🧨 Error: Binary $BINARY_PATH does not exist."
      exit 1
  fi

  # Prepare Docker context
  cp "$BINARY_PATH" "${DOCKER_DIR}/"
  rm -rf "${DOCKER_DIR}/web"
  cp -r "${FRONTEND_DIR}/build" "${DOCKER_DIR}/web"
  cp -r "${RESOURCES_DIR}" "${DOCKER_DIR}/resources"

  cd "${DOCKER_DIR}"
  echo "🛠️ Building Docker images for platform: $PLATFORM, version: ${VERSION}"

  for pair in ${BUILDS[$PLATFORM]}; do
      IMAGE_NAME="${pair%%:*}"
      BUILD_TARGET="${pair##*:}"

      echo "🎯 Building ${IMAGE_NAME} with target ${BUILD_TARGET}"

      docker build -f Dockerfile-manual \
        -t ghcr.io/euzu/${IMAGE_NAME}:${VERSION} \
        --target "$BUILD_TARGET" .

      docker tag ghcr.io/euzu/${IMAGE_NAME}:${VERSION} ghcr.io/euzu/${IMAGE_NAME}:latest

      BUILT_IMAGES+=("ghcr.io/euzu/${IMAGE_NAME}:${VERSION}")
      BUILT_IMAGES+=("ghcr.io/euzu/${IMAGE_NAME}:latest")
  done
done

cd "$WORKING_DIR"

echo "🔑 Logging into GitHub Container Registry..."
docker login ghcr.io -u euzu -p "${GHCR_IO_TOKEN}"
for img in "${BUILT_IMAGES[@]}"; do
     echo "📦 Pushing $img"
     docker push "$img"
done

# Clean up
echo "🗑 Cleaning up Docker context..."
rm -rf "${DOCKER_DIR}/web"
rm -f "${DOCKER_DIR}/tuliprox"
rm -rf "${DOCKER_DIR}/resources"

echo "🎉 Docker images for version ${VERSION} have been successfully built, tagged, and pushed."
