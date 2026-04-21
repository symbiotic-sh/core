#!/usr/bin/env bash
# Start the mock login server for testing auth scripts.
# Usage: ./start.sh [port]
# Default port: 3847

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
AUTH_DIR="$(dirname "$SCRIPT_DIR")"
PORT="${1:-3847}"

cd "$AUTH_DIR"
exec npx ts-node "$SCRIPT_DIR/server.ts" "$PORT"
