#!/usr/bin/env bash
set -euo pipefail

# ralph-loop.sh — Iteration manager for CLI agent backend
# Runs fresh-context agent iterations against a goal directory.
#
# Usage: ralph-loop.sh [--max-iterations N] [--claude-cmd CMD] <goal-dir>

MAX_ITERATIONS=5
CLAUDE_CMD="claude"
GOAL_DIR=""

usage() {
  echo "Usage: $0 [--max-iterations N] [--claude-cmd CMD] <goal-dir>"
  echo ""
  echo "  goal-dir           Path to data/goals/{goal-id}/ directory"
  echo "  --max-iterations N Maximum iterations (default: 5)"
  echo "  --claude-cmd CMD   Claude command to use (default: claude)"
  exit 1
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --max-iterations)
      MAX_ITERATIONS="$2"
      shift 2
      ;;
    --claude-cmd)
      CLAUDE_CMD="$2"
      shift 2
      ;;
    -h|--help)
      usage
      ;;
    -*)
      echo "Unknown option: $1" >&2
      usage
      ;;
    *)
      GOAL_DIR="$1"
      shift
      ;;
  esac
done

if [[ -z "$GOAL_DIR" ]]; then
  echo "Error: goal directory is required" >&2
  usage
fi

# Normalize to absolute path
GOAL_DIR="$(cd "$GOAL_DIR" 2>/dev/null && pwd)" || {
  echo "Error: goal directory does not exist: $GOAL_DIR" >&2
  exit 1
}

GOAL_FILE="$GOAL_DIR/GOAL.md"
PROGRESS_FILE="$GOAL_DIR/Progress.md"
DONE_FILE="$GOAL_DIR/DONE.md"
BLOCKED_FILE="$GOAL_DIR/BLOCKED.md"

if [[ ! -f "$GOAL_FILE" ]]; then
  echo "Error: GOAL.md not found in $GOAL_DIR" >&2
  exit 1
fi

ts() { date '+%Y-%m-%d %H:%M:%S'; }

echo "[$(ts)] ralph-loop starting"
echo "  Goal dir:       $GOAL_DIR"
echo "  Max iterations: $MAX_ITERATIONS"
echo "  Claude cmd:     $CLAUDE_CMD"
echo ""

for (( i=1; i<=MAX_ITERATIONS; i++ )); do
  # Check completion signals before each iteration
  if [[ -f "$DONE_FILE" ]]; then
    echo "[$(ts)] DONE.md found — goal complete"
    cat "$DONE_FILE"
    exit 0
  fi

  if [[ -f "$BLOCKED_FILE" ]]; then
    echo "[$(ts)] BLOCKED.md found — goal blocked"
    cat "$BLOCKED_FILE"
    exit 2
  fi

  LOG_FILE="$GOAL_DIR/iteration-${i}.log"

  echo "[$(ts)] === Iteration $i/$MAX_ITERATIONS ==="

  # Build the prompt with goal + progress context
  PROMPT="You are working on a goal. Read the task and continue where the last iteration left off.

## Goal
$(cat "$GOAL_FILE")

## Current Progress
$(if [[ -f "$PROGRESS_FILE" ]]; then cat "$PROGRESS_FILE"; else echo "No progress yet — this is the first iteration."; fi)

## Instructions
1. Work toward completing the goal described above.
2. Update Progress.md with what you accomplished this iteration.
3. If the goal is fully complete, create DONE.md with a summary of what was done.
4. If you are blocked and cannot proceed, create BLOCKED.md describing the blocker.
5. Be concrete and incremental — do real work each iteration."

  # Run claude with fresh context
  echo "[$(ts)] Running $CLAUDE_CMD..."
  if $CLAUDE_CMD --print -p "$PROMPT" > "$LOG_FILE" 2>&1; then
    echo "[$(ts)] Iteration $i finished (exit 0)"
  else
    EXIT_CODE=$?
    echo "[$(ts)] Iteration $i finished (exit $EXIT_CODE)"
  fi

  # Check signals after iteration
  if [[ -f "$DONE_FILE" ]]; then
    echo "[$(ts)] DONE.md created — goal complete"
    cat "$DONE_FILE"
    exit 0
  fi

  if [[ -f "$BLOCKED_FILE" ]]; then
    echo "[$(ts)] BLOCKED.md created — goal blocked"
    cat "$BLOCKED_FILE"
    exit 2
  fi
done

echo "[$(ts)] Max iterations ($MAX_ITERATIONS) reached without completion"
if [[ -f "$PROGRESS_FILE" ]]; then
  echo ""
  echo "Last progress:"
  cat "$PROGRESS_FILE"
fi
exit 1
