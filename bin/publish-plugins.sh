#!/usr/bin/env bash
#
# Publishes the official plugins as GitHub Release assets and keeps the marketplace catalog current.
#
# Usage:
#   bin/publish-plugins.sh publish   # package and release every plugin version that has no release yet
#   bin/publish-plugins.sh catalog   # rebuild marketplace.json from all plugin releases and upload it
#   bin/publish-plugins.sh all       # publish, then catalog
#
# Releases:
#   plugin-<id>-v<version>   one per plugin version, never overwritten: <id>-<version>.tlpx and
#                            <id>-<version>.release.json (the catalog entry of exactly this version)
#   plugin-marketplace       fixed release whose marketplace.json asset is the catalog
#
# Plugin metadata comes from plugins/inputs/<id>/manifest.yml. Older versions keep their own entry, so an older Tuliprox
# still finds the plugin versions it is compatible with.
#
# Needs: gh (authenticated, GH_TOKEN), yq (mikefarah v4), jq, and what bin/package-plugin.sh needs.
# Environment: GITHUB_REPOSITORY (owner/repo), optional GITHUB_SHA (release target commit).

set -euo pipefail

cd "$(dirname "$0")/.."

PLUGIN_GLOB="plugins/inputs/*/manifest.yml"
CATALOG_TAG="plugin-marketplace"
DIST_DIR="target/plugin-release"
REPO="${GITHUB_REPOSITORY:?GITHUB_REPOSITORY (owner/repo) is required}"

die() {
    echo "publish-plugins: $*" >&2
    exit 1
}

require() {
    local tool
    for tool in "$@"; do
        command -v "$tool" >/dev/null 2>&1 || die "$tool is required"
    done
}
require yq jq

release_exists() { gh release view "$1" --repo "$REPO" >/dev/null 2>&1; }

asset_url() { echo "https://github.com/${REPO}/releases/download/$1/$2"; }

# The catalog entry of one plugin version: plugin metadata plus the release, from the manifest of that version.
release_json() {
    local manifest="$1" tag="$2" package="$3"
    yq -o=json '.' "$manifest" | jq \
        --arg url "$(asset_url "$tag" "$package")" \
        --arg published "$(date -u +%Y-%m-%d)" \
        --arg homepage "https://github.com/${REPO}/tree/develop/$(dirname "$manifest")" '
        {
          id, name,
          description: ((.description // "") | gsub("\\s+$"; "")),
          author, category,
          homepage: (.homepage // $homepage),
          release: ({
            version,
            package_url: $url,
            min_tuliprox_version,
            plugin_api: .api_version,
            capabilities: ((.capabilities // {}) | to_entries | map(select(.value == true) | .key)),
            published: $published
          } | with_entries(select(.value != null)))
        } | with_entries(select(.value != null))'
}

publish() {
    mkdir -p "$DIST_DIR"
    local manifest id version tag package dir
    for manifest in $PLUGIN_GLOB; do
        dir="$(dirname "$manifest")"
        id="$(yq '.id' "$manifest")"
        version="$(yq '.version' "$manifest")"
        [ -n "$id" ] && [ "$id" != "null" ] || die "$manifest has no id"
        [ -n "$version" ] && [ "$version" != "null" ] || die "$manifest has no version"
        tag="plugin-${id}-v${version}"
        if release_exists "$tag"; then
            # A published version is immutable: the catalog has no checksum, so new bytes under the same URL would go
            # unnoticed. A change needs a version bump.
            if git rev-parse -q --verify "refs/tags/${tag}" >/dev/null && ! git diff --quiet "refs/tags/${tag}" -- "$dir"; then
                echo "::warning::${id} changed since ${tag}; bump the version in ${manifest} to publish it"
            fi
            echo "${tag} exists, skipped"
            continue
        fi
        package="${id}-${version}.tlpx"
        echo "==> Packaging ${id} ${version}"
        ./bin/package-plugin.sh "$dir" --out "${DIST_DIR}/${package}"
        release_json "$manifest" "$tag" "$package" >"${DIST_DIR}/${id}-${version}.release.json"
        gh release create "$tag" "${DIST_DIR}/${package}" "${DIST_DIR}/${id}-${version}.release.json" \
            --repo "$REPO" --target "${GITHUB_SHA:-HEAD}" --latest=false \
            --title "Plugin ${id} ${version}" \
            --notes "Plugin package ${package}. Install it from the Tuliprox marketplace or upload the .tlpx."
        echo "✅ Released ${tag}"
    done
}

# Builds marketplace.json from release.json files in $1 (one per plugin version).
build_catalog() {
    local source_dir="$1"
    jq -s '
        def semver: split("-") as $p
            | ($p[0] | split(".") | map(tonumber? // 0)) + [if ($p | length) > 1 then 0 else 1 end];
        {
          format_version: 1,
          name: "Tuliprox plugins",
          plugins: (
            group_by(.id)
            | map(sort_by(.release.version | semver) | reverse
                  | (.[0] | del(.release)) + { releases: map(.release) })
            | sort_by(.id)
          )
        }' "$source_dir"/*.release.json
}

catalog() {
    local work tags tag
    work="$(mktemp -d)"
    trap 'rm -rf "$work"' RETURN
    tags="$(gh release list --repo "$REPO" --limit 1000 --json tagName --jq '.[].tagName' | grep -E '^plugin-.+-v[0-9]' || true)"
    [ -n "$tags" ] || die "no plugin releases found"
    for tag in $tags; do
        gh release download "$tag" --repo "$REPO" --pattern '*.release.json' --dir "$work" --clobber
    done
    build_catalog "$work" >"$work/marketplace.json"
    jq -e '.plugins | length > 0' "$work/marketplace.json" >/dev/null || die "the catalog is empty"
    if ! release_exists "$CATALOG_TAG"; then
        gh release create "$CATALOG_TAG" --repo "$REPO" --target "${GITHUB_SHA:-HEAD}" --latest=false \
            --title "Plugin marketplace" \
            --notes "The Tuliprox plugin catalog. Marketplace source: $(asset_url "$CATALOG_TAG" marketplace.json)"
    fi
    gh release upload "$CATALOG_TAG" "$work/marketplace.json" --repo "$REPO" --clobber
    echo "✅ Catalog updated: $(asset_url "$CATALOG_TAG" marketplace.json)"
}

case "${1:-}" in
    publish) require gh && publish ;;
    catalog) require gh && catalog ;;
    all) require gh && publish && catalog ;;
    build-catalog) build_catalog "${2:?directory with *.release.json}" ;;
    release-json) release_json "${2:?manifest}" "${3:?tag}" "${4:?package}" ;;
    *) die "usage: bin/publish-plugins.sh publish|catalog|all" ;;
esac
