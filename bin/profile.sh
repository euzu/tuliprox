#!/bin/bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: bin/profile.sh [debug|release] [settings_dir] [output_dir]

Arguments:
  debug|release  Build/profile mode. Default: release
  settings_dir   Path passed to tuliprox via -H. Required.
  output_dir     Where to write the heaptrack artefact. Default: /tmp

Produces in output_dir:
  tuliprox-<mode>.heaptrack.zst      Heap allocations (only)

NOTE on CPU profiling: this script no longer wraps samply. Samply 0.13.1 in
this environment writes profile metadata but no `samples` payload, while
`perf record` captures ~19k samples on the same workload — so for CPU
profiles use `perf record -F 999 -o out.perf -- <cmd>` directly and convert
with `samply convert` if you want a Firefox Profiler view.

Environment:
  PROFILE_TARGET     Target name passed via `-t`. If unset, tuliprox runs in
                     server mode after the update and the script will hang
                     waiting for it to exit. REQUIRED for headless runs.
  RELEASE_DEBUG_INFO If 1, release builds keep debug info. Default: 1
  RELEASE_NO_LTO     If 1, disable LTO for release builds. Default: 1
                     (LTO inflates release build time ~10x without helping
                     the sample, and the slow link step eats into CI minutes)
EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
    usage
    exit 0
fi

MODE="${1:-release}"
SETTINGS_DIR="${2:-}"
OUTPUT_DIR="${3:-/tmp}"
RELEASE_DEBUG_INFO="${RELEASE_DEBUG_INFO:-1}"
RELEASE_NO_LTO="${RELEASE_NO_LTO:-1}"
PROFILE_TARGET="${PROFILE_TARGET:-}"

HEAP_BASE="$OUTPUT_DIR/tuliprox-${MODE}.heaptrack"
# heaptrack picks the compression based on the build; newer releases emit
# .zst, older ones fall back to .gz. Resolve the real path after the run.
HEAP_OUTPUT_ZST="${HEAP_BASE}.zst"
HEAP_OUTPUT_GZ="${HEAP_BASE}.gz"

case "$MODE" in
    debug)
        BUILD_ARGS=(cargo build --bin tuliprox)
        BIN_PATH="./target/debug/tuliprox"
        ;;
    release)
        # Debug-info without LTO or strip: the binary keeps symbols so heaptrack
        # addresses resolve against the source. LTO is off by default because it
        # 10x's the link step without helping the sample.
        ENV_VARS=(CARGO_PROFILE_RELEASE_STRIP=false)
        if [[ "$RELEASE_DEBUG_INFO" == "1" ]]; then
            ENV_VARS+=(CARGO_PROFILE_RELEASE_DEBUG=true)
        fi
        if [[ "$RELEASE_NO_LTO" == "1" ]]; then
            ENV_VARS+=(CARGO_PROFILE_RELEASE_LTO=false CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16)
        fi
        BUILD_ARGS=(env "${ENV_VARS[@]}" cargo build --release --bin tuliprox)
        BIN_PATH="./target/release/tuliprox"
        ;;
    *)
        echo "Unsupported mode: $MODE" >&2
        usage
        exit 1
        ;;
esac

if ! command -v heaptrack >/dev/null 2>&1; then
    echo "heaptrack is not installed" >&2
    exit 1
fi

if [[ -z "$SETTINGS_DIR" ]]; then
    echo "settings_dir is required" >&2
    usage
    exit 1
fi
if [[ ! -d "$SETTINGS_DIR" ]]; then
    echo "Settings directory not found: $SETTINGS_DIR" >&2
    usage
    exit 1
fi

if [[ -z "$PROFILE_TARGET" ]]; then
    echo "warning: PROFILE_TARGET is unset; tuliprox will stay in server mode after the update and this script will hang." >&2
    echo "         Set PROFILE_TARGET=<name> to force a one-shot update run." >&2
fi

echo "Building tuliprox ($MODE)..."
"${BUILD_ARGS[@]}"

mkdir -p "$OUTPUT_DIR"
echo "Recording heap profile..."
echo "  mode: $MODE"
echo "  binary: $BIN_PATH"
echo "  settings: $SETTINGS_DIR"
echo "  target: ${PROFILE_TARGET:-<none - will hang in server mode>}"
echo "  heaptrack: $HEAP_OUTPUT"

TARGET_ARGS=()
if [[ -n "$PROFILE_TARGET" ]]; then
    TARGET_ARGS=(-t "$PROFILE_TARGET")
fi

heaptrack -o "$HEAP_BASE" -- "$BIN_PATH" -H "$SETTINGS_DIR" "${TARGET_ARGS[@]}"
if [[ -f "$HEAP_OUTPUT_ZST" ]]; then
    HEAP_OUTPUT="$HEAP_OUTPUT_ZST"
elif [[ -f "$HEAP_OUTPUT_GZ" ]]; then
    HEAP_OUTPUT="$HEAP_OUTPUT_GZ"
else
    echo "heaptrack did not produce ${HEAP_OUTPUT_ZST} or ${HEAP_OUTPUT_GZ}" >&2
    exit 1
fi
echo "Profile written to $HEAP_OUTPUT"
