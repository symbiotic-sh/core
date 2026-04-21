#!/usr/bin/env bash
set -euo pipefail

PROFILE="${1:-all}" # local | vps | all
WORK_ROOT="${2:-/tmp/symbiotic-smoke-$(date +%s)}"
CONTROL_PLANE_URL="${SYMBIOTIC_CONTROL_PLANE_URL:-http://127.0.0.1:8080}"
CONTROL_PLANE_TOKEN="${SYMBIOTIC_CONTROL_PLANE_TOKEN:-}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
cd "${REPO_ROOT}"

json_field() {
  python3 - "$1" <<'PY'
import json
import sys
payload = sys.stdin.read().strip()
if not payload:
    print("")
    raise SystemExit(0)
try:
    data = json.loads(payload)
except Exception:
    print("")
    raise SystemExit(0)
value = data
for part in sys.argv[1].split('.'):
    if isinstance(value, dict):
        value = value.get(part)
    else:
        value = None
        break
if value is None:
    print("")
elif isinstance(value, bool):
    print('true' if value else 'false')
else:
    print(value)
PY
}

control_plane_headers=()
if [[ -n "${CONTROL_PLANE_TOKEN}" ]]; then
  control_plane_headers+=( -H "Authorization: Bearer ${CONTROL_PLANE_TOKEN}" )
fi
control_plane_headers+=( -H "x-user-id: smoke" )

control_plane_post() {
  local path="$1"
  local body="${2:-}"
  local id_key="${3:-smoke-$(date +%s)}"
  if [[ -n "${body}" ]]; then
    curl -fsS -X POST \
      "${control_plane_headers[@]}" \
      -H "idempotency-key: ${id_key}" \
      -H 'content-type: application/json' \
      -d "${body}" \
      "${CONTROL_PLANE_URL%/}${path}"
  else
    curl -fsS -X POST \
      "${control_plane_headers[@]}" \
      -H "idempotency-key: ${id_key}" \
      "${CONTROL_PLANE_URL%/}${path}"
  fi
}

control_plane_get() {
  local path="$1"
  curl -fsS \
    "${control_plane_headers[@]}" \
    "${CONTROL_PLANE_URL%/}${path}"
}

run_local() {
  echo "[local] workspace checks"
  cargo fmt --all --check
  cargo clippy --workspace --all-targets --all-features --offline -- -D warnings
  cargo test --workspace --offline

  echo "[local] install lifecycle worker tests"
  cargo test -p symbiotic-daemon install_provision_and_bootstrap_jobs_are_processed_by_worker
  cargo test -p symbiotic-daemon install_verify_job_is_processed_by_worker

  echo "[local] control-plane API flow (if available)"
  if curl -fsS "${CONTROL_PLANE_URL%/}/healthz" >/dev/null 2>&1; then
    local install_id="smoke-$(date +%s)"
    local create
    create="$(control_plane_post '/v1/installs' "{\"install_id\":\"${install_id}\",\"region\":\"fsn1\",\"provider\":\"hetzner\",\"matrix_domain\":\"matrix.symbiotic.sh\"}" "smoke-create-${install_id}")"
    echo "  create status=$(printf '%s' "${create}" | json_field status)"

    local provision
    provision="$(control_plane_post "/v1/installs/${install_id}/provision" "" "smoke-provision-${install_id}")"
    echo "  provision status=$(printf '%s' "${provision}" | json_field status)"

    local bootstrap
    bootstrap="$(control_plane_post "/v1/installs/${install_id}/bootstrap" "" "smoke-bootstrap-${install_id}")"
    echo "  bootstrap status=$(printf '%s' "${bootstrap}" | json_field status)"

    local verify
    verify="$(control_plane_post "/v1/installs/${install_id}/verify" "" "smoke-verify-${install_id}")"
    local final_status
    final_status="$(printf '%s' "${verify}" | json_field status)"
    echo "  verify status=${final_status}"
    if [[ "${final_status}" != "ready" ]]; then
      echo "FAIL: control-plane flow did not reach ready" >&2
      exit 1
    fi
  else
    echo "  SKIP: control-plane not reachable at ${CONTROL_PLANE_URL}"
  fi
}

run_vps() {
  local root="${WORK_ROOT}/vps"
  mkdir -p "${root}"

  echo "[vps] bootstrap script dry-run"
  ./scripts/bootstrap-vps.sh --access-mode public --public-domain matrix.example.com --dry-run

  local readiness_root="${REPO_ROOT}"
  if [[ -f "${readiness_root}/data/install/active-install-id" || -f "${readiness_root}/services/symbiotic-daemon/data/install/active-install-id" ]]; then
    echo "[vps] live readiness checks"
    ./scripts/live-readiness.sh --mode byok --root . --allow-no-push
  else
    echo "[vps] SKIP: no install state artifacts found; run install flow first"
  fi
}

case "${PROFILE}" in
  local)
    run_local
    ;;
  vps)
    run_vps
    ;;
  all)
    run_local
    run_vps
    ;;
  *)
    echo "usage: $0 [local|vps|all] [work_root]" >&2
    exit 2
    ;;
esac

echo "smoke_mvp_ok profile=${PROFILE} work_root=${WORK_ROOT}"
