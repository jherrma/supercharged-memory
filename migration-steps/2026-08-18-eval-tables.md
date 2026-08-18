commit: d98a362

# `eval_cases` + `eval_runs`: recall evaluation moves into the database

## What broke

**Schema, on-disk runtime state.** Deep sleep gained phase D6, which measures recall
quality against an authored eval set. The set used to be two JSON files next to the
DB; it now lives in two new tables so the `.sql` dump covers it.

Symptom on an unmigrated DB: `eval-harness.py` fails on any mode with
`no such table: eval_cases`, and deep sleep D6 cannot run. **Nothing else is
affected** — recall, `remember.py`, normal sleep, D0–D5, backup and restore all work
unmigrated. A machine that never runs deep sleep D6 can ignore this note entirely.

Creating a *fresh* database from `schema.sql` needs no migration; this note is only
for a database that already holds memories.

Related but NOT breaking, in the same change: `recall.py`'s ranking switched to
`dist - RECALL_ALPHA * kw`. That needs no migration — no schema, no CLI, no manual
step, and `RECALL_ALPHA` is additive with a default.

## How to resolve

1. Back up first — this adds tables to a live DB.

   ```bash
   bash scripts/supercharged-memory-backup.sh
   ```

2. Apply the two `CREATE TABLE` statements. They are `IF NOT EXISTS`, so re-running
   the whole schema is safe and is the simplest route:

   ```bash
   tursodb "$SUPERCHARGED_MEMORY_TURSO_PATH" --experimental-multiprocess-wal < schema.sql
   ```

   (Pipe it. `tursodb "$(cat schema.sql)"` fails — the leading `--` comment parses as
   a CLI flag.)

3. If this machine already had the file-based eval set from before the change,
   import it once:

   ```bash
   python3 investigations/eval-harness.py --import "$(dirname "$SUPERCHARGED_MEMORY_TURSO_PATH")/eval"
   ```

   Idempotent — cases are skipped by id, runs by exact value. Afterwards
   `eval.jsonl` and `history.jsonl` are redundant and can be deleted; keep
   `qvec.json`, which is still used as a cache (it is derived data and is
   deliberately not stored in the DB).

4. If this machine had no eval set, there is nothing to import. D6 skips itself
   when `eval_cases` is empty. Do **not** generate cases automatically — a query
   written from the row it should retrieve creates lexical overlap real users never
   produce and would bias every future `RECALL_ALPHA` upward.

## Verification

```bash
tursodb "$SUPERCHARGED_MEMORY_TURSO_PATH" --experimental-multiprocess-wal -q -m list \
  "SELECT name FROM sqlite_master WHERE name LIKE '%eval%' ORDER BY name;"
```

Proves it worked — five names (the last is SQLite's implicit index for
`eval_cases.id TEXT PRIMARY KEY`):

```
eval_cases
eval_runs
idx_eval_case_live
idx_eval_runs_time
sqlite_autoindex_eval_cases_1
```

Then, if you imported cases:

```bash
python3 investigations/eval-harness.py --validate
```

Exit 0 and `all cases point at live, current rows with matching timestamps`. A
non-zero exit lists which cases need repointing or retiring — that is the tool
working, not the migration failing.

With no cases imported, the harness exits with `no eval cases in the DB` — also
correct.
