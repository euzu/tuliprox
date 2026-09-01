#!/usr/bin/env bash
# DVR support diagnostics.
#
# Collects the DVR's runtime state into one readable dump for a support
# ticket: supervisor liveness, the effective configuration, and the
# on-disk artefacts the recording feature owns. Read-only — it never
# mutates config, the queue, or a recording.
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
WORKING_DIR="$(cd -- "${SCRIPT_DIR}/.." && pwd -P)"

die() {
  echo "🧨 Error: $*" >&2
  exit 1
}

usage() {
  cat <<'USAGE'
Usage: dvr_doctor.sh [--url URL] [--token TOKEN] [--storage-dir DIR]

  --url          Base URL of the running server. Default: http://localhost:8901
  --token        Bearer token for an administrator. Without it the health
                 section is skipped; the on-disk sections still work.
  --storage-dir  Server storage directory, for the on-disk sections.
                 Default: $TULIPROX_HOME/data, else ./data
  --backup-dir   Directory holding recovery generations (config `backup_dir`).
                 Default: $TULIPROX_HOME/backup, else ./backup

Environment: TULIPROX_URL, TULIPROX_TOKEN, TULIPROX_HOME.
USAGE
}

URL="${TULIPROX_URL:-http://localhost:8901}"
TOKEN="${TULIPROX_TOKEN:-}"
STORAGE_DIR="${TULIPROX_HOME:+${TULIPROX_HOME}/data}"
BACKUP_DIR="${TULIPROX_HOME:+${TULIPROX_HOME}/backup}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --url) URL="${2:?--url needs a value}"; shift 2 ;;
    --token) TOKEN="${2:?--token needs a value}"; shift 2 ;;
    --storage-dir) STORAGE_DIR="${2:?--storage-dir needs a value}"; shift 2 ;;
    --backup-dir) BACKUP_DIR="${2:?--backup-dir needs a value}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown argument: $1 (try --help)" ;;
  esac
done

STORAGE_DIR="${STORAGE_DIR:-${WORKING_DIR}/data}"
BACKUP_DIR="${BACKUP_DIR:-${WORKING_DIR}/backup}"

command -v curl >/dev/null 2>&1 || die "curl is required"
# jq is optional: without it the JSON is emitted raw rather than pretty.
if command -v jq >/dev/null 2>&1; then
  pretty() { jq . 2>/dev/null || cat; }
else
  pretty() { cat; }
fi

section() {
  printf '\n=== %s ===\n' "$1"
}

fetch() {
  # $1 = path. Prints the body; returns non-zero on a transport failure.
  local path="$1"
  if [[ -n "${TOKEN}" ]]; then
    # Token goes through stdin (curl `-H @-`) so it does not appear in
    # the process listing (`ps`, `/proc/<pid>/cmdline`).
    curl -fsS -H @- "${URL}${path}" <<<"Authorization: Bearer ${TOKEN}"
  else
    curl -fsS "${URL}${path}"
  fi
}

printf 'Tuliprox DVR diagnostics\n'
printf 'generated: %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
printf 'url:         %s\n' "${URL}"
printf 'storage dir: %s\n' "${STORAGE_DIR}"
printf 'backup dir:  %s\n' "${BACKUP_DIR}"
printf 'token:       %s\n' "$([[ -n "${TOKEN}" ]] && echo provided || echo '(none — health section skipped)')"

section "Supervisor health"
if [[ -z "${TOKEN}" ]]; then
  echo "skipped: needs an administrator token (--token)"
else
  if ! fetch /api/v1/recording/health | pretty; then
    echo "unavailable: the endpoint refused or the server is down."
    echo "A 403 means the token is not an administrator's."
    echo "A 404 means the build predates the health endpoint."
    echo "A 501 means recording.enabled is false."
  fi
fi

section "Effective recording configuration"
if [[ -z "${TOKEN}" ]]; then
  echo "skipped: needs a token (--token)"
elif command -v jq >/dev/null 2>&1; then
  # The DVR block only; the rest of the config may contain credentials.
  fetch /api/v1/config \
    | jq '.config.video.download.recording // "no recording block configured (defaults apply)"' \
    || echo "unavailable"
else
  echo "skipped: needs jq to extract the recording block without dumping"
  echo "the whole config, which may contain credentials."
fi

section "Recording quota"
if [[ -z "${TOKEN}" ]]; then
  echo "skipped: needs a token (--token)"
else
  fetch /api/v1/recording/quota | pretty || echo "unavailable"
fi

section "On-disk state"
recordings_db="${STORAGE_DIR}/recordings.db"
rules_file="${STORAGE_DIR}/recording_rules.json"
outbox_file="${STORAGE_DIR}/recording_notification_outbox.json"
recovery_dir="${BACKUP_DIR}/recordings_recovery"

# mtime_as_iso8601_utc <file>
#   Prints the file's modification time as `YYYY-MM-DDTHH:MM:SSZ`, or
#   `unknown` if neither `stat` flavour works. GNU `stat -c %Y` and BSD
#   / macOS `stat -f %m` both yield an mtime epoch; GNU `date -d @N`
#   accepts the `@`-prefixed epoch and formats it, BSD/macOS `date -r
#   N` accepts the raw epoch directly (no `@` prefix).
mtime_as_iso8601_utc() {
  local f="$1" ts
  if ts=$(stat -c %Y -- "${f}" 2>/dev/null); then
    date -u -d "@${ts}" '+%Y-%m-%dT%H:%M:%SZ'
  elif ts=$(stat -f %m -- "${f}" 2>/dev/null); then
    # BSD/macOS `date -r` reads the raw epoch as a numeric argument;
    # the `@N` form is GNU-only and would fail with "No such file or
    # directory" because date would try to stat a file named `@<ts>`.
    date -u -r "${ts}" '+%Y-%m-%dT%H:%M:%SZ'
  else
    echo unknown
  fi
}

for f in "${recordings_db}" "${rules_file}" "${outbox_file}"; do
  if [[ -f "${f}" ]]; then
    printf '%s  (%s bytes, modified %s)\n' \
      "${f}" \
      "$(wc -c <"${f}" | tr -d ' ')" \
      "$(mtime_as_iso8601_utc "${f}")"
  else
    printf '%s  (absent)\n' "${f}"
  fi
done

section "Recording recovery"
# The queue lives in a B+Tree, so there is no JSON to summarise here. What an
# operator can check without the server is whether the recovery history that
# would rebuild that B+Tree exists, is current, and is on its own filesystem.
if [[ ! -d "${recovery_dir}" ]]; then
  echo "${recovery_dir}  (absent)"
  echo "No recovery history. If ${recordings_db} exists, the server will refuse"
  echo "to start: the database is ahead of every surviving history."
else
  printf 'directory: %s\n' "${recovery_dir}"
  if [[ -f "${recovery_dir}/CURRENT" ]]; then
    printf 'CURRENT:   %s\n' "$(tr -d '\n' <"${recovery_dir}/CURRENT")"
  else
    echo "CURRENT:   (absent — the pointer is repaired from the newest valid generation on open)"
  fi
  generations=$(find "${recovery_dir}" -maxdepth 1 -type d -name 'gen-*' 2>/dev/null | sort)
  if [[ -z "${generations}" ]]; then
    echo "generations: none"
  else
    echo "generations:"
    while IFS= read -r gen; do
      [[ -n "${gen}" ]] || continue
      journal="${gen}/journal.bin"
      journal_bytes=0
      [[ -f "${journal}" ]] && journal_bytes=$(wc -c <"${journal}" | tr -d ' ')
      printf '  %s  (journal %s bytes, modified %s)\n' \
        "$(basename "${gen}")" "${journal_bytes}" "$(mtime_as_iso8601_utc "${gen}")"
    done <<<"${generations}"
    # Two is the retained pair: current plus one verified predecessor.
    printf 'retained:  %s (expected 1 or 2)\n' "$(printf '%s\n' "${generations}" | grep -c .)"
  fi

  # Recovery on the same filesystem as the database survives a corrupt file
  # but not the loss of the volume, which is the failure it exists for.
  db_dir="$(dirname "${recordings_db}")"
  if [[ -d "${db_dir}" ]]; then
    db_fs="$(df -P "${db_dir}" 2>/dev/null | awk 'NR==2 {print $1}')"
    rec_fs="$(df -P "${recovery_dir}" 2>/dev/null | awk 'NR==2 {print $1}')"
    if [[ -n "${db_fs}" && "${db_fs}" == "${rec_fs}" ]]; then
      echo
      echo "warning: the recovery history shares a filesystem with the database."
      echo "It survives a corrupt database file, but not the loss of the volume."
      echo "Point backup_dir at a different mount."
    fi
  fi
fi

if [[ -f "${rules_file}" ]] && command -v jq >/dev/null 2>&1; then
  section "Rules summary"
  jq '{
        version: .version,
        rules_total: (.rules | length),
        rules_enabled: (.rules | map(select(.enabled)) | length),
        new_episode_enabled:
          (.rules | map(select(.enabled and (.body | has("NewEpisode")))) | length),
        weekly_enabled:
          (.rules | map(select(.enabled and (.body | has("WeeklyTimeslot")))) | length),
        tombstones: (.tombstones.tombstones | length)
      }' "${rules_file}" || echo "could not parse ${rules_file}"
  echo
  echo "Note: enabled NewEpisode rules cannot currently match — the scheduler"
  echo "has no EPG horizon, so only WeeklyTimeslot rules materialize."
fi

if [[ -f "${outbox_file}" ]] && command -v jq >/dev/null 2>&1; then
  section "Notification outbox"
  jq '{ next_id: .next_id, pending: (.entries | length),
        attempts: (.entries | map(.attempts)),
        channels_pending: (.entries | map(.pending) | flatten | unique) }' \
    "${outbox_file}" || echo "could not parse ${outbox_file}"
fi

section "What to look at first"
cat <<'HINTS'
- retention_last_tick older than disk.cleanup_interval_secs → the retention
  supervisor is stalled or the DVR is disabled.
- reconciliation_last_run null → the supervisors never started; check the log
  for "Recording is disabled" or a startup error.
- recordings.db present but the recovery directory absent → the server will
  refuse to start; the database is ahead of every surviving history.
- more than two generations retained → a checkpoint failed to prune; check the
  log for a recovery error.
- recovery lag reported by /api/v1/recording/health → the journal is ahead of
  the database and the next open will rebuild it.
- notification outbox pending > 0 and not draining → grep the log for
  recording_notification_dead_lettered.
- no retention, no watermarks and no quota → recording disk use is unbounded;
  the server logs a warning for this at startup.
HINTS
