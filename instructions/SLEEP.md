# Sleep — consolidation pass

**This file is an instruction set for Claude Code.** When the user says "sleep",
"go to sleep", or similar, run the procedure below in order. Sleep is
**user-triggered only** — never scheduled or run automatically. Work from the
repository root; the scripts read `SUPERCHARGED_MEMORY_TURSO_PATH` same as
always.

Sleep does three things: condenses the raw episodic log into durable semantic
facts, consolidates/retires semantic memory, and rebuilds the topic index that
`CLAUDE.md` points sessions at. Nothing here is a new table replacing the old
ones — `episodic_memory`/`semantic_memory` are unchanged; sleep just adds a
`processed_at` marker (episodic) and a `retired_at` soft-delete (semantic), plus
one small unlinked `topic_keywords` table.

## How the work is split — read this first

**The orchestrating agent never holds `memory_text` in bulk.** Reading every row's
full text into the session that is running sleep is the single largest context cost
in this system, and it grows with the corpus. So sleep queries **skinny metadata
only** (ids, topics, dates) and hands the actual reading to subagents:

- Each worker prompt is **self-contained**: the ids it owns, the exact `tursodb`
  read command, its judgment rules, the exact `remember.py` invocation, and your
  model id to pass as `--model`.
- Spawn workers **in one message** so they run concurrently.
- Workers report back **one line per row**, not the text they read.
- A worker that dies is a gap to report, never a silent omission.

Read rows inside a worker with:

```bash
tursodb "$SUPERCHARGED_MEMORY_TURSO_PATH" --experimental-multiprocess-wal -q -m list \
  "SELECT id, created_at, topic, event_type, importance, memory_text
   FROM episodic_memory WHERE id IN (...);"
```

## Step 1 — Pull unprocessed episodic memory

Ad-hoc SQL via the turso MCP (read-only, no script needed). Ignore the MCP's
`current_database` tool — it reports `:memory: (default)` even when correctly
attached to the real file ([upstream #8061](https://github.com/tursodatabase/turso/issues/8061));
confirm with the query itself, or read via
`tursodb "$SUPERCHARGED_MEMORY_TURSO_PATH" --experimental-multiprocess-wal -q -m list "<sql>"`.

Pull **metadata only** — the text belongs in the workers, not here:

```sql
SELECT id, created_at, topic, event_type, importance
FROM episodic_memory WHERE processed_at IS NULL ORDER BY created_at;
```

Split the ids into batches of ≤8 and dispatch one sift worker per batch (Steps 2–3
are that worker's job; Step 4 stays yours).

## Step 2 — Sift: keep only what someone learned

This is the worker's judgment rule — give it verbatim. For each row:

- **Keep** — the row records a fact, a decision, a root cause, a fix, a
  gotcha: something that changes what a future session should believe or do.
- **Discard** — the row is a bare event sequence with no lasting lesson
  ("x happened, then y happened"). Routine narration, not insight.

There's no dedup guard on episodic (events recur legitimately), so this
judgment call is the only filter — apply it seriously, don't rubber-stamp.

## Step 3 — Promote kept rows into semantic memory

**The sift worker does this itself** — high volume, low risk, and routing merged
text back through the orchestrator would defeat the whole split. It runs
`recall.py` first to check the fact isn't already stored, then one `remember.py`
call per kept row (or per cluster of related rows condensed into one fact —
condensing several episodic rows into a single semantic memory is encouraged,
don't force a 1:1 mapping):

```bash
python3 scripts/remember.py --table semantic --category <c> --topic "<t>" \
  --keywords "<k1, k2, ...>" --source sleep --model <your-model-id> \
  --text "<condensed, self-contained fact>"
# revising an existing fact instead of adding a new one:
python3 scripts/remember.py --table semantic --supersedes <old-id> ...
```

Same writing rules as always (see `CLAUDE.md` — self-contained, situation →
what's true → how to apply, under the 2000-char cap).

Each worker returns **one line per id** and nothing more:
`<id> → kept(sem=<new-id>) | discarded(<short reason>)`.

## Step 4 — Mark scanned episodic rows processed

**This stays with the orchestrator, and only for ids a worker actually reported
back.** A worker that dies mid-batch must not leave its rows stamped as sifted with
nothing written — unprocessed is the recoverable state, and those rows simply get
re-sifted next sleep.

Every reported row, kept or discarded, in one call:

```bash
python3 scripts/sleep.py --mark-processed <id1,id2,id3,...>
```

Skipping a discarded row here means it gets re-sifted (and re-discarded) every
future sleep, forever.

## Step 5 — Consolidate semantic memory

Scope this to the **topics this pass touched** — the corpus-wide sweep is deep
sleep's job (`DEEP-SLEEP.md` D3), and duplicating it here would read all of
semantic memory on every ordinary sleep.

For each touched topic, dispatch a worker: it pulls that topic's current rows
(`WHERE superseded_by IS NULL AND retired_at IS NULL`), looks for near-duplicates
or overlapping facts — same signal as `remember.py`'s own dedup guard
(cosine < 0.10), but scanning deliberately rather than against one new insert —
and proposes merges. Apply an approved merge with:

```bash
python3 scripts/remember.py --table semantic --supersedes <id[,id...]> --text "<merged>" ...
```

Several ids merge N memories into one row in a single transaction.

## Step 6 — Retire obsolete memories

"Obsolete" has no fixed definition — a memory can be superseded by newer info,
describe something removed from the codebase, or simply no longer apply.
**When it's not clear-cut, ask the user before retiring.** Never hard-delete —
retirement is soft (audit trail stays intact, same philosophy as coworkers'
`active=0` and the `superseded_by` chain):

```bash
python3 scripts/sleep.py --retire <id>
```

## Step 7 — Rebuild the topic index

Pull current (non-superseded, non-retired) semantic memory, group by `topic`,
and compile a short deduped keyword list per topic. Then replace the whole
`topic_keywords` table in one shot — this is a full DELETE + re-INSERT every
time, never an accumulation, so it can't drift from what's actually current:

**HARD RULE**: this table loads into every session's context in full
(`recall.py --topics`), not on demand — so total size across all topic+keyword
text combined must stay around **50 words / 300–500 characters**, full stop.
`sleep.py --rebuild-topics` enforces a 500-char hard cap and refuses the write
if you're over it (reporting the largest offenders). If that happens: merge
overlapping topics tighter, shorten keyword lists, or drop the least useful
topics entirely — don't just trim a little and resubmit hoping it clears; get
meaningfully under the cap. This is not a judgment call to skip when in a
hurry.

```bash
echo '[{"topic": "turso setup", "keywords": "tursodb, wal, multiprocess, embedding, bge-m3"},
       {"topic": "coworkers", "keywords": "trust_level, appraisal, memory_coworkers, persona"}]' \
  | python3 scripts/sleep.py --rebuild-topics
```

`CLAUDE.md.template` loads this table **in full at every session start**
(`recall.py --topics`, same slot as `--baseline`) — so a topic here is only
useful if its keywords are actually the terms someone would type. Keep the
topic count itself bounded too: `recall.py --topics` warns past 50 rows, since
that's real context budget spent every session regardless of whether that
session needs it — consolidate overlapping topics harder in Step 5/7 if it's
creeping up, don't just let it grow.

Deriving the topics is judgment work over the whole corpus, so hand it to a single
worker: it reads every current row's `topic` plus the `Keywords:` tail of its text
and returns the finished JSON array. The orchestrator only pipes that into the
script.

## Step 8 — Report

One summary line: episodic rows processed (kept vs. discarded), semantic
consolidations and retirements made, and the resulting topic count. Name any
worker that died — its rows are unprocessed, not silently dropped.

## Step 9 — Offer deep sleep

Sleep is done. Print this offer and stop; do **not** start deep sleep unless the
user says yes:

> Deep sleep additionally: (1) lists superseded/retired memories and hard-deletes
> the ones you select, after a backup; (2) clusters all current semantic memory by
> embedding similarity and proposes merges where memories cover the same topic and
> differ only in nuance; (3) mines the whole episodic log for recurring patterns
> and trends, storing each as a new `pattern` memory with its evidence ids; (4)
> rebuilds the topic index. Merges and patterns come to you as one approval batch.
> Proceed?

On yes → follow `instructions/DEEP-SLEEP.md` from **D1** (the backup); D0 is this
pass, already done. On no → stop here; nothing further to do.

If the user asked for "deep sleep" in the first place, skip the question and
continue straight into `DEEP-SLEEP.md` — the request is the consent.
