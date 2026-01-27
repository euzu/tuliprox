#!/usr/bin/env bash

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

declare -a resources=("channel_unavailable" "user_connections_exhausted" "provider_connections_exhausted" "user_account_expired" "panel_api_provisioning")

for resource in "${resources[@]}"; do
  if [ "$flag_force" = false ]; then
    if [ -e "./resources/${resource}.ts" ]; then
      echo "Resource ${resource} exists, skipping creation"
      continue
    fi
  fi

  if which ffmpeg > /dev/null 2>&1; then
        ffmpeg -y -nostdin -loop 1 -i "./resources/${resource}.jpg" -f lavfi -i anullsrc=channel_layout=stereo:sample_rate=44100 \
           -c:v libx264 -r 30 -g 30 -keyint_min 30 -sc_threshold 0 -pix_fmt yuv420p -preset veryfast -crf 23 \
           -c:a aac -b:a 128k -ac 2 \
           -t 10 -muxrate 2000k \
           -f mpegts "./resources/${resource}.ts"
  else
    echo "ffmpeg not found"
    exit
  fi
done
