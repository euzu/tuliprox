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
        "tuliprox -> tuliprox-messaging") ;;
        "tuliprox -> tuliprox-parser") ;;
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
