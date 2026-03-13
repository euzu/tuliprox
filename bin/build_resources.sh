#!/usr/bin/env bash
set -euo pipefail

# Function to print usage instructions
print_usage() {
    echo "Usage: $(basename "$0") [-f] [-h]"
    echo
    echo "Options:"
    echo "  -f    Force resource creation"
    echo "  -h    Display this help message"
    exit 0
}

flag_force=false

# parse options
while getopts "fh" opt; do
  case $opt in
    f) flag_force=true ;;
    h) print_usage ;;
    \?) echo "Unknown option: -$OPTARG" >&2 ;;
  esac
done

if ! command -v ffmpeg > /dev/null 2>&1; then
  echo "ffmpeg not found" >&2
  exit 1
fi

mapfile -d '' -t resources < <(find ./resources -maxdepth 1 -type f -name '*.jpg' -print0 | sort -z)

if [ "${#resources[@]}" -eq 0 ]; then
  echo "No .jpg resources found in ./resources"
  exit 0
fi

for image in "${resources[@]}"; do
  output="${image%.jpg}.ts"
  resource_name="$(basename "${image%.jpg}")"

  if [ "$flag_force" = false ] && [ -e "${output}" ]; then
    echo "Resource ${resource_name} exists, skipping creation"
    continue
  fi

  if ! ffmpeg -y -nostdin -loop 1 -framerate 30 -i "${image}" \
    -f lavfi -i anullsrc=channel_layout=stereo:sample_rate=48000 \
    -t 10 -shortest \
    -c:v libx264 -pix_fmt yuv420p -preset veryfast -crf 23 \
    -x264-params "keyint=30:min-keyint=30:scenecut=0:bframes=0:open_gop=0" \
    -c:a aac -b:a 128k -ac 2 -ar 48000 \
    -mpegts_flags +resend_headers \
    -muxdelay 0 -muxpreload 0 \
    -f mpegts "${output}"; then
    echo "ffmpeg failed for resource ${resource_name}" >&2
    exit 1
  fi
done
