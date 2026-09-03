commit: c3f6592

# Deep sleep gains D6.5 and D7; three new `sleep.py` flags; two new writing rules

## What broke

**Instruction contract.** `instructions/DEEP-SLEEP.md` has two new phases — `D6.5`
(memory-quality metrics) and `D7` (Verify / staleness check) — and `instructions/SLEEP.md`
Step 5 has a new rule requiring a merge to carry every concrete identifier over verbatim.
An agent running a deep sleep from the old file simply skips both phases; nothing errors,
which is exactly why this note exists.

Note the ordering claim that changed with it: `README.md` used to say "D6 runs last for a
reason". It no longer does — D7 does. D6's placement *after* D2/D3 is unchanged and still
load-bearing; D7 sits after D6 because it is now the only remaining phase that can retire
a row, and putting it earlier would reintroduce the problem D6's placement solves.

**CLI.** `scripts/sleep.py` gained `--verify-candidates` (with `--limit`),
`--contradiction-candidates` and `--staleness`. All three are read-only additions, and
`cluster()` grew an `emit` parameter it defaults to `True` — `--cluster`'s output was
diffed byte-for-byte against the pre-refactor run and is unchanged. No existing flag
changed behavior, so a machine that pulls this and never reads the runbooks keeps working.

**Runtime instructions.** `CLAUDE.md.template` gained two rules under "Writing it" —
store the falsifier, and the altitude test — plus two named content shapes under trigger 6
(authoritative command, pointer). These reach a machine only when the template is
re-rendered, so **an install that skips the step below keeps writing memories under the old
rules while the repo claims otherwise.** That is the one part of this change that fails
silently.

**No schema change.** Nothing to migrate in the database, no table rebuild, no `ALTER`.
The retrieval-frequency counter that the original issue (#10, item B) proposed was
deliberately dropped so `recall.py` stays a pure reader rather than writing on every
query.

## How to resolve

```bash
cd <repo>
git pull
bash scripts/install-claude-md.sh
```

The installer is idempotent and rewrites only the managed block, so re-running it is safe.

## Verification

Three checks — one per thing that broke.

**1. The CLI works and reads the live corpus:**

```bash
python3 scripts/sleep.py --staleness
python3 scripts/sleep.py --verify-candidates --limit 3
```

`--staleness` must print JSON whose four `buckets` sum to `n_current`, and whose
`n_current` matches

```sql
SELECT count(*) FROM semantic_memory WHERE superseded_by IS NULL AND retired_at IS NULL;
```

Do **not** compare it against `recall.py --status`. That prints
`count(semantic_memory) + count(episodic_memory)` — every semantic row including
superseded and retired ones, plus the whole episodic log — so the two agree only on a DB
with no episodic rows and no supersede chain. Measured 2026-09-03: `READY 194`
(126 semantic + 68 episodic) against `n_current` 109. A low `n_current` is the supersede
chain doing its job, not a wrong DB path.

`--verify-candidates` must print at most 3 candidates, oldest `created_at` first, each with
a non-empty `artifacts` list.

Sanity-check the candidate share while you are there: `n_candidates / n_current` should sit
well under half. Measured at 236 of 561 (42%) on the corpus this shipped against. If it
climbs back toward 90%, the artifact patterns have been loosened and the ordering has
stopped meaning anything — see the comment above `ARTIFACT_CLASSES` in `scripts/sleep.py`.

**2. `--cluster` did not regress** (the only pre-existing behavior this commit touched):

```bash
python3 scripts/sleep.py --cluster | head -3
```

must still print `{`, `"table": "semantic",`, `"threshold": 0.22,`.

**3. The new writing rules actually reached the machine** — this is the check worth
running, because it is the one that fails quietly:

```bash
grep -c "falsifier" ~/.claude/CLAUDE.md
grep -n "^## D7 — Verify" instructions/DEEP-SLEEP.md
```

The first must print `1` (not `0` — a `0` means the installer was not run and the template
change is inert). The second must print the D7 heading's line number.

None of these three commands writes anything: all three `sleep.py` primitives are
read-only by construction, and that is enforced statically — none of `verify_candidates`,
`contradiction_candidates` or `staleness` contains an `INSERT`, `UPDATE`, `DELETE`, `BEGIN`
or `COMMIT`.

**Do not try to prove that with a row count.** Multiple Claude instances share this DB by
design, so `recall.py --status` moves on its own while you work — measured during
development: 883 → 908 across a session that wrote 5 rows itself. A count delta on this DB
is not evidence about your own process. This is a separate objection from check 1's: there
the problem is `--status` counting a *different set* than `n_current`, here it is `--status`
counting a *moving* one, and neither makes it a bad status indicator — it is only unusable
as a comparison target.
