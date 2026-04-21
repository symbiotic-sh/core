#!/usr/bin/env bash
set -euo pipefail

MODE="byok"           # byok | managed
ROOT="."
ALLOW_NO_PUSH=false
SKIP_PROBES=false      # kept for backward compatibility

usage() {
  cat <<'USAGE'
Usage: scripts/live-readiness.sh [options]

Options:
  --mode <byok|managed>   Install mode to validate (default: byok)
  --root <path>           Runtime root path (default: .)
  --allow-no-push         Do not fail if push gateway credentials are missing
  --skip-probes           Backward-compat flag (control-plane probe is skipped)
  -h, --help              Show this help
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --mode)
      MODE="${2:-}"
      shift 2
      ;;
    --root)
      ROOT="${2:-}"
      shift 2
      ;;
    --allow-no-push)
      ALLOW_NO_PUSH=true
      shift
      ;;
    --skip-probes)
      SKIP_PROBES=true
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [[ "${MODE}" != "byok" && "${MODE}" != "managed" ]]; then
  echo "invalid --mode: ${MODE} (expected byok|managed)" >&2
  exit 2
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
if [[ "${ROOT}" = /* ]]; then
  ROOT_ABS="${ROOT}"
else
  ROOT_ABS="${REPO_ROOT}/${ROOT#./}"
fi

if [[ ! -d "${ROOT_ABS}" ]]; then
  echo "root directory not found: ${ROOT_ABS}" >&2
  exit 2
fi
ROOT_ABS="$(cd "${ROOT_ABS}" && pwd)"

cd "${REPO_ROOT}"

pass() { echo "PASS: $*"; }
fail() { echo "FAIL: $*"; FAILED=1; }
warn() { echo "WARN: $*"; }

FAILED=0

load_env_file() {
  local file="$1"
  if [[ -f "${file}" ]]; then
    set -a
    # shellcheck disable=SC1090
    source "${file}"
    set +a
  fi
}

json_field() {
  local field="$1"
  python3 - "$field" <<'PY'
import json
import sys
payload = sys.stdin.read().strip()
field = sys.argv[1]
if not payload:
    print("")
    raise SystemExit(0)
try:
    data = json.loads(payload)
except Exception:
    print("")
    raise SystemExit(0)
value = data
for part in field.split('.'):
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

control_plane_get_install() {
  local install_id="$1"
  if [[ -z "${SYMBIOTIC_CONTROL_PLANE_URL:-}" ]]; then
    return 1
  fi

  local url="${SYMBIOTIC_CONTROL_PLANE_URL%/}/v1/installs/${install_id}"
  local headers=( -H "x-user-id: readiness-check" )
  if [[ -n "${SYMBIOTIC_CONTROL_PLANE_TOKEN:-}" ]]; then
    headers+=( -H "Authorization: Bearer ${SYMBIOTIC_CONTROL_PLANE_TOKEN}" )
  fi

  curl -fsS "${headers[@]}" "${url}" 2>/dev/null
}

echo "== Symbiotic Live Readiness =="
echo "mode=${MODE} root=${ROOT_ABS}"

load_env_file "${ROOT_ABS}/config/.env.runtime"
load_env_file "${ROOT_ABS}/config/.env.secrets"
load_env_file "${ROOT_ABS}/config/.env.rooms"

echo "--- Gate 1: Runtime baseline config ---"
if [[ -n "${SYMBIOTIC_MATRIX_HOMESERVER:-}" && -n "${SYMBIOTIC_MATRIX_USER:-}" ]]; then
  pass "matrix homeserver + user configured"
else
  fail "missing SYMBIOTIC_MATRIX_HOMESERVER or SYMBIOTIC_MATRIX_USER"
fi

if [[ -n "${SYMBIOTIC_MATRIX_PASSWORD:-}" || -f "${ROOT_ABS}/config/.secrets/matrix_password" ]]; then
  pass "matrix auth material present (env or docker secret file)"
else
  warn "matrix password not found in env or config/.secrets/matrix_password"
fi

echo "--- Gate 2: Install lifecycle state ---"
ACTIVE_INSTALL_FILE="${ROOT_ABS}/data/install/active-install-id"
if [[ ! -f "${ACTIVE_INSTALL_FILE}" && -f "${ROOT_ABS}/services/symbiotic-daemon/data/install/active-install-id" ]]; then
  ACTIVE_INSTALL_FILE="${ROOT_ABS}/services/symbiotic-daemon/data/install/active-install-id"
fi

if [[ ! -f "${ACTIVE_INSTALL_FILE}" ]]; then
  fail "missing active install marker (${ROOT_ABS}/data/install/active-install-id)"
else
  INSTALL_ID="$(tr -d '\n\r' < "${ACTIVE_INSTALL_FILE}")"
  if [[ -z "${INSTALL_ID}" ]]; then
    fail "active install marker is empty"
  else
    pass "active install marker present (${INSTALL_ID})"
    if [[ "${SKIP_PROBES}" == "true" ]]; then
      warn "control-plane probe skipped (--skip-probes)"
    else
      if cp_payload="$(control_plane_get_install "${INSTALL_ID}")"; then
        cp_status="$(printf '%s' "${cp_payload}" | json_field status)"
        if [[ "${cp_status}" == "ready" ]]; then
          pass "control-plane install status is ready"
        elif [[ -n "${cp_status}" ]]; then
          fail "control-plane install status is ${cp_status} (expected ready)"
        else
          fail "control-plane response missing status"
        fi
      else
        warn "control-plane install probe unavailable (set SYMBIOTIC_CONTROL_PLANE_URL to enforce)"
      fi
    fi
  fi
fi

echo "--- Gate 3: Matrix room mapping present ---"
required_rooms=(
  "SYMBIOTIC_MATRIX_ROOM_CONTROL"
  "SYMBIOTIC_MATRIX_ROOM_INTAKE"
  "SYMBIOTIC_MATRIX_ROOM_ALERTS"
  "SYMBIOTIC_MATRIX_ROOM_STATUS"
)
missing_rooms=()
for key in "${required_rooms[@]}"; do
  if [[ -z "${!key:-}" ]]; then
    missing_rooms+=("${key}")
  fi
done
if [[ ${#missing_rooms[@]} -gt 0 ]]; then
  fail "missing room IDs: ${missing_rooms[*]} (expected in config/.env.rooms)"
else
  pass "room mappings present"
fi

echo "--- Gate 4: Push provider credentials ---"
unified=false
split=false
if [[ -n "${SYMBIOTIC_PUSH_GATEWAY_URL:-}" && -n "${SYMBIOTIC_PUSH_GATEWAY_API_KEY:-}" ]]; then
  unified=true
fi
if [[ -n "${SYMBIOTIC_PUSH_APNS_GATEWAY_URL:-}" && -n "${SYMBIOTIC_PUSH_APNS_GATEWAY_API_KEY:-}" \
   && -n "${SYMBIOTIC_PUSH_FCM_GATEWAY_URL:-}" && -n "${SYMBIOTIC_PUSH_FCM_GATEWAY_API_KEY:-}" ]]; then
  split=true
fi
if [[ "${unified}" == "true" || "${split}" == "true" ]]; then
  pass "push gateway credentials configured"
elif "${ALLOW_NO_PUSH}"; then
  warn "push credentials missing (allowed by --allow-no-push)"
else
  fail "push credentials missing (set unified gateway or APNS+FCM gateways)"
fi

echo "--- Gate 5: Sender authorization policy ---"
if [[ -z "${SYMBIOTIC_ALLOWED_SENDERS:-}" ]]; then
  fail "SYMBIOTIC_ALLOWED_SENDERS is empty"
else
  pass "SYMBIOTIC_ALLOWED_SENDERS configured"
fi
if [[ "${SYMBIOTIC_ALLOW_OPEN_ACCESS:-false}" == "true" ]]; then
  fail "SYMBIOTIC_ALLOW_OPEN_ACCESS=true (must be false for VPS live run)"
else
  pass "SYMBIOTIC_ALLOW_OPEN_ACCESS is disabled"
fi

echo "--- Gate 6: Provider execution readiness ---"
if [[ "${MODE}" == "byok" ]]; then
  if [[ -n "${ANTHROPIC_API_KEY:-}" && -n "${OPENAI_API_KEY:-}" ]]; then
    pass "BYOK keys configured (ANTHROPIC_API_KEY + OPENAI_API_KEY)"
  else
    fail "missing BYOK keys (need ANTHROPIC_API_KEY and OPENAI_API_KEY)"
  fi
else
  if [[ -n "${SYMBIOTIC_METERED_PLAN_ID:-}" ]]; then
    pass "managed plan configured"
  else
    fail "managed mode requires SYMBIOTIC_METERED_PLAN_ID"
  fi
fi

echo
if [[ "${FAILED}" -eq 0 ]]; then
  echo "LIVE_READINESS=PASS"
  exit 0
fi
echo "LIVE_READINESS=FAIL"
exit 2
