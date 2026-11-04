#!/usr/bin/env bash
#
# Refreshes the Cargo.lock of every plugin crate outside the workspace (plugins/examples,
# plugins/inputs, plugins/notifiers, plugins/plugin-api/fixtures) after the workspace version
# changed.
#
# Those crates depend on tuliprox-plugin-api, tuliprox-plugin-sdk and tuliprox-plugin-sdk-macros
# by path, and their lock files record each path crate's version. A workspace version bump
# (`cargo set-version`, run by .github/workflows/docker-build.yml) changes that version, so every
# `cargo build --locked` of such a crate (make plugin-fixtures, make plugins) fails until its lock
# is refreshed.
#
# Only the entries of those workspace crates are updated (`cargo update -p <crate>`); registry
# dependencies stay as locked. It runs online on purpose: an `--offline` resolve can downgrade them.
#
# Usage:
#   bin/refresh-plugin-locks.sh          refresh every stale lock and name it
#   bin/refresh-plugin-locks.sh --check  only name the stale locks; exit 1 if there is one

set -euo pipefail

usage() {
    echo "usage: bin/refresh-plugin-locks.sh [--check]" >&2
    exit 2
}

check=false
case "${1:-}" in
    --check) check=true ;;
    "") ;;
    *) usage ;;
esac
[[ $# -le 1 ]] || usage

cd "$(dirname "$0")/.."

workspace_crates=(tuliprox-plugin-api tuliprox-plugin-sdk tuliprox-plugin-sdk-macros)
stale=0

while IFS= read -r lock; do
    dir=$(dirname "$lock")
    specs=()
    for crate in "${workspace_crates[@]}"; do
        if grep -qx "name = \"$crate\"" "$lock"; then
            specs+=(-p "$crate")
        fi
    done
    # A crate that locks none of the workspace crates is not affected by a version bump.
    [[ ${#specs[@]} -gt 0 ]] || continue
    if (cd "$dir" && cargo metadata --locked --format-version 1 >/dev/null 2>&1); then
        continue
    fi
    if $check; then
        echo "stale: $lock"
        stale=1
        continue
    fi
    (cd "$dir" && cargo update --quiet "${specs[@]}")
    echo "refreshed: $lock"
done < <(git ls-files -- ':(glob)plugins/**/Cargo.lock')

exit "$stale"
