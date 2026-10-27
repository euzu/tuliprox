#!/usr/bin/env bash
#
# Locale key parity gate.
#
# Every locale JSON must carry exactly the key set of `en.json`: a missing key
# renders the raw key in the UI, an extra key is dead text nobody sees.
# Adding a UI string therefore means touching every locale, and a branch that
# was merged after the trunk grew a key set is exactly when this drifts.
#
# The Rust test `frontend::i18n::locale_file_tests` enforces the same rule and
# stays authoritative - it also rejects duplicate keys, which `jq` cannot see
# because it silently keeps the last one. This script exists because it needs
# no compile step and because it reports every locale at once: the test aborts
# on the first failing locale and hides the rest.
#
# Usage:
#   bin/check-i18n-keys.sh
#
# Exit codes: 0 all locales match, 1 drift found, 2 unusable environment.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
I18N_DIR="$(cd -- "${SCRIPT_DIR}/.." && pwd -P)/frontend/public/assets/i18n"
BASE_LOCALE="en"

if ! command -v jq >/dev/null 2>&1; then
    echo "check-i18n-keys: jq is required but was not found in PATH" >&2
    exit 2
fi

if [ ! -f "${I18N_DIR}/index.json" ]; then
    echo "check-i18n-keys: ${I18N_DIR}/index.json is missing" >&2
    exit 2
fi

# jq cannot report duplicate keys, so only distinct key paths are compared.
# Non-object values are leaves; arrays count as one leaf, matching the Rust test.
read -r -d '' FLATTEN <<'JQ' || true
def leaves(p):
  . as $v
  | if ($v | type) == "object"
    then ($v | to_entries[] | .key as $k | .value | leaves(if p == "" then $k else "\(p).\($k)" end))
    else p
    end;
leaves("")
JQ

failures=0
report() {
    printf '  %s %s\n' "FAIL" "$1"
    failures=$((failures + 1))
}

# "1 key" reads better than "1 keys" in a gate people run before every merge.
keys_word() { if [ "$1" -eq 1 ]; then printf 'key'; else printf 'keys'; fi; }
count_of() { grep -c . <<<"$1" || true; }

if [ ! -f "${I18N_DIR}/${BASE_LOCALE}.json" ]; then
    echo "check-i18n-keys: ${BASE_LOCALE}.json is missing" >&2
    exit 2
fi
base_keys="$(jq -r "${FLATTEN}" "${I18N_DIR}/${BASE_LOCALE}.json" | sort -u)"
base_count="$(printf '%s\n' "${base_keys}" | wc -l | tr -d ' ')"
echo "==> Checking ${base_count} keys of ${BASE_LOCALE}.json against every locale"

# The UI offers these locales; a file without an entry is unreachable, an entry
# without a file is a broken download.
offered="$(jq -r '.languages[].code' "${I18N_DIR}/index.json" | sort -u)"
# Portable on purpose: BSD find has no -printf, and the project also builds on macOS.
present="$(for file in "${I18N_DIR}"/*.json; do
    name="${file##*/}"
    if [ "${name}" != "index.json" ]; then
        printf '%s\n' "${name%.json}"
    fi
done | sort -u)"

missing_files="$(comm -23 <(printf '%s\n' "${offered}") <(printf '%s\n' "${present}"))"
unoffered="$(comm -13 <(printf '%s\n' "${offered}") <(printf '%s\n' "${present}"))"

if [ -n "${missing_files}" ]; then
    report "index.json offers locales with no file: $(paste -sd' ' - <<<"${missing_files}")"
fi
if [ -n "${unoffered}" ]; then
    report "locale files index.json does not offer: $(paste -sd' ' - <<<"${unoffered}")"
fi

while IFS= read -r code; do
    [ -n "${code}" ] || continue
    [ "${code}" = "${BASE_LOCALE}" ] && continue
    file="${I18N_DIR}/${code}.json"
    [ -f "${file}" ] || continue

    if ! locale_keys="$(jq -r "${FLATTEN}" "${file}" 2>&1 | sort -u)"; then
        report "${code}.json does not parse as JSON"
        continue
    fi

    missing="$(comm -23 <(printf '%s\n' "${base_keys}") <(printf '%s\n' "${locale_keys}"))"
    extra="$(comm -13 <(printf '%s\n' "${base_keys}") <(printf '%s\n' "${locale_keys}"))"

    if [ -n "${missing}" ]; then
        missing_count="$(count_of "${missing}")"
        report "${code}.json is missing ${missing_count} $(keys_word "${missing_count}") ${BASE_LOCALE}.json has:"
        sed -e 's/^/         /' <<<"${missing}"
    fi
    if [ -n "${extra}" ]; then
        extra_count="$(count_of "${extra}")"
        report "${code}.json has ${extra_count} $(keys_word "${extra_count}") ${BASE_LOCALE}.json does not:"
        sed -e 's/^/         /' <<<"${extra}"
    fi
done <<<"${offered}"

if [ "${failures}" -ne 0 ]; then
    echo "==> ${failures} locale problem(s) found" >&2
    exit 1
fi

echo "==> All locales carry exactly the ${BASE_LOCALE}.json keys"
