#!/bin/bash
# Render CLAUDE.md.template (substituting {{BASE_PATH}}) and install it into
# ~/.claude/CLAUDE.md between managed markers. Re-running REPLACES the managed
# block, so it's safe to run after every template edit or laptop switch.
#
# Configuration is env-only: no positional arguments, no flags but -h/--help.
# `install-claude-md.sh --help` is the single in-script copy of the env vars and
# their defaults -- this comment deliberately does not restate them, because
# three copies of that list (here, usage, README.md) had already drifted apart.
#
# Every invocation WRITES TARGET, so the two values a bare re-run used to lose
# are recovered from the block being replaced BEFORE any default applies:
#   * EPISODIC_MODE is rendered into the block, so it is read back from there
#     (the same anchor instructions/UPDATE.md greps) rather than snapping back
#     to this script's default. That reset was the silent half of the incident.
#   * BASE_PATH is the prefix of every path in the block; if the block was
#     rendered from a different one, this REFUSES instead of silently
#     repointing a live config at, say, a throwaway git worktree.
set -euo pipefail

usage() {
  cat <<'USAGE'
usage: install-claude-md.sh          (no arguments; configure via env)

Renders CLAUDE.md.template and installs it into ~/.claude/CLAUDE.md between
managed markers. Re-running REPLACES the managed block -- but values already
recorded in that block are read back and reused, so a bare re-run keeps this
machine's settings instead of resetting them to the defaults below.

Precedence: explicit env > value recovered from the existing block > default.

  BASE_PATH=...       repo root: the folder holding README.md, scripts/ and
                      CLAUDE.md.template. default: the parent of the folder
                      this script lives in (so the repo root, not scripts/).
                      Rendered into every path in the block. If the existing
                      block names a different one, the run is REFUSED unless
                      ALLOW_BASE_PATH_CHANGE=1.
  TARGET=...          where to write the block   default: ~/.claude/CLAUDE.md
  EPISODIC_MODE=...   every-prompt | major-actions | major-events | manual
                      default: major-events, but recovered from TARGET's
                      existing block when there is one -- getting this wrong
                      silently changes how much the agent stores.
  SUPERCHARGED_MEMORY_TURSO_PATH=...
                      the live Turso DB path to write into the instructions.
                      default: the value in ~/.claude/settings.json (.env),
                      else ${XDG_DATA_HOME:-~/.local/share}/turso/
                      supercharged-memory.db. It has to agree with what the
                      python scripts read, because a wrong DB path is how an
                      agent ends up reporting MISSING against a healthy
                      database. Substituted wherever the template references
                      the placeholder -- the current template does not, so
                      today this value only shows up in the closing report.
  ALLOW_BASE_PATH_CHANGE=1
                      allow re-rendering an existing block from a different
                      BASE_PATH (the repo genuinely moved).

Every run writes TARGET. The closing report names where each value came from.
USAGE
}

if [ "$#" -gt 0 ]; then
  if [ "$#" -eq 1 ] && { [ "$1" = "-h" ] || [ "$1" = "--help" ]; }; then
    # `|| true`: with stdout closed the heredoc fails, and set -e would turn a
    # successful help request into exit 1.
    usage || true
    exit 0
  fi
  printf 'install-claude-md.sh takes no arguments (got: %s)\n\n' "$*" >&2 || true
  usage >&2 || true
  exit 2
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [ -n "${BASE_PATH:-}" ]; then
  BASE_SRC="env"
else
  BASE_PATH="$(dirname "$SCRIPT_DIR")"
  BASE_SRC="default (parent of $SCRIPT_DIR)"
fi
TEMPLATE="$BASE_PATH/CLAUDE.md.template"
TARGET="${TARGET:-$HOME/.claude/CLAUDE.md}"
BEGIN="<!-- BEGIN agentic-memory (managed by install-claude-md.sh) -->"
END="<!-- END agentic-memory -->"

# --- Recover what the block being replaced already records -------------------
# The rendered block IS the record of what this machine was installed with, and
# it is still on disk at the moment this script overwrites it. Read it first;
# defaulting before reading is what cost one machine its episodic mode.
BLOCK=""
if [ -f "$TARGET" ] && grep -qF "$BEGIN" "$TARGET"; then
  BLOCK="$(awk -v b="$BEGIN" -v e="$END" '
    index($0,b) {inb=1; next}
    inb && index($0,e) {exit}
    inb {print}
  ' "$TARGET")"
fi

PREV_MODE=""
PREV_BASE=""
if [ -n "$BLOCK" ]; then
  # Same anchor instructions/UPDATE.md Step 5 greps for.
  PREV_MODE="$(printf '%s\n' "$BLOCK" \
    | grep -o 'Episodic policy on this machine — `[a-z-]*`' \
    | grep -o '`[a-z-]*`' | tr -d '`' | head -1 || true)"
  # BASE_PATH is not labelled in the block, it is just the prefix of every path
  # in it. Take the most common prefix among the paths that end in a known repo
  # subpath, so one reworded template line cannot flip the answer. Paths are cut
  # out of the backtick spans the template writes them in, not split on
  # whitespace -- a BASE_PATH containing a space would otherwise come back
  # truncated, and the mismatch check below would fire on a path that matches.
  PREV_BASE="$(printf '%s\n' "$BLOCK" \
    | grep -oE '`[^`]+`' \
    | sed -E 's/^`//; s/`$//; s/^(bash|python3) //; s/^"//; s/"$//' \
    | sed -nE 's;^(/.+)/(README\.md|scripts|instructions|Backups)(/.*)?$;\1;p' \
    | sort | uniq -c | sort -rn | head -1 | sed -E 's/^ *[0-9]+ //' || true)"
fi

# --- Resolve each value, and remember where it came from ---------------------
if [ -n "${EPISODIC_MODE:-}" ]; then
  EPISODIC_SRC="env"
elif [ -n "$PREV_MODE" ]; then
  EPISODIC_MODE="$PREV_MODE"
  EPISODIC_SRC="recovered from the existing block"
elif [ -n "$BLOCK" ]; then
  # A block exists but its policy line is unreadable (hand-edited, or the
  # template moved the line). Falling back to the default here is exactly the
  # silent reset this guard exists to prevent, so refuse and make the caller say.
  echo "refusing: $TARGET has a managed block, but no readable \`Episodic policy on this machine\` line to recover EPISODIC_MODE from." >&2
  echo "re-run with the mode you want, e.g. EPISODIC_MODE=major-actions bash \"$SCRIPT_DIR/install-claude-md.sh\"" >&2
  exit 1
else
  EPISODIC_MODE="major-events"
  EPISODIC_SRC="script default (no existing block)"
fi

# The DB path is NOT recoverable from the block: the template stopped referencing
# {{SUPERCHARGED_MEMORY_TURSO_PATH}} in 05b1967, so nothing renders it. The
# machine's own record of it is ~/.claude/settings.json, which is also where the
# python scripts read it from (Claude Code's Bash tool never sources a profile).
settings_db() {
  local f="$HOME/.claude/settings.json"
  [ -f "$f" ] || return 0
  # python3 first (every other script here already needs it); jq only if it is
  # installed -- neither is a hard dependency of this script.
  if command -v python3 >/dev/null 2>&1; then
    python3 -c 'import json,sys
try:
    print(json.load(open(sys.argv[1])).get("env",{}).get("SUPERCHARGED_MEMORY_TURSO_PATH","") or "")
except Exception:
    pass' "$f" 2>/dev/null || true
  elif command -v jq >/dev/null 2>&1; then
    jq -r '.env.SUPERCHARGED_MEMORY_TURSO_PATH // empty' "$f" 2>/dev/null || true
  fi
}
if [ -n "${SUPERCHARGED_MEMORY_TURSO_PATH:-}" ]; then
  DB_SRC="env"
else
  SETTINGS_DB="$(settings_db | head -1 || true)"
  if [ -n "$SETTINGS_DB" ]; then
    SUPERCHARGED_MEMORY_TURSO_PATH="$SETTINGS_DB"
    DB_SRC="~/.claude/settings.json"
  else
    SUPERCHARGED_MEMORY_TURSO_PATH="${XDG_DATA_HOME:-$HOME/.local/share}/turso/supercharged-memory.db"
    DB_SRC="XDG default"
  fi
fi

# --- BASE_PATH mismatch: refuse rather than silently repoint a live config ---
# Compared with one trailing slash trimmed off each side: /x/repo and /x/repo/
# render the same install, and refusing over that would be a false alarm.
if [ -n "$PREV_BASE" ] && [ "${PREV_BASE%/}" != "${BASE_PATH%/}" ]; then
  if [ "${ALLOW_BASE_PATH_CHANGE:-0}" = "1" ]; then
    echo "warning: repointing the managed block from $PREV_BASE to $BASE_PATH (ALLOW_BASE_PATH_CHANGE=1)" >&2
  else
    cat >&2 <<EOF
refusing to rewrite $TARGET: its managed block was rendered from a different repo.

  installed from : $PREV_BASE
  running from   : $BASE_PATH

Every path in the block would be repointed at the second one. Nothing was written.

If you are running from a temporary clone or git worktree, run the installer from
the checkout the block already names, or point TARGET at a throwaway file.
If the repo really moved, re-run with:

  ALLOW_BASE_PATH_CHANGE=1 bash "$SCRIPT_DIR/install-claude-md.sh"
EOF
    exit 3
  fi
elif [ -n "$BLOCK" ] && [ -z "$PREV_BASE" ]; then
  echo "warning: $TARGET has a managed block but no recognisable repo paths in it; cannot check BASE_PATH for a mismatch." >&2
fi

# Render only the ACTIVE mode's rule. Listing all four costs context every
# session to describe three modes the agent must ignore.
case "$EPISODIC_MODE" in
  every-prompt)  EPISODIC_RULE="store an episodic \`note\` for EVERY prompt/turn: what was asked, what you did." ;;
  major-actions) EPISODIC_RULE="store one for every substantive action (a change made, a task carried out); skip quick questions, clarifications, and trivial back-and-forth." ;;
  major-events)  EPISODIC_RULE="store ONLY major events: feature completed, bug resolved, decision, milestone, incident." ;;
  manual)        EPISODIC_RULE="store episodic memories ONLY when the user explicitly asks; never auto-store events." ;;
  *) echo "invalid EPISODIC_MODE '$EPISODIC_MODE' ($EPISODIC_SRC; expected: every-prompt|major-actions|major-events|manual)" >&2; exit 1 ;;
esac

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

# Append the freshly rendered block ( | delimiter: paths contain slashes.
# EPISODIC_MODE is validated to a fixed keyword set above, so it's sed-safe ).
{
  printf '\n%s\n' "$BEGIN"
  printf '%s\n' "$STAMP"
  sed -e "s|{{BASE_PATH}}|$BASE_PATH|g" \
      -e "s|{{SUPERCHARGED_MEMORY_TURSO_PATH}}|$SUPERCHARGED_MEMORY_TURSO_PATH|g" \
      -e "s|{{EPISODIC_MODE}}|$EPISODIC_MODE|g" \
      -e "s|{{EPISODIC_RULE}}|$EPISODIC_RULE|g" "$TEMPLATE"
  printf '%s\n' "$END"
} >> "$TARGET"

# Name the SOURCE of every value, not just the value: a re-install that quietly
# reused, or quietly defaulted, one of these is the failure mode being audited.
echo "installed agentic-memory block into $TARGET"
echo "BASE_PATH                      = $BASE_PATH   [$BASE_SRC]"
echo "SUPERCHARGED_MEMORY_TURSO_PATH = $SUPERCHARGED_MEMORY_TURSO_PATH   [$DB_SRC]"
echo "EPISODIC_MODE                  = $EPISODIC_MODE   [$EPISODIC_SRC]"
echo "synced-at                      = $SYNC_SHA"
if [ -n "$BLOCK" ]; then
  echo "replaced a block rendered from ${PREV_BASE:-<unrecognised>} with EPISODIC_MODE ${PREV_MODE:-<unreadable>}"
fi
echo "Restart your Claude Code session to pick it up."
