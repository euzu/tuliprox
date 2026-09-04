#!/usr/bin/env bash
#
# Architecture gate for workspace dependencies.
#
# Cargo already rejects dependency cycles, so this script does not look for them.
# It requires every edge between two workspace packages to be listed explicitly
# with its dependency kind, so
# that adding one - or promoting a dev-only edge to a build-time one - is a
# deliberate, reviewed act rather than a side effect of an `use` statement.
#
# The check runs in both directions:
#   * an edge in `cargo metadata` that is not listed here fails the gate;
#   * an edge listed here that no longer exists fails it too, so the list cannot
#     rot into a record of dependencies the workspace has since dropped.
#
# Extend the allowlist in the same change that introduces a package edge.
#
# Usage:
#   check-workspace-deps.sh
#   check-workspace-deps.sh --metadata FILE --allow FILE   # for the self-test

set -euo pipefail

metadata_file=""
allow_file=""
while [ $# -gt 0 ]; do
    case "$1" in
        --metadata) metadata_file="$2"; shift 2 ;;
        --allow)    allow_file="$2";    shift 2 ;;
        *) echo "check-workspace-deps: unknown argument: $1" >&2; exit 2 ;;
    esac
done

command -v jq >/dev/null 2>&1 || {
    echo "check-workspace-deps: jq is required" >&2
    exit 2
}

cd "$(dirname "$0")/.."

# Every allowed edge, as "<kind> <from> -> <to>". `kind` is one of cargo's own
# dependency kinds: normal, dev, build.
read -r -d '' allowlist <<'ALLOWLIST' || true
normal tuliprox -> shared
normal tuliprox -> tuliprox-media-server
normal tuliprox -> tuliprox-mpegts
normal tuliprox -> tuliprox-core
normal tuliprox -> tuliprox-library
normal tuliprox -> tuliprox-iptv
normal tuliprox-iptv -> shared
normal tuliprox-iptv -> tuliprox-core
normal tuliprox-iptv -> tuliprox-parser
normal tuliprox-iptv -> tuliprox-repository
normal tuliprox -> tuliprox-messaging
normal tuliprox -> tuliprox-repository
normal tuliprox -> tuliprox-auth
normal tuliprox -> tuliprox-session
normal tuliprox -> tuliprox-dvr
normal tuliprox -> tuliprox-hls
normal tuliprox -> tuliprox-processing
normal tuliprox -> tuliprox-metadata
# Background metadata resolution. Sits above the pipeline: it implements
# `MetadataUpdateSink`, which `processing` declares.
normal tuliprox-metadata -> shared
normal tuliprox-metadata -> tuliprox-core
normal tuliprox-metadata -> tuliprox-processing
normal tuliprox-metadata -> tuliprox-repository
normal tuliprox-metadata -> tuliprox-session
# The playlist pipeline. It states what it needs from the background
# metadata worker as a trait (`MetadataUpdateSink`) that the binary
# implements, so it does not depend on the worker itself.
normal tuliprox-processing -> shared
normal tuliprox-processing -> tuliprox-core
normal tuliprox-processing -> tuliprox-iptv
normal tuliprox-processing -> tuliprox-library
normal tuliprox-processing -> tuliprox-media-server
normal tuliprox-processing -> tuliprox-parser
normal tuliprox-processing -> tuliprox-repository
normal tuliprox-processing -> tuliprox-session
# The HLS proxy. Reads the running server through `HlsCtx`; needs
# `session` for provider allocation and connection admission, `mpegts`
# for transport-stream rendering and `parser` for origin manifests.
normal tuliprox-hls -> shared
normal tuliprox-hls -> tuliprox-core
normal tuliprox-hls -> tuliprox-mpegts
normal tuliprox-hls -> tuliprox-parser
normal tuliprox-hls -> tuliprox-session
# The DVR. Reads the running server through `RecordingCtx`. It publishes
# recording changes through `shared`'s `EventSink`, so it does not depend on
# the streaming-session runtime that happens to implement that trait.
normal tuliprox-dvr -> shared
normal tuliprox-dvr -> tuliprox-auth
normal tuliprox-dvr -> tuliprox-core
normal tuliprox-dvr -> tuliprox-messaging
normal tuliprox-dvr -> tuliprox-repository
# Provider allocation and the streaming-session runtime. Reaches
# `repository` to persist stream history and resolve GeoIP, and `mpegts`
# for the transport-stream buffer it hands to preempted clients.
normal tuliprox-session -> shared
normal tuliprox-session -> tuliprox-core
normal tuliprox-session -> tuliprox-mpegts
normal tuliprox-session -> tuliprox-repository
normal tuliprox -> tuliprox-config-loader
normal tuliprox-auth -> shared
normal tuliprox-auth -> tuliprox-core
normal tuliprox-auth -> tuliprox-repository
normal tuliprox-config-loader -> shared
normal tuliprox-config-loader -> tuliprox-core
normal tuliprox-config-loader -> tuliprox-repository
normal tuliprox-repository -> shared
normal tuliprox-repository -> tuliprox-btree
normal tuliprox-repository -> tuliprox-core
normal tuliprox-repository -> tuliprox-parser
normal tuliprox-parser -> shared
normal tuliprox-parser -> tuliprox-core
normal tuliprox-messaging -> shared
normal tuliprox-messaging -> tuliprox-core
normal tuliprox-library -> shared
normal tuliprox-library -> tuliprox-core
normal tuliprox-core -> shared
normal tuliprox-core -> tuliprox-mpegts
normal tuliprox-core -> tuliprox-media-server
normal tuliprox-media-server -> shared
normal frontend -> shared

# Dev-only edges. These exist so a crate's tests can drive a layer it does not
# depend on at build time - typically by turning on that crate's `test-support`
# feature. They are listed separately because a dev edge carries none of the
# architectural weight of a normal one: promoting any of these to a normal
# dependency has to fail this gate.
dev tuliprox -> tuliprox-btree
dev tuliprox -> tuliprox-hls
dev tuliprox -> tuliprox-mpegts
dev tuliprox -> tuliprox-processing
dev tuliprox -> tuliprox-repository
dev tuliprox-hls -> tuliprox-mpegts
dev tuliprox-repository -> tuliprox-btree
ALLOWLIST

if [ -n "$allow_file" ]; then
    allowlist=$(cat "$allow_file")
fi

if [ -n "$metadata_file" ]; then
    metadata=$(cat "$metadata_file")
else
    metadata=$(cargo metadata --format-version 1 --no-deps)
fi

# Edges between two workspace members, as "<kind> <from> -> <to>". Dependencies
# on registry crates are not this gate's business and are filtered out here.
actual=$(jq -r '
    [.workspace_members[]] as $ids
    | [.packages[] | select(.id as $id | $ids | index($id)) | .name] as $members
    | .packages[]
    | select(.id as $id | $ids | index($id))
    | .name as $from
    | .dependencies[]
    | select(.name as $to | $members | index($to))
    | "\(.kind // "normal") \($from) -> \(.name)"
' <<<"$metadata" | sort -u)

declared=$(grep -v -e '^[[:space:]]*#' -e '^[[:space:]]*$' <<<"$allowlist" | sed 's/[[:space:]]\{1,\}/ /g; s/^ //; s/ $//' | sort -u)

undeclared=$(comm -13 <(printf '%s\n' "$declared") <(printf '%s\n' "$actual"))
stale=$(comm -23 <(printf '%s\n' "$declared") <(printf '%s\n' "$actual"))

status=0

# An edge that exists under one kind and is allowlisted under another shows up in
# both lists. Report that as the promotion it is, not as two unrelated problems.
while read -r kind from arrow to; do
    [ -n "${to:-}" ] || continue
    # `|| true`: no match is the common case, and `set -e` would abort on it.
    other=$( { grep -E "^[a-z]+ $from $arrow $to\$" <<<"$stale" || true; } | awk '{print $1}' | paste -sd, -)
    if [ -n "$other" ]; then
        echo "check-workspace-deps: $from -> $to changed dependency kind: allowlisted as '$other', found as '$kind'" >&2
        echo "                      a dev-only edge may not become a build-time one without review;" >&2
        echo "                      fix backend/*/Cargo.toml, or update the allowlist in $0" >&2
    else
        echo "check-workspace-deps: undeclared workspace edge: $from -> $to ($kind dependency)" >&2
        echo "                      declared in ${from}'s Cargo.toml; add '$kind $from -> $to' to the" >&2
        echo "                      allowlist in $0 if the edge is intended" >&2
    fi
    status=1
done <<<"$undeclared"

while read -r kind from arrow to; do
    [ -n "${to:-}" ] || continue
    if grep -qE "^[a-z]+ $from $arrow $to\$" <<<"$undeclared"; then continue; fi
    echo "check-workspace-deps: stale allowlist entry: $kind $from -> $to" >&2
    echo "                      ${from} no longer depends on ${to}; remove the line from $0" >&2
    status=1
done <<<"$stale"

if [ "$status" -eq 0 ]; then
    echo "check-workspace-deps: all workspace edges are declared ($(wc -l <<<"$actual" | tr -d ' ') edges)"
fi
exit "$status"
