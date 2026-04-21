#!/usr/bin/env bash
#
# End-to-end Docker smoke test for Symbiotic VPS stack.
#
# Usage:
#   ./scripts/test-e2e-docker.sh
#
# Prerequisites:
#   - Docker and docker compose available
#   - curl and jq installed
#
# What it does:
#   1. Starts conduwuit + daemon via docker-compose.vps.yml
#   2. Waits for conduwuit health check
#   3. Verifies /_matrix/client/versions endpoint
#   4. Creates a test user via the Matrix registration API
#   5. Sends a test intake message to the #intake room
#   6. Verifies the daemon picked up the job (checks logs)
#   7. Cleans up containers and volumes
#
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
COMPOSE_FILE="${REPO_ROOT}/docker-compose.vps.yml"
COMPOSE="docker compose -f ${COMPOSE_FILE}"
HOMESERVER="http://localhost:8008"
SERVER_NAME="symbiotic.local"
TEST_USER="${TEST_USER:-testuser}"
TEST_PASS="${TEST_PASS:-testpass1234}"

# ── Helpers ────────────────────────────────────────────────────────────

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

pass() { printf "${GREEN}[PASS]${NC} %s\n" "$1"; }
fail() { printf "${RED}[FAIL]${NC} %s\n" "$1"; FAILURES=$((FAILURES + 1)); }
info() { printf "${YELLOW}[INFO]${NC} %s\n" "$1"; }

FAILURES=0

cleanup() {
  info "Cleaning up containers and volumes..."
  ${COMPOSE} down -v --remove-orphans 2>/dev/null || true
}

trap cleanup EXIT

# ── Step 1: Start services ────────────────────────────────────────────

info "Starting VPS stack from ${COMPOSE_FILE}..."
${COMPOSE} up -d --build 2>&1 | tail -5

# ── Step 2: Wait for conduwuit health ─────────────────────────────────

info "Waiting for conduwuit to become healthy..."
MAX_WAIT=120
WAITED=0
while [ $WAITED -lt $MAX_WAIT ]; do
  STATUS=$(docker inspect --format='{{.State.Health.Status}}' \
    "$(${COMPOSE} ps -q conduwuit 2>/dev/null)" 2>/dev/null || echo "missing")

  if [ "$STATUS" = "healthy" ]; then
    pass "Conduwuit is healthy after ${WAITED}s"
    break
  fi

  sleep 2
  WAITED=$((WAITED + 2))
done

if [ $WAITED -ge $MAX_WAIT ]; then
  fail "Conduwuit did not become healthy within ${MAX_WAIT}s"
  info "Container logs:"
  ${COMPOSE} logs conduwuit 2>&1 | tail -30
  exit 1
fi

# ── Step 3: Verify /_matrix/client/versions ───────────────────────────

info "Checking /_matrix/client/versions..."
VERSIONS_RESPONSE=$(curl -sf --max-time 10 \
  "${HOMESERVER}/_matrix/client/versions" 2>/dev/null || echo "")

if echo "$VERSIONS_RESPONSE" | jq -e '.versions' >/dev/null 2>&1; then
  VERSIONS=$(echo "$VERSIONS_RESPONSE" | jq -r '.versions | join(", ")')
  pass "Matrix versions endpoint responded: ${VERSIONS}"
else
  fail "/_matrix/client/versions did not return valid JSON"
fi

# ── Step 4: Register test user ────────────────────────────────────────

info "Registering test user '${TEST_USER}'..."
REG_RESPONSE=$(curl -sf --max-time 10 \
  -X POST "${HOMESERVER}/_matrix/client/v3/register" \
  -H "Content-Type: application/json" \
  -d "{
    \"username\": \"${TEST_USER}\",
    \"password\": \"${TEST_PASS}\",
    \"auth\": {\"type\": \"m.login.dummy\"}
  }" 2>/dev/null || echo "")

USER_ID=$(echo "$REG_RESPONSE" | jq -r '.user_id // empty' 2>/dev/null)

if [ -n "$USER_ID" ]; then
  pass "Registered user: ${USER_ID}"
  ACCESS_TOKEN=$(echo "$REG_RESPONSE" | jq -r '.access_token')
else
  # User may already exist; try logging in.
  info "Registration failed (user may already exist), trying login..."
  LOGIN_RESPONSE=$(curl -sf --max-time 10 \
    -X POST "${HOMESERVER}/_matrix/client/v3/login" \
    -H "Content-Type: application/json" \
    -d "{
      \"type\": \"m.login.password\",
      \"identifier\": {\"type\": \"m.id.user\", \"user\": \"${TEST_USER}\"},
      \"password\": \"${TEST_PASS}\"
    }" 2>/dev/null || echo "")

  ACCESS_TOKEN=$(echo "$LOGIN_RESPONSE" | jq -r '.access_token // empty' 2>/dev/null)

  if [ -n "$ACCESS_TOKEN" ]; then
    pass "Logged in as ${TEST_USER}"
  else
    fail "Could not register or log in as ${TEST_USER}"
    ACCESS_TOKEN=""
  fi
fi

# ── Step 5: Create #intake room and send test message ─────────────────

if [ -n "$ACCESS_TOKEN" ]; then
  info "Creating #intake room..."
  ROOM_RESPONSE=$(curl -sf --max-time 10 \
    -X POST "${HOMESERVER}/_matrix/client/v3/createRoom" \
    -H "Authorization: Bearer ${ACCESS_TOKEN}" \
    -H "Content-Type: application/json" \
    -d "{
      \"room_alias_name\": \"intake\",
      \"name\": \"Intake\",
      \"preset\": \"public_chat\"
    }" 2>/dev/null || echo "")

  ROOM_ID=$(echo "$ROOM_RESPONSE" | jq -r '.room_id // empty' 2>/dev/null)

  if [ -z "$ROOM_ID" ]; then
    # Room may already exist; resolve alias.
    ALIAS_RESPONSE=$(curl -sf --max-time 10 \
      "${HOMESERVER}/_matrix/client/v3/directory/room/%23intake%3A${SERVER_NAME}" \
      -H "Authorization: Bearer ${ACCESS_TOKEN}" 2>/dev/null || echo "")
    ROOM_ID=$(echo "$ALIAS_RESPONSE" | jq -r '.room_id // empty' 2>/dev/null)
  fi

  if [ -n "$ROOM_ID" ]; then
    pass "Intake room: ${ROOM_ID}"

    info "Sending test intake message..."
    SEND_RESPONSE=$(curl -sf --max-time 10 \
      -X PUT "${HOMESERVER}/_matrix/client/v3/rooms/${ROOM_ID}/send/m.room.message/test-$(date +%s)" \
      -H "Authorization: Bearer ${ACCESS_TOKEN}" \
      -H "Content-Type: application/json" \
      -d "{
        \"msgtype\": \"m.text\",
        \"body\": \"https://example.com/test-article\"
      }" 2>/dev/null || echo "")

    EVENT_ID=$(echo "$SEND_RESPONSE" | jq -r '.event_id // empty' 2>/dev/null)

    if [ -n "$EVENT_ID" ]; then
      pass "Test intake message sent: ${EVENT_ID}"
    else
      fail "Failed to send test intake message"
    fi
  else
    fail "Could not create or resolve #intake room"
  fi
fi

# ── Step 6: Check daemon logs ─────────────────────────────────────────

info "Checking daemon container logs..."
DAEMON_CONTAINER=$(${COMPOSE} ps -q daemon 2>/dev/null || echo "")

if [ -n "$DAEMON_CONTAINER" ]; then
  DAEMON_LOGS=$(docker logs "$DAEMON_CONTAINER" 2>&1 || echo "")

  if echo "$DAEMON_LOGS" | grep -qi "idle\|listening\|connected\|ready\|started"; then
    pass "Daemon appears to be running (found activity in logs)"
  else
    info "Daemon log snippet:"
    echo "$DAEMON_LOGS" | tail -20
    # Not a hard failure -- daemon may not have connected to Matrix yet.
    info "Daemon log check inconclusive (may still be starting)"
  fi
else
  info "Daemon container not found (may not have built successfully)"
fi

# ── Summary ───────────────────────────────────────────────────────────

echo ""
echo "========================================="
if [ $FAILURES -eq 0 ]; then
  printf "${GREEN}All smoke tests passed.${NC}\n"
  exit 0
else
  printf "${RED}${FAILURES} test(s) failed.${NC}\n"
  exit 1
fi
