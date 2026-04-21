#!/usr/bin/env bash
# Demo 2 — Multi-agent tool evaluation.
#
# Sends ONE command to the orchestrator. The orchestrator reads the Archive,
# decides to dispatch 4 specialists (security-auditor, architecture-analyst,
# fit-analyst, risk-adversary) against 2 candidate frameworks (cua, trustgraph),
# collects all 8 analyses, synthesizes a decision memo, and writes it back to
# the Archive as a new entry. Zero wrapper glue — the orchestration happens
# inside the agent loop via the `dispatch_agent` tool.
#
# Usage:
#   ./scripts/demo-2.sh
#   ./scripts/demo-2.sh --goal "..."   # override the default question

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
repo_root="$(cd "$here/../.." && pwd)"
cli="$here/scripts/demo.sh"

goal='I am starting a new AI agent project. Evaluate two candidate frameworks already in my Archive: cua (arc_dd77a7311b9ac89d) and trustgraph (arc_17f3a6eb03a96017). For EACH candidate, dispatch all four specialists (security-auditor, architecture-analyst, fit-analyst, risk-adversary) with a focused goal that includes that candidate'"'"'s archive id. Then synthesize a decision memo: side-by-side comparison + go/no-go recommendation per candidate. Save it to workspace and store it in the Archive.'

if [[ "${1:-}" == "--goal" ]]; then
    shift
    goal="$1"
fi

echo "[demo-2] Archive before orchestration:"
awk -F'\t' '{ printf "  %s  %s\n", $1, $3 }' "$repo_root/data/archive/index.tsv"
echo
echo "[demo-2] Asking the orchestrator to evaluate cua vs trustgraph..."
echo "[demo-2] (This will spawn ~8 sub-agents sequentially — give it a few minutes.)"
echo
"$cli" agent run --role orchestrator --goal "$goal"
echo
echo
echo "[demo-2] Archive after orchestration (look for a new decision memo entry):"
awk -F'\t' '{ printf "  %s  %s\n", $1, $3 }' "$repo_root/data/archive/index.tsv"
