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
  else
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
  fi

  if [ "${resource_name}" = "panel_api_provisioning" ]; then
    hls_playlist="${image%/*}/panel_api_provisioning_hls.m3u8"
    hls_segment_pattern="${image%/*}/panel_api_provisioning_hls_%03d.ts"
    hls_segments_missing=false
    for index in 0 1 2 3 4 5; do
      if [ ! -e "$(printf "%s/panel_api_provisioning_hls_%03d.ts" "${image%/*}" "${index}")" ]; then
        hls_segments_missing=true
        break
      fi
    done
    if [ "$flag_force" = true ] || [ "$hls_segments_missing" = true ]; then
      rm -f "${image%/*}"/panel_api_provisioning_hls_*.ts "${hls_playlist}"
      if ! ffmpeg -y -nostdin -loop 1 -framerate 30 -i "${image}" \
        -f lavfi -i anullsrc=channel_layout=stereo:sample_rate=48000 \
        -t 12 -shortest \
        -c:v libx264 -pix_fmt yuv420p -preset veryfast -crf 23 \
        -g 60 -keyint_min 60 \
        -force_key_frames "expr:gte(t,n_forced*2)" \
        -x264-params "scenecut=0:bframes=0:open_gop=0" \
        -c:a aac -b:a 128k -ac 2 -ar 48000 \
        -mpegts_flags +resend_headers \
        -muxdelay 0 -muxpreload 0 \
        -f hls \
        -hls_time 2 \
        -hls_list_size 0 \
        -hls_segment_type mpegts \
        -hls_segment_filename "${hls_segment_pattern}" \
        "${hls_playlist}"; then
        echo "ffmpeg failed for HLS provisioning resource ${resource_name}" >&2
        exit 1
      fi
      rm -f "${hls_playlist}"
    fi
  fi
done
