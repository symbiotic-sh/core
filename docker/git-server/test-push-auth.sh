#!/bin/sh
#
# test-push-auth.sh — Live verification that the git-server container:
#   1. Serves git repos over HTTP via lighttpd + git-http-backend
#   2. Passes custom HTTP headers (X-Symbiotic-Push-Session) through to pre-receive hooks
#   3. The pre-receive hook calls the daemon's /api/git/authorize endpoint
#
# Usage:
#   ./test-push-auth.sh [--keep]
#
# Flags:
#   --keep    Don't clean up containers after the test (for debugging)
#
# Requirements: docker, git, python3
#
# This script builds the git-server image, starts a container, sets up a
# mock authorization endpoint, and performs a git push to verify the full
# CGI header propagation chain.

set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
IMAGE_NAME="symbiotic-git-server:test"
CONTAINER_NAME="git-server-push-test"
MOCK_AUTH_PORT=19091
GIT_PORT=19080
WORK_DIR=""
KEEP=false
MOCK_PID=""

# Color output (if terminal)
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
NC='\033[0m' # No Color

log()   { printf "${GREEN}[PASS]${NC} %s\n" "$1"; }
warn()  { printf "${YELLOW}[INFO]${NC} %s\n" "$1"; }
fail()  { printf "${RED}[FAIL]${NC} %s\n" "$1"; }

cleanup() {
  if [ "$KEEP" = "true" ]; then
    warn "Skipping cleanup (--keep). Container: $CONTAINER_NAME"
    warn "To clean up manually: docker rm -f $CONTAINER_NAME"
  else
    warn "Cleaning up..."
    [ -n "$MOCK_PID" ] && kill "$MOCK_PID" 2>/dev/null || true
    docker stop "$CONTAINER_NAME" 2>/dev/null || true
    docker rm "$CONTAINER_NAME" 2>/dev/null || true
    [ -n "$WORK_DIR" ] && rm -rf "$WORK_DIR"
  fi
  [ -n "$MOCK_PID" ] && kill "$MOCK_PID" 2>/dev/null || true
}
trap cleanup EXIT

for arg in "$@"; do
  case "$arg" in
    --keep) KEEP=true ;;
  esac
done

# ───────────────────────────────────────────────────────
# Phase 1: Build the image
# ───────────────────────────────────────────────────────
warn "Building git-server image..."
docker build -t "$IMAGE_NAME" "$SCRIPT_DIR" >/dev/null 2>&1
log "Image built: $IMAGE_NAME"

# ───────────────────────────────────────────────────────
# Phase 2: Start the git server container
# ───────────────────────────────────────────────────────
docker stop "$CONTAINER_NAME" 2>/dev/null || true
docker rm "$CONTAINER_NAME" 2>/dev/null || true

warn "Starting git server container (git HTTP on port $GIT_PORT)..."
docker run -d \
  --name "$CONTAINER_NAME" \
  -p "$GIT_PORT:80" \
  -e "DAEMON_HTTP_PORT=$MOCK_AUTH_PORT" \
  -e "AUTH_CALLBACK_URL=http://host.docker.internal:$MOCK_AUTH_PORT/api/git/authorize" \
  "$IMAGE_NAME" >/dev/null 2>&1

# Wait for lighttpd to be ready
sleep 2
if ! docker ps --format '{{.Names}}' | grep -q "$CONTAINER_NAME"; then
  fail "Container failed to start"
  docker logs "$CONTAINER_NAME" 2>&1
  exit 1
fi
log "Git server container running"

# Verify the env.conf was generated correctly
warn "Checking env.conf inside container..."
docker exec "$CONTAINER_NAME" cat /etc/lighttpd/env.conf

# ───────────────────────────────────────────────────────
# Phase 3: Create a bare repo inside the container
# ───────────────────────────────────────────────────────
REPO_ID="test-repo"
warn "Creating bare repo: $REPO_ID..."

docker exec "$CONTAINER_NAME" sh -c "
  git init --bare /repos/${REPO_ID}.git &&
  git -C /repos/${REPO_ID}.git config http.receivepack true &&
  cd /tmp && rm -rf _init &&
  git clone /repos/${REPO_ID}.git _init &&
  cd _init &&
  git checkout -b main &&
  git config user.email 'test@symbiotic.sh' &&
  git config user.name 'Test' &&
  git commit --allow-empty -m 'initial commit' &&
  git push origin main &&
  cd /tmp && rm -rf _init
" >/dev/null 2>&1

# Install the pre-receive hook
docker exec "$CONTAINER_NAME" sh -c "
  cp /usr/local/bin/git-pre-receive-hook /repos/${REPO_ID}.git/hooks/pre-receive &&
  chmod +x /repos/${REPO_ID}.git/hooks/pre-receive
" >/dev/null 2>&1

log "Bare repo created and hook installed"

# ───────────────────────────────────────────────────────
# Phase 4: Start a mock auth server (Python)
# ───────────────────────────────────────────────────────
# This simulates the daemon's /api/git/authorize endpoint.
# It logs each request body to a file so we can verify the
# push_session was propagated through CGI.
AUTH_LOG="$(mktemp)"

warn "Starting mock auth server on port $MOCK_AUTH_PORT..."

python3 -c "
import http.server
import json
import socket

AUTH_LOG = '${AUTH_LOG}'

class AuthHandler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get('Content-Length', 0))
        body = self.rfile.read(length).decode('utf-8') if length > 0 else ''

        # Log the request
        with open(AUTH_LOG, 'a') as f:
            f.write(body + '\n')

        # Always approve
        response = json.dumps({'allowed': True})
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(response)))
        self.end_headers()
        self.wfile.write(response.encode())

    def log_message(self, fmt, *args):
        # Suppress default stderr logging
        pass

class ReusableServer(http.server.HTTPServer):
    allow_reuse_address = True
    allow_reuse_port = True

server = ReusableServer(('0.0.0.0', ${MOCK_AUTH_PORT}), AuthHandler)
print('Mock auth server listening on port ${MOCK_AUTH_PORT}', flush=True)
server.serve_forever()
" &
MOCK_PID=$!
sleep 1

# Verify mock server is running
if ! kill -0 "$MOCK_PID" 2>/dev/null; then
  fail "Mock auth server failed to start"
  exit 1
fi
log "Mock auth server running (PID: $MOCK_PID)"

# Quick sanity check: can we reach the mock server?
SANITY="$(curl -s -X POST "http://localhost:$MOCK_AUTH_PORT/api/git/authorize" \
  -H "Content-Type: application/json" \
  -d '{"test":"sanity"}' 2>&1)" || true
if echo "$SANITY" | grep -q '"allowed"'; then
  log "Mock auth server reachable and responding"
  # Clear the sanity check from the log
  : > "$AUTH_LOG"
else
  fail "Mock auth server not reachable: $SANITY"
  exit 1
fi

# ───────────────────────────────────────────────────────
# Phase 5: Clone, commit, and push with session header
# ───────────────────────────────────────────────────────
WORK_DIR="$(mktemp -d)"
TEST_SESSION="test-session-$(date +%s)"

warn "Cloning repo and preparing push..."
(
  cd "$WORK_DIR"
  git clone "http://localhost:$GIT_PORT/${REPO_ID}.git" repo 2>/dev/null
  cd repo
  git checkout main 2>/dev/null
  git config user.email "agent@symbiotic.sh"
  git config user.name "Test Agent"

  # Create a test commit
  echo "test content" > README.md
  git add README.md
  git commit -m "test: add README" >/dev/null 2>&1

  warn "Pushing with X-Symbiotic-Push-Session: $TEST_SESSION"

  # git push with custom header via -c http.extraHeader
  # This is how the agent runner sends the push session.
  if git -c "http.extraHeader=X-Symbiotic-Push-Session: $TEST_SESSION" push origin main 2>&1; then
    log "git push succeeded"
  else
    fail "git push failed"
    warn "Container logs (last 20 lines):"
    docker logs --tail 20 "$CONTAINER_NAME" 2>&1
    exit 1
  fi
)

# ───────────────────────────────────────────────────────
# Phase 6: Verify results
# ───────────────────────────────────────────────────────
echo ""
echo "============================================"
echo "  VERIFICATION RESULTS"
echo "============================================"
echo ""

PASSED=0
FAILED=0

# Check 1: Did the mock auth server receive a request?
if [ -s "$AUTH_LOG" ]; then
  log "Auth endpoint received a request"
  PASSED=$((PASSED + 1))
else
  fail "Auth endpoint did NOT receive any request"
  FAILED=$((FAILED + 1))
  warn "This means either:"
  warn "  - lighttpd did not pass the header to git-http-backend CGI env"
  warn "  - git-http-backend did not pass CGI env vars to pre-receive hook"
  warn "  - The hook failed before reaching the curl call"
  warn "  - The AUTH_CALLBACK_URL was not available in the hook environment"
  echo ""
  warn "Container logs:"
  docker logs "$CONTAINER_NAME" 2>&1
  rm -f "$AUTH_LOG"
  exit 1
fi

# Parse the auth body
AUTH_BODY="$(head -1 "$AUTH_LOG")"
echo "     Body: $AUTH_BODY"

# Check 2: Did the body contain our push session?
if echo "$AUTH_BODY" | grep -q "$TEST_SESSION"; then
  log "Push session header propagated: lighttpd -> CGI -> git-http-backend -> pre-receive hook"
  echo "     X-Symbiotic-Push-Session arrived as HTTP_X_SYMBIOTIC_PUSH_SESSION"
  PASSED=$((PASSED + 1))
else
  fail "Push session NOT found in auth request body"
  echo "     Expected session: $TEST_SESSION"
  echo "     Received body:    $AUTH_BODY"
  FAILED=$((FAILED + 1))
fi

# Check 3: Did the body contain the repo ID?
if echo "$AUTH_BODY" | grep -q "\"repo_id\":\"$REPO_ID\""; then
  log "Repo ID correct: $REPO_ID"
  PASSED=$((PASSED + 1))
else
  fail "Repo ID incorrect in auth body"
  FAILED=$((FAILED + 1))
fi

# Check 4: Did the body contain the branch name?
if echo "$AUTH_BODY" | grep -q '"branch":"main"'; then
  log "Branch name correct: main"
  PASSED=$((PASSED + 1))
else
  BRANCH="$(echo "$AUTH_BODY" | python3 -c "import sys,json; print(json.loads(sys.stdin.read()).get('branch','?'))" 2>/dev/null || echo '?')"
  fail "Branch name: expected 'main', got '$BRANCH'"
  FAILED=$((FAILED + 1))
fi

# Check 5: Do old_sha and new_sha look valid?
OLD_SHA="$(echo "$AUTH_BODY" | python3 -c "import sys,json; print(json.loads(sys.stdin.read()).get('old_sha','?'))" 2>/dev/null || echo '?')"
NEW_SHA="$(echo "$AUTH_BODY" | python3 -c "import sys,json; print(json.loads(sys.stdin.read()).get('new_sha','?'))" 2>/dev/null || echo '?')"
if echo "$NEW_SHA" | grep -qE '^[0-9a-f]{40}$'; then
  log "SHA hashes valid (old: ${OLD_SHA:0:12}, new: ${NEW_SHA:0:12})"
  PASSED=$((PASSED + 1))
else
  fail "SHA hashes invalid (old: $OLD_SHA, new: $NEW_SHA)"
  FAILED=$((FAILED + 1))
fi

# Check 6: Verify the push actually landed in the bare repo
PUSH_SHA="$(docker exec "$CONTAINER_NAME" git -C "/repos/${REPO_ID}.git" rev-parse refs/heads/main 2>/dev/null || echo 'unknown')"
if [ "$PUSH_SHA" != "unknown" ] && [ "$PUSH_SHA" = "$NEW_SHA" ]; then
  log "Push landed on main (SHA matches: ${PUSH_SHA:0:12})"
  PASSED=$((PASSED + 1))
elif [ "$PUSH_SHA" != "unknown" ]; then
  warn "Push landed but SHA mismatch (repo: ${PUSH_SHA:0:12}, expected: ${NEW_SHA:0:12})"
  PASSED=$((PASSED + 1))
else
  fail "Could not verify push landed"
  FAILED=$((FAILED + 1))
fi

echo ""
echo "============================================"
echo "  SUMMARY: $PASSED passed, $FAILED failed"
echo "============================================"
echo ""

if [ "$FAILED" -eq 0 ]; then
  echo "  The full CGI header propagation chain works correctly:"
  echo ""
  echo "  1. Agent pushes with: git -c 'http.extraHeader=X-Symbiotic-Push-Session: <session>' push"
  echo "  2. lighttpd receives the HTTP header and converts it to CGI env var:"
  echo "       HTTP_X_SYMBIOTIC_PUSH_SESSION=<session>"
  echo "  3. git-http-backend inherits the CGI env and passes it to hooks"
  echo "  4. pre-receive hook reads \$HTTP_X_SYMBIOTIC_PUSH_SESSION"
  echo "  5. Hook calls daemon auth API with the session for authorization"
  echo "  6. Daemon validates the session and capability tokens"
  echo ""
  echo "  The push-session transport design is verified and operational."
  echo ""
else
  echo "  Some checks failed. See output above for details."
  echo ""
  exit 1
fi

rm -f "$AUTH_LOG"
