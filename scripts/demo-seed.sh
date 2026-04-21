#!/usr/bin/env bash
# Seed the Archive with three real articles for the hackathon researcher demo.
# Fetches each URL, strips HTML where needed, and intakes via `symbiotic intake
# --note` with an explicit `--title` and topic tags.
#
# Usage:
#   ./scripts/demo-seed.sh                # wipe + reseed the Archive
#   ./scripts/demo-seed.sh --keep         # add entries without wiping existing ones

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
repo_root="$(cd "$here/../.." && pwd)"
cli="$here/scripts/demo.sh"
tmp="/tmp/symbiotic-demo-seed"
mkdir -p "$tmp"

wipe=1
if [[ "${1:-}" == "--keep" ]]; then
    wipe=0
fi

if (( wipe == 1 )); then
    echo "[seed] wiping existing Archive entries..."
    rm -f "$repo_root/data/archive/records/arc_"* 2>/dev/null || true
    : > "$repo_root/data/archive/index.tsv"
fi

# Helper: fetch a URL to a local file, with browser UA.
fetch() {
    local url="$1" dest="$2"
    curl -sL -A "Mozilla/5.0 (SymbioticSeed)" "$url" -o "$dest"
}

# Helper: strip HTML to plain text, truncate to N chars.
strip_html() {
    local src="$1" dst="$2" max="${3:-6000}"
    python3 - "$src" "$dst" "$max" <<'PY'
import html as html_lib, re, sys
src, dst, max_chars = sys.argv[1], sys.argv[2], int(sys.argv[3])
with open(src, "r", encoding="utf-8", errors="ignore") as f:
    text = f.read()
text = re.sub(r"<script.*?</script>", " ", text, flags=re.S | re.I)
text = re.sub(r"<style.*?</style>", " ", text, flags=re.S | re.I)
text = re.sub(r"<[^>]+>", " ", text)
text = html_lib.unescape(text)
text = re.sub(r"\s+", " ", text).strip()
with open(dst, "w", encoding="utf-8") as f:
    f.write(text[:max_chars])
PY
}

echo "[seed] fetching article 1/3: trycua/cua"
fetch "https://raw.githubusercontent.com/trycua/cua/main/README.md" "$tmp/cua.md"
head -c 4000 "$tmp/cua.md" > "$tmp/cua.short.md"

echo "[seed] fetching article 2/3: trustgraph-ai/trustgraph"
fetch "https://raw.githubusercontent.com/trustgraph-ai/trustgraph/master/README.md" "$tmp/trustgraph.md"
head -c 4000 "$tmp/trustgraph.md" > "$tmp/trustgraph.short.md"

echo "[seed] fetching article 3/3: ghuntley/ralph"
fetch "https://ghuntley.com/ralph/" "$tmp/ralph.html"
strip_html "$tmp/ralph.html" "$tmp/ralph.txt" 6000

echo "[seed] intaking cua..."
"$cli" intake \
    --title "cua: macOS AI agents framework" \
    --tags "ai-agents,macos,tooling" \
    --note "$(cat "$tmp/cua.short.md")" \
    > /dev/null

echo "[seed] intaking trustgraph..."
"$cli" intake \
    --title "TrustGraph: modular AI agent data platform" \
    --tags "ai-agents,graph,data" \
    --note "$(cat "$tmp/trustgraph.short.md")" \
    > /dev/null

echo "[seed] intaking ralph..."
"$cli" intake \
    --title "Ralph Wiggum as a software engineer (ghuntley)" \
    --tags "ai-agents,context-engineering,coding" \
    --note "$(cat "$tmp/ralph.txt")" \
    > /dev/null

echo
echo "[seed] Archive now contains:"
awk -F'\t' '{ printf "  %s  %s  [%s]\n", $1, $3, $5 }' "$repo_root/data/archive/index.tsv"
echo
echo "[seed] done. Run a synthesis with:"
echo "  ./scripts/demo.sh agent run --role researcher \\"
echo "    --goal \"What do my notes say about AI agents? Where do they agree and disagree?\""
