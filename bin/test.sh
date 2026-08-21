#!/usr/bin/env bash
# Wrapper around `cargo test` that routes every test's tempfile
# activity through the system temp directory and skips the
# `Config::update_runtime()`-induced `override_temp_dir` poison that
# would otherwise dump every parallel test's `.tmp*` into
# `backend/tmp/` (see backend/src/model/config/base.rs and the
# `cfg(not(test))` gate around `tempfile::env::override_temp_dir`).
#
# `bin/test.sh` is the only way to invoke `cargo test` from the
# command line; the IDE test runner should set `TMPDIR=/tmp` in its
# environment the same way.
set -euo pipefail
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd -P)"
cd "${REPO_ROOT}"
export TMPDIR="${TMPDIR:-/tmp}"
exec cargo test "$@"
