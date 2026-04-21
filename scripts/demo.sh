#!/usr/bin/env bash
# Wrapper for the hackathon demo: sources `.env.demo` (Ollama provider, gemma4:e4b)
# and forwards every argument to the `symbiotic` CLI binary built under
# `target/debug/`. Use this so every command looks identical to the approval
# system: `./scripts/demo.sh <cmd> <args...>`.

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck disable=SC1091
source "$here/.env.demo"

exec "$here/target/debug/symbiotic" "$@"
