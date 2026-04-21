#!/bin/sh
set -eu

# Resolve configuration from container environment
REPOS_PATH="${REPOS_PATH:-/repos}"
DAEMON_HTTP_PORT="${DAEMON_HTTP_PORT:-8090}"
AUTH_CALLBACK_URL="${AUTH_CALLBACK_URL:-http://host.docker.internal:${DAEMON_HTTP_PORT}/api/git/authorize}"

# Ensure repos directory exists and is writable
mkdir -p "$REPOS_PATH"
chmod 755 "$REPOS_PATH"

# Configure git to trust the repos directory (prevents "dubious ownership" errors)
git config --global --add safe.directory '*'

# Generate lighttpd env config from container environment variables.
# lighttpd's setenv.add-environment provides the CGI environment for
# git-http-backend, which then passes it through to hooks. Container-level
# env vars are NOT automatically inherited by CGI processes, so we must
# inject them explicitly via this generated include file.
cat > /etc/lighttpd/env.conf <<ENVBLOCK
setenv.add-environment = (
  "GIT_PROJECT_ROOT" => "${REPOS_PATH}",
  "GIT_HTTP_EXPORT_ALL" => "1",
  "AUTH_CALLBACK_URL" => "${AUTH_CALLBACK_URL}",
  "DAEMON_HTTP_PORT" => "${DAEMON_HTTP_PORT}",
  "REPOS_PATH" => "${REPOS_PATH}"
)
ENVBLOCK

# Log environment for debugging
echo "=== Symbiotic Git Server ==="
echo "  REPOS_PATH:       $REPOS_PATH"
echo "  DAEMON_HTTP_PORT: $DAEMON_HTTP_PORT"
echo "  AUTH_CALLBACK_URL: $AUTH_CALLBACK_URL"
echo "  git-http-backend: $(which git-http-backend 2>/dev/null || echo '/usr/libexec/git-core/git-http-backend')"
echo "==========================="

# Start lighttpd in foreground (-D = no daemon mode)
exec lighttpd -D -f /etc/lighttpd/lighttpd.conf
