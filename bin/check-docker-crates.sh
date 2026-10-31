#!/usr/bin/env bash
#
# Docker skeleton gate for workspace crates.
#
# The Dockerfiles build a dependency-only workspace skeleton: they copy every
# backend manifest and create a placeholder `lib.rs` per crate before running
# `cargo metadata`. A crate missing from either list breaks the image build,
# and nothing on the cargo side notices. This script requires every directory
# under `backend/` (except `app`, which the Dockerfiles handle separately) to
# appear in both the COPY lines and the `for crate in` loop of each Dockerfile.

set -euo pipefail

cd "$(dirname "$0")/.."

dockerfiles=(docker/Dockerfile docker/cross.Dockerfile)

crates=$(printf '%s\n' backend/*/Cargo.toml | sed 's#^backend/\(.*\)/Cargo.toml$#\1#' | grep -vx app | sort)

status=0
for dockerfile in "${dockerfiles[@]}"; do
    copied=$(sed -n 's#^COPY backend/\([a-z0-9-]*\)/Cargo.toml .*#\1#p' "$dockerfile" | grep -vx app | sort -u)
    looped=$(sed -n 's#.*for crate in \(.*\); do.*#\1#p' "$dockerfile" | tr ' ' '\n' | sed '/^$/d' | sort -u)
    for crate in $crates; do
        if ! grep -qx "$crate" <<<"$copied"; then
            echo "check-docker-crates: $dockerfile does not copy backend/$crate/Cargo.toml" >&2
            status=1
        fi
        if ! grep -qx "$crate" <<<"$looped"; then
            echo "check-docker-crates: $dockerfile skeleton loop does not create backend/$crate/src/lib.rs" >&2
            status=1
        fi
    done
    for crate in $copied $looped; do
        if ! grep -qx "$crate" <<<"$crates"; then
            echo "check-docker-crates: $dockerfile lists backend/$crate, which is not a workspace crate" >&2
            status=1
        fi
    done
done

if [ "$status" -eq 0 ]; then
    echo "check-docker-crates: both Dockerfiles list all $(wc -l <<<"$crates" | tr -d ' ') backend crates"
fi
exit "$status"
