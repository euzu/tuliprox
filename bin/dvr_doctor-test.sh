#!/usr/bin/env bash
#
# Contract test for dvr_doctor.sh.
#
# The doctor talks to a running server, so `curl` is stubbed on PATH and the
# on-disk sections are pointed at a temporary tree. The test needs neither a
# server nor cargo.
#
# The route assertions are the point of this file. The doctor called
# `/api/v1/recording/quota` and read `video.download.recording` for a while
# after both had been removed, and nothing noticed -- the script still exited
# 0, it just reported "unavailable" forever.

set -uo pipefail

cd "$(dirname "$0")/.."
DOCTOR=bin/dvr_doctor.sh
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

failures=0
pass() { printf '  ok   %s\n' "$1"; }
fail() { printf '  FAIL %s\n' "$1"; failures=$((failures + 1)); }

# A `curl` that answers every request with one JSON blob and records the URL
# it was asked for, so the test can assert on the routes the doctor uses.
stub_curl() {
    mkdir -p "$tmp/bin"
    cat >"$tmp/bin/curl" <<STUB
#!/usr/bin/env bash
# Consume stdin so the doctor's header-on-stdin call does not block.
cat >/dev/null 2>&1 || true
for arg in "\$@"; do
  case "\$arg" in
    http*) printf '%s\n' "\$arg" >>"$tmp/urls.txt" ;;
  esac
done
cat <<'JSON'
{"enabled":true,"queue_revision":7,
 "recovery":{"state":"healthy","current_revision":7,"database_revision":7,
             "recovery_lag":0,"last_verified_checkpoint_revision":5,
             "journal_bytes":1024,"last_error":null,
             "storage_placement":"distinct_filesystem"}}
JSON
STUB
    chmod +x "$tmp/bin/curl"
}

run_doctor() { # args...
    : >"$tmp/urls.txt"
    PATH="$tmp/bin:$PATH" bash "$DOCTOR" --storage-dir "$tmp/data" --backup-dir "$tmp/backup" "$@" 2>&1
}

check_contains() { # name haystack needle
    local name=$1 haystack=$2 needle=$3
    if printf '%s' "$haystack" | grep -q -- "$needle"; then pass "$name"; else
        fail "$name (missing: $needle)"
    fi
}

check_absent() { # name haystack needle
    local name=$1 haystack=$2 needle=$3
    if printf '%s' "$haystack" | grep -q -- "$needle"; then
        fail "$name (found: $needle)"
    else pass "$name"; fi
}

printf 'dvr_doctor contract\n'

stub_curl
mkdir -p "$tmp/data" "$tmp/backup/recordings_recovery/gen-000001" "$tmp/backup/recordings_recovery/gen-000002"
printf 'x' >"$tmp/data/recordings.db"
printf 'gen-000002' >"$tmp/backup/recordings_recovery/CURRENT"
printf 'journal' >"$tmp/backup/recordings_recovery/gen-000002/journal.bin"

# --- shell syntax -----------------------------------------------------------
if bash -n "$DOCTOR"; then pass "script parses"; else fail "script parses"; fi

# --- help -------------------------------------------------------------------
help_out=$(bash "$DOCTOR" --help 2>&1); help_rc=$?
if [[ $help_rc -eq 0 ]]; then pass "--help exits 0"; else fail "--help exits 0 (got $help_rc)"; fi
check_contains "--help describes usage" "$help_out" "Usage: dvr_doctor.sh"

bad_out=$(bash "$DOCTOR" --nonsense 2>&1); bad_rc=$?
if [[ $bad_rc -ne 0 ]]; then pass "an unknown argument fails"; else fail "an unknown argument fails"; fi
check_contains "and says which one" "$bad_out" "unknown argument"

# --- without a token --------------------------------------------------------
# The on-disk sections must still work: an operator debugging a server that
# will not start has no token to give.
no_token=$(run_doctor)
check_contains "no token still reports on-disk state" "$no_token" "On-disk state"
check_contains "no token still reports recovery generations" "$no_token" "gen-000002"
check_contains "no token says why health was skipped" "$no_token" "needs an administrator token"

# --- with a token -----------------------------------------------------------
with_token=$(run_doctor --token "super-secret-token")
check_contains "a token reaches the health section" "$with_token" "Supervisor health"
check_absent "the token is never echoed" "$with_token" "super-secret-token"

# --- routes -----------------------------------------------------------------
# Static assertions against the script: a removed route still "works" at
# runtime because curl failures are swallowed, so only reading the source
# catches it.
source_text=$(cat "$DOCTOR")
check_absent "does not call the removed quota route" "$source_text" "recording/quota"
check_absent "does not call a removed download route" "$source_text" "file/download"
check_absent "does not read the old config path" "$source_text" "video.download.recording"
check_contains "reads the current config path" "$source_text" "video.recording"
check_contains "calls the health route" "$source_text" "/api/v1/recording/health"

# --- recovery health --------------------------------------------------------
# The contract from the recovery journal, surfaced for an operator.
check_contains "reports recovery health" "$with_token" "Recovery health"
for field in state current_revision database_revision recovery_lag \
             last_verified_checkpoint_revision journal_bytes storage_placement; do
    check_contains "recovery health carries $field" "$with_token" "$field"
done
check_contains "explains a repair_required state" "$with_token" "repair_required"
check_contains "explains recovery lag" "$with_token" "recovery_lag > 0"
check_contains "warns about a shared filesystem" "$with_token" "same_filesystem"

# --- same-filesystem warning ------------------------------------------------
# Database and recovery in one temp tree share a mount, which is the case the
# warning exists for.
check_contains "warns when history shares the database volume" "$no_token" \
    "shares a filesystem with the database"

# --- read-only --------------------------------------------------------------
# The doctor is a diagnostic. An operator runs it on a server that is already
# unwell, and it must not be capable of making things worse.
before=$(find "$tmp/data" "$tmp/backup" -type f -exec shasum {} \; | sort)
_=$(run_doctor --token "super-secret-token")
after=$(find "$tmp/data" "$tmp/backup" -type f -exec shasum {} \; | sort)
if [[ "$before" == "$after" ]]; then pass "leaves every file untouched"; else
    fail "leaves every file untouched"
fi
check_absent "never writes with curl" "$source_text" "curl -X"

# --- guidance ---------------------------------------------------------------
check_contains "tells an operator what to look at first" "$no_token" "What to look at first"
check_contains "covers the reference invariant" "$source_text" "recovery lag"

printf '\n'
if [[ $failures -eq 0 ]]; then
    printf 'all checks passed\n'
    exit 0
fi
printf '%d check(s) failed\n' "$failures"
exit 1
