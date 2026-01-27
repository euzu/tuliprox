#!/usr/bin/env bash
set -eo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
WORKING_DIR="$(cd -- "${SCRIPT_DIR}/.." && pwd -P)"
if ! cd "${WORKING_DIR}" >/dev/null 2>&1; then
  echo "🧨 Error: Unable to change directory to repo root: ${WORKING_DIR}" >&2
  exit 1
fi

TMPDIR_ORIG=$TMPDIR
RESOURCES_DIR="$WORKING_DIR/resources"
RELEASE_DIR="$WORKING_DIR/release"
FRONTEND_DIR="${WORKING_DIR}/frontend"
FRONTEND_BUILD_DIR="${FRONTEND_DIR}/dist"
BACKEND_DIR="${WORKING_DIR}/backend"
TARGET_DIR="${WORKING_DIR}/target"
BRANCH=$(git branch --show-current)
START_BRANCH="${BRANCH}"
START_HEAD="$(git rev-parse HEAD)"
RUN_KEY=""
DOCKER_BUILD_RUN_ID=""
ORIGIN_MASTER_AFTER_BUMP_SHA=""
ORIGIN_DEVELOP_BEFORE_SHA=""
ORIGIN_DEVELOP_AFTER_FF_SHA=""
GITHUB_MASTER_AFTER_BUMP_SHA=""
GITHUB_DEVELOP_BEFORE_SHA=""
GITHUB_DEVELOP_AFTER_FF_SHA=""
DEVELOP_BRANCH_FROZEN="false"
DEVELOP_BRANCH_PROTECTION_CREATED="false"

die() {
  echo "🧨 Error: $*" >&2
  exit 1
}

log_sha() {
  local label="$1"
  local value="${2:-}"
  if [ -n "${value}" ]; then
    echo "🔎 ${label}: ${value}"
  else
    echo "🔎 ${label}: <empty>"
  fi
}

csv_contains() {
  local csv="${1:-}"
  local needle="${2:-}"
  [[ ",${csv}," == *",${needle},"* ]]
}

csv_contains_any() {
  local csv="${1:-}"
  shift || true
  local needle
  for needle in "$@"; do
    if csv_contains "${csv}" "${needle}"; then
      return 0
    fi
  done
  return 1
}

gh_token_scopes_csv() {
  local scopes

  scopes="$(
    gh auth status 2>/dev/null \
      | awk -F'Token scopes: ' '/Token scopes:/{print $2; exit}' \
      || true
  )"
  scopes="$(echo "${scopes}" | tr -d "'" | tr -d '"' | tr -d ' ' || true)"
  if [ -n "${scopes}" ]; then
    echo "${scopes}"
    return 0
  fi

  scopes="$(
    gh api -i user 2>/dev/null \
      | awk -F': ' 'tolower($1)=="x-oauth-scopes"{print $2; exit}' \
      || true
  )"
  scopes="$(echo "${scopes}" | tr -d ' ' || true)"
  echo "${scopes}"
}

gh_guard_permissions() {
  local scopes_csv
  local -a missing_required
  scopes_csv="$(gh_token_scopes_csv || true)"

  if [ -z "${scopes_csv}" ]; then
    echo "⚠️ Could not determine GitHub token scopes; skipping scope validation." >&2
    return 0
  fi

  missing_required=()

  # Required for workflow dispatch + repo administration.
  if ! csv_contains_any "${scopes_csv}" repo public_repo; then
    missing_required+=("repo (or public_repo for public repos)")
  fi
  if ! csv_contains "${scopes_csv}" workflow; then
    missing_required+=("workflow")
  fi

  if [ "${#missing_required[@]}" -gt 0 ]; then
    echo "🧨 Missing required GitHub token scopes: ${missing_required[*]}" >&2
    echo "   Fix: gh auth refresh -h github.com -s repo -s workflow" >&2
    echo "   (or re-auth with a PAT that has these scopes via 'gh auth login --with-token')" >&2
    die "Insufficient GitHub token scopes."
  fi

  if [ "${FREEZE_DEVELOP_BRANCH:-1}" != "0" ]; then
    local is_admin
    is_admin="$(gh api "repos/{owner}/{repo}" --jq '.permissions.admin' 2>/dev/null || true)"
    if [ "${is_admin}" != "true" ]; then
      die "GitHub account lacks admin permission for {owner}/{repo} (required to lock/unlock 'develop'). Set FREEZE_DEVELOP_BRANCH=0 to skip."
    fi
  fi

  gh_guard_ghcr_cleanup_permissions() {
    local owner owner_type image list_endpoint
    owner="$(gh repo view --json owner -q .owner.login 2>/dev/null || true)"
    if [ -z "${owner}" ]; then
      echo "🧨 Could not resolve repo owner for GHCR cleanup permission check." >&2
      return 1
    fi

    owner_type="$(gh api "users/${owner}" --jq .type 2>/dev/null || true)"
    if [ -z "${owner_type}" ]; then
      owner_type="User"
    fi

    for image in tuliprox tuliprox-alpine; do
      if [ "${owner_type}" = "Organization" ]; then
        list_endpoint="orgs/${owner}/packages/container/${image}/versions?per_page=1"
      else
        list_endpoint="users/${owner}/packages/container/${image}/versions?per_page=1"
      fi

      if ! gh api "${list_endpoint}" >/dev/null 2>&1; then
        echo "🧨 Missing GitHub permissions for GHCR cleanup: cannot list container package versions for '${owner}/${image}'." >&2
        echo "   Fix: gh auth refresh -h github.com -s read:packages -s write:packages -s delete:packages" >&2
        echo "   Or skip cleanup: CLEANUP_DOCKER_IMAGES_ON_FAILURE=0" >&2
        return 1
      fi
    done

    return 0
  }

  if [ "${CLEANUP_DOCKER_IMAGES_ON_FAILURE:-1}" != "0" ]; then
    gh_guard_ghcr_cleanup_permissions || die "Insufficient GitHub permissions for GHCR cleanup."
  fi
}

extract_release_notes_from_changelog() {
  local version="${1#v}"
  local changelog_file="${2:-${WORKING_DIR}/CHANGELOG.md}"

  if [ -z "${version}" ]; then
    return 1
  fi
  if [ ! -f "${changelog_file}" ]; then
    return 1
  fi

  local notes
  notes="$(
    awk -v ver="${version}" '
      function is_version_heading(line) {
        return (line ~ "^#{1,6}[[:space:]]+v?[0-9]+\\.[0-9]+\\.[0-9]+")
      }
      BEGIN { in_section = 0 }
      {
        if ($0 ~ ("^#{1,6}[[:space:]]+v?" ver "([[:space:]]|$)")) {
          in_section = 1
          next
        }
        if (in_section && is_version_heading($0)) {
          exit
        }
        if (in_section) {
          print
        }
      }
    ' "${changelog_file}"
  )"

  if [ -z "$(echo "${notes}" | tr -d '[:space:]')" ]; then
    return 1
  fi

  printf "%s\n" "${notes}"
}

gh_cleanup_docker_images_on_failure() {
  local run_id="${1:-}"

  if [ "${CLEANUP_DOCKER_IMAGES_ON_FAILURE:-1}" = "0" ]; then
    echo "🧹 Skipping GHCR cleanup (CLEANUP_DOCKER_IMAGES_ON_FAILURE=0)" >&2
    return 0
  fi

  local bump_version="${BUMP_VERSION:-}"
  bump_version="${bump_version#v}"
  if [ -z "${bump_version}" ]; then
    return 0
  fi

  if ! command -v gh >/dev/null 2>&1; then
    return 0
  fi
  if ! gh auth status >/dev/null 2>&1; then
    return 0
  fi

  if [ -n "${run_id}" ]; then
    local status conclusion
    for _ in {1..30}; do
      status="$(gh run view "${run_id}" --json status --jq .status 2>/dev/null || true)"
      conclusion="$(gh run view "${run_id}" --json conclusion --jq .conclusion 2>/dev/null || true)"
      if [ "$status" = "completed" ] && [ "$conclusion" = "cancelled" ]; then
        break
      fi
      sleep 2
    done
    echo "🔎 docker-build run ${run_id}: status=${status:-unknown} conclusion=${conclusion:-unknown}" >&2
  fi

  local owner
  owner="$(gh repo view --json owner -q .owner.login 2>/dev/null || true)"
  if [ -z "${owner}" ]; then
    echo "⚠️ Could not resolve repo owner for GHCR cleanup; skipping." >&2
    return 0
  fi

  local owner_type
  owner_type="$(gh api "users/${owner}" --jq .type 2>/dev/null || true)"
  if [ -z "${owner_type}" ]; then
    owner_type="User"
  fi

  local previous_version="${VERSION:-}"
  previous_version="${previous_version#v}"

  declare -a images=("tuliprox" "tuliprox-alpine")
  for image in "${images[@]}"; do
    local restored_latest="false"

    # Best-effort: restore ':latest' to the previously released version before deleting the failed release tag.
    if [ -n "${previous_version}" ] && [ "${previous_version}" != "${bump_version}" ]; then
      if command -v docker >/dev/null 2>&1 && docker buildx version >/dev/null 2>&1; then
        local gh_user
        gh_user="$(gh api user --jq .login 2>/dev/null || true)"
        if [ -n "${gh_user}" ]; then
          echo "⏪ Restoring ghcr.io/${owner}/${image}:latest -> ${previous_version}" >&2
          if gh auth token 2>/dev/null | docker login ghcr.io --username "${gh_user}" --password-stdin >/dev/null 2>&1; then
            if docker buildx imagetools create \
              -t "ghcr.io/${owner}/${image}:latest" \
              "ghcr.io/${owner}/${image}:${previous_version}" \
              >/dev/null 2>&1; then
              restored_latest="true"
            else
              echo "⚠️ Failed to restore ':latest' for ${image} -> ${previous_version}. 'latest' may disappear if we delete the failed version." >&2
              restored_latest="false"
            fi
          else
            echo "⚠️ Failed to login to ghcr.io; cannot restore ':latest' for ${image}." >&2
          fi
        fi
      fi
    fi

    echo "🗑 Deleting GHCR images for failed release tag '${bump_version}' (${image})" >&2

    local list_endpoint delete_prefix
    if [ "${owner_type}" = "Organization" ]; then
      list_endpoint="orgs/${owner}/packages/container/${image}/versions?per_page=100"
      delete_prefix="orgs/${owner}/packages/container/${image}/versions"
    else
      list_endpoint="users/${owner}/packages/container/${image}/versions?per_page=100"
      delete_prefix="users/${owner}/packages/container/${image}/versions"
    fi

    local version_ids gh_err
    if ! version_ids="$(
      gh api "${list_endpoint}" \
        --jq ".[] | select((.metadata.container.tags // []) | index(\"${bump_version}\")) | [.id, (((.metadata.container.tags // []) | index(\"latest\")) != null)] | @tsv" \
        2>&1
    )"; then
      gh_err="${version_ids}"
      echo "⚠️ Failed to list GHCR package versions for ${image}. Skipping delete. (${gh_err})" >&2
      continue
    fi

    # Defensive: avoid treating API error JSON as an ID.
    version_ids="$(echo "${version_ids}" | awk -F'\t' '$1 ~ /^[0-9]+$/ { print }' || true)"

    if [ -z "${version_ids}" ]; then
      echo "ℹ️ No GHCR package versions found for ${image}:${bump_version}" >&2
      continue
    fi

    while IFS=$'\t' read -r vid has_latest; do
      [ -z "${vid}" ] && continue
      if [ "${has_latest}" = "true" ] && [ "${restored_latest}" != "true" ]; then
        echo "⚠️ Skipping delete of GHCR package version id ${vid} for ${image}:${bump_version} because it contains the 'latest' tag and we could not restore it." >&2
        continue
      fi
      echo "🗑 Deleting package version id ${vid} (${image}:${bump_version})" >&2
      gh api -X DELETE "${delete_prefix}/${vid}" >/dev/null 2>&1 || true
    done <<< "${version_ids}"
  done
}

gh_freeze_develop_branch() {
  if [ "${FREEZE_DEVELOP_BRANCH:-1}" = "0" ]; then
    echo "🧊 Skipping develop branch freeze (FREEZE_DEVELOP_BRANCH=0)"
    return 0
  fi

  if ! command -v gh >/dev/null 2>&1; then
    die "gh is required to freeze develop branch (install GitHub CLI)."
  fi
  gh auth status >/dev/null 2>&1 || die "Not logged into GitHub CLI. Run 'gh auth login' first."

  echo "🔒 Freezing pushes to 'develop' (branch lock)"

  if gh api "repos/{owner}/{repo}/branches/develop/protection" >/dev/null 2>&1; then
    DEVELOP_BRANCH_PROTECTION_CREATED="false"
  else
    DEVELOP_BRANCH_PROTECTION_CREATED="true"
  fi

  if ! gh api -X PUT "repos/{owner}/{repo}/branches/develop/protection" --input - >/dev/null 2>&1 <<'JSON'
{
  "required_status_checks": null,
  "enforce_admins": true,
  "required_pull_request_reviews": null,
  "restrictions": null,
  "lock_branch": true
}
JSON
  then
    die "Failed to enable/lock branch protection for 'develop'."
  fi

  DEVELOP_BRANCH_FROZEN="true"
}

gh_stop_actions_develop_branch() {
  if [ "${STOP_DEVELOP_ACTIONS_ON_RELEASE:-1}" = "0" ]; then
    echo "🛑 Skipping develop workflow cancellation (STOP_DEVELOP_ACTIONS_ON_RELEASE=0)"
    return 0
  fi

  if ! command -v gh >/dev/null 2>&1; then
    die "gh is required to stop workflows on develop (install GitHub CLI)."
  fi
  gh auth status >/dev/null 2>&1 || die "Not logged into GitHub CLI. Run 'gh auth login' first."

  echo "🛑 Stopping GitHub Actions workflows on 'develop' (queued/in_progress)"

  local runs
  runs="$(
    gh run list \
      -b develop \
      -L 50 \
      --json databaseId,status,workflowName,displayTitle \
      --jq '.[] | select(.status=="queued" or .status=="in_progress") | "\(.databaseId)\t\(.status)\t\(.workflowName)\t\(.displayTitle)"' \
      2>/dev/null || true
  )"

  if [ -z "${runs}" ]; then
    echo "✅ No queued/in_progress workflow runs found on 'develop'."
    return 0
  fi

  local fail_count=0
  local total_count=0
  local id status workflow_name title
  while IFS=$'\t' read -r id status workflow_name title; do
    [ -z "${id}" ] && continue
    total_count=$((total_count + 1))
    echo "🛑 Cancelling run ${id} (${status}) - ${workflow_name}: ${title}" >&2
    if ! gh run cancel "${id}" >/dev/null 2>&1; then
      echo "⚠️ Failed to cancel run ${id}" >&2
      fail_count=$((fail_count + 1))
    fi
  done <<< "${runs}"

  if [ "${fail_count}" -gt 0 ]; then
    die "Failed to cancel ${fail_count}/${total_count} workflow runs on 'develop'. Check your permissions (scopes: repo, workflow)."
  fi

  # Best-effort: wait until the queue is drained so the release doesn't race ongoing develop jobs.
  local remaining
  for _ in {1..20}; do
    remaining="$(
      gh run list \
        -b develop \
        -L 50 \
        --json status \
        --jq '[.[] | select(.status=="queued" or .status=="in_progress")] | length' \
        2>/dev/null || echo "0"
    )"
    if [ "${remaining}" = "0" ]; then
      echo "✅ All develop workflows are stopped."
      return 0
    fi
    sleep 3
  done

  echo "⚠️ Some develop workflows are still not stopped (remaining=${remaining:-unknown}). Continuing anyway." >&2
}

gh_unfreeze_develop_branch() {
  if [ "${DEVELOP_BRANCH_FROZEN}" != "true" ]; then
    return 0
  fi

  if ! command -v gh >/dev/null 2>&1; then
    return 0
  fi
  if ! gh auth status >/dev/null 2>&1; then
    return 0
  fi

  echo "🔓 Unfreezing pushes to 'develop'"

  if [ "${DEVELOP_BRANCH_PROTECTION_CREATED}" = "true" ]; then
    gh api -X DELETE "repos/{owner}/{repo}/branches/develop/protection" >/dev/null 2>&1 || true
  else
    # Best-effort unlock (keep existing protection settings)
    gh api -X PUT "repos/{owner}/{repo}/branches/develop/protection" --input - >/dev/null 2>&1 <<'JSON' || true
{
  "required_status_checks": null,
  "enforce_admins": true,
  "required_pull_request_reviews": null,
  "restrictions": null,
  "lock_branch": false
}
JSON
  fi
  DEVELOP_BRANCH_FROZEN="false"
}

cleanup() {
  exit_code=$?

  echo "🧰 Restoring TMPDIR to original value"
  if [ -n "${TMPDIR_ORIG:-}" ]; then
    export TMPDIR="$TMPDIR_ORIG"
  else
    unset TMPDIR
  fi

  # Some tools (rustc/gh) fail when the current directory was removed (e.g., 'cargo clean'
  # executed while being inside ./target). Re-anchor in the repo root for best-effort cleanup.
  cd "${WORKING_DIR}" >/dev/null 2>&1 || true

  gh_unfreeze_develop_branch || true
  if [ "${exit_code}" -eq 0 ]; then
    return
  fi

  echo "🧹 Cleanup after failure (exit ${exit_code})" >&2
  log_sha "START_BRANCH" "${START_BRANCH}" >&2
  log_sha "START_HEAD" "${START_HEAD}" >&2
  log_sha "ORIGIN_MASTER_AFTER_BUMP_SHA" "${ORIGIN_MASTER_AFTER_BUMP_SHA}" >&2
  log_sha "ORIGIN_DEVELOP_BEFORE_SHA" "${ORIGIN_DEVELOP_BEFORE_SHA}" >&2
  log_sha "ORIGIN_DEVELOP_AFTER_FF_SHA" "${ORIGIN_DEVELOP_AFTER_FF_SHA}" >&2
  log_sha "GITHUB_MASTER_AFTER_BUMP_SHA" "${GITHUB_MASTER_AFTER_BUMP_SHA}" >&2
  log_sha "GITHUB_DEVELOP_BEFORE_SHA" "${GITHUB_DEVELOP_BEFORE_SHA}" >&2
  log_sha "GITHUB_DEVELOP_AFTER_FF_SHA" "${GITHUB_DEVELOP_AFTER_FF_SHA}" >&2
  log_sha "RUN_KEY" "${RUN_KEY}" >&2
  log_sha "DOCKER_BUILD_RUN_ID" "${DOCKER_BUILD_RUN_ID}" >&2

  if command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
    run_id="${DOCKER_BUILD_RUN_ID:-}"
    if [ -z "${run_id}" ] && [ -n "${RUN_KEY:-}" ]; then
      run_id="$(
        gh run list \
          -w docker-build.yml \
          -e workflow_dispatch \
          -b master \
          -L 20 \
          --json databaseId,displayTitle \
          --jq ".[] | select(.displayTitle | contains(\"${RUN_KEY}\")) | .databaseId" \
          | head -n 1
      )"
    fi

    if [ -n "${run_id}" ]; then
      status="$(gh run view "${run_id}" --json status --jq .status 2>/dev/null || true)"
      if [ -n "${status}" ] && [ "${status}" != "completed" ]; then
        echo "🛑 Cancelling docker-build workflow run ${run_id}" >&2
        gh run cancel "${run_id}" >/dev/null 2>&1 || true
      fi

      gh_cleanup_docker_images_on_failure "${run_id}"
    fi
  fi

  echo "↩️ Resetting local git state to ${START_BRANCH}@${START_HEAD}" >&2
  git reset --hard "${START_HEAD}" >/dev/null 2>&1 || true
  git checkout -f "${START_BRANCH}" >/dev/null 2>&1 || true

  if [ -n "${ORIGIN_MASTER_AFTER_BUMP_SHA}" ]; then
    echo "⏪ Reverting remote 'origin/master' (force-with-lease)" >&2
    git push --force-with-lease=refs/heads/master:"${ORIGIN_MASTER_AFTER_BUMP_SHA}" origin "${START_HEAD}:refs/heads/master" >/dev/null 2>&1 || true
  fi

  if [ -n "${ORIGIN_DEVELOP_AFTER_FF_SHA}" ] && [ -n "${ORIGIN_DEVELOP_BEFORE_SHA}" ]; then
    echo "⏪ Reverting remote 'origin/develop' (force-with-lease)" >&2
    git push --force-with-lease=refs/heads/develop:"${ORIGIN_DEVELOP_AFTER_FF_SHA}" origin "${ORIGIN_DEVELOP_BEFORE_SHA}:refs/heads/develop" >/dev/null 2>&1 || true
  fi

  if [ -n "${GITHUB_MASTER_AFTER_BUMP_SHA}" ]; then
    echo "⏪ Reverting remote 'github/master' (force-with-lease)" >&2
    git push --force-with-lease=refs/heads/master:"${GITHUB_MASTER_AFTER_BUMP_SHA}" github "${START_HEAD}:refs/heads/master" >/dev/null 2>&1 || true
  fi

  if [ -n "${GITHUB_DEVELOP_AFTER_FF_SHA}" ] && [ -n "${GITHUB_DEVELOP_BEFORE_SHA}" ]; then
    echo "⏪ Reverting remote 'github/develop' (force-with-lease)" >&2
    git push --force-with-lease=refs/heads/develop:"${GITHUB_DEVELOP_AFTER_FF_SHA}" github "${GITHUB_DEVELOP_BEFORE_SHA}:refs/heads/develop" >/dev/null 2>&1 || true
  fi
}

trap cleanup EXIT INT TERM

# Validate release strategy
if [ $# -ne 1 ]; then
  die "Release strategy required (major|minor)"
fi

# Validate we're creating the release from master branch
if [ "$BRANCH" != "master" ]; then
  die "Creating the release from your current branch '${BRANCH}' is prohibited!"
fi

# Guards: ensure we're releasing the right state
if ! git diff --quiet || ! git diff --cached --quiet; then
  die "Working tree is not clean. Commit/stash your changes first."
fi

echo "🧭 Release starting on '${START_BRANCH}'"
log_sha "START_HEAD" "${START_HEAD}"

git fetch --quiet --tags origin master develop || die "Failed to fetch from 'origin'."

ORIGIN_MASTER_SHA="$(git rev-parse origin/master)"
ORIGIN_DEVELOP_SHA="$(git rev-parse origin/develop)"
log_sha "origin/master" "${ORIGIN_MASTER_SHA}"
log_sha "origin/develop" "${ORIGIN_DEVELOP_SHA}"

if [ "$(git rev-parse HEAD)" != "${ORIGIN_MASTER_SHA}" ]; then
  die "Local 'master' is not at 'origin/master'. Please pull/push before releasing."
fi

if ! git merge-base --is-ancestor origin/develop HEAD; then
  die "'master' does not contain 'origin/develop'. Merge develop into master before releasing."
fi

ORIGIN_DEVELOP_BEFORE_SHA="$(git rev-parse origin/develop)"
log_sha "ORIGIN_DEVELOP_BEFORE_SHA (saved)" "${ORIGIN_DEVELOP_BEFORE_SHA}"
if git remote get-url github >/dev/null 2>&1; then
  git fetch --quiet github develop || die "Failed to fetch from 'github'."
  GITHUB_DEVELOP_BEFORE_SHA="$(git rev-parse github/develop)"
  log_sha "GITHUB_DEVELOP_BEFORE_SHA (saved)" "${GITHUB_DEVELOP_BEFORE_SHA}"
fi

LAST_TAG="$(git describe --tags --abbrev=0 2>/dev/null || true)"
if [ -n "$LAST_TAG" ]; then
  if git diff --quiet "${LAST_TAG}..HEAD" -- backend frontend shared resources docker config Cargo.toml Cargo.lock; then
    die "No relevant changes since last release tag '${LAST_TAG}'."
  fi
fi

if ! command -v gh &> /dev/null; then
  die "GitHub CLI could not be found. Please install gh toolset: https://cli.github.com"
fi

gh auth status >/dev/null 2>&1 || die "Not logged into GitHub CLI. Run 'gh auth login' first."
gh_guard_permissions

# Read current tag on HEAD
VERSION="$(git describe --tags --exact-match 2>/dev/null || true)"
if [ -z "${VERSION}" ]; then
  VERSION="$(git describe --tags --abbrev=0 2>/dev/null || true)"
fi

if [ -z "${VERSION}" ]; then
  die "Failed to read current tag."
fi

case "$1" in
  major) ./bin/inc_version.sh m ;;
  minor) ./bin/inc_version.sh p ;;
  *) die "Unknown option '$1' (expected: major|minor)" ;;
esac

# Marker: version bump push + trigger docker-build workflow (master)
echo "📦 Committing version bump"
echo
FILES=(Cargo.lock backend/Cargo.lock backend/Cargo.toml frontend/Cargo.toml shared/Cargo.toml)
for f in "${FILES[@]}"; do
  if [ -f "$f" ]; then
    git add "$f"
  fi
done

if git diff --cached --quiet; then
  die "Version bump produced no changes to commit."
fi

BUMP_VERSION="$(grep '^version' "${BACKEND_DIR}/Cargo.toml" | head -n1 | cut -d'"' -f2)"
if [ -z "${BUMP_VERSION}" ]; then
  die "Failed to read version from '${BACKEND_DIR}/Cargo.toml' after bump."
fi

RUN_KEY="$(uuidgen 2>/dev/null || true)"
if [ -z "${RUN_KEY}" ]; then
  RUN_KEY="run-$(date +%s)-$$"
fi

RUN_KEY="${1}-${RUN_KEY}"

log_sha "VERSION (current)" "${VERSION}"
log_sha "VERSION (release)" "v${BUMP_VERSION}"
log_sha "RUN_KEY (saved)" "${RUN_KEY}"

read -rp "Releasing version: '${BUMP_VERSION}', please confirm? [y/N] " answer

# Default 'N', cancel, if not 'y' or 'Y'
if [[ ! "$answer" =~ ^[Yy]$ ]]; then
    die "Canceled."
fi

gh_freeze_develop_branch

gh_stop_actions_develop_branch

echo "🧰 Setting TMPDIR to '$PWD/dev/tmp' for isolated builds"
TMPDIR_WORK="${WORKING_DIR}/dev/tmp"
mkdir -p "${TMPDIR_WORK}" || die "Failed to create TMPDIR: ${TMPDIR_WORK}"
if [ ! -d "${TMPDIR_WORK}" ] || [ ! -w "${TMPDIR_WORK}" ]; then
  die "TMPDIR is not writable: ${TMPDIR_WORK}"
fi
export TMPDIR="${TMPDIR_WORK}"

git commit -m "ci: bump version v${BUMP_VERSION}"
git push origin HEAD:master
ORIGIN_MASTER_AFTER_BUMP_SHA="$(git rev-parse HEAD)"
log_sha "ORIGIN_MASTER_AFTER_BUMP_SHA (saved)" "${ORIGIN_MASTER_AFTER_BUMP_SHA}"
ORIGIN_MASTER_REMOTE_SHA="$(git ls-remote origin refs/heads/master | awk '{print $1}' | head -n 1)"
log_sha "origin/master (remote)" "${ORIGIN_MASTER_REMOTE_SHA}"
if git remote get-url github >/dev/null 2>&1; then
  git push github HEAD:master
  GITHUB_MASTER_AFTER_BUMP_SHA="${ORIGIN_MASTER_AFTER_BUMP_SHA}"
  log_sha "GITHUB_MASTER_AFTER_BUMP_SHA (saved)" "${GITHUB_MASTER_AFTER_BUMP_SHA}"
  GITHUB_MASTER_REMOTE_SHA="$(git ls-remote github refs/heads/master | awk '{print $1}' | head -n 1)"
  log_sha "github/master (remote)" "${GITHUB_MASTER_REMOTE_SHA}"
fi

echo "🚀 Triggering docker-build workflow (master, key: ${RUN_KEY})"
gh workflow run docker-build.yml --ref master -f branch=master -f choice=none -f run_key="${RUN_KEY}"

echo "🔎 Resolving docker-build workflow run id..."
for _ in {1..30}; do
  DOCKER_BUILD_RUN_ID="$(
    gh run list \
      -w docker-build.yml \
      -e workflow_dispatch \
      -b master \
      -L 20 \
      --json databaseId,displayTitle \
      --jq ".[] | select(.displayTitle | contains(\"${RUN_KEY}\")) | .databaseId" \
      | head -n 1
  )"
  if [ -n "${DOCKER_BUILD_RUN_ID}" ]; then
    break
  fi
  sleep 2
done

if [ -z "${DOCKER_BUILD_RUN_ID}" ]; then
  die "Timed out waiting for docker-build workflow run id (run_key=${RUN_KEY})."
fi

echo "🧩 docker-build run id: ${DOCKER_BUILD_RUN_ID}"

./bin/build_resources.sh

cd "$FRONTEND_DIR" || die "Can't find frontend directory"

echo "🛠️ Building version $BUMP_VERSION"

declare -A ARCHITECTURES=(
    [LINUX]=x86_64-unknown-linux-musl
    [WINDOWS]=x86_64-pc-windows-gnu
    [ARM7]=armv7-unknown-linux-musleabihf
    [AARCH64]=aarch64-unknown-linux-musl
    # [DARWIN64]=aarch64-apple-darwin
    # [DARWIN86]=x86_64-apple-darwin
)

declare -A DIRS=(
    [LINUX]=tuliprox_${BUMP_VERSION}_linux_x86_64
    [WINDOWS]=tuliprox_${BUMP_VERSION}_windows_x86_64
    [ARM7]=tuliprox_${BUMP_VERSION}_armv7
    [AARCH64]=tuliprox_${BUMP_VERSION}_aarch64
    # [DARWIN86]=tuliprox_${BUMP_VERSION}_apple-darwin_x86_64
    # [DARWIN64]=tuliprox_${BUMP_VERSION}_apple-darwin_aarch64
)

# Special case mapping for binary extensions (e.g., Windows needs .exe)
declare -A BIN_EXTENSIONS=(
    [WINDOWS]=.exe
)

if ! command -v cross >/dev/null 2>&1; then
  die "'cross' is required to install Rust targets."
fi

if ! command -v docker >/dev/null 2>&1; then
  die "'docker' is required for 'cross' builds. Install/start Docker Desktop (macOS) or Docker Engine (Linux)."
fi

if ! docker info >/dev/null 2>&1; then
  die "Docker daemon is not available. Start Docker so 'cross' can run builds in containers (otherwise it may fall back to host builds)."
fi

# On Apple Silicon, many cross images are linux/amd64-only. Force linux/amd64 containers so Docker
# doesn't fail with 'no matching manifest' and cross doesn't fall back to host builds.
HOST_OS="$(uname -s)"
HOST_ARCH="$(uname -m)"
if [ "${HOST_OS}" = "Darwin" ] && [ "${HOST_ARCH}" = "arm64" ]; then
  CROSS_PLATFORM_OPT="--platform=linux/amd64"
  if [ -z "${CROSS_CONTAINER_OPTS:-}" ]; then
    export CROSS_CONTAINER_OPTS="${CROSS_PLATFORM_OPT}"
    echo "🧰 Using CROSS_CONTAINER_OPTS='${CROSS_CONTAINER_OPTS}' (Apple Silicon compatibility)"
  elif [[ " ${CROSS_CONTAINER_OPTS} " == *" ${CROSS_PLATFORM_OPT} "* ]]; then
    echo "🧰 Keeping existing CROSS_CONTAINER_OPTS='${CROSS_CONTAINER_OPTS}' (Apple Silicon compatibility)"
  else
    export CROSS_CONTAINER_OPTS="${CROSS_CONTAINER_OPTS} ${CROSS_PLATFORM_OPT}"
    echo "🧰 Extending CROSS_CONTAINER_OPTS='${CROSS_CONTAINER_OPTS}' (Apple Silicon compatibility)"
  fi
fi

DARWIN_CROSS_ENABLED="false"
for TARGET in "${ARCHITECTURES[@]}"; do
  if [[ "${TARGET}" == *"-apple-darwin" ]]; then
    DARWIN_CROSS_ENABLED="true"
    break
  fi
done

if [ "${DARWIN_CROSS_ENABLED}" = "true" ]; then
  if [ "${BUILD_DARWIN_CROSS_IMAGES:-1}" = "0" ]; then
    echo "🍎 Skipping Darwin cross image build (BUILD_DARWIN_CROSS_IMAGES=0)"
  else
    echo "🍎 Ensuring local cross toolchain images for Darwin targets"
    darwin_targets=()
    for darwin_target in "${ARCHITECTURES[@]}"; do
      if [[ "${darwin_target}" == *"-apple-darwin" ]]; then
        darwin_targets+=("${darwin_target}")
      fi
    done
    DARWIN_TARGET_TRIPLES="${darwin_targets[*]}" "${WORKING_DIR}/bin/build_cross_toolchains_darwin_images.sh"
  fi
fi

if ! command -v rustup >/dev/null 2>&1; then
  die "'rustup' is required to install Rust targets."
fi

if command -v rustc >/dev/null 2>&1; then
  RUSTC_HOST_TRIPLE="$(rustc -vV 2>/dev/null | awk -F': ' '/^host:/{print $2}' | head -n 1)"
  if [ -n "${RUSTC_HOST_TRIPLE}" ]; then
    echo "🧾 rustc host triple: ${RUSTC_HOST_TRIPLE}"
  fi
fi

echo "🧰 Ensuring rustup targets are installed"
RUSTUP_TOOLCHAIN_FOR_TARGETS="${RUSTUP_TOOLCHAIN_FOR_TARGETS:-stable}"
echo "🧰 Using rustup toolchain for target installs: ${RUSTUP_TOOLCHAIN_FOR_TARGETS}"

# cross expects a Linux toolchain sysroot on non-Linux hosts (mounted into the container).
# rustup refuses to install non-host toolchains unless explicitly forced.
if [ "$(uname -s)" != "Linux" ]; then
  CROSS_SYSROOT_TOOLCHAIN="${CROSS_SYSROOT_TOOLCHAIN:-${RUSTUP_TOOLCHAIN_FOR_TARGETS}-x86_64-unknown-linux-gnu}"
  if rustup toolchain list | cut -d' ' -f1 | grep -Fxq "${CROSS_SYSROOT_TOOLCHAIN}"; then
    echo "✅ cross sysroot toolchain already installed: ${CROSS_SYSROOT_TOOLCHAIN}"
  else
    echo "➕ Installing cross sysroot toolchain: ${CROSS_SYSROOT_TOOLCHAIN}"
    rustup toolchain install "${CROSS_SYSROOT_TOOLCHAIN}" --profile minimal --force-non-host
  fi
fi

RUSTUP_REQUIRED_TARGETS=(
  wasm32-unknown-unknown
)

for TARGET in "${ARCHITECTURES[@]}"; do
  RUSTUP_REQUIRED_TARGETS+=("${TARGET}")
done

for TARGET in "${RUSTUP_REQUIRED_TARGETS[@]}"; do
  if rustup +"${RUSTUP_TOOLCHAIN_FOR_TARGETS}" target list --installed | grep -Fxq "${TARGET}"; then
    echo "✅ rust target already installed: ${TARGET}"
  else
    echo "➕ Installing rust target: ${TARGET}"
    rustup +"${RUSTUP_TOOLCHAIN_FOR_TARGETS}" target add "${TARGET}"
  fi
done

cd "$WORKING_DIR"
mkdir -p "$RELEASE_DIR"

# Clean previous builds
cargo clean || true

rm -rf "${FRONTEND_BUILD_DIR}"
cd "${FRONTEND_DIR}" && env RUSTFLAGS="--remap-path-prefix $HOME=~" trunk build --release
# Check if the frontend build directory exists
if [ ! -d "${FRONTEND_BUILD_DIR}" ]; then
    die "🧨 Error: Web directory '${FRONTEND_BUILD_DIR}' does not exist."
fi

cd "$WORKING_DIR"

# Prepare release package folder (assets to be uploaded to GitHub Release later)
RELEASE_PKG="$RELEASE_DIR/release_${BUMP_VERSION}"
rm -rf "${RELEASE_PKG}"
mkdir -p "${RELEASE_PKG}"

# Build binaries
for PLATFORM in "${!ARCHITECTURES[@]}"; do
    ARCHITECTURE="${ARCHITECTURES[$PLATFORM]}"
    DIR="${DIRS[$PLATFORM]}"

    BIN_NAME="tuliprox${BIN_EXTENSIONS[$PLATFORM]:-}"
    BIN_REL="${ARCHITECTURE}/release/${BIN_NAME}"
    BIN_PATH="${TARGET_DIR}/${BIN_REL}"

    # Ensure target is installed (guarded above, keep here as a safety net)
    rustup +"${RUSTUP_TOOLCHAIN_FOR_TARGETS}" target add "${ARCHITECTURE}"

    # Build for each platform
    cd "${WORKING_DIR}"
    cargo clean || true # Clean before each build to avoid conflicts
    cd "${WORKING_DIR}" >/dev/null 2>&1 || true
    env RUSTFLAGS="--remap-path-prefix $HOME=~" cross build -p tuliprox --release --target "${ARCHITECTURE}"

    if [ ! -f "${BIN_PATH}" ]; then
      die "Expected binary not found: ${BIN_PATH}"
    fi

    # Create staging directory and copy binaries and config files
    STAGING_DIR="${TARGET_DIR}/${DIR}"
    rm -rf "${STAGING_DIR}"
    mkdir -p "${STAGING_DIR}"
    cp "${BIN_PATH}" "${STAGING_DIR}/"
    cp "${WORKING_DIR}/config/"*.yml "${STAGING_DIR}/"
    cp -rf "${FRONTEND_BUILD_DIR}" "${STAGING_DIR}/web"
    cp -rf "${RESOURCES_DIR}"/*.ts "${STAGING_DIR}/"

    # Create archive for the platform
    if [[ "${PLATFORM}" == "WINDOWS" ]]; then
        if ! command -v zip >/dev/null 2>&1; then
          die "'zip' is required to package Windows artifacts."
        fi
        ARC="${RELEASE_PKG}/${DIR}.zip"
        (cd "${TARGET_DIR}" && zip -r "${ARC}" "${DIR}") >/dev/null
    else
        ARC="${RELEASE_PKG}/${DIR}.tgz"
        tar -C "${TARGET_DIR}" -czf "${ARC}" "${DIR}"
    fi

    CHECKSUM_FILE="${RELEASE_PKG}/checksum_$(basename "${ARC}").txt"
    shasum -a 256 "${ARC}" > "${CHECKSUM_FILE}"
done

# Marker: wait for docker-build workflow completion
echo "⏳ Waiting for docker-build workflow to finish (run id: ${DOCKER_BUILD_RUN_ID})"
gh run watch "${DOCKER_BUILD_RUN_ID}" --exit-status

echo "🔀 Back-merging master into develop (fast-forward)"
git fetch --quiet origin develop || die "Failed to fetch 'origin/develop'."
if ! git merge-base --is-ancestor origin/develop HEAD; then
  die "Refusing to update develop: 'origin/develop' is not an ancestor of current master."
fi

gh_unfreeze_develop_branch

git push origin HEAD:develop
ORIGIN_DEVELOP_AFTER_FF_SHA="$(git rev-parse HEAD)"
log_sha "ORIGIN_DEVELOP_AFTER_FF_SHA (saved)" "${ORIGIN_DEVELOP_AFTER_FF_SHA}"
ORIGIN_DEVELOP_REMOTE_SHA="$(git ls-remote origin refs/heads/develop | awk '{print $1}' | head -n 1)"
log_sha "origin/develop (remote)" "${ORIGIN_DEVELOP_REMOTE_SHA}"
if git remote get-url github >/dev/null 2>&1; then
  git push github HEAD:develop
  GITHUB_DEVELOP_AFTER_FF_SHA="${ORIGIN_DEVELOP_AFTER_FF_SHA}"
  log_sha "GITHUB_DEVELOP_AFTER_FF_SHA (saved)" "${GITHUB_DEVELOP_AFTER_FF_SHA}"
  GITHUB_DEVELOP_REMOTE_SHA="$(git ls-remote github refs/heads/develop | awk '{print $1}' | head -n 1)"
  log_sha "github/develop (remote)" "${GITHUB_DEVELOP_REMOTE_SHA}"
fi

echo "🗑 Cleaning up build artifacts"
# Clean up the build directories
cd "$WORKING_DIR"
cargo clean

RELEASE_TAG="v${BUMP_VERSION#v}"
echo "🏷️ Tagging release: ${RELEASE_TAG}"

if git rev-parse -q --verify "refs/tags/${RELEASE_TAG}" >/dev/null; then
  die "Tag '${RELEASE_TAG}' already exists."
fi

RELEASE_PKG="$RELEASE_DIR/release_${BUMP_VERSION}"
if [ ! -d "${RELEASE_PKG}" ]; then
  die "Release package directory not found: ${RELEASE_PKG}"
fi

echo "📝 Extracting release notes from CHANGELOG.md (version: ${BUMP_VERSION})"
RELEASE_NOTES_FILE="${RELEASE_PKG}/release-notes-${RELEASE_TAG}.md"
if ! extract_release_notes_from_changelog "${BUMP_VERSION}" "${WORKING_DIR}/CHANGELOG.md" >"${RELEASE_NOTES_FILE}"; then
  die "No release notes found in CHANGELOG.md for version '${BUMP_VERSION}'."
fi

shopt -s nullglob
candidate_assets=("${RELEASE_PKG}"/*)
shopt -u nullglob
assets=()
for f in "${candidate_assets[@]}"; do
  if [ -f "${f}" ] && [ "${f}" != "${RELEASE_NOTES_FILE}" ]; then
    assets+=("${f}")
  fi
done
if [ "${#assets[@]}" -eq 0 ]; then
  die "No release assets found in ${RELEASE_PKG}."
fi

git tag -a "${RELEASE_TAG}" -m "${RELEASE_TAG}"

echo "📦 Pushing release tag: ${RELEASE_TAG}"
git push origin "${RELEASE_TAG}"
if git remote get-url github >/dev/null 2>&1; then
  git push github "${RELEASE_TAG}"
fi

echo "🚢 Creating GitHub release '${RELEASE_TAG}' (uploading ${#assets[@]} assets)"
if gh release view "${RELEASE_TAG}" >/dev/null 2>&1; then
  die "GitHub release '${RELEASE_TAG}' already exists."
fi
gh release create "${RELEASE_TAG}" --verify-tag -t "${RELEASE_TAG}" -F "${RELEASE_NOTES_FILE}" "${assets[@]}"

echo "🎉 Done!"
