#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
WORKING_DIR="$(cd -- "${SCRIPT_DIR}/.." && pwd -P)"
cd "${WORKING_DIR}"

die() {
  echo "🧨 Error: $*" >&2
  exit 1
}

info() {
  echo "ℹ️ $*" >&2
}

IMAGE_TAG="${IMAGE_TAG:-local}"
PLATFORM="${PLATFORM:-linux/amd64}"
CROSS_REPO_DIR="${CROSS_REPO_DIR:-}"
CROSS_REPO_REF="${CROSS_REPO_REF:-}"
OSXCROSS_REPO_DIR="${OSXCROSS_REPO_DIR:-}"
OSXCROSS_REPO_REF="${OSXCROSS_REPO_REF:-}"
SDK_CACHE_DIR="${SDK_CACHE_DIR:-${WORKING_DIR}/dev/osxcross/tarballs}"
SDK_PATH="${SDK_PATH:-}"
SDK_URL="${SDK_URL:-}"
XCODE_DMG="${XCODE_DMG:-}"
XCODE_XIP="${XCODE_XIP:-}"
CLT_DMG="${CLT_DMG:-}"
FORCE_SDK_REPACK="${FORCE_SDK_REPACK:-0}"
DARWIN_TARGET_TRIPLES="${DARWIN_TARGET_TRIPLES:-}"
OSXCROSS_COMMIT_FOR_TOOLCHAINS="${OSXCROSS_COMMIT_FOR_TOOLCHAINS:-master}"

IMAGES=(
  "x86_64-apple-darwin-cross"
  "aarch64-apple-darwin-cross"
)

apply_darwin_target_filter() {
  local triples="${1:-}"
  if [ -z "${triples}" ]; then
    return 0
  fi

  local -a images_filtered
  images_filtered=()

  local triple
  for triple in ${triples}; do
    case "${triple}" in
      x86_64-apple-darwin) images_filtered+=("x86_64-apple-darwin-cross") ;;
      aarch64-apple-darwin) images_filtered+=("aarch64-apple-darwin-cross") ;;
      *)
        die "Unsupported Darwin target triple for toolchain image build: ${triple} (expected x86_64-apple-darwin or aarch64-apple-darwin)"
        ;;
    esac
  done

  if [ "${#images_filtered[@]}" -eq 0 ]; then
    die "DARWIN_TARGET_TRIPLES provided but no supported Darwin targets found: ${triples}"
  fi

  IMAGES=("${images_filtered[@]}")
}

apply_darwin_target_filter "${DARWIN_TARGET_TRIPLES}"

CROSS_IMAGE_REPOSITORY="${CROSS_IMAGE_REPOSITORY:-ghcr.io/cross-rs}"

if [ -n "${SDK_PATH}" ] && [ -n "${SDK_URL}" ]; then
  die "Provide only one of SDK_PATH or SDK_URL."
fi

if ! command -v docker >/dev/null 2>&1; then
  die "'docker' is required."
fi
if ! docker info >/dev/null 2>&1; then
  die "Docker daemon is not available."
fi
if ! command -v git >/dev/null 2>&1; then
  die "'git' is required."
fi
if ! command -v cargo >/dev/null 2>&1; then
  die "'cargo' is required to run cargo build-docker-image."
fi
if ! command -v docker >/dev/null 2>&1 || ! docker buildx version >/dev/null 2>&1; then
  die "'docker buildx' is required."
fi

missing_images=()
for image in "${IMAGES[@]}"; do
  if docker image inspect "${image}:${IMAGE_TAG}" >/dev/null 2>&1; then
    info "Image already present: ${image}:${IMAGE_TAG}"
  elif docker image inspect "${CROSS_IMAGE_REPOSITORY}/${image}:${IMAGE_TAG}" >/dev/null 2>&1; then
    info "Found upstream-tagged image locally: ${CROSS_IMAGE_REPOSITORY}/${image}:${IMAGE_TAG} (retagging)"
    docker tag "${CROSS_IMAGE_REPOSITORY}/${image}:${IMAGE_TAG}" "${image}:${IMAGE_TAG}"
  else
    missing_images+=("${image}")
  fi
done

if [ "${#missing_images[@]}" -eq 0 ]; then
  info "All Darwin cross images already present."
  exit 0
fi

if [ -n "${SDK_URL}" ] && ! command -v curl >/dev/null 2>&1 && ! command -v wget >/dev/null 2>&1; then
  die "SDK_URL provided but neither curl nor wget is available."
fi

mkdir -p "${SDK_CACHE_DIR}"

find_cached_sdk() {
  # Pick the highest SDK version by filename (not mtime).
  # Example: prefer MacOSX26.2.sdk.tar.xz over MacOSX26.sdk.tar.xz.
  shopt -s nullglob
  local files=("${SDK_CACHE_DIR}"/MacOSX*.sdk.tar.xz)
  shopt -u nullglob
  if [ "${#files[@]}" -eq 0 ]; then
    return 0
  fi

  (
    local f
    for f in "${files[@]}"; do
      printf '%s\t%s\n' "${f##*/}" "${f}"
    done
  ) | sort --version-sort -t $'\t' -k1,1 | tail -n 1 | cut -f2- || true
}

download_sdk() {
  local url="${1}"
  local filename
  filename="$(basename "${url}")"
  local dest="${SDK_CACHE_DIR}/${filename}"
  info "Downloading SDK to ${dest}"
  if command -v curl >/dev/null 2>&1; then
    curl -L -o "${dest}" "${url}"
  else
    wget -O "${dest}" "${url}"
  fi
  echo "${dest}"
}

find_osxcross_tools_dir() {
  local base="${1}"
  local candidates=(
    "${base}/osxcross/tools"
    "${base}/tools"
  )
  local dir
  for dir in "${candidates[@]}"; do
    if [ -d "${dir}" ]; then
      echo "${dir}"
      return 0
    fi
  done
  return 1
}

find_cross_toolchains_docker_dir() {
  local base="${1}"
  local candidates=(
    "${base}/cross-toolchains/docker"
    "${base}/docker/cross-toolchains/docker"
  )
  local dir
  for dir in "${candidates[@]}"; do
    if [ -d "${dir}" ]; then
      echo "${dir}"
      return 0
    fi
  done
  return 1
}

patch_cross_toolchains_darwin_sh() {
  local cross_toolchains_docker_dir="${1}"
  local darwin_sh="${cross_toolchains_docker_dir}/darwin.sh"
  if [ ! -f "${darwin_sh}" ]; then
    return 0
  fi

  if grep -Eq '^[[:space:]]*local commit=' "${darwin_sh}"; then
    if grep -Eq "^[[:space:]]*local commit=${OSXCROSS_COMMIT_FOR_TOOLCHAINS}$" "${darwin_sh}"; then
      :
    else
      info "Patching cross-toolchains darwin.sh osxcross commit -> ${OSXCROSS_COMMIT_FOR_TOOLCHAINS}"
      sed -i.bak -E "s/^([[:space:]]*local commit=).*/\\1${OSXCROSS_COMMIT_FOR_TOOLCHAINS}/" "${darwin_sh}"
      rm -f "${darwin_sh}.bak"
    fi
  fi

  # Workaround for an osxcross/tools.sh bug: it filters out SDK paths containing
  # a ".<digit>" substring when SDK_VERSION has no dot (e.g. "26"). cross-toolchains'
  # darwin.sh clones osxcross into a mktemp dir like "/tmp/tmp.5abcd...", which then
  # triggers the filter. Use a mktemp template without dots to avoid ".<digit>".
  if grep -Fq 'td="$(mktemp -d)"' "${darwin_sh}"; then
    info "Patching cross-toolchains darwin.sh mktemp template (osxcross SDK detection workaround)"
    sed -i.bak 's/td=\"\\$(mktemp -d)\"/td=\"$(mktemp -d -t osxcross-aXXXXXXXX)\"/' "${darwin_sh}"
    rm -f "${darwin_sh}.bak"
  fi
}

ensure_osxcross_repo() {
  if [ -n "${OSXCROSS_REPO_DIR}" ]; then
    if [ ! -d "${OSXCROSS_REPO_DIR}" ]; then
      die "OSXCROSS_REPO_DIR not found: ${OSXCROSS_REPO_DIR}"
    fi
    return 0
  fi

  osxcross_cleanup_dir="$(mktemp -d)"
  info "Cloning osxcross into ${osxcross_cleanup_dir}"
  git clone https://github.com/tpoechtrager/osxcross.git "${osxcross_cleanup_dir}/osxcross"
  OSXCROSS_REPO_DIR="${osxcross_cleanup_dir}/osxcross"
}

run_sdk_packaging() {
  local tools_dir="${1}"
  local host_os
  host_os="$(uname -s)"
  local after

  info "Packaging macOS SDK via osxcross tools (${tools_dir})"
  if [ "${host_os}" = "Darwin" ]; then
    if [ -n "${XCODE_DMG}" ]; then
      (cd "${SDK_CACHE_DIR}" && bash "${tools_dir}/gen_sdk_package.sh" "${XCODE_DMG}")
    elif [ -n "${CLT_DMG}" ]; then
      (cd "${SDK_CACHE_DIR}" && bash "${tools_dir}/gen_sdk_package_tools.sh" "${CLT_DMG}")
    elif [ -n "${XCODE_XIP}" ]; then
      (cd "${SDK_CACHE_DIR}" && bash "${tools_dir}/gen_sdk_package_pbzx.sh" "${XCODE_XIP}")
    else
      (cd "${SDK_CACHE_DIR}" && bash "${tools_dir}/gen_sdk_package.sh")
    fi
  else
    if [ -n "${XCODE_XIP}" ]; then
      (cd "${SDK_CACHE_DIR}" && bash "${tools_dir}/gen_sdk_package_pbzx.sh" "${XCODE_XIP}")
    elif [ -n "${XCODE_DMG}" ]; then
      if [ -x "${tools_dir}/gen_sdk_package_darling_dmg.sh" ]; then
        (cd "${SDK_CACHE_DIR}" && bash "${tools_dir}/gen_sdk_package_darling_dmg.sh" "${XCODE_DMG}")
      elif [ -x "${tools_dir}/gen_sdk_package_p7zip.sh" ]; then
        (cd "${SDK_CACHE_DIR}" && bash "${tools_dir}/gen_sdk_package_p7zip.sh" "${XCODE_DMG}")
      else
        die "No DMG packaging script found in ${tools_dir}."
      fi
    elif [ -n "${CLT_DMG}" ]; then
      (cd "${SDK_CACHE_DIR}" && bash "${tools_dir}/gen_sdk_package_tools_dmg.sh" "${CLT_DMG}")
    else
      die "No SDK source provided. Set XCODE_XIP, XCODE_DMG, or CLT_DMG."
    fi
  fi

  after="$(find_cached_sdk || true)"
  if [ -z "${after}" ]; then
    die "SDK packaging did not produce any MacOSX*.sdk.tar.xz in ${SDK_CACHE_DIR}"
  fi
  echo "${after}"
}

ensure_sdk_tarball() {
  if [ "${FORCE_SDK_REPACK}" = "1" ]; then
    return 1
  fi
  local cached
  cached="$(find_cached_sdk)"
  if [ -n "${cached}" ]; then
    echo "${cached}"
    return 0
  fi
  return 1
}

SDK_PATH_EFFECTIVE=""
if [ -n "${SDK_PATH}" ]; then
  SDK_PATH_EFFECTIVE="${SDK_PATH}"
elif [ -n "${SDK_URL}" ]; then
  SDK_PATH_EFFECTIVE="$(download_sdk "${SDK_URL}")"
elif SDK_PATH_EFFECTIVE="$(ensure_sdk_tarball)"; then
  :
else
  SDK_PATH_EFFECTIVE=""
fi

SDK_BASENAME=""
if [ -n "${SDK_PATH_EFFECTIVE}" ]; then
  if [ ! -f "${SDK_PATH_EFFECTIVE}" ]; then
    die "SDK file not found: ${SDK_PATH_EFFECTIVE}"
  fi
  SDK_BASENAME="$(basename "${SDK_PATH_EFFECTIVE}")"
  if [[ "${SDK_BASENAME}" != MacOSX*.sdk.tar.xz ]]; then
    die "SDK file name must match MacOSX*.sdk.tar.xz (osxcross naming). Got: ${SDK_BASENAME}"
  fi
fi

cleanup_dir=""
osxcross_cleanup_dir=""
if [ -n "${CROSS_REPO_DIR}" ]; then
  if [ ! -d "${CROSS_REPO_DIR}" ]; then
    die "CROSS_REPO_DIR not found: ${CROSS_REPO_DIR}"
  fi
else
  cleanup_dir="$(mktemp -d)"
  info "Cloning cross into ${cleanup_dir}"
  git clone https://github.com/cross-rs/cross.git "${cleanup_dir}/cross"
  CROSS_REPO_DIR="${cleanup_dir}/cross"
fi

cd "${CROSS_REPO_DIR}"
if [ -n "${CROSS_REPO_REF}" ]; then
  git fetch --all --tags
  git checkout "${CROSS_REPO_REF}"
fi
git submodule update --init --recursive

cross_toolchains_docker_dir="$(find_cross_toolchains_docker_dir "${CROSS_REPO_DIR}" || true)"
if [ -z "${cross_toolchains_docker_dir}" ]; then
  die "cross-toolchains docker dir not found in ${CROSS_REPO_DIR} (expected: cross-toolchains/docker or docker/cross-toolchains/docker)"
fi

patch_cross_toolchains_darwin_sh "${cross_toolchains_docker_dir}"

if [ -z "${SDK_PATH_EFFECTIVE}" ]; then
  info "No macOS SDK tarball found; attempting to package one via osxcross"
  ensure_osxcross_repo
  if [ -z "${OSXCROSS_REPO_DIR}" ]; then
    die "Unable to obtain osxcross checkout; set OSXCROSS_REPO_DIR or provide SDK_PATH/SDK_URL."
  fi
  if [ -n "${OSXCROSS_REPO_REF}" ]; then
    (cd "${OSXCROSS_REPO_DIR}" && git fetch --all --tags && git checkout "${OSXCROSS_REPO_REF}")
  fi

  tools_dir="$(find_osxcross_tools_dir "${OSXCROSS_REPO_DIR}" || true)"
  if [ -z "${tools_dir}" ]; then
    die "osxcross tools not found under ${OSXCROSS_REPO_DIR}; set OSXCROSS_REPO_DIR or provide SDK_PATH/SDK_URL instead."
  fi
  SDK_PATH_EFFECTIVE="$(run_sdk_packaging "${tools_dir}")"
  SDK_BASENAME="$(basename "${SDK_PATH_EFFECTIVE}")"
fi

info "Using macOS SDK tarball: ${SDK_BASENAME}"

BUILD_ARGS=()
if [ -n "${SDK_URL}" ]; then
  BUILD_ARGS+=(--build-arg "MACOS_SDK_URL=${SDK_URL}")
else
  SDK_DIR="sdk"
  mkdir -p "${CROSS_REPO_DIR}/docker/${SDK_DIR}"
  cp -f "${SDK_PATH_EFFECTIVE}" "${CROSS_REPO_DIR}/docker/${SDK_DIR}/${SDK_BASENAME}"
  BUILD_ARGS+=(--build-arg "MACOS_SDK_DIR=${SDK_DIR}")
  BUILD_ARGS+=(--build-arg "MACOS_SDK_FILE=${SDK_BASENAME}")
fi

export DOCKER_DEFAULT_PLATFORM="${PLATFORM}"

for image in "${missing_images[@]}"; do
  info "Building ${image}:${IMAGE_TAG}"
  cargo build-docker-image --tag "${IMAGE_TAG}" "${BUILD_ARGS[@]}" "${image}"

  if docker image inspect "${CROSS_IMAGE_REPOSITORY}/${image}:${IMAGE_TAG}" >/dev/null 2>&1; then
    docker tag "${CROSS_IMAGE_REPOSITORY}/${image}:${IMAGE_TAG}" "${image}:${IMAGE_TAG}"
  fi

  if docker image inspect "${image}:${IMAGE_TAG}" >/dev/null 2>&1; then
    continue
  fi

  if docker image inspect "${image}:latest" >/dev/null 2>&1; then
    docker tag "${image}:latest" "${image}:${IMAGE_TAG}"
  fi

  if ! docker image inspect "${image}:${IMAGE_TAG}" >/dev/null 2>&1; then
    die "Failed to build image ${image}:${IMAGE_TAG}"
  fi
done

if [ -n "${cleanup_dir}" ]; then
  rm -rf "${cleanup_dir}"
fi

if [ -n "${osxcross_cleanup_dir}" ]; then
  rm -rf "${osxcross_cleanup_dir}"
fi
