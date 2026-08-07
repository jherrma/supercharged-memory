commit: d7f1af5

# `semantic_memory.category` gains `pattern`

## What broke

**Schema, on-disk runtime state.** Deep sleep's D4 stores derived recurrences and
trends as `semantic_memory` rows with `category='pattern'`. That value is new;
`category` is constrained by a table-level `CHECK`, and SQLite/Turso cannot `ALTER`
a `CHECK` — so `schema.sql` in the repo now allows six categories while an existing
database still allows five.

Symptom on an unmigrated DB: everything works until deep sleep D4 (or any manual
`remember.py --category pattern`) tries to write, which fails with a CHECK
constraint violation. Nothing else is affected — `--purge`, `--cluster`,
`--supersedes <csv>`, recall and normal sleep all work unmigrated.

Rebuilding `schema.sql` into a *fresh* database needs no migration; this note is
only for a database that already holds memories.

## How to resolve

**1. Back up first.** The rebuild drops and recreates a table holding every
semantic memory you have.

```bash
cd <repo>
bash scripts/supercharged-memory-backup.sh
```

**2. Record the before-state** so step 4 has something to compare against:

```bash
DB="${SUPERCHARGED_MEMORY_TURSO_PATH:-${XDG_DATA_HOME:-$HOME/.local/share}/turso/supercharged-memory.db}"
q() { tursodb "$DB" --experimental-multiprocess-wal -q -m list "$1"; }
q 'SELECT count(*) FROM semantic_memory;'
q 'SELECT count(*) FROM semantic_memory WHERE embedding IS NULL;'
q 'SELECT count(*) FROM semantic_memory WHERE superseded_by IS NOT NULL;'
q 'SELECT count(*) FROM semantic_memory WHERE retired_at IS NOT NULL;'
# cosine between any two ids that exist in YOUR db — proves the vectors survived
q 'SELECT round(vector_distance_cos((SELECT embedding FROM semantic_memory ORDER BY id LIMIT 1),
                                    (SELECT embedding FROM semantic_memory ORDER BY id DESC LIMIT 1)),6);'
```

**3. Rebuild the table.** Check no other process holds the DB first
(`pgrep -fl tursodb`); a Claude session with the `turso` MCP attached counts.

```bash
tursodb "$DB" --experimental-multiprocess-wal <<'SQL'
BEGIN;
CREATE TABLE semantic_memory_new (
  id             INTEGER PRIMARY KEY,
  created_at     TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at     TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  project        TEXT CHECK(length(project) <= 128),
  topic          TEXT CHECK(length(topic) <= 128),
  category       TEXT NOT NULL CHECK(category IN ('baseline','user','feedback','project','reference','pattern')),
  source         TEXT CHECK(length(source) <= 128),
  model          TEXT CHECK(length(model) <= 128),
  embed_model    TEXT CHECK(length(embed_model) <= 128),
  memory_text    TEXT NOT NULL CHECK(length(memory_text) <= 2000),
  file_reference TEXT,
  embedding      F32_BLOB(1024),
  superseded_by  INTEGER REFERENCES semantic_memory(id),
  retired_at     TEXT
);
INSERT INTO semantic_memory_new SELECT * FROM semantic_memory;
DROP TABLE semantic_memory;
ALTER TABLE semantic_memory_new RENAME TO semantic_memory;
CREATE INDEX IF NOT EXISTS idx_sem_category ON semantic_memory(category);
CREATE INDEX IF NOT EXISTS idx_sem_current  ON semantic_memory(superseded_by);
CREATE INDEX IF NOT EXISTS idx_sem_project  ON semantic_memory(project);
COMMIT;
SQL
```

Two details that matter:

- The three indexes are **recreated explicitly** because they die with the dropped
  table.
- `superseded_by REFERENCES semantic_memory(id)` deliberately names the *old* table
  at creation time. After the drop and rename it resolves to the new table itself,
  which is the self-reference the schema wants — verified on Turso 0.7.1.

**Rehearsing on a copy first?** Copy `.db`, `.db-wal` **and** `.db-tshm` together.
A bare `.db` copy silently reads older, because un-checkpointed rows live in the
WAL — measured here as 169 rows in the copy versus 227 live.

## Verification

```bash
# 1. The CHECK now lists six categories
q "SELECT sql FROM sqlite_master WHERE name='semantic_memory';" | grep -o "category IN ([^)]*)"
#    -> category IN ('baseline', 'user', 'feedback', 'project', 'reference', 'pattern')

# 2. Every count from step 2 is UNCHANGED, and the cosine is bit-identical
#    (a differing cosine or a new NULL-embedding row means the vectors did not copy)

# 3. All three indexes are back
q "SELECT group_concat(name) FROM sqlite_master WHERE type='index' AND tbl_name='semantic_memory';"
#    -> idx_sem_category,idx_sem_current,idx_sem_project

# 4. The scripts still see a healthy DB
python3 scripts/recall.py --status      # READY n, with the same n as before
python3 scripts/recall.py --baseline    # same rows as before

# 5. The new category actually writes
python3 scripts/remember.py --table semantic --category pattern \
  --topic "migration smoke test" --source migration --model "<your-model-id>" \
  --text "Migration smoke test: the pattern category is accepted after the CHECK rebuild."
#    -> stored semantic memory (migration smoke test)
#    Retire it afterwards if you don't want it: python3 scripts/sleep.py --retire <id>
```

If step 2 disagrees, **stop and restore** rather than continuing:

```bash
gzcat "$(ls -1t Backups/*-supercharged-memory*.sql.gz | head -1)" \
  | tursodb <a FRESH db path> --experimental-multiprocess-wal
```

## Already applied

The machine this note was written on was migrated on 2026-08-07 (228 rows in and
out, cosine `0.118092` unchanged, `READY 340` afterwards). Other machines still
need it.
