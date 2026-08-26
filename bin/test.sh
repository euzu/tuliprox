#!/usr/bin/env bash
# Wrapper around `cargo test` that routes every test's tempfile
# activity through the system temp directory and disables the process-global
# override installed by `Config::update_runtime()`.
#
# `bin/test.sh` is the only way to invoke `cargo test` from the
# command line; IDE test runners should set `TMPDIR=/tmp` and
# `TULIPROX_DISABLE_TEMP_DIR_OVERRIDE=1` in their environment.
set -euo pipefail
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd -P)"
cd "${REPO_ROOT}"
export TMPDIR="${TMPDIR:-/tmp}"
export TULIPROX_DISABLE_TEMP_DIR_OVERRIDE=1
exec cargo +stable test "$@"
