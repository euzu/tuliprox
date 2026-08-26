#!/usr/bin/env bash
#
# Regression test for check-workspace-deps.sh.
#
# Every case below is a failure the gate did not catch before it learned about
# dependency kinds and stale entries. The fixtures are hand-written `cargo
# metadata` documents, so the test needs neither cargo nor a real workspace.

set -uo pipefail

cd "$(dirname "$0")/.."
GATE=bin/check-workspace-deps.sh
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

failures=0
pass() { printf '  ok   %s\n' "$1"; }
fail() { printf '  FAIL %s\n' "$1"; failures=$((failures + 1)); }

# One workspace: `app` (normal -> lib, dev -> testkit), `lib` (normal -> shared).
# `serde` is a registry crate and must be ignored.
metadata() {
    cat >"$tmp/metadata.json" <<'JSON'
{
  "workspace_members": ["app 1.0.0 (path+file:///w/app)", "lib 1.0.0 (path+file:///w/lib)",
                        "shared 1.0.0 (path+file:///w/shared)", "testkit 1.0.0 (path+file:///w/testkit)"],
  "packages": [
    {"id": "app 1.0.0 (path+file:///w/app)", "name": "app", "dependencies": [
      {"name": "lib", "kind": null}, {"name": "testkit", "kind": "dev"}, {"name": "serde", "kind": null}]},
    {"id": "lib 1.0.0 (path+file:///w/lib)", "name": "lib", "dependencies": [
      {"name": "shared", "kind": null}]},
    {"id": "shared 1.0.0 (path+file:///w/shared)", "name": "shared", "dependencies": []},
    {"id": "testkit 1.0.0 (path+file:///w/testkit)", "name": "testkit", "dependencies": []}
  ]
}
JSON
}

allow() { printf '%s\n' "$@" >"$tmp/allow.txt"; }

run() { bash "$GATE" --metadata "$tmp/metadata.json" --allow "$tmp/allow.txt" 2>&1; }

check() { # name expected_exit expected_substring
    local name=$1 want=$2 needle=$3 out rc
    out=$(run); rc=$?
    if [ "$rc" != "$want" ]; then
        fail "$name (exit $rc, wanted $want)"; printf '       %s\n' "$out"; return
    fi
    if [ -n "$needle" ] && ! grep -qF -- "$needle" <<<"$out"; then
        fail "$name (no '$needle' in output)"; printf '       %s\n' "$out"; return
    fi
    pass "$name"
}

metadata

# 1. A complete allowlist passes, and the registry dependency is not reported.
allow '# a comment' \
      'normal app -> lib' \
      'dev app -> testkit' \
      'normal lib -> shared'
check "complete allowlist passes" 0 "all workspace edges are declared"
if run | grep -q serde; then fail "registry dependency ignored"; else pass "registry dependency ignored"; fi

# 2. An edge the allowlist does not mention fails, and the message names both crates.
allow 'normal app -> lib' 'dev app -> testkit'
check "undeclared edge is caught" 1 "undeclared workspace edge: lib -> shared"

# 3. An allowlist entry with no matching edge fails. This is the case the old
#    `case` statement could not see at all.
allow 'normal app -> lib' 'dev app -> testkit' 'normal lib -> shared' 'normal app -> shared'
check "stale allowlist entry is caught" 1 "stale allowlist entry: normal app -> shared"

# 4. A dev-only edge promoted to a normal dependency fails, and is reported as a
#    kind change rather than as an unrelated add plus remove.
allow 'normal app -> lib' 'normal app -> testkit' 'normal lib -> shared'
check "dev edge allowlisted as normal is caught" 1 "changed dependency kind"

# 5. ... and the reverse: a normal dependency allowlisted as dev-only.
allow 'dev app -> lib' 'dev app -> testkit' 'normal lib -> shared'
check "normal edge allowlisted as dev is caught" 1 "allowlisted as 'dev', found as 'normal'"

# 6. The real workspace passes its own allowlist.
if command -v cargo >/dev/null 2>&1; then
    if bash "$GATE" >/dev/null 2>&1; then pass "real workspace passes"; else fail "real workspace passes"; fi
else
    printf '  skip real workspace (cargo not on PATH)\n'
fi

if [ "$failures" -ne 0 ]; then
    echo "check-workspace-deps-test: $failures failure(s)" >&2
    exit 1
fi
echo "check-workspace-deps-test: all cases pass"
