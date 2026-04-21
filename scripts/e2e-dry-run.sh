#!/usr/bin/env bash
# e2e-dry-run.sh
#
# Validates the full critical path (app-setup -> artifact)
# WITHOUT requiring a live VPS or real credentials.
#
# Checks: schema, daemon build/tests, provider CLIs,
#          secret ingestion, Flutter gate, artifact path.
#
# Exit 0 = all pass (or pass + warn only)
# Exit 1 = any fail

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
cd "${REPO_ROOT}"

# --- Counters (no associative arrays; bash 3.2 compatible) ---
PASS_COUNT=0
FAIL_COUNT=0
WARN_COUNT=0

pass() {
  echo "[PASS] $*"
  PASS_COUNT=$((PASS_COUNT + 1))
}

fail() {
  echo "[FAIL] $*"
  FAIL_COUNT=$((FAIL_COUNT + 1))
}

warn() {
  echo "[WARN] $*"
  WARN_COUNT=$((WARN_COUNT + 1))
}

echo "== Symbiotic E2E Dry Run =="
echo "repo_root=${REPO_ROOT}"
echo

# ---------------------------------------------------------------
# 1. Schema validation
# ---------------------------------------------------------------
echo "--- Step 1: Schema validation ---"
SCHEMA_FILE="${REPO_ROOT}/schemas/control-command.json"
if [ ! -f "${SCHEMA_FILE}" ]; then
  fail "Schema validation: control-command.json not found"
else
  schema_ok=true
  # Validate JSON parses and contains required commands
  schema_result="$(python3 - "${SCHEMA_FILE}" <<'PY'
import json, sys

path = sys.argv[1]
try:
    data = json.loads(open(path).read())
except Exception as e:
    print("PARSE_ERROR:" + str(e))
    sys.exit(0)

# Check required commands
required = [
    "install.secret.put",
    "install.secret.validate",
    "goal.start",
    "goal.stop",
    "goal.list",
]
commands_enum = []
try:
    commands_enum = data["properties"]["command"]["enum"]
except (KeyError, TypeError):
    print("MISSING_ENUM")
    sys.exit(0)

missing = [c for c in required if c not in commands_enum]
if missing:
    print("MISSING_COMMANDS:" + ",".join(missing))
else:
    print("OK:" + str(len(commands_enum)) + " commands")
PY
  )" || schema_ok=false

  if [ "${schema_ok}" = false ]; then
    fail "Schema validation: python3 execution failed"
  else
    case "${schema_result}" in
      PARSE_ERROR:*)
        fail "Schema validation: invalid JSON (${schema_result#PARSE_ERROR:})"
        ;;
      MISSING_ENUM)
        fail "Schema validation: command enum not found in schema"
        ;;
      MISSING_COMMANDS:*)
        fail "Schema validation: missing commands: ${schema_result#MISSING_COMMANDS:}"
        ;;
      OK:*)
        pass "Schema validation (${schema_result#OK:})"
        ;;
      *)
        fail "Schema validation: unexpected result: ${schema_result}"
        ;;
    esac
  fi
fi

# ---------------------------------------------------------------
# 2. Daemon binary check
# ---------------------------------------------------------------
echo "--- Step 2: Daemon binary check ---"
if cargo build -p symbiotic-daemon --offline 2>&1; then
  pass "Daemon binary builds"
else
  fail "Daemon binary build failed"
fi

# ---------------------------------------------------------------
# 3. Unit test gate
# ---------------------------------------------------------------
echo "--- Step 3: Daemon unit tests ---"
if cargo test -p symbiotic-daemon --offline 2>&1; then
  pass "Daemon unit tests"
else
  fail "Daemon unit tests failed"
fi

# ---------------------------------------------------------------
# 4. Provider readiness
# ---------------------------------------------------------------
echo "--- Step 4: Provider readiness ---"
claude_found=false
codex_found=false
if command -v claude >/dev/null 2>&1; then
  claude_found=true
fi
if command -v codex >/dev/null 2>&1; then
  codex_found=true
fi

if [ "${claude_found}" = true ] && [ "${codex_found}" = true ]; then
  pass "Provider CLI: claude and codex available"
elif [ "${claude_found}" = true ]; then
  warn "Provider CLI: codex not found (claude available)"
elif [ "${codex_found}" = true ]; then
  warn "Provider CLI: claude not found (codex available)"
else
  warn "Provider CLI: neither claude nor codex found"
fi

# ---------------------------------------------------------------
# 5. Secret ingestion dry-run
# ---------------------------------------------------------------
echo "--- Step 5: Secret ingestion dry-run ---"
SECRETS_FILE="${REPO_ROOT}/services/symbiotic-daemon/src/secrets.rs"
if [ ! -f "${SECRETS_FILE}" ]; then
  fail "Secret ingestion: secrets.rs not found"
else
  # Run tests that cover secrets module (key validation, permissions, redaction)
  if cargo test -p symbiotic-daemon --offline -- secrets 2>&1; then
    pass "Secret ingestion tests"
  else
    fail "Secret ingestion tests failed"
  fi
fi

# ---------------------------------------------------------------
# 6. Flutter gate
# ---------------------------------------------------------------
echo "--- Step 6: Flutter gate ---"
resolve_flutter_dir() {
  if [ -n "${SYMBIOTIC_APP_REPO:-}" ] && [ -f "${SYMBIOTIC_APP_REPO}/pubspec.yaml" ]; then
    echo "${SYMBIOTIC_APP_REPO}"
    return 0
  fi

  for candidate in \
    "${REPO_ROOT}/../symbiotic-app" \
    "${REPO_ROOT}/../app" \
    "${REPO_ROOT}/../../submodules/app"
  do
    if [ -f "${candidate}/pubspec.yaml" ]; then
      echo "${candidate}"
      return 0
    fi
  done

  return 1
}

FLUTTER_DIR=""
if FLUTTER_DIR="$(resolve_flutter_dir)"; then
  :
else
  FLUTTER_DIR=""
fi

if ! command -v flutter >/dev/null 2>&1; then
  warn "Flutter: flutter CLI not found in PATH (skipping)"
elif [ -z "${FLUTTER_DIR}" ]; then
  warn "Flutter: app repo not found (set SYMBIOTIC_APP_REPO to app repo root)"
else
  flutter_fail=false

  echo "  Running flutter analyze..."
  if (cd "${FLUTTER_DIR}" && flutter analyze) 2>&1; then
    pass "Flutter analyze"
  else
    fail "Flutter analyze"
    flutter_fail=true
  fi

  echo "  Running flutter test..."
  if (cd "${FLUTTER_DIR}" && flutter test) 2>&1; then
    pass "Flutter tests"
  else
    fail "Flutter tests"
    flutter_fail=true
  fi
fi

# ---------------------------------------------------------------
# 7. Artifact path check
# ---------------------------------------------------------------
echo "--- Step 7: Artifact path check ---"
GOALS_DIR="${REPO_ROOT}/data/goals"
if [ -d "${GOALS_DIR}" ]; then
  if [ -w "${GOALS_DIR}" ]; then
    pass "Artifact path: data/goals/ exists and is writable"
  else
    fail "Artifact path: data/goals/ exists but is NOT writable"
  fi
else
  # Try to create it
  if mkdir -p "${GOALS_DIR}" 2>/dev/null; then
    if [ -w "${GOALS_DIR}" ]; then
      pass "Artifact path: data/goals/ created and writable"
    else
      fail "Artifact path: data/goals/ created but NOT writable"
    fi
  else
    fail "Artifact path: cannot create data/goals/"
  fi
fi

# ---------------------------------------------------------------
# 8. Summary
# ---------------------------------------------------------------
echo
echo "== Summary =="
echo "  PASS: ${PASS_COUNT}"
echo "  FAIL: ${FAIL_COUNT}"
echo "  WARN: ${WARN_COUNT}"
echo

if [ "${FAIL_COUNT}" -gt 0 ]; then
  echo "E2E_DRY_RUN=FAIL"
  exit 1
fi

if [ "${WARN_COUNT}" -gt 0 ]; then
  echo "E2E_DRY_RUN=PASS (with warnings)"
else
  echo "E2E_DRY_RUN=PASS"
fi
exit 0
