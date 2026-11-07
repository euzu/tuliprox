#!/usr/bin/env bash
#
# The plugin SDK version: tuliprox-plugin-api, tuliprox-plugin-sdk and tuliprox-plugin-sdk-macros
# share one version (the plugin contract), independent of the tuliprox release version.
#
# Usage:
#   bin/plugin-sdk-version.sh <version>  set all three crates to <version> and refresh the Cargo.lock
#                                        of every plugin crate outside the workspace
#   bin/plugin-sdk-version.sh --check    exit 1 if the three versions differ or a plugin Cargo.lock
#                                        records another version
#
# The lock refresh runs online on purpose: an `--offline` resolve can downgrade registry crates.

set -euo pipefail

cd "$(dirname "$0")/.."

crates=(tuliprox-plugin-api tuliprox-plugin-sdk tuliprox-plugin-sdk-macros)
manifests=(plugins/plugin-api/Cargo.toml plugins/plugin-sdk/Cargo.toml plugins/plugin-sdk-macros/Cargo.toml)

usage() {
    echo "usage: bin/plugin-sdk-version.sh <version> | --check" >&2
    exit 2
}

[[ $# -eq 1 ]] || usage

package_version() { sed -n 's/^version = "\(.*\)"$/\1/p' "$1" | head -n1; }

# The version a lock file records for one crate, empty when the crate is not in it.
locked_version() { { grep -A1 -x "name = \"$2\"" "$1" || true; } | sed -n 's/^version = "\(.*\)"$/\1/p'; }

plugin_locks() { git ls-files -- ':(glob)plugins/**/Cargo.lock'; }

if [[ "$1" == "--check" ]]; then
    expected=$(package_version "${manifests[0]}")
    failed=0
    for manifest in "${manifests[@]}"; do
        actual=$(package_version "$manifest")
        if [[ "$actual" != "$expected" ]]; then
            echo "plugin-sdk-version: $manifest has $actual, ${manifests[0]} has $expected" >&2
            failed=1
        fi
    done
    while IFS= read -r lock; do
        for crate in "${crates[@]}"; do
            actual=$(locked_version "$lock" "$crate")
            if [[ -n "$actual" && "$actual" != "$expected" ]]; then
                echo "plugin-sdk-version: $lock locks $crate $actual, expected $expected (run make plugin-sdk-version VERSION=$expected)" >&2
                failed=1
            fi
        done
    done < <(plugin_locks)
    [[ $failed -eq 0 ]] && echo "plugin-sdk-version: all three crates and every plugin lock are at $expected"
    exit "$failed"
fi

version=$1
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || usage

specs=()
for crate in "${crates[@]}"; do
    specs+=(-p "$crate")
done
cargo set-version "${specs[@]}" "$version"

while IFS= read -r lock; do
    lock_specs=()
    for crate in "${crates[@]}"; do
        if grep -qx "name = \"$crate\"" "$lock"; then
            lock_specs+=(-p "$crate")
        fi
    done
    [[ ${#lock_specs[@]} -gt 0 ]] || continue
    (cd "$(dirname "$lock")" && cargo update --quiet "${lock_specs[@]}")
    echo "refreshed: $lock"
done < <(plugin_locks)
