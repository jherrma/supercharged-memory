# Investigation: compare embedding-model candidates against bge-m3

Status: **proposed, nothing implemented.** Opened 2026-08-18.
Owner: unassigned.
Prerequisite: **Phase 0 of `investigations/2026-08-18-recall-keyword-layer.md`** (the
evaluation set). The two investigations share it. Do not build a second one.

## Why

The question raised was whether bge-m3 is the right embedder now that memories are to
be stored in English only. The short answer from the 2026-08-18 analysis: **dropping
the German requirement unlocks less than expected, because the binding constraint is
document length, not language.**

Baseline, verified locally with `ollama show bge-m3`:

```
architecture        bert
parameters          566.70M
context length      8192
embedding length    1024
quantization        F16
```

The strong English-only specialists — `mxbai-embed-large`, `bge-large-en-v1.5`,
`gte-large`, `snowflake-arctic-embed-l` — are all BERT with a **512-token** context.
`memory_text` is capped at 2000 chars and averages ~1621; technical English carrying
identifiers, snake_case and quoted error strings tokenises at roughly 3.0–3.6
chars/token, so a typical row is **~550–650 tokens**. Those rows already overflow 512.

That estimate is unverified — no tokenizer is installed on this machine. **Measuring it
is a task in this plan (Phase 1), not an assumption to build on.**

Two traps make this fail silently rather than loudly:

1. **Truncation is silent.** A 512-token model handed 650 tokens truncates and returns
   a normal 1024-dim vector. No error, no dimension mismatch, nothing for
   `memlib.embed()`'s dimension assert (`memlib.py:118-126`) to catch.
2. **Keywords are appended to the END of `memory_text`** — that is the documented
   invariant ("keywords are not a column"). So truncation eats the keyword tail
   *first*, destroying precisely the tokens engineered to make ids and error strings
   findable. Keyword tails average 219 chars, max 417.

A third trap applies to a different subset of candidates:

3. **Asymmetric models need distinct query and document prefixes.** `recall.py:123`
   calls `M.embed(query)` — the *same* function documents go through
   (`memlib.py:118`). bge-m3 is symmetric, so this is correct today.
   `bge-large-en-v1.5`, `nomic-embed-text` and `snowflake-arctic-embed-l` are not.
   Feeding them symmetric input still produces a usable-looking vector and still
   ranks — just worse. Any comparison that does not implement `embed(text, role)`
   for those candidates is measuring the prefix bug, not the model.

## Non-goals

- Not changing the ranking formula. That is the sibling investigation, and it must be
  held **fixed** during this comparison or the two changes confound each other.
- Not changing the 2000-char cap. Rejected in both directions on 2026-08-18: 4000
  dilutes a single dense vector and doubles per-recall context cost; 512 is
  structurally impossible (only **4 of 376** semantic and **0 of 184** episodic rows
  are currently under 512 chars, and keyword tails alone average 219).

## Candidates

Hard filter: must run locally in Ollama (cost, privacy, offline). 1024 dims is a
strong preference — `schema.sql:44,82` declare `F32_BLOB(1024)` and `memlib.py:23`
asserts `DIM = 1024`, so any other width means a full table rebuild.

| candidate | dim | ctx | why it is in the set |
|-----------|-----|-----|----------------------|
| `bge-m3` | 1024 | 8192 | **Control.** Everything is measured as a delta against it |
| `qwen3-embedding:0.6b` | 1024 | 32k | Primary challenger. Drop-in width, ample context, strong English, supports instruction prefixes |
| `snowflake-arctic-embed2` | 1024 | 8192 | Secondary challenger. Drop-in width, long context |
| `mxbai-embed-large` | 1024 | 512 | **Falsification control.** English specialist that *should* lose on truncation. If it wins, the length hypothesis is wrong and this whole plan needs rethinking |
| `nomic-embed-text` (v1.5) | 768 | 8192 | Gated. Only worth measuring if the 1024 candidates disappoint, since 768 forces a schema rebuild. Requires `search_query:` / `search_document:` prefixes |

Context lengths and dimensions above are **claims to verify with `ollama show`**, not
established facts. Correct this table from real output during Phase 1.

`mxbai-embed-large` earns its place by being the candidate most likely to falsify the
central claim. Dropping it would make this a plan that can only confirm itself.

---

## Phase 1 — establish the facts the plan rests on

1. Pull each candidate; record `ollama show <model>` output verbatim into this file
   (architecture, parameters, context length, embedding length, quantization).
2. **Measure the real token count of this corpus.** Install a tokenizer (each model
   family has its own — do not use one model's tokenizer as a proxy for another) and
   report the token-length distribution of `memory_text`, at minimum: median, p90,
   max, and **the fraction of rows exceeding 512 tokens**.
3. For every 512-context candidate, prove truncation empirically rather than by
   arithmetic: embed a long row, embed the same row with its keyword tail removed,
   and compare the vectors. Near-identical vectors prove the tail was never seen.

**Gate:** if step 2 shows the corpus comfortably under 512 tokens, the central premise
is wrong. Stop, correct this document, and re-open the 512-context candidates.

## Phase 2 — per-candidate scratch corpus

For each candidate, on a **copy** of the DB — never the live file:

```bash
cp "$SUPERCHARGED_MEMORY_TURSO_PATH" /path/to/scratch/<candidate>.db
EMBED_MODEL=<candidate> SUPERCHARGED_MEMORY_TURSO_PATH=/path/to/scratch/<candidate>.db \
  python3 scripts/<reembed script>
```

Notes:
- `remember.py:69-72` refuses a table already holding a different `embed_model`. The
  re-embed pass has to rewrite `embed_model` alongside the vector; it cannot go
  through `remember.py`.
- If the candidate is not 1024-dim, `memlib.py:124` aborts. That is correct behaviour
  — the scratch DB needs a schema rebuilt at the candidate's width first, which is
  itself part of the cost being measured.
- Implement `embed(text, role)` before measuring any asymmetric candidate. See trap 3.
- ~550 rows through a local Ollama is minutes, not hours. Record the wall-clock
  anyway — it is the cost of every future `remember.py` call.

## Phase 3 — score against the shared evaluation set

Run the Phase 0 harness against each scratch DB with the ranking formula **held
fixed**. Report per class, not just aggregate:

- `recall@1`, `recall@5`, `MRR@10`
- separate columns for `exact-id`, `exact-error`, `identifier`, `semantic`,
  `adversarial`

A candidate that improves `semantic` while regressing `exact-id` has not won — it has
moved work onto the keyword layer, which is the other investigation's problem, not a
justification here.

## Phase 4 — measure the re-calibration cost (do not skip)

A model swap invalidates every tuned distance constant in the system, because cosine
distances are only comparable within one vector space. For each candidate that
survives Phase 3, produce **new values**, measured the same way the originals were:

| constant | location | current | why it breaks |
|----------|----------|---------|---------------|
| semantic cluster threshold | `sleep.py` `--cluster` default | 0.22 | Connected components chain; measured against a ~230-row bge-m3 corpus |
| episodic cluster threshold | `sleep.py` `--cluster` default | 0.25 | Same |
| near-duplicate threshold | `remember.py:21` `DUP_DIST` | 0.10 | Gates **every write**; too loose refuses real memories, too tight lets duplicates in |
| blend weight `alpha` | sibling investigation | TBD | If Phase 2 of that plan has landed, it is calibrated per vector space too |

Reproduce the original clustering method: sweep the threshold and report where the
largest cluster starts to blow up. For reference, on bge-m3 at ~230 rows: 0.10 found
nothing, 0.22 gave ~11 pairs/triples, 0.30 produced a 33-row blob, 0.35 collapsed 154
of ~200 rows into a single cluster.

A candidate that wins Phase 3 by a small margin and costs a full re-calibration has
probably not earned the switch. Say so explicitly in the recommendation.

## Phase 5 — decision

Switch only if **all** hold:

1. Beats bge-m3 on aggregate `MRR@10` by a margin larger than the run-to-run noise of
   the harness (measure that noise; do not assume it is zero).
2. No regression on `exact-id` or `exact-error`.
3. 1024-dim, or the schema-rebuild cost is explicitly accepted.
4. Phase 4 has produced concrete replacement constants — not a promise to tune later.
5. Any required `embed(text, role)` change is implemented and tested, not deferred.

Otherwise the outcome is **stay on bge-m3**, and that is a legitimate result worth
writing down here so the question is not re-opened from scratch in six months.

## If the switch happens

It is a breaking change to every existing install, and touches runtime state this
repo does not contain:

- `EMBED_MODEL` default in `memlib.py:21`, and `DIM` at `memlib.py:23` if the width
  changes
- `schema.sql:1` header comment plus the two `F32_BLOB` declarations at `:44` and `:82`
- Every existing row must be re-embedded — `remember.py:69-72` will refuse writes
  against a mixed-model table, so a half-migrated DB is a **hard-stopped** DB
- The re-calibrated constants from Phase 4
- `README.md` and `CLAUDE.md.template` both name bge-m3
- The "one embedding model per DB" invariant in `CLAUDE.md` stays — this is a rebuild,
  not a mixed-mode migration

Ship a `migration-steps/YYYY-MM-DD-<slug>.md` note in the commit **after** the
breaking one, with the breaking sha as its first line, per `CLAUDE.md`. Take a backup
before the re-embed; there is no partial-rollback path.
