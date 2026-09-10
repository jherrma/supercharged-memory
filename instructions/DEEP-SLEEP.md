# Deep sleep — purge, compact, mine patterns

**This file is an instruction set for Claude Code.** Reached one of three ways: the
user said "deep sleep", they answered yes to the offer at the end of `SLEEP.md`, or
the optional weekly preparation job ran. **Never proactive** — do not start one on
your own judgement.

--- the scheduled preparation run ---
`scripts/scheduled-sleep.py --mode weekly` drives this file **propose-only**: D0, D1
backup, D3 clustering and its proposal workers, D4 proposals, D6.2. It must never
purge (D2), apply a merge, write a pattern row, or touch an eval case — every gate
below still belongs to the user, and D2 is the one operation here that destroys a
memory. Its output is a decision queue at
`<state dir>/deep-sleep-review-<date>.md`. When you are the supervised session the
user approves it in, read that file first: the analysis is already done, so do not
redo the clustering. Treat its lists as leads — pairs that share vocabulary while
stating different things are the common false positive.

Deep sleep does what a normal sleep pass deliberately doesn't: it deletes what the
user has agreed is dead weight, looks at *all* current semantic memory at once
instead of only what this pass touched, and reasons **across** episodic events
instead of one at a time.

Phases are numbered in execution order. **Every phase that writes runs its
judgment in subagents and its decisions past the user** — see *Subagent contract*
below for why, and how.

> **On Windows, run every `python3` in this file as `python`, and add `--vfs
> experimental_win_iocp` to every `tursodb` command in it.** There is no
> `python3` on Windows: the name is a Microsoft Store alias stub that prints
> `Python was not found` **and exits 0**, so a command reads as a successful,
> empty result and the agent reports work it never did. And
> `--experimental-multiprocess-wal` on its own is refused by Windows' default IO
> backend (`experimental multiprocess WAL is not supported by the active IO
> backend`), so a `tursodb` line without the VFS does nothing at all — pair the
> two flags, never drop the WAL one. See `instructions/SETUP.md`, section *Windows*.

## Subagent contract

The orchestrating agent must never hold `memory_text` in bulk: that context cost
scales with the corpus and is the reason this file exists. So:

- The orchestrator queries **skinny metadata only** (ids, topics, dates, counts)
  and dispatches subagents that read the actual text themselves.
- Each worker prompt is **self-contained**: the ids it owns, the exact read
  command, its judgment rules, and (if it writes) the exact `remember.py`
  invocation plus your model id for `--model`.
- Workers are spawned **in one message** so they run concurrently.
- **Compaction and pattern workers propose only.** They return JSON; the
  orchestrator writes after the user approves. Only normal sleep's episodic-sift
  workers write on their own.
- A worker that dies is a **gap, not a silent omission** — say so in the report.

Read rows in a worker with:

```bash
python3 -c "
import sys; sys.path.insert(0, 'scripts')
import memlib as M
print(M.exec_sql('SELECT id, topic, category, memory_text FROM semantic_memory WHERE id IN (...);'))
"
```

Deliberately not `tursodb "$SUPERCHARGED_MEMORY_TURSO_PATH" ...` — see the same note
in `SLEEP.md`. A `$VAR` in a Bash command is refused when nobody can approve it, so
that form works attended and fails in the scheduled preparation run.

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

The "preserve every concrete detail" clause in that rule is the load-bearing part.
Compression drifts: each pass silently drops low-frequency details, and after enough
passes the corpus remembers a sanitized, generic version of events — precisely the
version that fails on the edge case someone actually hits. In *this* corpus the
low-frequency details are the whole value: exact error strings, row ids, CLI flags,
paths, version numbers. A merge that reads more smoothly than its inputs while
holding fewer identifiers is a bad merge.

So treat a row that has been merged **repeatedly** as a smell rather than a win.
Check before proposing: if a cluster member is itself the survivor of an earlier
merge (it supersedes several rows) and it is being merged again, say so in
`why_safe` and prefer leaving it alone.

**3. Present the batch.** One compact table: cluster, ids, proposed topic, merged
length, `why_safe` — **not** the full texts. Ask the user which to apply ("do
1,3,4"). Then per approved merge:

```bash
# Pair --experimental-multiprocess-wal with a --vfs wherever a tursodb line is handed
# out: on Windows the default IO backend refuses the flag outright, so this lookup
# returns nothing and the merge below then backdates the survivor to an empty string.
# `none` is memlib's sentinel for "no --vfs at all", so it must not be passed
# through as a backend name -- tursodb refuses that with `no such VFS: none`.
# Built with set --/"$@": `${v:+--vfs "$v"}` expands to TWO arguments in bash but
# stays ONE in zsh, where tursodb then rejects `--vfs experimental_win_iocp` whole.
set -- --experimental-multiprocess-wal
case "${TURSO_VFS-}" in ""|none|NONE) ;; *) set -- "$@" --vfs "$TURSO_VFS" ;; esac
oldest="$("${TURSO_BIN:-$HOME/.turso/tursodb}" "$SUPERCHARGED_MEMORY_TURSO_PATH" "$@" \
  -q -m list \
  "SELECT min(created_at) FROM semantic_memory WHERE id IN (<id1,id2,id3>);")"
# On Windows run this as `python` — the `python3` stub exits 0 having written nothing.
python3 scripts/remember.py --table semantic --category <c> --topic "<t>" \
  --keywords "<k1, k2, ...>" --source deep-sleep --model <your-model-id> \
  --supersedes <id1,id2,id3> --created-at "$oldest" --text "<merged>"
```

One call, one transaction: the new row is inserted and **all** listed ids are
pointed at it. `--supersedes` also skips the near-duplicate guard, which would
otherwise reject a merge for resembling its own inputs.

**Why the survivor keeps the oldest input's date.** Without `--created-at` it gets
`CURRENT_TIMESTAMP`. Measured on 2026-09-09: two rows dated `2025-03-04` and
`2025-01-15` merged into a survivor with `created_at 2026-09-09`, `age_days 0` —
D6.5's `--staleness` counted a 601-day-old fact in `0-30d`, and D7's oldest-first
ordering sorted it last, behind rows a fraction of its age. Both phases run in
*this* pass, right after this one, and `created_at` is the only signal either has.
Overlap is why rows merge, and overlapping rows are prime candidates for being
stale, so a merge that resets the clock hides staleness exactly where it is most
likely.

**Advisory: this one cannot be enforced in the writer.** A merge and a revision
use the same `--supersedes`. Merging N equivalent facts keeps the oldest date;
*revising* one fact because something was learned (`--supersedes <one id>`, the
store rule in `CLAUDE.md`) is new knowledge and must keep today's, or a
just-corrected row sorts to the back of D7. `remember.py` cannot tell the two
apart from its arguments — this phase can, so the rule lives here. Note
`--created-at` backdates the survivor's `updated_at` too (the writer sets both);
nothing reads a semantic row's `updated_at` for judgment today.

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
# On Windows run this as `python` — the `python3` stub exits 0 having written nothing.
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

## D6 — Recall check and eval-set upkeep

Runs **after** D2 and D3 on purpose: those two phases are what break an eval set.
D2 hard-deletes rows, D3 merges rows into a survivor and marks the originals
superseded — and a case whose `expect` id was purged or superseded looks exactly
like a ranking regression while being nothing of the sort.

Mechanical throughout. The harness measures; **you never change a ranking constant
on your own.**

**Where the eval set lives:** in the DB — `eval_cases` (authored cases, soft-deleted
via `retired_at`) and `eval_runs` (one row per run). Both are covered by the `.dump`
backup, which is the point: the cases are authored and cannot be regenerated, and a
past corpus cannot be re-measured. The only thing still on disk is the
query-embedding cache at `<db parent>/eval/qvec.json`
(`SUPERCHARGED_MEMORY_EVAL_DIR` overrides) — pure derived data, delete it freely.

**If `eval_cases` is empty: skip D6 and say so.** Do NOT generate one automatically.
An LLM writing a query while looking at the row it should retrieve produces lexical
overlap a real user never produces, which biases the whole exercise toward the
keyword layer — see `investigations/2026-08-18-recall-keyword-layer.md`.

### D6.1 — Validate

```bash
python3 <repo>/investigations/eval-harness.py --validate
```

Exit 0 = every case points at a live, current row; go to D6.2. Otherwise it prints
each broken case with a proposed replacement from the supersede chain.

Exit 0 with `nothing to validate` = `eval_cases` holds no cases at all. That is the
fresh-install case the preamble above already tells you to skip D6 on, reported as
what it is rather than as a failure — an empty case set has nothing pointing at a
purged, superseded or reused row. The scoring modes (`--report`, `--sweep`,
`--variants`) still exit 1 there: there is no metric to compute.

- `<old> -> <new>` — the target was merged in D3. Repointing is correct: same fact,
  new row id. Apply it, and refresh that target's stamp:
  `UPDATE eval_cases SET expect_ids='...', expect_stamps=(SELECT created_at FROM
  semantic_memory WHERE id=<new>) WHERE id='<case>';`
- `<old> -> GONE` — purged in D2 or retired. **Retire the case**
  (`UPDATE eval_cases SET retired_at=CURRENT_TIMESTAMP WHERE id='<case>';`). Do not
  repoint it at a loosely-related row; that quietly changes what the metric measures,
  and a metric whose definition drifts is worse than no metric.
- `<old> -> ID REUSED` — the row id came back attached to a different memory.
  `semantic_memory.id` is a rowid alias with **no AUTOINCREMENT**, so SQLite hands a
  purged high id to the next insert; the stored `expect_stamps` is the only thing
  that sees it. Retire the case — the target it was written for is gone.

All three are proposals — show them, let the user confirm, then write.

### D6.2 — Regression report

```bash
python3 <repo>/investigations/eval-harness.py --report
```

Scores the shipped `recall.py` at the configured `RECALL_ALPHA`, appends one row to
`eval_runs`, and diffs against the previous run. Report the three numbers and the
delta. Note the noise floor it prints — with a small set one case is several points,
so a one-case swing is **not** a finding.

The harness flags a regression when recall@5 drops by more than one case. If it does,
and D6.1 was clean, the cause is external: an embedding-model change, or the corpus
becoming much narrower or much broader. Say which you suspect; do not guess in the
report as if it were measured.

### D6.3 — Grow the eval set (propose only, ≤3 cases)

The set's value is its size — one case is `100/N` percentage points of every metric,
and that noise floor is what limits every conclusion drawn from it.

Dispatch **one** worker over semantic rows created since the newest `eval_runs.ran_at`. Its contract:

- It reads `id`, `topic` and `keywords` for its candidate rows — **never
  `memory_text`.**
- It drafts a query a real user would type to find that memory, and it **must not
  reuse a distinctive token that appears in the row's topic or keywords.** Force
  paraphrase. A query built from the row's own wording tests the keyword layer
  against itself and will drag any future alpha upward.
- It returns JSON only: `{"id","class","table","query","expect"}` per case, with
  `class` drawn from the classes already present in `eval_cases`.
- It proposes at most 3.

Show the drafts to the user, then insert each approved one — stamping the targets so
a later id reuse is detectable:

```sql
INSERT INTO eval_cases (id, class, memory_table, query, expect_ids, expect_stamps)
VALUES ('s09','semantic','semantic','<query>','288',
        (SELECT created_at FROM semantic_memory WHERE id=288));
```

For a multi-target case, `expect_ids` and `expect_stamps` are comma-separated **in
the same order**. Re-run D6.1 afterwards to confirm.

### D6.4 — Alpha: ask, never change

Run the sweep **only** if D6.2 flagged a regression, or the user asks:

```bash
python3 <repo>/investigations/eval-harness.py --sweep 0.05,0.1,0.15,0.2,0.3,0.5
```

It prints the recall@5 plateau, the best value inside it, and whether the configured
`RECALL_ALPHA` still sits in that plateau.

- **Inside the plateau → report "no change needed" and stop.** Do not nudge alpha
  toward the argmax; within a plateau the differences are noise.
- **Outside the plateau → ASK the user**, showing the sweep table and the proposed
  value. Changing it is their call, never yours.

If they accept, the persistent place for it is `env` in `~/.claude/settings.json`
(Claude Code's Bash tool never sources `~/.zshrc`, so a profile export would not
reach the scripts). Editing the default in `recall.py` instead is a repo change that
would need a `migration-steps/` note; per-machine calibration belongs in settings.

Record in the report: the numbers, the plateau, what was asked, and what the user
decided.

### D6.5 — Memory-quality metrics

`recall@k` and MRR measure the *retrieval* layer only. The memory-quality layer is
two more numbers, and both are computable from what the DB already holds:

```bash
python3 scripts/sleep.py --staleness
python3 scripts/sleep.py --contradiction-candidates
```

**Staleness distribution** is the age profile of current rows. Age is not wrongness
— an old row about a stable fact is fine — so read it next to D7's confirmed-stale
count, never alone. What matters is the *trend* across passes: a growing `180d+`
bucket with a flat stale count means the corpus is aging well; growing together
means Verify is falling behind.

**Contradiction rate** needs adjudication, because distance cannot tell "same
subject, disagreeing" from "same subject, complementary". Dispatch **one worker per
≤12 pairs**, ids only:

> For each pair, read both rows and decide: do they make claims that cannot both be
> true right now? Sharing vocabulary is not contradiction; adding detail is not
> contradiction; being about the same tool is not contradiction. Return
> `{"ids":[a,b],"verdict":"contradiction|compatible","one_line":"..."}` and nothing
> else.

Report `contradiction rate = confirmed / candidates` with both raw counts, because
the rate alone hides how big the shortlist was. A confirmed contradiction is fixed
the normal way — `remember.py --supersedes` with text that resolves it — and that is
the user's call, not the worker's.

**Neither number is persisted.** `eval_runs` has fixed columns and this work adds no
schema, so there is no stored baseline to diff against: carry the numbers in the
report and compare by eye. If they prove worth trending, that is a schema change and
a `migration-steps/` note of its own.

## D7 — Verify (staleness check)

The one thing ranking cannot tell you: whether a row is still **true**. D6 measures
whether the right row comes back; a row can come back first and still name a flag
that was renamed, a script that moved, or a version nobody runs any more. Steps 1–3
of the Keep / Remove / Move / **Verify** review framework are already covered
(leave it / D2+`--retire` / D3+promotion) — this is the fourth.

Runs **last** on purpose. It is the heaviest phase per unit of value, and unlike D2
and D3 it does not invalidate the eval set *before* D6 measures it. The cost of that
placement: **if you retire anything here, re-run `eval-harness.py --validate`
afterwards** (D6.1's command) so no case is left pointing at a row you just retired.

**1. Get the candidate list.** Read-only, no LLM, no embeddings — this works with
Ollama down:

```bash
python3 scripts/sleep.py --verify-candidates --limit 40
```

Returns `{"table","n_current","n_candidates","n_no_artifacts","limit","candidates":[...]}`,
each candidate carrying `id`, `topic`, `created_at`, `age_days`, `artifacts` (which
classes it mentions: `paths`, `flags`, `env`, `versions`) and `artifact_classes`.
Oldest first, artifact-class count as tiebreak. `n_no_artifacts` is the remainder
that quotes nothing checkable — those rows are not unverifiable, they are simply
**out of scope for a mechanical pass**, and saying so is part of the report.

There is deliberately no "most-retrieved" ordering: a frequently-recalled stale row
is the worst case, but the only way to know which rows those are is to make
`recall.py` write on every query, and keeping recall a pure reader was judged the
better trade. Age plus artifact density is the accepted proxy.

**2. One worker per batch of ≤12 candidates**, spawned in one message. The
orchestrator passes ids only — the worker reads the text itself. Give each worker
this rule verbatim:

> For each memory id you own: read the row, extract every concrete artifact it
> tells a reader to use — script paths, CLI flags, env var names, file paths,
> version numbers, model ids — and CHECK EACH ONE against the repo and the machine
> as it is today. A path: does the file exist? A flag: does `--help` still list it,
> or does the script's argparse still define it? An env var: is it still read
> anywhere? A version: is that still what is installed?
> Report per id, and NOTHING else: `{"id":N,"checked":["--flag","path/x.py"],
> "stale":["--old-flag"],"verdict":"current|stale|unverifiable","evidence":"one line
> naming what you ran or read"}`. `stale` lists only artifacts you CONFIRMED are
> gone or renamed — an artifact you could not check is `unverifiable`, never
> `stale`. On Windows invoke a script as `python`: the `python3` stub prints an
> advert and exits 0, so a flag checked through it is `unverifiable`, not `stale`.
> Do not propose replacement text and do not retire anything.

**3. Present the batch.** One compact table: id, topic, age, verdict, the stale
artifact, and the worker's evidence line — **not** the full texts. Then ask the user
per stale row which action they want, and never pick for them:

- the fact still holds, only the artifact was renamed → **supersede** with corrected
  text (`remember.py --supersedes <id>`), which is the normal fix
- the fact itself no longer applies → **retire** (`sleep.py --retire <id>`)
- unclear → **leave it and say so in the report**

A `stale` verdict is never grounds to retire on your own authority. `SLEEP.md`
Step 6's rule holds here: when it is not clear-cut, ask.

**4. If anything was retired or superseded**, re-run D6.1's validation and rebuild
the topic index (D5's command), because the corpus changed after both ran:

```bash
python3 <repo>/investigations/eval-harness.py --validate
```

On a corpus with no authored eval cases this prints `nothing to validate` and exits
0. That is the expected outcome, not a failing step to stop on: a fresh install has
no cases, and `MIGRATE-EXISTING-MEMORY.md` M4 reaches this step with D6 deliberately
skipped for exactly that reason. Note it in the report and carry on to the topic
rebuild.

**5. Report:** candidates listed vs. checked, rows found stale, what the user chose
per row, workers that died (a gap, not a silent omission), and the `n_no_artifacts`
remainder that a mechanical pass cannot reach.
