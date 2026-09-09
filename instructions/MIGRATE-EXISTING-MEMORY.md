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
  of composing it. That 1800 is not advice the probe leaves you to remember:
  `find-existing-memory.py` flags a file against the same effective budget
  (`MAX_TEXT` minus a stated 200-char keyword reserve) and says which of the two
  limits each flagged file hit, and `remember.py`'s over-cap refusal splits the
  total into your text and the appended keyword line.
- **Never mint a `baseline` row on your own.** That category needs
  `--confirm-baseline` and the user's explicit go-ahead, per `CLAUDE.md`.
- **Not `backfill.py`, even though it looks like the tool for this.** It is the
  bulk `.md` importer `CLAUDE.md` points at on an `EMPTY` database, but it is
  built for a fresh start from arbitrary notes, not for this corpus: it passes
  `force=True` (so it stores every near-duplicate — the opposite of the rule
  above, on a corpus whose premise is overlap), sets `topic=<file stem>` (the
  per-file topic explosion M3 calls unfixable afterwards), writes
  `source='backfill'`, and passes neither `--file-reference` nor `--created-at`
  (so no resume position, no audit trail, and every row dated today). Use it only
  if the user explicitly asks for a raw dump and accepts all four.
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

Returns `config_dir`, `nothing_to_do`, `n_memory_files`, `n_index_files`, `n_claude_files`,
`n_empty_files`, `n_global`, `n_project`, `projects`, `total_chars`, and a `files`
list where each entry carries `kind`, `scope` (`global` or `project`) and
`project_dir`. Every count and total of memory *to import* covers `kind: memory`
files only, so a user who has a `CLAUDE.md` and no memory files gets
`n_memory_files: 0` and is offered nothing.

`nothing_to_do` is the combined "there is nothing here" verdict `SETUP.md` Step 7
gates on: no importable memory **and** nothing skipped. Reaching this runbook means
it was `false`.

Four fields are the probe's gaps — **report each non-empty one**, because nothing
else will:

| field | what it holds |
|---|---|
| `over_max_text` | `{path, chars, reason}`: memory files that will not fit — split or condense, never truncate. The flag is against the **1800-char effective budget**, not the raw 2000-char cap, because `remember.py` appends `--keywords` into the same field the cap counts; each entry's `reason` says which of the two it hit, so a 1900-char file is flagged here instead of being refused mid-import |
| `empty` | `.md` files with no content. `remember.py` exits `refused: --text is empty` on one, so there is nothing to import — they are counted as `n_empty_files`, not as memory |
| `unreadable` | `{path, reason}`: a file that could not be read, a broken symlink, a directory that could not be listed (its contents are invisible, not absent), a symlink loop |
| `excluded_dirs` | `{path, n_md, reason}`: directories left out on purpose — `.git`/`agents` at a memory root (Claude Code's own protected subdirectories), or a symlink pointing above the memory root. `n_md` says how much is in there, so a *topic* directory that happens to be named `agents` is visible rather than silently dropped |

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
   not an id, and a slug **starts with a hyphen**, so `--project <slug>` never
   even reaches the database: argparse reads it as a flag and exits
   `remember.py: error: argument --project: expected one argument`. Only
   `--project=<slug>` gets through, and then a slug long enough to exceed
   `length(project) <= 128` surfaces as an uncaught
   `RuntimeError: ... CHECK constraint failed: length (project) <= 128`, after
   the embedding call was already paid for (verified 2026-09-09; a 53-char slug
   passed the check and stored, which is the point — the column accepts it and it
   is still not an id). So leave `--project` unset unless the user names a real
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
what you want if a classification turns out wrong at scale. **But `restore.py`
refuses to write over an existing file**, so a restore is a new database at a new
path — and until everything that reads the database is pointed at that new file,
the live one is still the database holding the import. Reporting "restored" after
step 1 alone leaves the user exactly where they were. All three steps:

1. `python3 scripts/restore.py --dump <that dump> --out <fresh path>` — into a
   **new** file, never over the live database. Read its per-table `in dump` vs
   `restored` counts and treat a mismatch as a failed restore.
2. Repoint `SUPERCHARGED_MEMORY_TURSO_PATH` at the new file. `~/.claude/settings.json`
   under `env` is the authoritative location — Claude Code runs its Bash tool
   non-interactively and never sources `~/.zshrc` / `~/.bashrc`, so a profile
   export alone leaves every script on the old path. `SETUP.md` Step 4 has that
   procedure and its `jq -e` verification, and Step 6's candidate branch says the
   same thing in one line ("settings.json + profile + MCP"); follow it rather than
   improvising a second one.
3. Re-register the `turso` MCP server on the new path. It carries the path in its
   argv — `tursodb "<path>" --mcp --experimental-multiprocess-wal`, `SETUP.md`'s
   Done section — so a registration left behind keeps every ad-hoc SQL read
   pointed at the imported database while the scripts read the restored one.
   Worse, `tursodb` **creates** a file at whatever path it is opened with, so a
   stale registration (or an old export in a shell) re-creates one at the *old*
   path if the file was moved away: verified 2026-09-09, opening a non-existent
   path left a 0-byte `.db` plus `-tshm`/`-wal` behind, and pointing
   `SUPERCHARGED_MEMORY_TURSO_PATH` at that file made `recall.py --status` print
   `ERROR × Parse error: no such table: semantic_memory` instead of `MISSING` —
   `memlib.db_exists()` only checks that the path exists, so the one branch that
   would have listed the real database as a `CANDIDATE DB` never runs. That is the
   "wrong path, not lost data" failure `README.md` describes, in its most
   confusing form.

Then restart the session and confirm with `python3 scripts/recall.py --status`,
which prints the configured path and where it came from. On `EMPTY` there is no
earlier state to restore, so the equivalent reset is a rebuild from `schema.sql`
followed by another import. Either way the source files are untouched, and there
is no bulk undo of the rows themselves — see the header.

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

Substitute the **absolute** path for `<file>` throughout — the same one M3 passes
to `--file-reference`. A pathspec is resolved relative to `git -C`'s directory, so
`-- <relative path>` under `git -C "$d"` matches nothing and returns empty with
exit 0, which is the same silent "no history" this snippet exists to avoid
(measured: absolute path → `2026-09-08 17:18:33`, the repo-relative one → empty).

```bash
d="$(dirname "<file>")"
when=""
if git -C "$d" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  # --reverse | head -1 is the ADD commit; no `tail` swallowing a git failure
  when="$(git -C "$d" log --diff-filter=A --reverse \
            --format=%ad --date=format:'%Y-%m-%d %H:%M:%S' -- "<file>" | head -1)"
fi
if [ -n "$when" ]; then src=git
elif s="$(stat -c %y "<file>" 2>/dev/null)"; then when="${s%%.*}"; src=mtime  # GNU
else when="$(date -r "$(stat -f %m "<file>")" '+%Y-%m-%d %H:%M:%S')"; src=mtime  # BSD
fi
```

**`date -r "<file>"` is GNU-only — do not use it for the fallback.** GNU `date -r`
takes a *reference file*; BSD/macOS `date -r` takes epoch **seconds** (Apple's
`shell_cmds` `date.c` parses the argument with `strtoq` and prints usage if
anything is left over), so on macOS it fails and `when` stays empty. That is the
*normal* path there: `~/.claude/memory` is not a git work tree, so every file
takes the fallback. `remember.py` used to read an empty `--created-at` as "not
given" and stamp `CURRENT_TIMESTAMP` while the worker still reported `mtime` — a
whole batch dated today reading as a clean run. It now refuses the write
(`refused: --created-at is empty`), which turns that into a reported failure
rather than a wrong date, but the fallback still has to be right. `stat -c %y` (GNU) /
`stat -f %m` (BSD) is the portable pair; the `${s%%.*}` trims GNU's fractional
seconds and zone off `2025-03-04 09:12:07.000000000 +0100`.

**A malformed value is refused too, not stored.** `remember.py` accepts
`YYYY-MM-DD HH:MM:SS`, or a bare `YYYY-MM-DD` which it stores as that day at
`00:00:00`; anything else exits `refused: --created-at '<value>' is not a date
this system can store`, before the embedding call. That guard exists because the
column is plain TEXT with no `CHECK`: measured 2026-09-09, `--created-at
"March 2024"` was stored verbatim, `julianday()` then returned NULL, the row fell
into `sleep.py --staleness`'s `180d+` bucket whatever its real age was, and
`min(created_at)` sorted `'M'` after `'2'` so `oldest_current` ignored it. Since
this snippet builds the string **in shell**, a wrong `stat`/`date` branch is
exactly how such a value gets produced — so keep the trim, and if a file yields
something that is not one of those two shapes, treat it like the empty case
below.

Each worker reports which source it used per file (`git` or `mtime`), so a batch
that fell back for every file is visible instead of reading as a clean run. **If
`when` is empty after the branch above, report the file as `date=none` and omit
`--created-at` entirely** — an empty value is now refused rather than stored, so
the row is simply not written until you decide what its date is.

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
**Skip the files it already returns — do not re-import them.** Re-derive the
remaining set as:

- *files with no rows at all* — the work still to do, and
- *every file from a batch whose worker died* — those may be half-written, and
  nothing else can tell you which.

Then dispatch only that set, and give each resumed worker the query above so it
re-checks its own files before its first write.

**Do not lean on the near-duplicate guard to absorb a re-import.** It compares
embeddings against `DUP_DIST = 0.10`, so it reliably catches only a re-run whose
text is near-identical to what is already stored — and a worker composing the same
fact a second time from the same file does not reproduce its own earlier wording.
Measured 2026-09-09 (bge-m3), four rewrites of one already-imported fact from one
memory file: a near-verbatim restatement came back at cosine `0.0331` and was
**refused**, while a terse restatement (`0.1622`), a rewrite emphasising a
different consequence (`0.1394`) and — worst — the *situation / what is true / how
to apply* shape this runbook's own worker contract asks for, with its own keyword
list (`0.1796`), were all **accepted**. That is a duplicate row the worker reports
as a clean `stored(<id>)`. The guard is best-effort in two separate ways: it sees
wording, not facts, and it is a SELECT followed by an unguarded INSERT (see Rules).
Do **not** raise `DUP_DIST` to compensate: it is a global constant that changes
what every future write and every recall-quality measurement counts as a
duplicate, and it is not this pass's to tune.

The one file worth re-running is a half-written one, and that is a deliberate
trade: the guard rejects most of what is already stored and the missing facts get
written, at the risk of a paraphrase slipping through as above. Say in the report
which files were re-run for that reason, so a duplicate found later has an
explanation — and note that M4's D3 is where such a pair actually gets merged.

## M4 — Consolidate (the deep-sleep phases that apply)

An import writes N rows from files that overlapped, with no topic index at all.
Follow `DEEP-SLEEP.md`, but only the phases that have anything to do — and say in
the report which you skipped and why.

**Every "skip" below is a fact about an `EMPTY` database, not about this runbook.**
M0 admits `READY n` too, and on a database that already held rows four of them
flip — the episodic log, the superseded rows and the eval cases are all the user's
own, and they predate the import. Check which case M0 reported before stating a
skip to the user as a reason.

| Phase | Run it? | Why |
|---|---|---|
| D0 normal sleep first | skip on `EMPTY` | D0 exists to sift unprocessed episodic rows into semantic facts; an import writes none, so on a database it just filled there is nothing to sift. Skipping it is the one deviation from `DEEP-SLEEP.md`'s "always enter through a sleep pass" — say so in the report. **On `READY n`: run it.** Unprocessed episodic rows are exactly what D0 demands a sleep pass for, and compacting without them merges against stale content |
| D1 backup | **yes** | D3 writes; this is the undo |
| D2 purge | skip on `EMPTY` | nothing is superseded or retired in a database the import just filled. **On `READY n`: run the D2 listing** before telling the user nothing is there — earlier revisions and retirements are what it finds, and the decision is theirs every run |
| D3 compaction | **yes** | the point of this phase: the files overlapped, so the rows do too. Carry the oldest input's `--created-at` — see below |
| D4 pattern mining | skip on `EMPTY` | patterns come from episodic events, and an import writes none. **On `READY n`: it applies unchanged** — that episodic log is the user's own history |
| D5 re-index | **yes, required** | `CLAUDE.md` loads the topic index every session, and it is empty until rebuilt |
| D6 eval upkeep | skip on `EMPTY` | `eval_cases` is empty on a fresh install, and D6 says to skip and say so. **On `READY n`: required.** D6 runs after D3 precisely because a merge breaks an eval case. Reproduced 2026-09-09: a case pointing at a row D3 merged makes `eval-harness.py --validate` print `s01 [semantic] NO VALID TARGET LEFT` over `2 -> 3 (superseded; repoint)` and exit 1. Skipping D6 leaves every case D3 just broke pointing at a superseded row, and the next run reads it as a ranking regression |
| D7 Verify | **yes** | imported memory is old by definition; its paths, flags and versions may already be stale. Its step 4 re-runs `eval-harness.py --validate` after a retirement; on `EMPTY` that prints `nothing to validate` and exits 0, which is the expected outcome here and not a failed step |

**D3 must carry each merge's oldest input date.** `DEEP-SLEEP.md`'s D3 has the
command and the reasoning; it matters here more than anywhere, because M3 dated
every imported row on purpose and D3 proposes a lot of merges on this corpus (see
below). A survivor stamped today throws that work away for exactly the rows most
likely to be stale.

**D3 does not cost you the provenance — the database stays the audit trail.** A
`--supersedes` merge neither deletes nor rewrites its inputs: they keep
`source='migration'` and their `file_reference`, gain a `superseded_by` pointing
at the survivor, and both the audit query at the top of this file and the M3b
resume query still return every one of them (verified after a 2-row merge,
2026-09-09). The merge *adds* the old-id → survivor link, so after D3 the database
holds strictly more than any export of it does.

What can destroy that link is a **later** deep sleep's D2, which hard-deletes the
superseded rows the user selects — and those are precisely the imported ones D3
just superseded. So export the mapping as cheap insurance against that day, and
take it **after** D3, when `superseded_by` is populated (before D3 it is all NULL,
and that column is the half worth having):

```bash
"${TURSO_BIN:-$HOME/.turso/tursodb}" "$SUPERCHARGED_MEMORY_TURSO_PATH" \
  --experimental-multiprocess-wal -q -m list \
  "SELECT id, superseded_by, file_reference, topic FROM semantic_memory WHERE source='migration' ORDER BY id;" \
  > "${BACKUP_DIR:-$PWD/Backups}/migration-manifest-$(date +%F).psv"
```

Three details that are wrong if copied carelessly: `-m list` emits `|`-delimited
rows, so the file is not a `.csv`; `tursodb` is not necessarily on `PATH` (every
script resolves it through `TURSO_BIN`, default `~/.turso/tursodb`); and the
backup directory is `BACKUP_DIR`, which every script honours and which a user may
well have moved off the repo. (The same query through the `turso` MCP does just as
well; what matters is that the mapping lands in a file. On Windows the open needs
`--vfs experimental_win_iocp` alongside the WAL flag.)

Report where it was written — and describe it as insurance, not as the audit
trail. M4's D1 backup, one phase earlier, already holds the same mapping and more;
the manifest's only advantage is that it answers a question with `grep` instead of
a `restore.py` into a fresh database. It is optional. D1 is not.

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
python3 scripts/recall.py --table semantic "<a question the old memory answered>"
python3 scripts/recall.py --topics
python3 scripts/recall.py --status      # expect READY n
```

`--table semantic` is deliberate: the import writes no episodic rows, so a
semantic-only query is both the one worth running and the one that cannot trip
over an empty `episodic_memory` (`recall.py`'s default `both` sums over both
tables; the fix for that lives in `recall.py`, not here).

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
