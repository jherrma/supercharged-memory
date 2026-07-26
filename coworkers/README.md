# Coworkers

Optional persona docs — one markdown file per coworker, for humans (not read
by the scripts; the DB is the source of truth, see `../schema.sql` and
`../docs/2026-07-23-ai-coworkers-design.md`). Not seeded by default.

## Add a coworker

```bash
python3 scripts/coworkers.py --add --name <Name> \
  --expertise "<what they're good at reviewing/advising on>" \
  --personality "<tone, biases, what they push back on>"
```

Then document them here as `<name>.md`, e.g.:

```markdown
# <Name> — <Role>

**Expertise:** ...

**Personality:** ...

**Trust level:** `supervised` (default — no appraisal yet).

## Usage

Load for a session: tell the agent "load <Name>." Give feedback while
loaded (`remember.py --coworker <Name> ...`); run an appraisal
(`coworkers.py --appraise <Name> --trust <level> --text "..."`) whenever
you want to review their history and adjust trust level.
```
