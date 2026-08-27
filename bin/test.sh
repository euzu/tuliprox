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
export RUST_BACKTRACE="${RUST_BACKTRACE:-1}"
# Default to `info` logging if the user hasn't set a level; can be overridden.
export RUST_LOG="${RUST_LOG:-info}"
exec cargo +stable test "$@"
