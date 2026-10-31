#!/usr/bin/env bash
#
# Locale drift gate for the Web UI translations.
#
# Every locale offered in `index.json` must carry exactly the keys of `en.json`.
# Unlike the Rust test `frontend::i18n::locale_file_tests`, which stops at the
# first drifting locale, this script reports every missing and extra key of
# every locale at once. It cannot see duplicate keys (jq keeps the last one);
# the Rust test covers those.

set -euo pipefail

cd "$(dirname "$0")/.."

command -v jq >/dev/null 2>&1 || { echo "check-i18n: jq is required (https://jqlang.org)" >&2; exit 1; }

i18n_dir=frontend/public/assets/i18n
# tr drops the CRLF that jq writes on Windows.
leaf_keys() { jq -r 'paths(scalars) | map(tostring) | join(".")' "$1" | tr -d '\r' | sort -u; }

reference=$(leaf_keys "$i18n_dir/en.json")
locales=$(jq -r '.languages[].code | select(. != "en")' "$i18n_dir/index.json" | tr -d '\r')

status=0
for locale in $locales; do
    file="$i18n_dir/$locale.json"
    if [ ! -f "$file" ]; then
        echo "check-i18n: $file is offered in index.json but missing" >&2
        status=1
        continue
    fi
    keys=$(leaf_keys "$file")
    # grep . keeps an empty locale from adding a blank key line.
    missing=$(comm -23 <(echo "$reference") <(grep . <<<"$keys" || true))
    extra=$(comm -13 <(echo "$reference") <(grep . <<<"$keys" || true))
    if [ -n "$missing" ]; then
        sed "s/^/check-i18n: $locale is missing /" <<<"$missing" >&2
        status=1
    fi
    if [ -n "$extra" ]; then
        sed "s/^/check-i18n: $locale has extra key /" <<<"$extra" >&2
        status=1
    fi
done

if [ "$status" -eq 0 ]; then
    echo "check-i18n: all locales match en.json ($(wc -l <<<"$reference" | tr -d ' ') keys)"
fi
exit "$status"
