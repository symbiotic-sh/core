#!/usr/bin/env bash
# One-shot hackathon demo:
#   1. (optional) reseed the Archive with three real articles
#   2. run the researcher agent synthesis
#   3. print the result
#
# Usage:
#   ./scripts/demo-all.sh                # synth against whatever's in Archive
#   ./scripts/demo-all.sh --seed         # wipe + reseed + synth
#   ./scripts/demo-all.sh --goal "..."   # override the default goal

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cli="$here/scripts/demo.sh"

goal="I have notes about AI agents in my Archive. Use the recall tool to retrieve them all (try 'agent', 'agents', 'AI agents' if needed), then synthesize: summarize each source's main technique or argument, highlight where they agree about what makes agents work, and flag any places they disagree about approach. Cite by archive entry id."

seed=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --seed) seed=1 ;;
        --goal) shift; goal="$1" ;;
        *) echo "unknown flag: $1" >&2; exit 1 ;;
    esac
    shift
done

if (( seed == 1 )); then
    "$here/scripts/demo-seed.sh"
fi

echo "[demo] Archive contents:"
awk -F'\t' '{ printf "  %s  %s\n", $1, $3 }' "$(cd "$here/../.." && pwd)/data/archive/index.tsv"
echo
echo "[demo] asking the researcher..."
echo
"$cli" agent run --role researcher --goal "$goal"
