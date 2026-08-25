#!/usr/bin/env bash
#
# Architecture gate for the modularization plan.
#
# Cargo already rejects dependency cycles, so this script does not look for them.
# What it enforces is the stricter rule the plan asks for: every edge between two
# workspace packages must be listed here explicitly, so that adding one is a
# deliberate, reviewed act rather than a side effect of an `use` statement.
#
# Extend the allowlist below when a phase introduces a new package edge, in the
# same change that introduces it.

set -euo pipefail

command -v jq >/dev/null 2>&1 || {
    echo "check-workspace-deps: jq is required" >&2
    exit 2
}

cd "$(dirname "$0")/.."

metadata=$(cargo metadata --format-version 1 --no-deps)

# Names of the packages that live in this workspace, as opposed to registry
# dependencies. Only edges between two of these are checked.
members=$(jq -r '.workspace_members[]' <<<"$metadata")
member_names=$(jq -r --argjson m "$(jq -c '[.workspace_members[]]' <<<"$metadata")" '
    .packages[] | select(.id as $id | $m | index($id)) | .name
' <<<"$metadata" | sort -u)

is_member() {
    grep -qx -- "$1" <<<"$member_names"
}

status=0

while IFS=$'\t' read -r from to; do
    [ -n "$from" ] || continue
    is_member "$to" || continue

    case "$from -> $to" in
        "tuliprox -> shared") ;;
        "tuliprox -> tuliprox-btree") ;;
        "tuliprox -> tuliprox-media-server") ;;
        "tuliprox -> tuliprox-mpegts") ;;
        "tuliprox -> tuliprox-core") ;;
        "tuliprox -> tuliprox-library") ;;
        "tuliprox -> tuliprox-iptv") ;;
        "tuliprox-iptv -> shared") ;;
        "tuliprox-iptv -> tuliprox-core") ;;
        "tuliprox-iptv -> tuliprox-messaging") ;;
        "tuliprox-iptv -> tuliprox-parser") ;;
        "tuliprox-iptv -> tuliprox-repository") ;;
        "tuliprox -> tuliprox-messaging") ;;
        "tuliprox -> tuliprox-parser") ;;
        "tuliprox -> tuliprox-repository") ;;
        "tuliprox -> tuliprox-auth") ;;
        "tuliprox -> tuliprox-session") ;;
        "tuliprox -> tuliprox-dvr") ;;
        "tuliprox -> tuliprox-hls") ;;
        # The HLS proxy. Reads the running server through `HlsCtx`; needs
        # `session` for provider allocation and connection admission, `mpegts`
        # for transport-stream rendering and `parser` for origin manifests.
        "tuliprox-hls -> shared") ;;
        "tuliprox-hls -> tuliprox-core") ;;
        "tuliprox-hls -> tuliprox-mpegts") ;;
        "tuliprox-hls -> tuliprox-parser") ;;
        "tuliprox-hls -> tuliprox-session") ;;
        # The DVR. Reads the running server through `RecordingCtx`; needs
        # `session` for the event bus it publishes recording changes on.
        "tuliprox-dvr -> shared") ;;
        "tuliprox-dvr -> tuliprox-auth") ;;
        "tuliprox-dvr -> tuliprox-core") ;;
        "tuliprox-dvr -> tuliprox-messaging") ;;
        "tuliprox-dvr -> tuliprox-repository") ;;
        "tuliprox-dvr -> tuliprox-session") ;;
        # Provider allocation and the streaming-session runtime. Reaches
        # `repository` to persist stream history and resolve GeoIP, and `mpegts`
        # for the transport-stream buffer it hands to preempted clients.
        "tuliprox-session -> shared") ;;
        "tuliprox-session -> tuliprox-core") ;;
        "tuliprox-session -> tuliprox-mpegts") ;;
        "tuliprox-session -> tuliprox-repository") ;;
        "tuliprox -> tuliprox-config-loader") ;;
        "tuliprox-auth -> shared") ;;
        "tuliprox-auth -> tuliprox-core") ;;
        "tuliprox-auth -> tuliprox-repository") ;;
        "tuliprox-config-loader -> shared") ;;
        "tuliprox-config-loader -> tuliprox-core") ;;
        "tuliprox-config-loader -> tuliprox-repository") ;;
        "tuliprox-repository -> shared") ;;
        "tuliprox-repository -> tuliprox-btree") ;;
        "tuliprox-repository -> tuliprox-core") ;;
        "tuliprox-repository -> tuliprox-parser") ;;
        "tuliprox-parser -> shared") ;;
        "tuliprox-parser -> tuliprox-core") ;;
        "tuliprox-messaging -> shared") ;;
        "tuliprox-messaging -> tuliprox-core") ;;
        "tuliprox-library -> shared") ;;
        "tuliprox-library -> tuliprox-core") ;;
        "tuliprox-core -> shared") ;;
        "tuliprox-core -> tuliprox-mpegts") ;;
        "tuliprox-core -> tuliprox-media-server") ;;
        "tuliprox-media-server -> shared") ;;
        "frontend -> shared") ;;
        *)
            echo "check-workspace-deps: undeclared workspace edge: $from -> $to" >&2
            status=1
            ;;
    esac
done < <(jq -r --argjson m "$(jq -c '[.workspace_members[]]' <<<"$metadata")" '
    .packages[]
    | select(.id as $id | $m | index($id))
    | .name as $from
    | .dependencies[]
    | [$from, .name]
    | @tsv
' <<<"$metadata" | sort -u)

if [ "$status" -eq 0 ]; then
    echo "check-workspace-deps: all workspace edges are declared"
fi
exit "$status"
