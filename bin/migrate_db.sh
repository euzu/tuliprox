#!/usr/bin/env bash
# Database migration & inspection utility for tuliprox B+Tree files.
#
# Inspects, repairs, and safely migrates legacy (V1/V2) database files (such as
# series.db, live.db, video.db, m3u.db, epg.db, id_mapping.db, etc.) to B+Tree V3.
# If corrupted LZ4 blocks or unreadable entries are encountered, the tool recovers
# all healthy records, skips the damaged entries, and creates a clean, valid V3 database.
#
# Can run directly on the host using the local tuliprox binary (or cargo),
# or remotely inside a running Docker container via --docker.
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
PROJECT_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd -P)"

die() {
  echo "❌ Error: $*" >&2
  exit 1
}

info() {
  echo "ℹ️  $*"
}

success() {
  echo "✅ $*"
}

usage() {
  cat <<'USAGE'
Usage: migrate_db.sh [OPTIONS] <PATH_TO_DB>

Arguments:
  <PATH_TO_DB>           Path to the database file to inspect or migrate
                         (e.g. series.db, live.db, video.db, m3u.db, epg.db)

Options:
  -i, --inspect          Inspect database without migrating (version, healthy/corrupt records)
  -m, --migrate          Migrate database to B+Tree V3 (default action)
  -t, --type <TYPE>      Explicit database type: series, xtream, target-xtream, m3u, target-m3u,
                         epg, id-mapping, uuid-mapping, metadata-retry, qos, geoip, library
                         (Default: auto-detected from filename and path)
  -d, --docker <CONTAINER>
                         Run the migration inside the specified Docker container
  --no-backup            Do not create a timestamped backup before migrating
  -h, --help             Show this help message

Examples:
  # Inspect a database:
  ./bin/migrate_db.sh --inspect /path/to/series.db
  ./bin/migrate_db.sh --inspect /path/to/live.db

  # Migrate a database with automatic backup:
  ./bin/migrate_db.sh /path/to/series.db
  ./bin/migrate_db.sh /path/to/epg.db

  # Migrate a database inside a running Docker container:
  ./bin/migrate_db.sh --docker tuliprox /app/data/input_dir/series.db
USAGE
}

ACTION="migrate"
DB_PATH=""
DB_TYPE=""
DOCKER_CONTAINER=""
BACKUP=true

while [[ $# -gt 0 ]]; do
  case "$1" in
    -i|--inspect)
      ACTION="inspect"
      shift
      ;;
    -m|--migrate)
      ACTION="migrate"
      shift
      ;;
    -t|--type)
      DB_TYPE="${2:?--type requires a value}"
      shift 2
      ;;
    -d|--docker)
      DOCKER_CONTAINER="${2:?--docker requires a container name}"
      shift 2
      ;;
    --no-backup)
      BACKUP=false
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    -*)
      die "Unknown option: $1 (run with --help for usage)"
      ;;
    *)
      if [[ -z "$DB_PATH" ]]; then
        DB_PATH="$1"
        shift
      else
        die "Unexpected argument: $1"
      fi
      ;;
  esac
done

[[ -n "$DB_PATH" ]] || { usage; die "Please provide a database file path"; }

# -----------------------------------------------------------------------------
# Docker execution mode
# -----------------------------------------------------------------------------
if [[ -n "$DOCKER_CONTAINER" ]]; then
  info "Executing in Docker container '${DOCKER_CONTAINER}'..."
  docker ps -q --filter "name=^/${DOCKER_CONTAINER}$" >/dev/null 2>&1 || \
    docker ps -q --filter "name=${DOCKER_CONTAINER}" >/dev/null 2>&1 || \
    die "Docker container '${DOCKER_CONTAINER}' is not running"

  CMD=(tuliprox)
  if [[ "$ACTION" == "inspect" ]]; then
    CMD+=(--inspect-db "$DB_PATH")
  else
    CMD+=(--migrate-db "$DB_PATH")
    if [[ "$BACKUP" == false ]]; then
      CMD+=(--no-backup)
    fi
  fi

  if [[ -n "$DB_TYPE" ]]; then
    CMD+=(--db-type "$DB_TYPE")
  fi

  info "Running: docker exec -it \"${DOCKER_CONTAINER}\" ${CMD[*]}"
  docker exec -it "${DOCKER_CONTAINER}" "${CMD[@]}"
  exit $?
fi

# -----------------------------------------------------------------------------
# Local host execution mode
# -----------------------------------------------------------------------------
[[ -f "$DB_PATH" ]] || die "Database file not found: $DB_PATH"

# Locate tuliprox binary
TULIPROX_BIN=""
if [[ -x "${PROJECT_ROOT}/target/release/tuliprox" ]]; then
  TULIPROX_BIN="${PROJECT_ROOT}/target/release/tuliprox"
elif [[ -x "${PROJECT_ROOT}/target/debug/tuliprox" ]]; then
  TULIPROX_BIN="${PROJECT_ROOT}/target/debug/tuliprox"
elif command -v tuliprox >/dev/null 2>&1; then
  TULIPROX_BIN="$(command -v tuliprox)"
fi

EXEC_CMD=()
if [[ -n "$TULIPROX_BIN" ]]; then
  EXEC_CMD=("$TULIPROX_BIN")
else
  info "No compiled tuliprox binary found, running via cargo..."
  EXEC_CMD=(cargo run --quiet --manifest-path "${PROJECT_ROOT}/Cargo.toml" --bin tuliprox --)
fi

if [[ "$ACTION" == "inspect" ]]; then
  EXEC_CMD+=(--inspect-db "$DB_PATH")
else
  EXEC_CMD+=(--migrate-db "$DB_PATH")
  if [[ "$BACKUP" == false ]]; then
    EXEC_CMD+=(--no-backup)
  fi
fi

if [[ -n "$DB_TYPE" ]]; then
  EXEC_CMD+=(--db-type "$DB_TYPE")
fi

"${EXEC_CMD[@]}"
