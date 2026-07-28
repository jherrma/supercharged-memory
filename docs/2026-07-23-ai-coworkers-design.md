# AI Coworkers — design spec

Status: approved by user, pre-implementation.
Builds on: `supercharged-memory` (turso + Ollama memory system, see `../README.md`).

## Purpose

Give named AI personas ("jeff, my review coworker") that:
- Persist across sessions with a fixed expertise/personality.
- Learn from your feedback (and their own observations) specifically, not just
  contribute to the global memory pool.
- Go through periodic **appraisal reviews** that distill their history into a
  current profile and set a trust/autonomy level.

## Schema additions

No changes to the existing `semantic_memory` / `episodic_memory` tables or
columns — zero migration risk to current data. Three new tables:

```sql
CREATE TABLE IF NOT EXISTS coworkers (
  id           INTEGER PRIMARY KEY,
  created_at   TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at   TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  name         TEXT NOT NULL UNIQUE CHECK (length(name) <= 64),
  expertise    TEXT NOT NULL CHECK (length(expertise) <= 256),
  personality  TEXT NOT NULL CHECK (length(personality) <= 1000),
  trust_level  TEXT NOT NULL DEFAULT 'supervised'
               CHECK (trust_level IN ('supervised','trusted','autonomous')),
  active       INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0,1))
);

-- Many-to-many: a memory can be relevant to zero (=global), one, or several coworkers.
CREATE TABLE IF NOT EXISTS memory_coworkers (
  memory_table TEXT NOT NULL CHECK (memory_table IN ('semantic','episodic')),
  memory_id    INTEGER NOT NULL,
  coworker_id  INTEGER NOT NULL REFERENCES coworkers(id),
  PRIMARY KEY (memory_table, memory_id, coworker_id)
);
CREATE INDEX IF NOT EXISTS idx_mc_coworker ON memory_coworkers(coworker_id);

-- One current appraisal per coworker; history preserved via superseded_by.
CREATE TABLE IF NOT EXISTS appraisals (
  id            INTEGER PRIMARY KEY,
  coworker_id   INTEGER NOT NULL REFERENCES coworkers(id),
  created_at    TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  period_start  TEXT,              -- start of feedback window folded in; NULL = since coworker created
  trust_level   TEXT NOT NULL CHECK (trust_level IN ('supervised','trusted','autonomous')), -- snapshot AT this review
  memory_text   TEXT NOT NULL CHECK (length(memory_text) <= 2000),
  superseded_by INTEGER REFERENCES appraisals(id)
);
CREATE INDEX IF NOT EXISTS idx_appr_coworker ON appraisals(coworker_id);
CREATE INDEX IF NOT EXISTS idx_appr_current  ON appraisals(superseded_by);
```

No embedding column on `coworkers` or `appraisals` — both are always looked up
by id/coworker_id, never semantically searched. `memory_coworkers` is a pure
link table, no embedding either.

## Visibility rule

A `semantic_memory`/`episodic_memory` row with **no** rows in `memory_coworkers`
is global (today's behavior, unchanged). A row with 1+ `memory_coworkers`
entries is visible only when one of those coworkers is loaded. This mirrors
the existing `--project` filter pattern, just via a join instead of a column
(needed because a memory can belong to more than one coworker, unlike project).

## Protocols

### Load (`load jeff`)
Triggered by you saying so in chat. Reads (via MCP, plain SQL, no embedding):
1. `coworkers` row for jeff (expertise, personality, trust_level).
2. His current appraisal: `appraisals WHERE coworker_id=? AND superseded_by IS NULL`.
3. His feedback since then: `semantic_memory` joined through `memory_coworkers`
   for jeff, `category='feedback'`, `created_at` > the appraisal's `created_at`
   (or > the coworker's own `created_at` if no appraisal exists yet).

I read all three, confirm out loud ("Loaded jeff — code review, trust:
supervised, N notes since last review"), and adopt his personality/expertise/
trust_level for the rest of the session. Global `baseline` memories are a hard
floor underneath — jeff is a lens on top, never a replacement, and no
trust_level can loosen a baseline rule (e.g. the no-auto-commit policy still
applies regardless of jeff's rating).

If jeff is `active=0` (retired), load still works but the output banners
`RETIRED` so I never silently act as a shelved persona.

### Feedback capture (two sources, same mechanism)
Both go through `remember.py --table <semantic|episodic> --coworker jeff[,anna,...] --source <src>`
— same table choice the main agent already makes for non-coworker memories
(semantic for a durable fact/preference, episodic for a one-off event):
- **Your correction/praise** while jeff is loaded: `--source user-stated`.
  Identical to the memory system's existing feedback-memory pattern — the only
  change is tagging it to jeff instead of leaving it global.
- **Jeff's own observation** ("I noticed this pattern is worth remembering"):
  `--source self-observed`. Gated by `trust_level`:
  - `supervised` → I propose the memory text to you, wait for an ok, then store.
  - `trusted`/`autonomous` → I store it directly and mention it in passing.

`--coworker` accepts multiple comma-separated names (resolved to
`coworkers.id`, refusing on an unknown name) so one memory can be tagged to
several coworkers in a single call.

Dedup (the existing near-duplicate cosine<0.10 guard) scopes its neighbor
search to memories *visible to the tagged coworker(s)* — untagged ∪
tagged-to-any-of-them — not the whole table. Otherwise two coworkers getting
independently similar feedback would false-collide.

### Appraisal review
You trigger it explicitly (not autofilled/scheduled):
1. Pull jeff's `category='feedback'` history since his current appraisal
   (same query as load). Self-observed `reference`-category memories are
   NOT pulled in — they're domain knowledge, not performance signal, and
   would clutter the review.
2. Discuss it conversationally.
3. I draft one distilled appraisal text + a trust_level recommendation.
4. You approve.
5. `coworkers.py --appraise jeff --trust <level> --text "<distilled>"` runs
   one atomic transaction: insert the new `appraisals` row, mark the prior
   current one `superseded_by` (skipped if this is the first-ever appraisal),
   update `coworkers.trust_level`. `period_start` is computed by the script,
   not passed in — the prior current appraisal's `created_at`, or the
   coworker's own `created_at` if this is the first appraisal.

`coworkers.trust_level` is always the fast-read current value;
`appraisals.trust_level` is the historical snapshot at each review point —
both updated together, never drift apart.

## Tooling split

**`coworkers.py` — writes only** (guardrails matter: unknown-name refusal,
atomic appraisal transaction, first-appraisal-vs-supersede logic):
- `--add --name --expertise --personality`
- `--appraise <name> --trust <level> --text "..."`
- `--set-trust <name> <level>` (manual override outside the formal appraisal
  flow, e.g. an acute mistake that shouldn't wait for the next review)
- `--retire <name>` / `--reactivate <name>` (name stays unique forever; no
  hard delete, so history under a retired name is never orphaned — and no
  `ON DELETE` cascade is needed anywhere)

**Reads — ad-hoc MCP SQL, no script**: listing coworkers, loading a
coworker's current state. These are plain chronological `SELECT`s with no
ranking and nothing that a script's guardrails would protect against.

**`remember.py`**: add `--coworker <name>[,<name>...]` flag (a write, needs
embedding — stays in the existing embedding-capable script).

**`recall.py`**: add `--coworker <name>` flag, for a genuine *semantic search*
scoped to a coworker (as opposed to "just load his current state," which is
the plain-SQL load protocol above).

## Edge cases
- Unknown coworker name anywhere → refuse, don't silently no-op.
- Loading a retired coworker → works, banners `RETIRED`.
- First-ever appraisal → insert-only, no supersede step.
- Feedback given with no persona loaded → unchanged, stays global.
- Trust_level never loosens a global baseline rule (hard floor, no exceptions).

## Out of scope (explicitly not building)
- Numeric trust score (three-tier enum only — coarse on purpose).
- Coworker-specific model/effort pinning (persona only; runs on whatever
  model/effort the session is already using).
- Promoting a coworker-scoped memory to global, or vice versa (no migration
  path needed yet — revisit if it comes up in practice).
- Scheduled/automatic appraisals (always user-triggered).
