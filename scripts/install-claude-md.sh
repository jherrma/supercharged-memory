#!/bin/bash
# Render CLAUDE.md.template (substituting {{BASE_PATH}}) and install it into
# ~/.claude/CLAUDE.md between managed markers. Idempotent: re-running REPLACES the
# managed block, so it's safe to run after every template edit or laptop switch.
#
# BASE_PATH defaults to this script's parent folder (the "Agentic Development"
# dir), so it stays correct on any machine. Override with env:
#   BASE_PATH=...      repo root (holds README.md, scripts/, CLAUDE.md.template)
#   TARGET=...         where to write the block (default ~/.claude/CLAUDE.md)
#   SUPERCHARGED_MEMORY_TURSO_PATH=...        where the live Turso DB lives (must match what scripts use)
#   EPISODIC_MODE=...  every-prompt | major-actions | major-events | manual
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BASE_PATH="${BASE_PATH:-$(dirname "$SCRIPT_DIR")}"
TEMPLATE="$BASE_PATH/CLAUDE.md.template"
TARGET="${TARGET:-$HOME/.claude/CLAUDE.md}"
SUPERCHARGED_MEMORY_TURSO_PATH="${SUPERCHARGED_MEMORY_TURSO_PATH:-$HOME/Documents/turso/agent-foundations.db}"
EPISODIC_MODE="${EPISODIC_MODE:-major-events}"
case "$EPISODIC_MODE" in
  every-prompt|major-actions|major-events|manual) ;;
  *) echo "invalid EPISODIC_MODE '$EPISODIC_MODE' (expected: every-prompt|major-actions|major-events|manual)" >&2; exit 1 ;;
esac
BEGIN="<!-- BEGIN agentic-memory (managed by install-claude-md.sh) -->"
END="<!-- END agentic-memory -->"

[ -f "$TEMPLATE" ] || { echo "template not found: $TEMPLATE" >&2; exit 1; }
mkdir -p "$(dirname "$TARGET")"
touch "$TARGET"

# Drop any previous managed block (inclusive of markers) for a clean re-install.
if grep -qF "$BEGIN" "$TARGET"; then
  tmp="$(mktemp)"
  awk -v b="$BEGIN" -v e="$END" '
    $0==b {skip=1; next}
    skip && $0==e {skip=0; next}
    !skip {print}
  ' "$TARGET" > "$tmp"
  mv "$tmp" "$TARGET"
fi

# Append the freshly rendered block ( | delimiter: paths contain slashes.
# EPISODIC_MODE is validated to a fixed keyword set above, so it's sed-safe ).
{
  printf '\n%s\n' "$BEGIN"
  sed -e "s|{{BASE_PATH}}|$BASE_PATH|g" \
      -e "s|{{SUPERCHARGED_MEMORY_TURSO_PATH}}|$SUPERCHARGED_MEMORY_TURSO_PATH|g" \
      -e "s|{{EPISODIC_MODE}}|$EPISODIC_MODE|g" "$TEMPLATE"
  printf '%s\n' "$END"
} >> "$TARGET"

echo "installed agentic-memory block into $TARGET"
echo "BASE_PATH                      = $BASE_PATH"
echo "SUPERCHARGED_MEMORY_TURSO_PATH = $SUPERCHARGED_MEMORY_TURSO_PATH"
echo "EPISODIC_MODE                  = $EPISODIC_MODE"
echo "Restart your Claude Code session to pick it up."
