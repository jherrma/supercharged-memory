# Migrate existing memory — bring file-based Claude memory into this system

**This file is an instruction set for Claude Code.** Reached one of two ways: the
user said yes to the offer in `SETUP.md` Step 7, or they asked directly ("migrate
my memory", "import my old memories"). Work from the repository root.

A user who has been running Claude Code with ordinary file-based memory has a
curated `~/.claude/CLAUDE.md`, a `~/.claude/memory/*.md` set, and often
project-scoped `~/.claude/projects/<slug>/memory/`. None of it is in this system,
and `SETUP.md` Step 6 will not find it — that step looks for a Turso database or
a backup dump, which is a different thing entirely.

This runbook imports those files as semantic memory, then runs the consolidation
phases that make the result usable rather than a few hundred loose rows.

**It is additive and reversible.** Nothing here deletes or edits a source file.
Every row it writes carries `--source migration` and a `--file-reference` back to
the file it came from, so the whole import can be identified afterwards:

```sql
SELECT id, topic, file_reference FROM semantic_memory WHERE source='migration';
```

## Rules

- **The user decides what gets imported.** Do not import a file whose content you
  cannot state as a fact — report it and move on.
- **Never `--force`.** `remember.py`'s near-duplicate guard rejecting a row is a
  correct outcome on a corpus of overlapping notes, not an obstacle.
- **Never truncate.** A file over the 2000-char `MAX_TEXT` is split into separate
  facts, or condensed with the user's agreement.
- **Never mint a `baseline` row on your own.** That category needs
  `--confirm-baseline` and the user's explicit go-ahead, per `CLAUDE.md`.
- The orchestrator does not read memory files in bulk — same subagent contract as
  `SLEEP.md`. The context cost of reading a few hundred files into the session
  running the migration is the reason that contract exists.

## M0 — Preconditions

The database must already exist and be the one the user intends to keep
(`SETUP.md` Step 6, including its candidate check — importing into a database
that is about to be replaced wastes the whole pass).

```bash
python3 scripts/recall.py --status
```

- `EMPTY` or `READY n` → continue.
- **`DEGRADED n` → stop.** Ollama is down, so nothing can be embedded, and a
  semantic row written without an embedding is invisible to recall. Fix Ollama
  first; there is no partial-credit version of this pass.

## M1 — Scan, report, and ask

Mechanical. Needs neither the database nor Ollama:

```bash
python3 scripts/find-existing-memory.py
```

Returns `config_dir`, `n_memory_files`, `n_index_files`, `total_chars`,
`over_max_text`, and a `files` list where each entry carries a `kind`:

| `kind` | What it is | How to treat it |
|---|---|---|
| `memory` | one memory file, one fact | the unit a semantic row is written from |
| `index` | a pointer list (`MEMORY.md`) | read it for structure and topics; **never import it as a fact** — that stores a table of contents |
| `claude` | `CLAUDE.md` outside this system's managed block | the user's own instructions and index; mostly *not* memories (see M5) |

**Show the user the totals before asking**, so the answer is informed — file
count, total characters, and how many files exceed `MAX_TEXT`. A real corpus runs
to a few hundred files and several hundred thousand characters; that is a real
amount of work and several minutes of embedding, not a background detail.

Then ask whether to migrate, and offer the scope choice explicitly:

1. **global only** — `~/.claude/memory/*.md`. The safe default: these facts apply
   everywhere.
2. **global + project-scoped** — also `~/.claude/projects/*/memory/*.md`, written
   with `--project` set so their narrower scope survives the move.
3. **a subset** — the user names topics or files.

If they decline, say so in one line and return to `SETUP.md`. Declining is a
normal outcome; the files keep working exactly as they did.

## M2 — Backup first

Only meaningful if the database already holds rows — a fresh one has nothing to
lose, and `EMPTY` in M0 tells you which case you are in:

```bash
bash scripts/supercharged-memory-backup.sh
```

An import is a bulk write. If the classification turns out wrong at scale, this
dump is what makes "start over" cheap.

## M3 — Classify and write, in subagents

Batch the files into groups of **≤8** and dispatch one worker per batch, all in
one message. Each worker prompt must be self-contained: the file paths it owns,
the category table below, the exact `remember.py` invocation, and your model id
for `--model`.

### Category mapping

The file-based convention and this system's `category` column line up almost
exactly, because both describe the same kinds of fact. A memory file's
frontmatter `type` maps straight across:

| Frontmatter `type` | `--category` |
|---|---|
| `user` | `user` |
| `feedback` | `feedback` |
| `project` | `project` |
| `reference` | `reference` |

Files without frontmatter get judged on content. The two categories with no
file-based equivalent are `pattern` (derived across events — deep sleep's output,
never an import) and `baseline` (see the rules above). A file that fits no
category is reported, not forced into one.

### Worker contract

Give each worker this rule verbatim:

> For each file you own: read it and write ONE semantic memory per fact it
> asserts. Most files are one fact. A file that asserts several unrelated facts
> becomes several rows — do not concatenate them, and do not drop the ones that
> are harder to phrase.
>
> Rewrite the content to be **self-contained**: situation, what is true, how to
> apply it. A memory file often leans on its filename, its index entry, or a
> `[[wiki-link]]` for context, and none of that survives the move. **Resolve
> every `[[link]]`**: either fold in the fact it points at, or name the thing in
> plain words. A row that says "see [[other-memory]]" is useless in a database.
>
> Preserve every concrete identifier verbatim — exact error strings, ids, CLI
> flags, paths, version numbers, dates. These are the whole value of the corpus,
> and they are what a rewrite silently smooths away.
>
> Stay under 2000 chars per row. If a file needs more, split it by fact.
>
> Write each row with:
>
> ```bash
> python3 scripts/remember.py --table semantic --category <c> --topic "<t>" \
>   --keywords "<k1, k2, ...>" --source migration --model <your-model-id> \
>   --file-reference "<absolute path of the source file>" \
>   --created-at "<original date, YYYY-MM-DD HH:MM:SS>" \
>   [--project "<project>"] --text "<self-contained fact>"
> ```
>
> If the writer rejects a row as a near-duplicate, **do not retry with
> `--force`** — report it as `duplicate` and move on.
>
> Report one line per file and nothing else:
> `<file> -> stored(<id>[,<id>]) | duplicate | skipped(<short reason>)`

### Why `--created-at` and `--file-reference` are not optional

`--created-at` should carry the source file's original date, not today's. Take it
from git when the memory directory is a repo, otherwise from the file's mtime:

```bash
git log --diff-filter=A --format=%ad --date=format:'%Y-%m-%d %H:%M:%S' -- <file> | tail -1
```

Importing everything with today's date makes `sleep.py --staleness` and deep
sleep's D7 describe a corpus that looks brand new while holding facts that are a
year old. That is precisely backwards: old imported memory is the most likely to
be stale, and the date is the only signal those phases have.

`--file-reference` is what makes the import auditable and reversible, and it is
the only link back to the file a row came from.

## M4 — Consolidate (the deep-sleep phases that apply)

An import writes N rows from files that overlapped, with no topic index at all.
Follow `DEEP-SLEEP.md`, but only the phases that have anything to do — and say in
the report which you skipped and why:

| Phase | Run it? | Why |
|---|---|---|
| D1 backup | **yes** | D3 writes; this is the undo |
| D2 purge | skip | nothing is superseded or retired yet |
| D3 compaction | **yes** | the point of this phase: the files overlapped, so the rows do too |
| D4 pattern mining | skip | patterns come from episodic events, and there are none |
| D5 re-index | **yes, required** | `CLAUDE.md` loads the topic index every session, and it is empty until rebuilt |
| D6 eval upkeep | skip | no eval cases exist on a fresh install |
| D7 Verify | **yes** | imported memory is old by definition; its paths, flags and versions may already be stale |

Two things to expect at import scale, so they do not read as failures:

- **D3 will propose a lot of merges.** A file-based corpus is deliberately
  granular — one fact per file — so clustering it surfaces every overlap at once.
  Keep `--threshold` at the default and do not raise it to force bigger clusters:
  D3's own warning about connected components chaining applies with full force on
  a few hundred freshly written rows.
- **D5 will be tight.** The topic index has a 500-char hard cap and the script
  refuses the write if you exceed it. With a large imported corpus, getting under
  the cap means genuinely consolidating topics, not shortening keyword lists.
  Budget for that rather than resubmitting a slightly trimmed list.

## M5 — Verify, then deal with the old instructions

**Verify first.** Pick queries the user would actually type for facts you know
were imported, and check that the right rows come back:

```bash
python3 scripts/recall.py "<a question the old memory answered>"
python3 scripts/recall.py --topics
python3 scripts/recall.py --status      # expect READY n
```

Do not proceed to the cleanup below until the user agrees recall works.

**Then the double-instruction problem.** After `SETUP.md` Step 6 the user's
`~/.claude/CLAUDE.md` holds this system's managed block *plus* whatever memory
instructions were already there — commonly an index of memory-file pointers and
rules about how to write them. Both now instruct the agent, they describe
different storage, and neither mentions the other.

Show the user what is now redundant — the pointer index and the file-writing
rules, since the facts they point at are in the database and this system's block
describes how to write new ones. Then let **them** decide when to remove it. Do
not edit that part of `CLAUDE.md` unprompted: it lives outside the managed
markers, which is exactly why `install-claude-md.sh` leaves it alone, and it is
the user's curated file.

What belongs in neither system, and should stay where it is: project instructions
that are not memories — build commands, code style rules, conventions. Those are
instructions, not facts, and nothing here replaces them.

**Leave the source files in place** either way. They cost nothing, they are the
only copy of anything the import skipped, and a user who deletes them the same
day they migrate has no way back if the classification turns out wrong.

## Report

One summary: files scanned, imported (and how many rows they became), skipped as
duplicates, skipped for other reasons and which, workers that died (a gap, not a
silent omission), what D3/D5/D7 changed, and what the user decided about the old
instructions in `CLAUDE.md`.
