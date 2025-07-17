#!/usr/bin/env bash
set -euo pipefail
source "${HOME}/.ghcr.io"

# Function to print usage instructions
print_usage() {
    echo "Usage: $(basename "$0") [-r] [-h]"
    echo
    echo "Options:"
    echo "  -r    Enable release image mode (default: false)"
    echo "  -h    Display this help message"
    exit 0
}

beta_image=true

# parse options
while getopts "rh" opt; do
  case $opt in
    r) beta_image=false ;;
    h) print_usage ;;
    \?) echo "Unknown option: -$OPTARG" >&2 ;;
  esac
done

WORKING_DIR=$(pwd)
BIN_DIR="${WORKING_DIR}/bin"
RESOURCES_DIR="${WORKING_DIR}/resources"
DOCKER_DIR="${WORKING_DIR}/docker"
BACKEND_DIR="${WORKING_DIR}/backend"
FRONTEND_DIR="${WORKING_DIR}/frontend"
TARGET=aarch64-unknown-linux-musl
VERSION=$(grep -Po '^version\s*=\s*"\K[0-9\.]+' ${BACKEND_DIR}/Cargo.toml || true)
if [ -z "${VERSION}" ]; then
    echo "Error: Failed to determine the version from Cargo.toml."
    exit 1
fi

if [ "$beta_image" = true ]; then
  # Split the version into its components using '.' as a delimiter
  IFS='.' read -r major minor patch <<< "$VERSION"
  # Increment the patch version
  patch=$((patch + 1))
  # Combine the components back into a version string
  VERSION="$major.$minor.${patch}-beta"
fi

if [ ! -f "${BIN_DIR}/build_resources.sh" ]; then
  "${BIN_DIR}/build_resources.sh"
fi

cd "$FRONTEND_DIR" && rm -rf build && yarn  && yarn build

# Check if the frontend build directory exists
if [ ! -d "${FRONTEND_DIR}/build" ]; then
    echo "Error: Web directory '$FRONTEND_DIR/build' does not exist."
    exit 1
fi

cd "$WORKING_DIR"

cargo clean
env RUSTFLAGS="--remap-path-prefix $HOME=~" cross build -p tuliprox --release --target "$TARGET"

# Check if the binary exists
if [ ! -f "${WORKING_DIR}/target/${TARGET}/release/tuliprox" ]; then
    echo "Error: Static binary '${WORKING_DIR}/target/${TARGET}/release/tuliprox' does not exist."
    exit 1
fi

cd "$WORKING_DIR"

# Prepare Docker build context
cp "${WORKING_DIR}/target/${TARGET}/release/tuliprox" "${DOCKER_DIR}/"
rm -rf "${DOCKER_DIR}/web"
cp -r "${FRONTEND_DIR}/build" "${DOCKER_DIR}/web"
cp -r "${RESOURCES_DIR}" "${DOCKER_DIR}/"

cd "${DOCKER_DIR}"
echo "Building Docker images for version ${VERSION}"
if [ "$beta_image" = true ]; then
  SCRATCH_IMAGE_NAME=tuliprox-aarch64-beta
else
  SCRATCH_IMAGE_NAME=tuliprox-aarch64
fi

# Build scratch image and tag as "latest"
docker build -f Dockerfile-manual -t ghcr.io/euzu/${SCRATCH_IMAGE_NAME}:"${VERSION}" --target scratch-final .
docker tag ghcr.io/euzu/${SCRATCH_IMAGE_NAME}:"${VERSION}" ghcr.io/euzu/${SCRATCH_IMAGE_NAME}:latest

echo "Logging into GitHub Container Registry..."
docker login ghcr.io -u euzu -p "${GHCR_IO_TOKEN}"

# Push scratch
docker push ghcr.io/euzu/${SCRATCH_IMAGE_NAME}:"${VERSION}"
docker push ghcr.io/euzu/${SCRATCH_IMAGE_NAME}:latest

# Clean up
echo "Cleaning up build artifacts..."
rm -rf "${DOCKER_DIR}/web"
rm -f "${DOCKER_DIR}/tuliprox"
rm -rf "${DOCKER_DIR}/resources"

echo "Docker images ghcr.io/euzu/${SCRATCH_IMAGE_NAME}:${VERSION} have been successfully built, tagged, and pushed."
