#!/bin/bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: bin/profile.sh [debug|release] [settings_dir] [output_file]

Arguments:
  debug|release  Build/profile mode. Default: release
  settings_dir   Path passed to tuliprox via -H. Required.
  output_file    Samply output path. Default: /tmp/tuliprox-<mode>.samply.json.gz

Environment:
  PROFILE_DURATION_SECONDS   Samply record duration in seconds. Default: 30
  RELEASE_DEBUG_INFO         If set to 1, release builds keep debug info. Default: 1
EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
    usage
    exit 0
fi

MODE="${1:-release}"
SETTINGS_DIR="${2:-}"
OUTPUT_FILE="${3:-/tmp/tuliprox-${MODE}.samply.json.gz}"
PROFILE_DURATION_SECONDS="${PROFILE_DURATION_SECONDS:-30}"
RELEASE_DEBUG_INFO="${RELEASE_DEBUG_INFO:-1}"

case "$MODE" in
    debug)
        BUILD_ARGS=(cargo build --bin tuliprox)
        BIN_PATH="./target/debug/tuliprox"
        ;;
    release)
        if [[ "$RELEASE_DEBUG_INFO" == "1" ]]; then
            BUILD_ARGS=(env CARGO_PROFILE_RELEASE_DEBUG=true cargo build --release --bin tuliprox)
        else
            BUILD_ARGS=(cargo build --release --bin tuliprox)
        fi
        BIN_PATH="./target/release/tuliprox"
        ;;
    *)
        echo "Unsupported mode: $MODE" >&2
        usage
        exit 1
        ;;
esac

if ! command -v samply >/dev/null 2>&1; then
    echo "samply is not installed" >&2
    exit 1
fi

if [[ -z "$SETTINGS_DIR" ]]; then
    echo "settings_dir is required" >&2
    usage
    exit 1
fi

if [[ ! -d "$SETTINGS_DIR" ]]; then
    echo "Settings directory not found: $SETTINGS_DIR" >&2
    exit 1
fi

echo "Building tuliprox ($MODE)..."
"${BUILD_ARGS[@]}"

echo "Recording profile..."
echo "  mode: $MODE"
echo "  binary: $BIN_PATH"
echo "  settings: $SETTINGS_DIR"
echo "  output: $OUTPUT_FILE"
echo "  duration: ${PROFILE_DURATION_SECONDS}s"

mkdir -p "$(dirname "$OUTPUT_FILE")"

samply record \
    --save-only \
    --unstable-presymbolicate \
    --duration "$PROFILE_DURATION_SECONDS" \
    --output "$OUTPUT_FILE" \
    "$BIN_PATH" \
    -s \
    -H "$SETTINGS_DIR"

echo "Profile written to $OUTPUT_FILE"
