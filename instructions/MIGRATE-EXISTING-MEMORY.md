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

**It is additive: nothing here deletes or edits a source file.** Every row it
writes carries `--source migration` and a `--file-reference` back to the file it
came from, so the import can be identified afterwards:

```sql
SELECT id, topic, file_reference FROM semantic_memory WHERE source='migration';
```

It is **not** one-click reversible, and do not tell the user it is. **There is no
bulk undo of the imported rows**: `sleep.py --retire` takes one id per call, and
`--purge` only touches rows that are already superseded or retired. What the way
back *is* depends on what M0 found:

- **The source files, always.** Nothing here deletes or edits one, so everything
  the import read from is still on disk, unchanged, either way. That is the
  guarantee to state to the user.
- **`READY n`** (the database already held rows) — M2's pre-import dump restores
  it to exactly its pre-import state, which is why M2 is a step and not a
  suggestion there.
- **`EMPTY`** (the common case: Step 7 runs right after the database is created)
  — M2 is skipped deliberately and there is **no** pre-import dump. Nothing was
  at risk, and "start over" means rebuilding from `schema.sql` and importing
  again.

M4's D1 backup is taken *after* the import, so it is not an undo of it either —
it protects D3.

## Rules

- **The user decides what gets imported.** Do not import a file whose content you
  cannot state as a fact — report it and move on.
- **Never `--force`.** `remember.py`'s near-duplicate guard rejecting a row is a
  correct outcome on a corpus of overlapping notes, not an obstacle.
- **That guard is best-effort under concurrency.** It is a SELECT followed by an
  INSERT with no transaction around the pair, so two workers holding overlapping
  facts can both pass it and both insert. Overlapping content across files is the
  premise of this migration, so this does happen: group overlapping files into
  the same batch (M3) to shrink the window, and treat **D3 in M4 as the real
  dedup** — which is why it is required here rather than optional.
- **Never truncate.** A file over the 2000-char `MAX_TEXT` is split into separate
  facts, or condensed with the user's agreement. Note the writer appends
  `--keywords` into the stored text and counts them inside the same limit, so aim
  at roughly 1800 chars of prose — otherwise the write is refused after the work
  of composing it.
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

Returns `config_dir`, `n_memory_files`, `n_index_files`, `n_claude_files`,
`n_global`, `n_project`, `projects`, `total_chars`, `over_max_text`, `unreadable`
(files it could not open — a gap, report it), and a `files` list where each entry
carries `kind`, `scope` (`global` or `project`) and `project_dir`. Every count and
total covers `kind: memory` files only, so a user who has a `CLAUDE.md` and no
memory files gets `n_memory_files: 0` and is offered nothing.

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

1. **global only** — the `scope: "global"` entries. The safe default: these facts
   apply everywhere.
2. **global + project-scoped** — also the `scope: "project"` entries. Their
   narrower scope has to survive the move, and **`--project` is not how it
   survives**: that column means a tracking-tool work-item id (ClickUp/Jira, e.g.
   `869e7xzp6`), while all the scanner can offer is `project_dir` — Claude Code's
   mangled cwd (`-home-alex-Documents-Repositories-supercharged-memory`). That is
   not an id, and a long one exceeds the column's `length(project) <= 128` check,
   which `remember.py` does not pre-validate: it surfaces as a raw constraint
   failure mid-batch. So leave `--project` unset unless the user names a real
   work-item id, and carry the scope in the **text** instead — the fact's
   situation line names the repository or directory it applies to, which the
   `project_dir` slug is readable enough to derive. `file_reference` keeps the
   exact origin either way.
3. **a subset** — the user names topics, files, or one `project_dir`.

If they decline, say so in one line and return to `SETUP.md`. Declining is a
normal outcome; the files keep working exactly as they did.

## M2 — Backup first

**On `READY n`, run it. On `EMPTY`, skip it deliberately:**

```bash
bash scripts/supercharged-memory-backup.sh      # only when M0 said READY n
```

`EMPTY` is the common case here — Step 7 runs right after the database is created
— and the backup script *fails* on it: it validates a dump by requiring
`INSERT INTO` lines, a database just built from `schema.sql` has none, so the
script retries 5x5s and exits 1. Nothing is at risk in that state, but do not run
it and then explain the error away. A fresh database has nothing to lose, and the
source files are the fallback.

On a database that already holds rows this dump is what makes "start over" cheap:
restoring it puts the database back exactly as it was before the import, which is
what you want if a classification turns out wrong at scale — with
`python3 scripts/restore.py --dump <that dump> --out <fresh path>`, into a **new**
file, never over the live database. On `EMPTY` there is
no earlier state to restore, so the equivalent reset is a rebuild from
`schema.sql` followed by another import. Either way the source files are
untouched, and there is no bulk undo of the rows themselves — see the header.

## M3 — Classify and write, in subagents

**Agree a closed topic vocabulary BEFORE dispatching anything.** This is the one
decision that cannot be fixed afterwards cheaply. `remember.py` accepts any
`--topic` string, so independent workers invent one topic per file — and the topic
index in M4/D5 has a hard 500-char cap across *all* topics, which a few hundred
per-file topics cannot be squeezed into. Derive ten to fifteen topics from the
structure the corpus already has (a `MEMORY.md` index's own section headings are
the obvious source), give every worker that exact list, and tell them to pick the
closest match and say so in the report rather than inventing a new one. Fixing
this later means re-topicking every row by hand.

Batch the files into groups of **≤8** and dispatch one worker per batch, all in
one message. **Group by subject, not alphabetically**: one worker's writes are
serial, so putting the files that cover the same thing in the same batch is what
lets the near-duplicate guard see them at all (across concurrent workers it
cannot — see Rules). Each worker prompt must be self-contained: the file paths it owns,
the topic vocabulary, the category table below, the exact `remember.py`
invocation, and your model id for `--model`.

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
> Do NOT pass `--project`. A `projects/<slug>` directory name is not a work-item
> id, which is what that column means; name the repository or directory in the
> fact text instead (see M1, scope choice).
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
>   --text "<self-contained fact>"
> ```
>
> Compose the memory text with the Write tool into a temp file and pass it as
> `--text "$(cat <file>)"`. Do NOT build it in a bash heredoc: the texts contain
> backticks, `$` and emoji, which a heredoc mangles silently.
>
> If the writer rejects a row as a near-duplicate, **do not retry with
> `--force`** — report it as `duplicate` and move on.
>
> If a write is refused with `Blocked by classifier`, report that row as
> `skipped(blocked by classifier)` and name the fact. Do **not** look for a
> rewording that gets through: the block is a guardrail, and a memory whose
> content is a recipe for evading one should not be persisted for every future
> session to read.
>
> Report one line per file and nothing else:
> `<file> -> stored(<id>[,<id>]) | duplicate | skipped(<short reason>)`

### Why `--created-at` and `--file-reference` are not optional

`--created-at` should carry the source file's original date, not today's. Take it
from git when the file's own directory is a work tree, otherwise from the file's
mtime. Run git **in that directory** — not from the repo root, which is a
different repository: `git log -- ~/.claude/memory/foo.md` from here fails with
`fatal: ... is outside repository`, and a trailing `tail -1` turns that into exit
0 with empty output. The failure then looks exactly like "this file has no git
history", and every row quietly gets today's date — the outcome the
paragraph below warns about:

```bash
d="$(dirname "<file>")"
when=""
if git -C "$d" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  # --reverse | head -1 is the ADD commit; no `tail` swallowing a git failure
  when="$(git -C "$d" log --diff-filter=A --reverse \
            --format=%ad --date=format:'%Y-%m-%d %H:%M:%S' -- "<file>" | head -1)"
fi
[ -n "$when" ] && src=git || { when="$(date -r "<file>" '+%Y-%m-%d %H:%M:%S')"; src=mtime; }
```

Each worker reports which source it used per file (`git` or `mtime`), so a batch
that fell back for every file is visible instead of reading as a clean run.

Importing everything with today's date makes `sleep.py --staleness` and deep
sleep's D7 describe a corpus that looks brand new while holding facts that are a
year old. That is precisely backwards: old imported memory is the most likely to
be stale, and the date is the only signal those phases have.

`--file-reference` is what makes the import auditable and resumable (M3b), and it
is the only link back to the file a row came from.

## M3b — If the run is interrupted, resume from the database

A bulk import will not always finish in one go — a worker can die, and a session
can hit a spend or rate limit mid-batch. Do not restart from the beginning, and
do not assume the batch list is still where you were:

```sql
SELECT file_reference, count(*) FROM semantic_memory WHERE source='migration'
GROUP BY file_reference;
```

That is the resume position, and it is why `--file-reference` is mandatory in M3.
Re-derive the remaining set as *files with no rows*, plus *every file from a batch
whose worker died* — the latter may be half-written, and nothing else can tell you
which. Re-running a fully imported file is cheap and safe: every row comes back
`duplicate`. Re-running a half-written one is the whole point, since the guard
rejects the facts already stored and accepts only the missing ones.

Give resumed workers the same "check what is already there first" query, so they
skip finished files instead of re-reading them.

## M4 — Consolidate (the deep-sleep phases that apply)

An import writes N rows from files that overlapped, with no topic index at all.
Follow `DEEP-SLEEP.md`, but only the phases that have anything to do — and say in
the report which you skipped and why:

| Phase | Run it? | Why |
|---|---|---|
| D0 normal sleep first | skip | D0 exists to sift unprocessed episodic rows into semantic facts; an import writes none, so there is nothing for it to do. Skipping it is the one deviation from `DEEP-SLEEP.md`'s "always enter through a sleep pass" — say so in the report |
| D1 backup | **yes** | D3 writes; this is the undo |
| D2 purge | skip | nothing is superseded or retired yet |
| D3 compaction | **yes** | the point of this phase: the files overlapped, so the rows do too |
| D4 pattern mining | skip | patterns come from episodic events, and there are none |
| D5 re-index | **yes, required** | `CLAUDE.md` loads the topic index every session, and it is empty until rebuilt |
| D6 eval upkeep | skip | no eval cases exist on a fresh install |
| D7 Verify | **yes** | imported memory is old by definition; its paths, flags and versions may already be stale |

**Capture the import manifest before D3 runs.** D3 merges with
`remember.py --supersedes` and `--source deep-sleep`, and passes no
`--file-reference` — so a merged survivor drops out of both the
`source='migration'` audit query at the top of this file and the M3b resume query.
The rows most likely to be merged are exactly the imported ones, since overlap is
why D3 is required here. Write the mapping to a file first, so "which rows came
from the import, and from which file" stays answerable:

```bash
tursodb "$SUPERCHARGED_MEMORY_TURSO_PATH" --experimental-multiprocess-wal -q -m list \
  "SELECT id, file_reference, topic FROM semantic_memory WHERE source='migration' ORDER BY id;" \
  > "Backups/migration-manifest-$(date +%F).csv"
```

(The same query through the `turso` MCP does just as well; what matters is that
the mapping lands in a file. On Windows the open needs
`--vfs experimental_win_iocp` alongside the WAL flag.)

After D3, that file is the audit trail; the database no longer is. Report where
it was written.

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
