#!/usr/bin/env bash
# Smoke test: local Ollama + security-auditor agent + archive handoff.
#
# Verifies the end-to-end specialist flow we rely on in the multi-agent
# orchestration demo:
#   1. Specialist `recall`s a fixture archive entry
#   2. Archives its findings as a new entry
#   3. Emits a `done` wrapper with the arc_id handoff
#
# Runs in ~30-60s on a warm Ollama instance. Exits non-zero on any failure
# so it slots into CI once a runner has Ollama pre-installed.
#
# Usage:
#   ./scripts/test-agent-ollama-smoke.sh            # uses env defaults
#   OLLAMA_MODEL=gemma4:e4b ./scripts/test-agent-ollama-smoke.sh

set -euo pipefail

RUNTIME_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$RUNTIME_DIR"

# Honor .env.demo for Ollama URL / model defaults without forcing the
# caller to source it first.
if [[ -f .env.demo ]]; then
    # shellcheck disable=SC1091
    source .env.demo
fi

OLLAMA_URL="${SYMBIOTIC_OLLAMA_URL:-http://127.0.0.1:11434}"
OLLAMA_MODEL="${SYMBIOTIC_OLLAMA_CHAT_MODEL:-gemma4:e4b}"
CLI="$RUNTIME_DIR/target/debug/symbiotic"

# ── Preflight ────────────────────────────────────────────────────────

if [[ ! -x "$CLI" ]]; then
    echo "[test] CLI not built at $CLI — run: cargo build -p symbiotic-cli"
    exit 2
fi

if ! curl -sf "$OLLAMA_URL/" >/dev/null 2>&1; then
    echo "[test] Ollama unreachable at $OLLAMA_URL — skipping (start with: ollama serve)"
    exit 77  # autotools "skip" exit code; CI can treat as non-failure
fi

echo "[test] Ollama: $OLLAMA_URL"
echo "[test] Model:  $OLLAMA_MODEL"
echo "[test] CLI:    $CLI"

# ── Fixture archive ──────────────────────────────────────────────────

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

mkdir -p "$WORK/data/archive/records"
ARC_ID="arc_fixturecua00001"
cat > "$WORK/data/archive/records/$ARC_ID.md" <<'EOF'
# cua: macOS AI agents framework

A framework for building, benchmarking, and deploying agents that use
computers. Actions happen on macOS via a deep-OS-interaction harness.
The agent drives the system through simulated user input and reads
system state via accessibility APIs.

No sandboxing is documented. No source code visible in the README.
Authors publish release artifacts via public hosting with no signed
provenance.
EOF

# index.tsv columns: arc_id \t agent_id \t title \t subtitle \t tags \t visibility \t unix_ts
printf "%s\tagent-fixture\tcua: macOS AI agents framework\t\tagent-framework,fixture\tshareable\t1700000000\n" \
    "$ARC_ID" > "$WORK/data/archive/index.tsv"

echo "[test] fixture archive at $WORK/data/archive"

# ── Run the specialist ───────────────────────────────────────────────

LOG="$WORK/run.log"
cd "$WORK"

echo "[test] invoking security-auditor…"
set +e
"$CLI" agent run \
    --role security-auditor \
    --goal "Audit the artifact at archive id $ARC_ID. Recall that id first, then produce security findings using the report template in your system prompt." \
    2>&1 | tee "$LOG"
exit_code="${PIPESTATUS[0]}"
set -e

echo ""
echo "[test] CLI exit: $exit_code"

# ── Assertions ───────────────────────────────────────────────────────

fail() {
    echo "FAIL: $1"
    echo "--- log tail ---"
    tail -40 "$LOG"
    exit 1
}

[[ "$exit_code" -eq 0 ]] || fail "CLI exited non-zero ($exit_code)"

# Specialist must have archived findings — new row in index.tsv
new_rows=$(wc -l < "$WORK/data/archive/index.tsv")
[[ "$new_rows" -ge 2 ]] || fail "expected >=2 rows in index.tsv; got $new_rows"

# The new row's title should mention 'security audit' (case-insensitive)
grep -iE 'security audit' "$WORK/data/archive/index.tsv" | grep -v 'fixture' >/dev/null \
    || fail "no new archive entry with 'security audit' title"

# The done handoff should reference an arc_id that exists on disk
handoff_arc=$(grep -oE 'arc_[a-f0-9]+' "$LOG" | tail -1)
[[ -n "$handoff_arc" ]] || fail "no arc_id in log output"
[[ -f "$WORK/data/archive/records/$handoff_arc.md" ]] \
    || fail "handoff arc_id $handoff_arc not found on disk"

echo ""
echo "PASS: specialist recalled fixture, archived findings, and emitted handoff."
echo "      archived at $handoff_arc"
