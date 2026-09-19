#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd -P)"
cd "${REPO_ROOT}"

TARGET_DIR="${CARGO_TARGET_DIR:-${REPO_ROOT}/target}"
SUT_BINARY="${TARGET_DIR}/debug/tuliprox"
TESTKIT_BINARY="${TARGET_DIR}/debug/tuliprox-testkit"
REPORT_DIR="${REPO_ROOT}/testkit-report"

if [[ ! -x "${SUT_BINARY}" ]]; then
  echo "❌ Tuliprox SUT binary not found at ${SUT_BINARY}. Build it first: cargo build -p tuliprox" >&2
  exit 1
fi

if [[ ! -x "${TESTKIT_BINARY}" ]]; then
  echo "❌ Testkit binary not found at ${TESTKIT_BINARY}. Build it first: cargo build -p tuliprox-testkit" >&2
  exit 1
fi

mkdir -p "${REPORT_DIR}"
export TULIPROX_TESTKIT_SUT_BINARY="${SUT_BINARY}"

SUITE_RUN_ID="${TULIPROX_TESTKIT_RUN_ID:-$(uuidgen 2>/dev/null || cat /proc/sys/kernel/random/uuid 2>/dev/null || echo "suite-$(date +%s)")}"

FAILED_SCENARIOS=()
PASSED_COUNT=0

echo "🚀 Starting testkit scenario suite (run_id: ${SUITE_RUN_ID})..."

for scenario_path in test/fixtures/testkit/scenarios/*.yml; do
  scenario_name="$(basename "${scenario_path}" .yml)"
  scenario_dir="${REPORT_DIR}/${scenario_name}"
  rm -rf "${scenario_dir}"
  mkdir -p "${scenario_dir}"
  echo "  ▶ Running scenario: ${scenario_name}"
  
  set +e
  "${TESTKIT_BINARY}" controller --scenario "${scenario_path}" --report-directory "${scenario_dir}" --run-id "${SUITE_RUN_ID}" > "${scenario_dir}/scenario.log" 2>&1
  exit_code=$?
  set -e

  if [[ ${exit_code} -eq 0 ]]; then
    echo "    ✅ ${scenario_name} PASSED"
    PASSED_COUNT=$((PASSED_COUNT + 1))
  else
    echo "    ❌ ${scenario_name} FAILED (exit code ${exit_code})"
    FAILED_SCENARIOS+=("${scenario_name}")
    if [[ ! -f "${scenario_dir}/run.json" ]]; then
      cat <<EOF > "${scenario_dir}/run.json"
{
  "schema_version": 1,
  "scenario": "${scenario_name}",
  "outcome": "failed",
  "policy_hash": "pre-start",
  "run_id": "${SUITE_RUN_ID}",
  "error_kind": "controller_crashed",
  "error_message": "Controller exited with code ${exit_code} before run.json could be generated",
  "exit_code": ${exit_code},
  "events": []
}
EOF
      cat <<EOF > "${scenario_dir}/summary.txt"
scenario: ${scenario_name}
outcome: failed
policy: pre-start
run_id: ${SUITE_RUN_ID}
events: 0
EOF
      touch "${scenario_dir}/events.ndjson"
      cat <<EOF > "${scenario_dir}/junit.xml"
<?xml version="1.0" encoding="UTF-8"?>
<testsuite name="tuliprox-testkit" tests="1" failures="1"><testcase name="${scenario_name}"><failure message="failed"/></testcase></testsuite>
EOF
    fi
    cat "${scenario_dir}/scenario.log" >&2
  fi
done

echo ""
echo "========================================"
echo "Testkit Scenario Summary:"
echo "  Run ID: ${SUITE_RUN_ID}"
echo "  Passed: ${PASSED_COUNT}"
echo "  Failed: ${#FAILED_SCENARIOS[@]}"
echo "========================================"

FAILED_JSON_ARRAY="[]"
if [[ ${#FAILED_SCENARIOS[@]} -gt 0 ]]; then
  FAILED_JSON_ARRAY="[$(printf '"%s",' "${FAILED_SCENARIOS[@]}" | sed 's/,$//')]"
fi

cat <<EOF > "${REPORT_DIR}/summary.json"
{
  "run_id": "${SUITE_RUN_ID}",
  "total": $((PASSED_COUNT + ${#FAILED_SCENARIOS[@]})),
  "passed": ${PASSED_COUNT},
  "failed": ${#FAILED_SCENARIOS[@]},
  "failing_scenarios": ${FAILED_JSON_ARRAY}
}
EOF

if [[ ${#FAILED_SCENARIOS[@]} -gt 0 ]]; then
  echo "❌ Failing scenarios: ${FAILED_SCENARIOS[*]}" >&2
  exit 1
fi

echo "✅ All scenarios passed successfully!"
