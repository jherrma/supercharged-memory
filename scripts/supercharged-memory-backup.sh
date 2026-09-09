#!/bin/bash
# Daily backup of the local Turso memory DB (supercharged-memory) -> local backups folder.
# Concurrent-safe: opens with --experimental-multiprocess-wal (plus the IOCP VFS on
# Windows, which that flag requires there), so the dump runs
# as a reader even while Claude sessions hold the DB. Produces a gzipped SQL
# dump, VALIDATES it (non-empty + has INSERTs + gzip intact), and retains
# 3 daily + 4 weekly (Monday) copies so a long weekend can't rotate out every
# good dump.
set -uo pipefail

TURSO="${TURSO_BIN:-$HOME/.turso/tursodb}"
# Windows' default IO backend refuses --experimental-multiprocess-wal outright, so
# the dump below fails on open unless the flag is paired with the IOCP backend
# (tursodb --help names it). Same detection as memlib.py's VFS/OPEN_ARGS: the
# uname cases below are the shell spelling of memlib's sys.platform set
# (win32/msys/cygwin), so Git Bash and a Cygwin Python agree.
# TURSO_VFS overrides in BOTH directions -- a different backend name, or
# "none"/empty to drop --vfs for a tursodb that no longer needs it.
# Written as an array with the ${a[@]+...} guard because macOS still ships bash
# 3.2, where "${empty[@]}" under `set -u` is an unbound-variable error.
if [ "${TURSO_VFS+set}" = set ]; then
  # memlib.py does _VFS_ENV.strip().lower(), so trim before deciding AND before
  # passing the name on: without the trim, TURSO_VFS=" none " reaches --vfs
  # verbatim and every open dies with `no such VFS:  none` while every Python
  # script in the repo keeps working. Parameter expansion, not `tr -d`, so an
  # interior space is preserved exactly as Python's .strip() preserves it.
  VFS_NAME="$TURSO_VFS"
  VFS_NAME="${VFS_NAME#"${VFS_NAME%%[![:space:]]*}"}"
  VFS_NAME="${VFS_NAME%"${VFS_NAME##*[![:space:]]}"}"
  case "$(printf '%s' "$VFS_NAME" | tr '[:upper:]' '[:lower:]')" in
    ""|none) VFS_ARGS=() ;;
    *)       VFS_ARGS=(--vfs "$VFS_NAME") ;;
  esac
else
  case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*) VFS_ARGS=(--vfs experimental_win_iocp) ;;
    *)                    VFS_ARGS=() ;;
  esac
fi
DB="${SUPERCHARGED_MEMORY_TURSO_PATH:-${XDG_DATA_HOME:-$HOME/.local/share}/turso/supercharged-memory.db}"
DEST="${BACKUP_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/Backups}"
ERR=/tmp/supercharged-memory-backup.err

[ -f "$DB" ] || { echo "[backup] DB missing: $DB" >&2; exit 1; }
mkdir -p "$DEST"
STAMP="$(date +%F)"
TMP="$(mktemp)"

# Dump with retry (a concurrent writer may briefly hold the write lock).
ok=0
for i in 1 2 3 4 5; do
  if "$TURSO" "$DB" --experimental-multiprocess-wal ${VFS_ARGS[@]+"${VFS_ARGS[@]}"}        -q ".dump" > "$TMP" 2>"$ERR"; then
    if [ -s "$TMP" ] && grep -q "INSERT INTO" "$TMP"; then ok=1; break; fi
  else
    # tursodb refused to run at all. Retry only on contention -- the same phrase
    # gate as memlib._is_busy, read off both streams because a contended write
    # prints a bare `database is busy` with no diagnostic prefix and tursodb puts
    # some diagnostics on stdout. Anything else (a bad --vfs name, a missing
    # binary) fails identically on all five attempts, so the loop only delays the
    # report by 25s. A false match on a dumped row quoting the phrase just costs
    # the backoff we already paid today, so it cannot skip a genuine retry.
    grep -Eqi 'database is (busy|locked)' "$ERR" "$TMP" || break
  fi
  sleep 5
done
if [ "$ok" != "1" ]; then
  echo "[backup] FAILED (no valid dump after retries): $(cat "$ERR" 2>/dev/null)" >&2
  rm -f "$TMP"; exit 1
fi

OUT="$DEST/$STAMP-supercharged-memory.sql"
mv "$TMP" "$OUT"
gzip -f "$OUT"
gzip -t "$OUT.gz" || { echo "[backup] gzip corrupt: $OUT.gz" >&2; rm -f "$OUT.gz"; exit 1; }

# Weekly tier: on Mondays keep a separate weekly copy.
[ "$(date +%u)" = "1" ] && cp "$OUT.gz" "$DEST/$STAMP-supercharged-memory-weekly.sql.gz"

# Prune: 3 most recent daily, 4 most recent weekly (globs are disjoint by suffix).
ls -1t "$DEST"/*-supercharged-memory.sql.gz 2>/dev/null | tail -n +4 | xargs -I{} rm -f {}
ls -1t "$DEST"/*-supercharged-memory-weekly.sql.gz 2>/dev/null | tail -n +5 | xargs -I{} rm -f {}
echo "[backup] ok: $OUT.gz"
