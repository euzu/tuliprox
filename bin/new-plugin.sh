#!/usr/bin/env bash
#
# Renders the plugin template (plugins/examples/template) into a new directory,
# without cargo-generate. The output matches `cargo generate` with the same name.
#
# Usage:
#   bin/new-plugin.sh <id> <dest-dir>
#
# <id> is the plugin id: ^[a-z][a-z0-9-]{2,40}$. It replaces {{project-name}}
# (manifest id, package name). {{sdk_path}} becomes the absolute path of this
# checkout's plugins/plugin-sdk/, so the rendered crate depends on the SDK in place.
# <dest-dir> must not exist, or must be an empty directory.

set -euo pipefail

usage() {
    echo "usage: bin/new-plugin.sh <id> <dest-dir>" >&2
    exit 2
}

die() {
    echo "new-plugin: $*" >&2
    exit 1
}

[[ $# -eq 2 ]] || usage
id="$1"
dest="$2"

id_pattern='^[a-z][a-z0-9-]{2,40}$'
if [[ ! "$id" =~ $id_pattern ]]; then
    die "invalid id '$id': use 3 to 41 lowercase letters, digits or dashes, starting with a letter"
fi

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
template="$root/plugins/examples/template"
[[ -d "$template" ]] || die "template not found at $template"
sdk="$root/plugins/plugin-sdk"
[[ -f "$sdk/Cargo.toml" ]] || die "the tuliprox plugin SDK is not at $sdk"

# Escapes a value for use as the replacement text of `s|...|...|g`: backslash, the
# delimiter and & are special there.
escape_replacement() {
    printf '%s' "$1" | sed -e 's/[\\|&]/\\&/g'
}
sdk_value="$(escape_replacement "$sdk")"

if [[ -e "$dest" ]]; then
    if [[ ! -d "$dest" ]] || [[ -n "$(ls -A "$dest")" ]]; then
        die "$dest exists and is not an empty directory"
    fi
fi

mkdir -p "$dest"

# Every template file except cargo-generate.toml, which is template config only.
# Names contain no newlines, so a newline-separated list is safe.
files="$(cd "$template" && find . -type f ! -name cargo-generate.toml | LC_ALL=C sort)"

while IFS= read -r rel; do
    rel="${rel#./}"
    mkdir -p "$dest/$(dirname "$rel")"
    sed -e "s|{{project-name}}|$id|g" -e "s|{{sdk_path}}|$sdk_value|g" "$template/$rel" > "$dest/$rel"
done <<< "$files"

if grep -rn '{{' "$dest" >/dev/null 2>&1; then
    grep -rn '{{' "$dest" >&2 || true
    die "unrendered placeholders are left in $dest"
fi

echo "new-plugin: rendered $template into $dest (id $id, sdk $sdk)"
echo "next: bin/package-plugin.sh $dest"
