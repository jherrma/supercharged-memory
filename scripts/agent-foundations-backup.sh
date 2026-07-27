#!/bin/bash
# Daily backup of the local Turso memory DB (agent-foundations) -> local backups folder.
# Concurrent-safe: opens with --experimental-multiprocess-wal, so the dump runs
# as a reader even while Claude sessions hold the DB. Produces a gzipped SQL
# dump, VALIDATES it (non-empty + has INSERTs + gzip intact), and retains
# 3 daily + 4 weekly (Monday) copies so a long weekend can't rotate out every
# good dump.
set -uo pipefail

TURSO="${TURSO_BIN:-$HOME/.turso/tursodb}"
DB="${SUPERCHARGED_MEMORY_TURSO_PATH:-$HOME/Documents/turso/agent-foundations.db}"
DEST="${BACKUP_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/Backups}"
ERR=/tmp/agent-foundations-backup.err

[ -f "$DB" ] || { echo "[backup] DB missing: $DB" >&2; exit 1; }
mkdir -p "$DEST"
STAMP="$(date +%F)"
TMP="$(mktemp)"

# Dump with retry (a concurrent writer may briefly hold the write lock).
ok=0
for i in 1 2 3 4 5; do
  if "$TURSO" "$DB" --experimental-multiprocess-wal -q ".dump" > "$TMP" 2>"$ERR"; then
    if [ -s "$TMP" ] && grep -q "INSERT INTO" "$TMP"; then ok=1; break; fi
  fi
  sleep 5
done
if [ "$ok" != "1" ]; then
  echo "[backup] FAILED (no valid dump after retries): $(cat "$ERR" 2>/dev/null)" >&2
  rm -f "$TMP"; exit 1
fi

OUT="$DEST/$STAMP-agent-foundations.sql"
mv "$TMP" "$OUT"
gzip -f "$OUT"
gzip -t "$OUT.gz" || { echo "[backup] gzip corrupt: $OUT.gz" >&2; rm -f "$OUT.gz"; exit 1; }

# Weekly tier: on Mondays keep a separate weekly copy.
[ "$(date +%u)" = "1" ] && cp "$OUT.gz" "$DEST/$STAMP-agent-foundations-weekly.sql.gz"

# Prune: 3 most recent daily, 4 most recent weekly (globs are disjoint by suffix).
ls -1t "$DEST"/*-agent-foundations.sql.gz 2>/dev/null | tail -n +4 | xargs -I{} rm -f {}
ls -1t "$DEST"/*-agent-foundations-weekly.sql.gz 2>/dev/null | tail -n +5 | xargs -I{} rm -f {}
echo "[backup] ok: $OUT.gz"
