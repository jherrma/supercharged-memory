commit: bb7cbcb

# `coworkers.expertise` and `coworkers.personality` raised to 3000 characters

## What broke

**Schema.** `schema.sql` now declares:

```
expertise    TEXT NOT NULL CHECK (length(expertise)   <= 3000)   -- was 256
personality  TEXT NOT NULL CHECK (length(personality) <= 3000)   -- was 1000
```

An existing DB still carries the old CHECKs. SQLite/Turso cannot `ALTER` a CHECK
constraint, so pulling this commit changes nothing on its own — the running DB keeps
refusing anything longer than 256 / 1000 characters until the table is rebuilt by hand.

Nothing else changed: no script CLI, no env var, no instruction contract. `name` stays at
64 and `appraisals.memory_text` stays at 2000.

## How to resolve

Back up first, then rebuild the table. Run the four statements as separate **arguments** —
do **not** pipe them; the CLI splits piped input on line boundaries.

```bash
bash scripts/supercharged-memory-backup.sh

DB="${SUPERCHARGED_MEMORY_TURSO_PATH:-$HOME/.local/share/turso/supercharged-memory.db}"
T="${TURSO_BIN:-$HOME/.turso/tursodb}"; F=--experimental-multiprocess-wal

"$T" "$DB" $F -q "CREATE TABLE coworkers_new (id INTEGER PRIMARY KEY, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, name TEXT NOT NULL UNIQUE CHECK (length(name) <= 64), expertise TEXT NOT NULL CHECK (length(expertise) <= 3000), personality TEXT NOT NULL CHECK (length(personality) <= 3000), trust_level TEXT NOT NULL DEFAULT 'supervised' CHECK (trust_level IN ('supervised','trusted','autonomous')), active INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0,1)));"
"$T" "$DB" $F -q "INSERT INTO coworkers_new (id,created_at,updated_at,name,expertise,personality,trust_level,active) SELECT id,created_at,updated_at,name,expertise,personality,trust_level,active FROM coworkers;"
"$T" "$DB" $F -q "DROP TABLE coworkers;"
"$T" "$DB" $F -q "ALTER TABLE coworkers_new RENAME TO coworkers;"
```

`coworkers` carries no indexes of its own, so there is nothing to recreate — unlike the
`semantic_memory` rebuild, which must restore `idx_sem_category` / `idx_sem_current` /
`idx_sem_project`.

`memory_coworkers.coworker_id` and `appraisals.coworker_id` both declare
`REFERENCES coworkers (id)`. Verified on a full scratch copy: the `DROP` + `RENAME` leaves
their rows untouched and ids still resolve. No FK cleanup is needed.

## Verification

Read the constraints back — this proves the rebuild took effect **without writing
anything**:

```bash
DB="${SUPERCHARGED_MEMORY_TURSO_PATH:-$HOME/.local/share/turso/supercharged-memory.db}"
T="${TURSO_BIN:-$HOME/.turso/tursodb}"; F=--experimental-multiprocess-wal

"$T" "$DB" $F -q -m list "SELECT sql FROM sqlite_master WHERE name='coworkers';" \
  | grep -o 'expertise[^,]*\|personality[^,]*'
```

must print

```
expertise TEXT NOT NULL CHECK (length (expertise) <= 3000)
personality TEXT NOT NULL CHECK (length (personality) <= 3000)
```

Then confirm nothing was lost:

```bash
"$T" "$DB" $F -q -m list "SELECT id, name, length(expertise), length(personality), trust_level, active FROM coworkers ORDER BY id;"
python3 scripts/recall.py --status        # READY n, not ERROR
```

The row count and every `trust_level` / `active` value must match what they were before
the rebuild.

**Do not test the new ceiling by `UPDATE`-ing a real coworker row.** That overwrites live
content, and it is how this change nearly lost a coworker's `expertise` text during
development. If you want a write test, create a throwaway DB with the same `CREATE TABLE`
above and `INSERT` into that instead: a 3000-character value must succeed and 3001 must
fail with `CHECK constraint failed`.

## If you need to roll back from the backup

Use `python3 scripts/restore.py --out <fresh-path>`; never pipe a dump into `tursodb`.
See `migration-steps/2026-08-18-restore-py.md`.
