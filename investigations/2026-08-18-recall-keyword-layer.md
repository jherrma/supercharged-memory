# Investigation: fix the recall keyword layer

Status: **proposed, nothing implemented.** Opened 2026-08-18.
Owner: unassigned.
Related: `investigations/2026-08-18-embedding-model-candidates.md` (shares Phase 0).

## Why

`recall.py` advertises itself as hybrid ranking — cosine plus a keyword boost. It is
not a boost. The ranking is a strict lexicographic sort:

```python
# scripts/recall.py:136
ORDER BY kw DESC, dist ASC LIMIT {k}
```

Keyword count is the **primary** sort key; cosine distance only breaks ties *within*
an equal keyword count. Because `LIMIT k` is applied after the sort, a row with a
higher keyword count does not merely outrank a better semantic match — it **evicts it
from the result set entirely**.

On top of that the keyword count itself is unweighted, matched by substring, and
computed over an arbitrarily truncated token list.

## Defects, with locations

| # | Defect | Location | Effect |
|---|--------|----------|--------|
| P1 | Lexicographic ordering, not a blend | `recall.py:136` | One extra common-word match beats any cosine gap; `LIMIT` then drops the better match |
| P2 | No IDF weighting — boost is a raw count | `recall.py:52-58` | `service` is worth exactly as much as `vector_distance_cos` |
| P3 | Substring match, not token match | `memlib.py:137` `like_lit()` → `'%tok%'` | `case` matches "because"/"showcase"; `test` matches "latest"/"greatest" |
| P4 | Token list truncated by position | `recall.py:49` `uniq[:8]` | Long queries silently drop tokens 9+, keeping the *earliest* rather than the most discriminating |
| P5 | `len(t) >= 4` filter drops short high-signal tokens | `recall.py:42` | Kills `sql`, `wal`, `api`, `di`, `ef`, `pr`, `mcp` — discriminating terms in this corpus |
| P6 | German stopwords in `STOP` | `recall.py:32-35` | Dead weight once memories are English-only; harmless but noise |

P2 and P3 compound with P1: substring false positives hand junk rows free keyword
count, and P1 makes free keyword count decisive.

## Non-goals

- Not changing the embedding model. That is the sibling investigation.
- Not changing `memory_text`'s 2000-char CHECK. Measured separately; both the 4000
  and 512 directions were rejected (see "Prior findings" below).
- Not introducing FTS5 or an external search index. The corpus is ~550 rows; a table
  scan per query is already fast enough, and a second index is a second thing to keep
  in sync.

## Prior findings this plan depends on

- Corpus size at time of writing: **361** current semantic rows (`superseded_by IS NULL
  AND retired_at IS NULL`) + **184** episodic. `memory_text` averages ~1621 chars.
- Observed `vector_distance_cos` values for *relevant* hits cluster in a narrow band,
  roughly **0.53–0.65**. A blended score is therefore sensitive to its weight.
- Keyword tails (the `Keywords: …` suffix appended by `remember.py`) average **219**
  chars, max **417**.
- Threshold constants in this system are **measured, never guessed** — see the comment
  block at `sleep.py:60` and the memory recorded for issue #5. `alpha` below is the
  same class of constant and gets the same treatment.

---

## Phase 0 — build the evaluation set (blocking; shared with the embedding investigation)

Nothing downstream is verifiable without this. Do not skip it and do not shortcut it
by eyeballing a few queries.

**Deliverable:** a JSONL file, one line per case:

```json
{"id": "q014", "query": "vector_distance_cos fails on null", "table": "semantic", "expect": [<row ids>], "class": "exact-error"}
```

**Where it lives:** *outside* this repo — the expected values are row ids of the live
personal DB, i.e. runtime state, and this repo holds no runtime state. Put it next to
the DB or in a scratch dir and reference it by env var. The **never store PII** rule
applies to the query strings too.

**Composition — target 25–35 cases, deliberately spread across classes:**

| class | what it probes | min cases |
|-------|----------------|-----------|
| `exact-id` | ticket/commit ids, e.g. `869ceb2kd`, `d7f1af5` | 5 |
| `exact-error` | verbatim error strings, e.g. `Invalid vector type`, `Missing type map configuration` | 5 |
| `identifier` | code symbols, e.g. `DocumentProfile`, `vector_distance_cos` | 5 |
| `semantic` | paraphrase with **no** lexical overlap with the target row | 8 |
| `mixed` | natural questions an agent would actually ask at session start | 5 |
| `adversarial` | queries whose tokens appear incidentally in many rows (`service`, `event`, `error`, `case`) | 3 |

The `semantic` and `adversarial` classes are the ones that expose P1–P3. A set made
only of `exact-*` cases will report that the current ranker is perfect, because for
those cases it *is*.

**How to source targets honestly:** pick the target row **first**, then write a query
for it without looking at its wording. Writing the query from the row's text produces
lexical overlap that does not occur in real use.

**Metrics to record per configuration:**

- `recall@1`, `recall@5` (k=5 is the `--k` default at `recall.py:149`)
- `MRR@10`
- per-class breakdown — an aggregate number will hide an exact-id regression behind a
  semantic improvement

**Success criterion for the phase:** the harness reproduces today's `recall.py`
behaviour exactly and reports a baseline. Record that baseline in this file before
changing any ranking code.

---

## Phase 1 — IDF weighting (fixes P2)

Highest value per line changed. No schema change, no new table, no staleness.

The corpus is small enough that document frequency can be computed **at query time**
in a single extra table scan — one `SUM(CASE …)` per token in one statement:

```sql
SELECT SUM(CASE WHEN lower(memory_text) LIKE '%tok1%' THEN 1 ELSE 0 END) AS df1,
       SUM(CASE WHEN lower(memory_text) LIKE '%tok2%' THEN 1 ELSE 0 END) AS df2
FROM semantic_memory WHERE <same base predicate as the search>;
```

Then `w(t) = log(N / (1 + df(t)))`, and `kw_expr()` (`recall.py:52`) emits a weighted
sum instead of a count of 1s.

Notes:
- Compute `df` against the **same** base predicate the search uses (current rows only
  for semantic, coworker scoping if `--coworker` is passed). Otherwise the weights
  describe a different corpus than the one being ranked.
- Keep the `kw` column in the printed output but relabel it — it is no longer "# of
  matching keywords" and the header at `recall.py:137` says it is.
- The degraded (Ollama-down) path at `recall.py:140` must get the same weighting, or
  recall quality silently differs depending on whether Ollama happens to be up.

**Gate:** must not regress any `exact-id` or `exact-error` case. Expect improvement on
`adversarial`.

---

## Phase 2 — blended score (fixes P1)

Replace the lexicographic sort with a single score:

```
ORDER BY (dist - alpha * kw_weighted) ASC
```

**`alpha` must be swept and measured, not chosen.** Sweep it across a range, run the
Phase 0 harness at each value, and report the metric curve. Record the chosen value
*and the curve* in this file, in the style of the `sleep.py:60` comment block.

**The regression that matters most:** `recall.py "869ceb2kd"` returns the right row
today **only because** keyword count dominates absolutely. Blending puts that at risk.
The hypothesis is that IDF rescues it — a token appearing in 3 of ~550 rows carries
enough weight to dominate the blend — but that is a hypothesis, and the `exact-id`
class exists to test it.

If the sweep shows no `alpha` that satisfies both the semantic and the exact-id
classes, the fallback is a two-tier rule (any digit-bearing token match sorts first,
everything else blended) rather than forcing a single score. Prefer the blend; take
the fallback only on evidence.

---

## Phase 3 — token selection (fixes P4, P5)

Nearly free once Phase 1 exists, because both defects are "we guessed which tokens
matter" and IDF now answers that question directly.

- Replace `uniq[:8]` (`recall.py:49`) with "keep the 8 highest-IDF tokens" — drop by
  rarity, not by position in the query string.
- Replace the blanket `len(t) >= 4` filter (`recall.py:42`) with an IDF admission
  test, so `sql`, `wal`, `api`, `ef` survive when they are rare, while genuinely
  common short words fall out on their own.
- Revisit whether `STOP` (`recall.py:28`) is needed at all afterwards — IDF demotes
  stopwords automatically. If it stays, drop the German half (P6).

**Gate:** no regression on any class. This phase is expected to be neutral-to-positive;
if it moves the numbers a lot, something in Phase 1 is wrong.

---

## Phase 4 — word-boundary matching (fixes P3) — CONDITIONAL

**Do not start this phase without evidence from Phase 0.** Measure the false-positive
rate first: for each `adversarial` case, count how many of the matched rows matched
only as a substring of a longer word.

SQLite/Turso `LIKE` has no regex and no word-boundary operator, so every option costs
real complexity:

- pad-and-match (`' tok '`) — misses tokens adjacent to punctuation, which is most of
  them in this corpus (`vector_distance_cos,` / `(recall.py)` / `"Invalid vector type"`)
- normalise into a separate lowercased, punctuation-stripped, space-padded column —
  a schema change plus a backfill, and a second copy of the text to keep in sync
- a real token table — the "second index to keep in sync" this plan's non-goals reject

If the measured false-positive rate is low, close this phase as **won't fix** and say
so here. Long tokens are already fairly safe as substrings; the risk is concentrated
in short ones, which Phase 3 already gates on rarity.

---

## Definition of done

- The Phase 0 harness exists and is runnable in one command.
- Baseline and post-change metrics for every phase are written into this file, per
  class, not just in aggregate.
- The chosen `alpha` is recorded together with the sweep that produced it.
- `recall.py`'s docstring (`recall.py:2-5`) and the printed column header
  (`recall.py:137`) describe what the ranker actually does afterwards.
- `README.md`'s "Hybrid recall" paragraph updated to match.
- If any user-visible behaviour or flag changes: a `migration-steps/` note, in the
  commit *after* the breaking one, per `CLAUDE.md`.

---

# FINDINGS — run 2026-08-18

Harness: `investigations/eval-harness.py` (uncommitted). Eval set: 32 cases, held
outside the repo (row ids are runtime state); point the harness at it with
`EVAL_DIR=<dir> python3 investigations/eval-harness.py <alpha,alpha,...>`.
Corpus at time of run: 362 current semantic, 186 episodic. Read-only against the live
DB. Query embeddings are cached so every variant is scored on identical vectors.

`V2_blend a=0.0` reproduces `V_vector_only` exactly — harness sanity check passes.

## Results (class columns are recall@5)

```
variant                    R@1   R@5   MRR    adversari  exact-err   exact-id  identifie      mixed   semantic
V0_baseline               0.66  0.84  0.74         0.67       1.00       1.00       1.00       0.80       0.62
V_vector_only             0.78  0.94  0.86         0.67       1.00       1.00       1.00       1.00       0.88
V1_idf_lex                0.72  0.72  0.73         0.33       1.00       1.00       1.00       0.40       0.50
V2_blend a=0.05           0.88  0.94  0.90         0.67       1.00       1.00       1.00       1.00       0.88
V2_blend a=0.10           0.88  0.97  0.91         1.00       1.00       1.00       1.00       1.00       0.88
V2_blend a=0.15           0.84  0.97  0.89         1.00       1.00       1.00       1.00       1.00       0.88
V2_blend a=0.20           0.78  0.97  0.85         1.00       1.00       1.00       1.00       1.00       0.88
V2_blend a=0.30           0.78  0.97  0.84         1.00       1.00       1.00       1.00       1.00       0.88
V2_blend a=0.40           0.78  0.84  0.83         0.67       1.00       1.00       1.00       0.40       0.88
V2_blend a=0.60           0.75  0.84  0.80         0.67       1.00       1.00       1.00       0.40       0.88
V2_blend a=1.00           0.72  0.81  0.76         0.67       1.00       1.00       1.00       0.40       0.75
V3_blend+idftok a=0.10    0.88  0.97  0.91         1.00       1.00       1.00       1.00       1.00       0.88
V3_blend+idftok a=0.15    0.84  0.97  0.90         1.00       1.00       1.00       1.00       1.00       0.88
V3_blend+idftok a=0.20    0.84  0.97  0.90         1.00       1.00       1.00       1.00       1.00       0.88
V3_blend+idftok a=0.30    0.81  0.97  0.88         1.00       1.00       1.00       1.00       1.00       0.88
V3_blend+idftok a=0.40    0.81  0.94  0.88         0.67       1.00       1.00       1.00       1.00       0.88
V3_blend+idftok a=0.60    0.78  0.94  0.85         0.67       1.00       1.00       1.00       1.00       0.88
V3_blend+idftok a=1.00    0.72  0.88  0.79         0.67       1.00       1.00       1.00       0.80       0.75
```

**Noise floor: one case = 3.1pp.** Any difference of a single case is noise. Read the
plateaus, not the argmax.

## Two hypotheses in this plan were FALSIFIED

**1. "The keyword layer helps." It does not, as currently shipped.**
`V_vector_only` — cosine alone, keyword layer deleted entirely — beats production
`V0_baseline` on every single metric and on every class (R@1 0.78 vs 0.66, R@5 0.94 vs
0.84, MRR 0.86 vs 0.74). The keyword layer as written is a **net negative**. P1 is not
a refinement issue; the lexicographic sort actively destroys ranking quality.

**2. "Exact-id lookup works only because kw dominates absolutely."** Stated in Phase 2
of this plan as the main regression risk. **Wrong.** `exact-id`, `exact-error` and
`identifier` all score 1.00 under `V_vector_only`, with no keyword layer at all. At
this corpus size bge-m3 embeds ticket ids and quoted error strings well enough on its
own. The feared regression does not exist, and the two-tier fallback proposed in
Phase 2 is unnecessary — do not build it.

Caveat: this is a ~550-row corpus scanned brute-force. Exact-match robustness from
dense vectors alone will not necessarily hold at 5,000 rows. Re-run before assuming it
scales.

## The phase order in this plan was wrong

**IDF alone makes things worse.** `V1_idf_lex` (IDF weighting, ordering still
lexicographic) scores R@5 **0.72** — below both baseline (0.84) and vector-only (0.94),
with `adversarial` collapsing to 0.33 and `mixed` to 0.40. Weighting a sort key that
already dominates absolutely just lets one rare-token match dictate the whole ranking.

**Phase 1 must not ship on its own.** P1 (blending) is the fix; IDF is only valuable
*inside* a blend. Correct order: blend first, then IDF, then token selection —
or land them as one change.

## What wins

`V3_blend+idftok` at **alpha 0.10–0.15**: R@1 0.88/0.84, R@5 0.97, MRR 0.91/0.90.
Against production baseline that is **+0.22 R@1, +0.13 R@5, +0.17 MRR** — 7, 4 and
~5 cases respectively, all well beyond the 1-case noise floor.

`V2_blend` (blend + IDF, original token selection) ties V3 at the optimum. V3 earns its
place on **robustness, not peak score**: as alpha rises past the optimum V2 degrades
faster (a=0.4: R@5 0.84 / MRR 0.83) than V3 (0.94 / 0.88). Dropping `len>=4` and
`uniq[:8]`-by-position buys tolerance to a mis-set alpha, which matters because alpha
will drift as the corpus grows.

**Recommended alpha: 0.125** — the middle of the flat 0.10–0.15 plateau, not the
argmax. 0.10 and 0.15 differ by one case on R@1, i.e. by noise.

## Phase 4 (word-boundary matching): close as WON'T FIX

No measured evidence it matters. Substring false positives are already neutralised by
blending (they carry low IDF and can no longer dominate a lexicographic sort), and
`adversarial` reaches 1.00 without touching `like_lit`. Revisit only if a future eval
set shows otherwise. Do not pay the schema/complexity cost on suspicion.

## Known limitation of this eval set

One case (`s05`, "mocking library hides an untested guard clause", expected row 288)
misses at @5 in **every** variant including pure cosine — row 288 ranks 32nd by
cosine. Inspection shows row 247 (guards that silently never fire) is a defensible
answer to that query that the eval set does not list.

**The eval set was deliberately left unchanged after seeing results.** Editing
`expect` post-hoc is the exact bias this plan warned about; leaving it penalises all
variants equally and keeps the comparison honest. It does mean every R@5 figure above
is understated by up to one case (3.1pp).

Broader caveat: the set was authored in one session by one author from row *topics*.
It is adequate to rank these variants against each other; it is not a general-purpose
recall benchmark.

## Recommended action

Land P1+P2+P3 as **one** change (blend at alpha≈0.125, IDF weights, IDF-based token
selection). Do not land IDF alone. Close P4 won't-fix. P6 (German stopwords) becomes
moot — with IDF-based selection the hand-rolled `STOP` list can likely be deleted
entirely; verify with the harness before removing it.

Still owed if this ships: update `recall.py:2-5` docstring, the `kw` column header at
`recall.py:137`, and README's "Hybrid recall" paragraph — all three currently describe
a keyword *boost*, which is not what the code does now and not what it would do after.

---

# IMPLEMENTED — 2026-08-18 (uncommitted)

P1+P2+P3 landed as one change in `scripts/recall.py`, alpha **0.15** (user's call;
0.125 was the plateau midpoint, 0.15 sits on the same plateau with slightly more
lexical weight). P4 closed won't-fix. P6 resolved by deletion — `STOP` is gone.

- `candidate_tokens()` — every distinct word token, no length filter, no stopword
  list. Capped at `MAX_CANDIDATES = 24`; when the cap bites it drops the *least
  promising* (digit-bearing and longer tokens kept) rather than the last-typed, so
  the P4 defect is not reintroduced at 24.
- `token_weights()` — one extra scan computing `df` per candidate against **the same
  base predicate the search ranks with** (so `--project` / `--coworker` scoping is
  respected), then keeps the `MAX_TOKENS = 8` rarest by `log((N+1)/(df+1))`.
- `kw_expr()` — IDF-weighted sum normalised to 0..1, so `ALPHA` means the same thing
  regardless of query length.
- Ranking — `ORDER BY (dist - ALPHA*kw)`. Output gained a `score` column; `kw` is now
  a 0..1 share, not a count.
- Degraded (Ollama-down) path keeps keyword-only ordering, now IDF-weighted.
- `ALPHA` reads `RECALL_ALPHA` (default 0.15).

**Verification:** the shipped code was scored end-to-end against the eval set —
**R@1 0.84, R@5 0.97, MRR 0.90**, exactly matching the harness's prediction for
`V3_blend+idftok a=0.15`. Re-verified after the `MAX_CANDIDATES` ordering fix.
Manually exercised: normal search, `--project`, `--table`, degraded mode via a dead
`OLLAMA_URL`, `RECALL_ALPHA` override, a token-free query (`kw = 0.0`), and a
29-token query that overflows the candidate cap.

The harness's `V0_baseline` now carries a **frozen inline copy** of the old
`tokens()` and `STOP`. It no longer imports `recall.py` — otherwise the baseline
would track the new code and the comparison would silently evaporate.

Docs updated: `README.md` (intro, script list, env table), `CLAUDE.md` (env list,
"Hybrid recall"), `CLAUDE.md.template` (the one-line ranking description).

**Still owed by whoever commits this:** re-run `bash scripts/install-claude-md.sh` so
`~/.claude/CLAUDE.md` picks up the template change — deliberately not run here,
because rendering from a dirty tree writes a `-dirty` sync stamp.

**Migration note:** judged *not* required. Nothing breaks on an existing install — no
schema change, no CLI change, no manual step; `RECALL_ALPHA` is additive and old
installs keep working with a stale prose description until they re-render. Worth a
second opinion if you disagree, since the bar in `CLAUDE.md` is deliberately low.

## Is RECALL_ALPHA portable across machines/corpora?

**No — but it is far more portable than a raw threshold, and the tolerance is wide.**

- `kw` is normalised to 0..1 per query, so alpha does *not* drift with query length,
  and IDF is recomputed per query against the live corpus, so it does not drift with
  corpus size either. Those are the two variables that would otherwise break it.
- What alpha *is* sensitive to is the **spread of cosine distances** in the corpus,
  which is a property of the embedding model and of how topically diverse the memories
  are. A narrow, single-domain corpus produces tightly clustered distances, and the
  same alpha then buys proportionally more rank movement.
- Measured tolerance here is broad: R@5 held at 0.97 across **alpha 0.10–0.30**, and
  MRR only fell from 0.91 to 0.88 over that range. A different corpus will very likely
  land somewhere inside a similarly wide band.

Practical rule: **0.15 is a good default anywhere; re-measure after an embedding-model
change** (mandatory — different vector space) **or if a corpus is unusually narrow or
unusually broad** (optional). The harness is the re-calibration tool, and it needs an
eval set built for that corpus.

## Follow-up: wired into deep sleep (2026-08-18, uncommitted)

`investigations/eval-harness.py` became a proper CLI — `--validate`, `--report`,
`--sweep`, `--variants` — and `instructions/DEEP-SLEEP.md` gained **D6**, which runs
after D2/D3 because those are what break an eval set. The set now lives at
`<db parent>/eval/` (`SUPERCHARGED_MEMORY_EVAL_DIR`), holding `eval.jsonl`,
`qvec.json` and `history.jsonl`. It is deliberately NOT auto-generated and NOT
auto-tuned: D6 validates, reports against history, proposes ≤3 new cases drafted
from `topic`/`keywords` only, and asks the user before any `RECALL_ALPHA` change.
