commit: cff6f30

# Restoring a backup by piping the dump silently dropped most rows

## What broke

**Instruction contract**, and possibly **your data**.

Every version of this repo before `cff6f30` documented the restore as:

```bash
gunzip -c "$(ls -1t Backups/*-supercharged-memory*.sql.gz | head -1)" \
  | tursodb "$SUPERCHARGED_MEMORY_TURSO_PATH" --experimental-multiprocess-wal
```

That command does not work, and it fails **partially**. The tursodb CLI splits its input
on line boundaries, but `memory_text` contains newlines, so most of a dump's lines are
continuations inside string literals — 4378 of 5034 in a measured dump, against only 636
`INSERT`s and 20 DDL statements. The first multi-line `INSERT` ends its statement
mid-value; everything after it is parsed as garbage until something happens to parse
again. What lands is an arbitrary fraction of the rows — 98 of 389 semantic rows in the
observed case — alongside a misleading `Parse error: table 'eval_cases' does not exist`.
tursodb's `.read` has the same defect.

**The dumps themselves are fine.** Every `CREATE` precedes its `INSERT`s and every row is
present. Only the replay was broken, so nothing in `Backups/` needs to be regenerated.

## How to resolve

Pulling this commit is enough for future restores — use `scripts/restore.py`:

```bash
python3 scripts/restore.py --out /path/to/new.db                      # newest backup
python3 scripts/restore.py --dump Backups/<file>.sql.gz --out /path/to/new.db
```

**If you have ever restored this database from a backup, check it now.** A partial restore
looks healthy: `recall.py --status` reports `READY n` for any non-zero `n`, so the row
count is the only thing that gives it away.

```bash
python3 scripts/restore.py --dump "$(ls -1t Backups/*-supercharged-memory*.sql.gz | head -1)" \
    --out /tmp/restore-check.db
```

Compare its per-table output against your live database:

```bash
DB="${SUPERCHARGED_MEMORY_TURSO_PATH:-$HOME/.local/share/turso/supercharged-memory.db}"
"${TURSO_BIN:-$HOME/.turso/tursodb}" "$DB" --experimental-multiprocess-wal -q -m list \
  "SELECT 'semantic', count(*) FROM semantic_memory UNION ALL SELECT 'episodic', count(*) FROM episodic_memory;"
```

If the live DB holds materially fewer rows than the backup, it is the product of a partial
restore. Move it aside — never overwrite it — and restore properly:

```bash
mv "$DB" "$DB.partial-$(date +%Y%m%d)"
python3 scripts/restore.py --out "$DB"
```

## Verification

`scripts/restore.py` verifies itself. A good restore ends with:

```
table                 in dump   restored
coworkers                   4          4
episodic_memory           195        195
semantic_memory           389        389
...
RESTORE OK — every row in the dump is present in <path>
```

Any `<<< MISMATCH` line, or a non-zero exit, means the database must not be used. Then:

```bash
python3 scripts/recall.py --status        # READY n, with n matching the dump's row total
python3 scripts/recall.py "<something you know is stored>"
```

The second call matters: `--status` only counts rows, so it cannot tell a restored
embedding from a missing one. A query that returns its expected row proves the vectors
survived too.
