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
#   PYTHON_BIN=...     interpreter for the rendered prefix (default: python3;
#                      python on Windows, where python3 is a Store alias stub).
#                      Either a command (`python`, `py -3`) or a path; a path
#                      containing spaces is quoted for you, other whitespace is
#                      refused. See the case statement below.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BASE_PATH="${BASE_PATH:-$(dirname "$SCRIPT_DIR")}"
# On Windows this script runs under Git Bash, so `pwd` yields an MSYS path
# (/c/Users/...). That path is baked into ~/.claude/CLAUDE.md verbatim and only Git
# Bash can resolve it -- a session driving PowerShell cannot open a single script it
# names. cygpath -m gives the mixed form (C:/Users/...), which both shells accept.
# No-op where cygpath does not exist.
if command -v cygpath >/dev/null 2>&1; then
  BASE_PATH="$(cygpath -m "$BASE_PATH")"
fi
# Interpreter for the rendered command prefix. `python3` does not exist on Windows:
# the name is taken by a Microsoft Store alias stub that prints "Python was not
# found" and EXITS 0 -- so `python3 recall.py --status` reads as a successful empty
# result, and a session could then offer to restore a backup over a healthy DB.
# Override with PYTHON_BIN.
if [ -n "${PYTHON_BIN:-}" ]; then
  PY="$PYTHON_BIN"
  # PYTHON_BIN's documented case is an interpreter PATH on Windows, and such a path
  # survives neither Git Bash nor a template substitution. cygpath -m normalises it
  # to C:/... the way BASE_PATH is normalised; a bare command name passes through
  # unchanged, and this is a no-op where cygpath does not exist.
  if command -v cygpath >/dev/null 2>&1; then PY="$(cygpath -m "$PY")"; fi
else
  case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*) PY="python" ;;
    *)                    PY="python3" ;;
  esac
fi
# The template renders the interpreter UNQUOTED -- `{{PY}} "{{BASE_PATH}}/scripts/x.py"`
# -- because PYTHON_BIN has two legitimate shapes and one blanket rule breaks one of
# them. An interpreter PATH must be quoted: an all-users Windows install lands in
# `C:\Program Files\Python314\python.exe`, which cygpath -m turns into
# `C:/Program Files/Python314/python.exe`, and unquoted that prefix splits at the
# space into the command `C:/Program` plus a stray argument. A multi-word launcher
# command (`py -3`) must NOT be quoted, or the shell looks the whole string up as one
# filename. Nothing distinguishes them but what they are, so decide it here, once, and
# bake the quotes into the value itself. Whitespace that is neither is refused rather
# than rendered: the result lands in a file nobody re-reads, and every memory command
# of every future session would fail on it.
case "$PY" in
  *[[:space:]]*)
    if [ -f "$PY" ]; then
      PY="\"$PY\""                                        # interpreter path
    elif command -v "${PY%%[[:space:]]*}" >/dev/null 2>&1; then
      :                                                   # launcher command, e.g. `py -3`
    else
      echo "PYTHON_BIN='$PY' contains whitespace but is neither an existing file" >&2
      echo "(an interpreter path, which would be quoted) nor a command whose first" >&2
      echo "word resolves (a launcher such as 'py -3'). Refusing: the rendered" >&2
      echo "prefix would split at the space and every command in the installed" >&2
      echo "instructions would fail." >&2
      exit 1
    fi ;;
esac
TEMPLATE="$BASE_PATH/CLAUDE.md.template"
TARGET="${TARGET:-$HOME/.claude/CLAUDE.md}"
SUPERCHARGED_MEMORY_TURSO_PATH="${SUPERCHARGED_MEMORY_TURSO_PATH:-${XDG_DATA_HOME:-$HOME/.local/share}/turso/supercharged-memory.db}"
EPISODIC_MODE="${EPISODIC_MODE:-major-events}"
# Render only the ACTIVE mode's rule. Listing all four costs context every
# session to describe three modes the agent must ignore.
case "$EPISODIC_MODE" in
  every-prompt)  EPISODIC_RULE="store an episodic \`note\` for EVERY prompt/turn: what was asked, what you did." ;;
  major-actions) EPISODIC_RULE="store one for every substantive action (a change made, a task carried out); skip quick questions, clarifications, and trivial back-and-forth." ;;
  major-events)  EPISODIC_RULE="store ONLY major events: feature completed, bug resolved, decision, milestone, incident." ;;
  manual)        EPISODIC_RULE="store episodic memories ONLY when the user explicitly asks; never auto-store events." ;;
  *) echo "invalid EPISODIC_MODE '$EPISODIC_MODE' (expected: every-prompt|major-actions|major-events|manual)" >&2; exit 1 ;;
esac
BEGIN="<!-- BEGIN agentic-memory (managed by install-claude-md.sh) -->"
END="<!-- END agentic-memory -->"

# Sync stamp — which repo commit this install was rendered from. It is the ledger
# instructions/UPDATE.md reads back: `git log <stamp>..HEAD` yields both the template
# changes to explain and the pending breaking-change notes in migration-steps/. Keeping
# it inside the managed block means no separate state file to drift.
if SYNC_SHA="$(git -C "$BASE_PATH" rev-parse HEAD 2>/dev/null)"; then
  # Tracked-file modifications only; an untracked file can't change rendered behavior.
  git -C "$BASE_PATH" diff --quiet HEAD 2>/dev/null || SYNC_SHA="${SYNC_SHA}-dirty"
else
  SYNC_SHA="unknown"   # BASE_PATH is not a git work tree (copied files, or no git)
fi
STAMP="<!-- supercharged-memory: synced-at $SYNC_SHA -->"

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

# Trim trailing blank lines. Without this, the '\n' separator printed before the
# block below survives every strip and one blank line accumulates per re-install.
tmp2="$(mktemp)"
awk '{l[NR]=$0} END{e=NR; while(e>0 && l[e]~/^[[:space:]]*$/) e--; for(i=1;i<=e;i++) print l[i]}' \
  "$TARGET" > "$tmp2"
mv "$tmp2" "$TARGET"

# Substitute the placeholders LITERALLY. A sed replacement is NOT literal: GNU sed
# reads \U \L \t \n and & inside one, so PYTHON_BIN=C:\Python314\python.exe rendered
# as C:Python314python.exe -- and the report line below still echoed the value
# correctly, so nothing surfaced until a session tried to run a script. awk with
# index/substr does plain string replacement, and the values arrive through ENVIRON,
# so nothing processes escapes on the way in either.
render() {
  R_BASE_PATH="$BASE_PATH" R_PY="$PY" R_DB="$SUPERCHARGED_MEMORY_TURSO_PATH" \
  R_EPISODIC_MODE="$EPISODIC_MODE" R_EPISODIC_RULE="$EPISODIC_RULE" \
  awk '
    function rep(s, from, to,   out, i) {
      out = ""
      while ((i = index(s, from)) > 0) {
        out = out substr(s, 1, i - 1) to
        s = substr(s, i + length(from))
      }
      return out s
    }
    {
      l = $0
      l = rep(l, "{{BASE_PATH}}", ENVIRON["R_BASE_PATH"])
      l = rep(l, "{{PY}}", ENVIRON["R_PY"])
      l = rep(l, "{{SUPERCHARGED_MEMORY_TURSO_PATH}}", ENVIRON["R_DB"])
      l = rep(l, "{{EPISODIC_MODE}}", ENVIRON["R_EPISODIC_MODE"])
      l = rep(l, "{{EPISODIC_RULE}}", ENVIRON["R_EPISODIC_RULE"])
      print l
    }
  ' "$1"
}

# Append the freshly rendered block.
{
  printf '\n%s\n' "$BEGIN"
  printf '%s\n' "$STAMP"
  render "$TEMPLATE"
  printf '%s\n' "$END"
} >> "$TARGET"

echo "installed agentic-memory block into $TARGET"
echo "BASE_PATH                      = $BASE_PATH"
echo "SUPERCHARGED_MEMORY_TURSO_PATH = $SUPERCHARGED_MEMORY_TURSO_PATH"
echo "EPISODIC_MODE                  = $EPISODIC_MODE"
echo "PY                             = $PY   (as rendered)"
echo "synced-at                      = $SYNC_SHA"
echo "Restart your Claude Code session to pick it up."
