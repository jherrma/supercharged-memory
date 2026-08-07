# Deep sleep — purge, compact, mine patterns

**This file is an instruction set for Claude Code.** Reached one of two ways: the
user said "deep sleep", or they answered yes to the offer at the end of
`SLEEP.md`. **User-triggered only** — never scheduled, never proactive.

Deep sleep does what a normal sleep pass deliberately doesn't: it deletes what the
user has agreed is dead weight, looks at *all* current semantic memory at once
instead of only what this pass touched, and reasons **across** episodic events
instead of one at a time.

Phases are numbered in execution order. **Every phase that writes runs its
judgment in subagents and its decisions past the user** — see *Subagent contract*
below for why, and how.

## Subagent contract

The orchestrating agent must never hold `memory_text` in bulk: that context cost
scales with the corpus and is the reason this file exists. So:

- The orchestrator queries **skinny metadata only** (ids, topics, dates, counts)
  and dispatches subagents that read the actual text themselves.
- Each worker prompt is **self-contained**: the ids it owns, the exact `tursodb`
  read command, its judgment rules, and (if it writes) the exact `remember.py`
  invocation plus your model id for `--model`.
- Workers are spawned **in one message** so they run concurrently.
- **Compaction and pattern workers propose only.** They return JSON; the
  orchestrator writes after the user approves. Only normal sleep's episodic-sift
  workers write on their own.
- A worker that dies is a **gap, not a silent omission** — say so in the report.

Read rows in a worker with:

```bash
tursodb "$SUPERCHARGED_MEMORY_TURSO_PATH" --experimental-multiprocess-wal -q -m list \
  "SELECT id, topic, category, memory_text FROM semantic_memory WHERE id IN (...);"
```

## D0 — Normal sleep (prerequisite) + health check

The normal pass must already be done — compacting a corpus that is missing this
session's lessons merges against stale content. If you arrived here from
`SLEEP.md`, it is. If the user said "deep sleep" cold, run `SLEEP.md` first, then
come back.

```bash
python3 scripts/recall.py --status
```

`READY n` → continue. **`DEGRADED n` → stop here**: Ollama is down, so no merged
memory and no pattern row can be embedded, and `--cluster` would have nothing to
work with. Report and stop; the rest of deep sleep is pointless without it.

## D1 — Backup

```bash
bash scripts/supercharged-memory-backup.sh
```

Not optional. D2 hard-deletes rows, and this dump is the only undo. `sleep.py
--purge` refuses to run unless it finds a dump newer than the DB file.

## D2 — Purge gate (the user decides, every run)

List everything that is no longer current truth:

```sql
SELECT id, topic, created_at, superseded_by, retired_at, substr(memory_text,1,80)
FROM semantic_memory
WHERE superseded_by IS NOT NULL OR retired_at IS NOT NULL
ORDER BY created_at;
```

Show it as a compact table and ask **which ids to delete** — all, none, or a
subset. There is deliberately **no default policy and no recommendation to skip
asking**: this is the one operation in the whole system that destroys a memory.
A superseded row is the audit trail of a revision; a retired one is a fact
someone decided no longer applies. Whether that history still earns its space is
the user's call, not yours.

```bash
python3 scripts/sleep.py --purge <id1,id2,...> --confirm-purge
```

**Pass every selected id in ONE call.** The backup gate compares the dump against
the DB's mtime, and a purge is itself a write — so a second call in the same pass
is refused until you re-run the backup. The script also refuses a current row, an
unknown id, a missing `--confirm-purge`, and any id whose deletion would strand a
surviving row that points at it (purge the whole chain together instead). It
clears the rows' `memory_coworkers` entries too.

If the user says "none", say so in the report and move on — an empty purge is a
normal outcome, not a failure.

## D3 — Compaction

**1. Cluster mechanically.** No LLM, no new embeddings — this reuses the vectors
already stored:

```bash
python3 scripts/sleep.py --cluster                      # semantic, default threshold 0.22
```

Returns `{"table", "threshold", "clusters":[{"cluster":N,"ids":[...]}], "no_embedding":[...]}`.
Singletons are omitted (nothing to merge). A non-empty `no_embedding` list is worth
reporting — those rows could not be clustered and were skipped, not judged.

**If the output carries a `warning` key, stop and lower `--threshold`.** Clusters
are connected components, so distance chains: a–b close and b–c close puts a and c
together even when they are unrelated. Past roughly 0.25 on a bge-m3 corpus that
tips over — measured on ~230 rows, 0.30 produced a 33-row blob and 0.35 collapsed
154 of ~200 rows into a single "cluster". A blob is not a merge candidate; it is a
sign the threshold is too loose.

**2. One worker per cluster.** Clusters over 12 ids: split into sub-batches of
≤12, ordered by topic, each proposing independently. Give each worker this rule
verbatim:

> Merge these memories into one **only** if they cover the same topic or issue and
> differ merely in nuance, detail, or perspective. Two facts that share vocabulary
> but state different things must NOT be merged. Preserve every concrete detail
> that survives — ids, exact error strings, versions, paths. Stay under 2000
> chars. Return JSON:
> `{"merge":[ids],"topic":"...","category":"...","keywords":"...","merged_text":"...","why_safe":"one line"}`
> or `{"no_merge":"reason"}` if they should stay separate.

**3. Present the batch.** One compact table: cluster, ids, proposed topic, merged
length, `why_safe` — **not** the full texts. Ask the user which to apply ("do
1,3,4"). Then per approved merge:

```bash
python3 scripts/remember.py --table semantic --category <c> --topic "<t>" \
  --keywords "<k1, k2, ...>" --source deep-sleep --model <your-model-id> \
  --supersedes <id1,id2,id3> --text "<merged>"
```

One call, one transaction: the new row is inserted and **all** listed ids are
pointed at it. `--supersedes` also skips the near-duplicate guard, which would
otherwise reject a merge for resembling its own inputs.

Merges create fresh superseded rows, which the *next* deep sleep offers for purge
in D2. That cycle is intended.

## D4 — Pattern mining

Input is **every** episodic row, processed or not: `processed_at` means "sifted
for lessons", not "pattern-checked".

There are two kinds of pattern and no single worker can see both:

- **Within-topic recurrence** — "this migration conflicted again", "that service
  restarted four times". Needs a coherent slice of related rows.
- **Cross-topic shape** — "a whole class of bug keeps coming back", "every
  incident this month traced to a derived artifact rather than the primary one".
  Spans topics by definition; a topic-scoped worker structurally cannot see it.

**1. Map — partitioned by episodic cluster.**

```bash
python3 scripts/sleep.py --cluster --table episodic     # slightly looser: default 0.25
```

Group by embedding similarity, **not** by exact `topic` — episodic topics are
near-unique (often one per ticket), so exact grouping yields all singletons and
finds nothing. Collect the leftover singletons into one mixed batch so no row is
skipped. Each worker reads its group's full texts and returns **both**:

- a digest line per row: `id | date | event_type | importance | topic | <=8-word gist | tags`
- any **within-topic** pattern it can already justify from its own group

**2. Reduce — cross-topic.** One worker gets all digest lines (small) plus the
full text of existing `category='pattern'` rows. Its scope is only what stage 1
could not see: shapes spanning groups, and refreshing or superseding prior
patterns. It must not re-derive within-topic patterns.

**3. Thresholds.** A pattern needs **≥3 supporting episodic events**. A claim
phrased as a *trend* additionally needs evidence spanning **≥2 distinct calendar
months**. Below that it is a single event, not a pattern — drop it. Drop a
stage-2 proposal whose `evidence_ids` are a subset of a stage-1 proposal's: same
finding, narrower view.

**4. Verifiability.** The `evidence_ids` go **into the memory text**, so a future
session can check the claim against the episodic rows instead of trusting it.

**5. Apply after approval:**

```bash
python3 scripts/remember.py --table semantic --category pattern --topic "<t>" \
  --keywords "<k1, k2, ...>" --source deep-sleep --model <your-model-id> \
  --text "<claim> ... Derived from episodic ids: 12, 44, 91." \
  [--supersedes <old-pattern-id>]
```

Use `--supersedes` when refreshing an existing pattern — otherwise the
near-duplicate guard rejects a re-stated trend, correctly, as a near-copy of
itself.

## D5 — Re-index and report

The corpus changed in D2/D3/D4, so rebuild the topic index exactly as normal
sleep's last phase does (same 500-char hard cap, same full-replace semantics) —
see `SLEEP.md` phase 7.

Then one report:

- rows purged (or "user chose none")
- clusters found → merges proposed → merges applied
- patterns added / refreshed, and how many candidates fell below the ≥3 threshold
- resulting topic count and char total against the cap
- any worker that died, and any row skipped for having no embedding
