# Investigation: compare embedding-model candidates against bge-m3

Status: **proposed, nothing implemented.** Opened 2026-08-18; candidate set reworked
the same day after reading real sizes/params/quantization out of the Ollama registry
(the first set contained a near-clone of the control and a confounded challenger).
Owner: unassigned.
Prerequisite: the shared evaluation set, which **already exists** — the sibling
investigation `investigations/2026-08-18-recall-keyword-layer.md` built it and it
shipped. The cases live in the `eval_cases` table and the harness is
`investigations/eval-harness.py`. Do not build a second one, and do not edit a case
after seeing a candidate's score. Those 32 cases are the **realism guard** (Phase 3
Tier D), not the primary metric — at n=32 the noise floor is one whole case. The
primary metric is derived from the entire corpus; see Phase 3.

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
   `nomic-embed-text` and `embeddinggemma` are not —
   `embeddinggemma` wants a strict `task: … | query: …` vs `title: none | text: …`
   format, and `qwen3-embedding` expects an instruction prefix on the query side only.
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

**Size, parameter count, architecture family and quantization below are verified.** They
were read from the Ollama registry on 2026-08-18 without pulling anything:

```bash
curl -s  "https://registry.ollama.ai/v2/library/<name>/manifests/<tag>"  # layer sizes
curl -sL "https://registry.ollama.ai/v2/library/<name>/blobs/<config-digest>"
#     ^^ -L is required: the blob endpoint 302s to a CDN, and without it you get
#        an empty body that looks like a parse error
```

`dim` and `ctx` are **vendor claims**, not measured. Phase 1 replaces them with real
`ollama show` output.

### Tier 0 — control

| candidate | size | params | family | quant | dim | ctx |
|-----------|------|--------|--------|-------|-----|-----|
| `bge-m3` | 1.16 GB | 566.70M | bert | F16 | 1024 | 8192 |

Everything is measured as a delta against this. It is the only candidate already
installed, and the only one whose `dim`/`ctx` are verified locally.

### Tier 1 — drop-in (1024-dim, no schema rebuild)

| candidate | size | params | family | quant | dim | ctx | role |
|-----------|------|--------|--------|-------|-----|-----|------|
| `qwen3-embedding:0.6b` | 0.64 GB | 595.78M | qwen3 | **Q8_0** | 1024 | 32k | **The challenger.** The only genuinely independent backbone at 1024 dims |
| `snowflake-arctic-embed2` | 1.16 GB | 566.70M | bert | F16 | 1024 | 8192 | **Control variant**, not a challenger — see below |
| `mxbai-embed-large` | 0.67 GB | 334M | bert | F16 | 1024 | 512 | **Falsification control**, conditional on the Phase 1 gate — see below |

**`snowflake-arctic-embed2` is a variant of the control, not a second opinion.** It
reports the same architecture family, the same 566.70M parameters, the same F16, and a
model layer within 2.6 MB of bge-m3's (1 160 285 024 vs 1 157 671 200 bytes). The
digests differ, so the weights are not identical — it is a fine-tune on the same
backbone. Consequence: **if bge-m3 loses for an architectural reason, arctic inherits
that failure.** It cannot falsify anything the control cannot. Keep it only because it
is drop-in and therefore nearly free to measure, and read its result as one number —
how much a fine-tune buys on top of this backbone — not as an independent vote.

**`qwen3-embedding:0.6b` carries a quantization confound.** It is the only candidate
shipped `Q8_0`; every other is `F16`/`BF16`. It is also the largest by parameters
(595.78M) yet nearly the smallest on disk, which is what Q8_0 buys. So the comparison
is not like-for-like, and the two outcomes are **not symmetric**:

- qwen3 **wins** → conservative. It won carrying a handicap; the real F16 margin is at
  least that large. Act on it.
- qwen3 **loses** → **inconclusive**, not a rejection. Re-run against an F16/BF16 build
  (`ollama pull hf.co/Qwen/Qwen3-Embedding-0.6B-GGUF:F16` or equivalent) before
  recording a verdict. Do not close the question on the Q8_0 number alone.

**`mxbai-embed-large`'s falsification role only holds if the corpus really exceeds 512
tokens.** That is the Phase 1 gate, and it now cuts both ways. 2000 chars is roughly
550–650 tokens *if* the 3.0–3.6 chars/token estimate holds — but 2000 chars is also
close enough to 512 tokens that a kinder tokenizer puts the corpus **under** the limit,
in which case mxbai never truncates and is not a falsifier at all, just an ordinary
contender. Phase 1 step 2 decides which of the two it is. Say which, in writing, before
Phase 3 is scored.

**Deliberately only one 512-context BERT-large.** `bge-large` (0.67 GB, 334.09M, bert,
F16) and `snowflake-arctic-embed:335m` (0.67 GB, 334M, bert, F16) are the same size,
family and generation as `mxbai-embed-large`. Measuring all three answers the same
question three times. If mxbai clears the Phase 1 gate and then wins, add one of the
others as a confirmation — not before.

### Tier 2 — gated (768-dim: costs a full schema rebuild)

Only worth pulling if Tier 1 disappoints. Each forces `F32_BLOB(768)`, `memlib.DIM`,
and a re-embed of every row.

| candidate | size | params | family | quant | dim | ctx | why it would be worth the rebuild |
|-----------|------|--------|--------|-------|-----|-----|-----------------------------------|
| `jina-embeddings-v2-base-code` | 0.32 GB | 160.28M | jina-bert-v2 | F16 | 768 | 8192 | **The only code-specialised candidate in the set.** Trained on github-code plus ~150M docstring/code pairs; long context via ALiBi. This is the one that directly answers the question that opened the whole investigation |
| `embeddinggemma` | 0.62 GB | 307.58M | gemma3 | BF16 | 768 | 2048 | Independent modern backbone, MRL (768/512/256/128). **Requires a strict prompt format** (`task: … \| query: …` vs `title: none \| text: …`) — trap 3 applies hard |
| `nomic-embed-text` (v1.5) | 0.27 GB | 137M | nomic-bert | F16 | 768 | 8192 | Long context at the smallest footprint. Requires `search_query:` / `search_document:` prefixes |

**`jina-embeddings-v2-base-code` has a provenance problem.** There is no official
`library/` entry — the only Ollama-registry copy found on 2026-08-18 is a
community re-upload (`unclemusclez/jina-embeddings-v2-base-code`, HTTP 200; the
`jina/…` and `hf.co/jinaai/…` registry paths both 404). An unofficial GGUF re-upload is
untrusted third-party weights. Before pulling it, either verify it against the official
HuggingFace release or pull from HuggingFace directly. Its 160.28M parameter reading
does match the vendor's stated 161M, which is consistent but is not provenance.

It is also **the model whose result matters most and whose cost is highest**: it is the
only one that could beat bge-m3 for a reason specific to this corpus (identifiers,
snake_case, stack traces, error strings) rather than by being generically better — and
it is 768-dim, so acting on a win means the rebuild. Do not gate it behind Tier 1 being
a disaster; gate it behind Tier 1 being *merely fine*.

### Rejected, with reasons

| candidate | size | params | why not |
|-----------|------|--------|---------|
| `qwen3-embedding:4b` / `:8b` | 2.50 / 4.68 GB | 4.0B / 7.6B | Both `Q4_K_M`, and both far wider than 1024. Per-write latency on every `remember.py` call is the cost that matters here, not peak quality |
| `granite-embedding:278m` | 0.56 GB | 277.45M | 768-dim and short-context; multilingual strength is the requirement that was just dropped |
| `paraphrase-multilingual` | 0.56 GB | 277.45M | Same. Optimised for paraphrase similarity, not retrieval |
| `all-minilm` | 0.05 GB | 23M | 384-dim, 256-token context. Included only to note it was considered and is not viable |
| `bge-large-en-v1.5`, `gte-large` | — | — | Same 512-context BERT-large class as `mxbai-embed-large`, which already represents it |

### What this set can and cannot prove

Three independent backbones are actually being compared: **bert/bge-m3** (control, plus
its arctic fine-tune), **qwen3** (Tier 1), and — only if Tier 2 opens —
**jina-bert-v2** and **gemma3**. Everything else in the local Ollama ecosystem at
1024 dims is either the same backbone or the same 512-context BERT-large generation.

So a null result here means "no drop-in 1024-dim local model beats bge-m3", which is a
narrower claim than "bge-m3 is the best choice". Write the recommendation with that
scope, and name Tier 2 as the untested remainder if it stays untested.

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

For each candidate, into a **fresh scratch DB** — never the live file. The tool is
`investigations/embed-swap.py`; it refuses to write to the live path, copies every row
verbatim (ids, `created_at`, every scalar column) and replaces only `embedding` and
`embed_model`, so `eval_cases.expect_ids` / `expect_stamps` stay valid:

```bash
export SUPERCHARGED_MEMORY_EVAL_DIR=/path/to/scratch
python3 investigations/embed-swap.py --model <candidate> --dim <dim> \
    --out /path/to/scratch/<candidate>.db
```

A non-1024 candidate is handled by rendering `schema.sql` at the candidate's width —
that rebuild is itself part of the cost being measured. Embeddings are cached on disk
per (model, sha256(text)), so a re-run after a SQL bug costs seconds, not minutes.

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

## Phase 3 — score against the whole database, not just the authored cases

The 32 authored `eval_cases` are **too small to decide this question.** The shipped
ranking already measures `recall@5 = 0.96875` on them — 31 of 32. The noise floor of a
32-case set is `100/32 = 3.1` percentage points, i.e. **one case**, so any model
difference smaller than "one whole case" is unmeasurable, and `recall@5` has exactly one
case of headroom left. A candidate could be meaningfully better and score identically.

So the primary metric comes from the **entire existing corpus**, using ground truth that
is already in the database and needs no authoring. Live corpus as of 2026-08-18: 369
current semantic rows + 192 episodic = **561 rows.**

The one property that makes this legitimate: **`topic` is a separate column and is never
embedded.** `remember.py:39-40` appends only `--keywords` into `memory_text`; the topic
goes to its own column (`remember.py:96`). So a topic used as a query is text the
embedding model has never seen, not a substring lifted out of the target row. Verify this
still holds before trusting any number from tiers A–C.

### Tier A — known-item retrieval over the whole corpus (n = 561)

For every live row: query = that row's `topic`, target = that row's `id`, ranked against
the full table. 561 cases, noise floor **0.18 pp** — two orders of magnitude finer than
the authored set. Topics are near-unique (381 distinct across 384 semantic rows, 190
across 192 episodic), so the task is well-posed.

Expect this tier to run high for every candidate; it is an easy task. That is fine — its
job is **resolution**, not difficulty. Report `recall@1` and `MRR@10`; `recall@5` will
saturate here too and should be treated as uninformative.

### Tier B — supersede pairs (n = 15)

`WHERE superseded_by IS NOT NULL` gives 15 rows that were each replaced by a newer row
about the same subject, written independently and usually weeks apart. Query with the
**superseded** row's topic, expect the **current** row. This is the only free ground truth
in the DB for "same subject, genuinely different wording" — the thing an embedding is
supposed to be good at and a keyword layer is not.

### Tier C — pattern rows to their cited episodic events (n ≈ 22)

`category='pattern'` rows are derived by deep sleep D4 and carry the episodic ids they
were mined from: 27 of 27 mention `episodic`, 22 carry a 3-or-more digit run. Query with
the pattern row's topic against `episodic_memory`, expect the cited ids — a many-to-one,
deliberately **cross-topic** retrieval. Hardest tier and the one closest to what recall is
actually for. Parse the ids once and check them against `episodic_memory` before scoring;
the schema comment promises they are there, and a promise is not a check.

### Tier D — the 32 authored cases (realism guard)

Keep running `eval-harness.py --report`. Tiers A–C all use topics as queries, which is
**not how anyone actually searches.** Tier D is the only tier written in real query
phrasing, so it is the veto: a candidate that wins A–C but regresses D has learned to
match headlines, not questions. Run `--validate` first — a scratch DB is a copy that gets
re-embedded, and that is exactly what breaks `expect_stamps` quietly.

### Running it

`investigations/whole-db-eval.py` builds all four tiers from the DB itself and scores
them in one pass:

```bash
python3 investigations/whole-db-eval.py --db <scratch.db> --model <candidate> \
    --dim <dim> --blend --json-out res-<candidate>.json
```

Primary score is **pure cosine**: the keyword layer is model-independent and only
compresses the differences being measured. `--blend` additionally reports the shipped
`dist - RECALL_ALPHA*kw` ranking at 0.15, which is what a user actually experiences.
Report both; neither alone is the answer.

### Rules for all tiers

- Ranking formula **held fixed** at the shipped `RECALL_ALPHA=0.15` while comparing
  models. Sweep alpha per candidate only *after* a winner is picked (Phase 4).
- Report per class for Tier D (`exact-id`, `exact-error`, `identifier`, `semantic`,
  `mixed`, `adversarial`) — 5/5/6/8/5/3 cases respectively.
- A candidate that improves `semantic` while regressing `exact-id` has not won — it has
  moved work onto the keyword layer, which is the other investigation's problem, not a
  justification here.
- Tiers A–C are **regenerated from the DB at run time**, not stored. They are derived
  data and must never be written into `eval_cases`, which holds authored cases only.

## Phase 4 — measure the re-calibration cost (do not skip)

A model swap invalidates every tuned distance constant in the system, because cosine
distances are only comparable within one vector space. For each candidate that
survives Phase 3, produce **new values**, measured the same way the originals were:

| constant | location | current | why it breaks |
|----------|----------|---------|---------------|
| semantic cluster threshold | `sleep.py` `--cluster` default | 0.22 | Connected components chain; measured against a ~230-row bge-m3 corpus |
| episodic cluster threshold | `sleep.py` `--cluster` default | 0.25 | Same |
| near-duplicate threshold | `remember.py:21` `DUP_DIST` | 0.10 | Gates **every write**; too loose refuses real memories, too tight lets duplicates in |
| blend weight `alpha` | `recall.py` `RECALL_ALPHA` | 0.15 | Shipped 2026-08-18. It trades cosine-distance units against keyword credit, so it is calibrated per vector space — sweep it with `eval-harness.py --sweep` on each candidate's scratch DB |

Reproduce the original clustering method: sweep the threshold and report where the
largest cluster starts to blow up. For reference, on bge-m3 at ~230 rows: 0.10 found
nothing, 0.22 gave ~11 pairs/triples, 0.30 produced a 33-row blob, 0.35 collapsed 154
of ~200 rows into a single cluster.

A candidate that wins Phase 3 by a small margin and costs a full re-calibration has
probably not earned the switch. Say so explicitly in the recommendation.

## Phase 5 — decision

Switch only if **all** hold:

1. Beats bge-m3 on **Tier A** `recall@1` and `MRR@10` by a margin larger than the
   run-to-run noise of the harness (measure that noise; do not assume it is zero — at
   n=561 the counting floor is 0.18 pp, but embedding and tie-breaking jitter is not).
2. **No regression on Tier D**, and none on its `exact-id` or `exact-error` classes.
   Tier D is the veto: winning A–C while losing D means the candidate matches headlines
   rather than questions.
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

---

# FINDINGS — run 2026-08-18

Executed: Phase 1 (partial), Phase 2, Phase 3 for `bge-m3`, `qwen3-embedding:0.6b` and
`jina-embeddings-v2-base-code`. Phase 4 not reached (no candidate survived Phase 3).

## Phase 1 — verified

| model | arch | params | ctx | dim | quant | on disk |
|-------|------|--------|-----|-----|-------|---------|
| `bge-m3` | bert | 566.70M | 8192 | 1024 | F16 | 1.2 GB |
| `qwen3-embedding:0.6b` | qwen3 | 595.78M | 32768 | 1024 | Q8_0 | 639 MB |
| `jina-embeddings-v2-base-code` (f16) | jina-bert-v2 | 160M | 8192 | 768 | F16 | 322 MB |

**The truncation gate is settled without a tokenizer.** `memory_text` is capped at 2000
chars, so no row can exceed ~700 tokens, and every candidate here has ≥8192 context.
Truncation is impossible for all three. The Phase 1 tokenizer measurement is therefore
only needed if a 512-context candidate is ever revisited.

Provenance: no official Jina GGUF exists for v2-base-code. Used
`hf.co/second-state/jina-embeddings-v2-base-code-GGUF:f16` (WasmEdge org) rather than
the individual re-upload on the Ollama registry. Still third-party weights.

## Phase 3 — results

635 cases: Tier A 561, Tier B 15, Tier C 27, Tier D 32. Primary = **Tier A, pure
cosine**; `blend` = the shipped `dist - 0.15*kw`.

**Pure cosine (isolates the embedding):**

| model | A R@1 | A R@5 | A MRR | B R@1 | C R@1 | D R@1 | D MRR |
|-------|-------|-------|-------|-------|-------|-------|-------|
| **bge-m3** | **0.9127** | **0.9893** | **0.9467** | **0.9333** | **0.3704** | **0.7812** | **0.8594** |
| qwen3-embedding:0.6b | 0.8200 | 0.9537 | 0.8818 | 0.8667 | 0.2222 | 0.4688 | 0.5673 |
| jina-…-base-code | 0.7754 | 0.9323 | 0.8447 | 0.8667 | 0.2222 | 0.7188 | 0.7587 |

**Shipped blend (what a user experiences):**

| model | A R@1 | A R@5 | A MRR | ALL R@1 | D R@1 | D MRR |
|-------|-------|-------|-------|---------|-------|-------|
| **bge-m3** | **0.9340** | **0.9964** | **0.9617** | **0.9087** | 0.8438 | 0.8969 |
| qwen3-embedding:0.6b | 0.8984 | 0.9893 | 0.9395 | 0.8677 | 0.6875 | 0.7878 |
| jina-…-base-code | 0.8610 | 0.9768 | 0.9125 | 0.8441 | **0.9062** | **0.9271** |

### Decision: stay on bge-m3

Phase 5 criterion 1 is not met by any candidate. bge-m3 wins Tier A by **9.3 pp** over
qwen3 and **13.7 pp** over jina on pure-cosine R@1 at n=561, where the counting floor is
0.18 pp. That is not a close call.

### Five things the run established

1. **Pipeline validity check passed exactly.** Re-embedding the corpus with bge-m3 and
   re-scoring Tier D reproduced the live shipped numbers to four decimals
   (R@1 0.8438, R@5 0.9688, MRR 0.8969). The scratch-DB path introduces no drift.

2. **The keyword layer helps every model on every tier.** Blend beats cosine everywhere,
   which independently re-confirms the sibling investigation's result on a corpus 20×
   larger than its 32-case set.

3. **The small authored set would have given the wrong answer.** On blend-Tier D, jina
   *beats* bge-m3 (0.9062 vs 0.8438 R@1) — a two-case difference at n=32. On Tier A at
   n=561 the same model loses by 7.3 pp. This is exactly the failure mode Phase 3 was
   restructured to prevent, and it fired on the first run. **Tier D is a veto for
   regressions, never a promotion on its own.**

4. **The keyword layer is load-bearing for exact-id, unevenly across models.** qwen3
   scores `D:exact-id` **0.0000** on pure cosine and **1.0000** with blend; bge-m3 scores
   0.80 on cosine alone. A model swap therefore silently changes how much of exact-id
   retrieval rests on `RECALL_ALPHA`. Any future candidate must be reported per class on
   *both* rankings, or this is invisible.

5. **Tier C is the hard one, and everything is bad at it.** Cross-topic pattern→episodic
   retrieval tops out at R@1 0.37 (bge-m3, blend). That is where the real headroom is —
   not in the embedder.

### What is NOT concluded

**qwen3's loss is inconclusive by the pre-registered rule, not a rejection.** Two
confounds, both named in the candidate section before the run:

- It was embedded and queried **symmetrically**, with no instruction prefix. Trap 3
  predicted this degrades an instruction-aware model silently.
- It is **Q8_0** while both others are F16.

`D:exact-id` 0.0000 on pure cosine is the signature of a model being used wrong, not
of a bad model. Closing this properly means re-running with a query-side instruction
prefix and an F16 build. Until that is done, the honest statement is "no drop-in
1024-dim local model beat bge-m3 *as used today*", and qwen3 remains open.

jina's loss carries no such asterisk — it is symmetric by design and was run at F16.
Its result is clean: **a code-specialised embedder does not help this corpus**, which is
English prose *about* code rather than code, and it costs a 768-dim schema rebuild.

Tooling written for this run: `investigations/embed-swap.py`,
`investigations/whole-db-eval.py`.
