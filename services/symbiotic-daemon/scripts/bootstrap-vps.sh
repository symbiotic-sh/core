#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="."
if ! command -v docker >/dev/null 2>&1; then
  echo "docker is required" >&2
  exit 1
fi
if ! command -v tailscale >/dev/null 2>&1; then
  echo "tailscale is required" >&2
  exit 1
fi

cd "$ROOT_DIR"
docker compose -f docker-compose.vps.yml pull || true
docker compose -f docker-compose.vps.yml up -d
echo "bootstrap complete"
