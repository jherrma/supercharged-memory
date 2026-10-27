# Moving from the Python scripts to the native CLI (2.0)

Read this if your machine was installed from a checkout of this repository: the
managed block in `~/.claude/CLAUDE.md` runs `python3` scripts from `scripts/`, and
Claude Code's `settings.json` has an `env` entry pointing at your database. From 2.0
on, one executable, `supercharged-memory`, does what those scripts did. This file is
the whole move. It takes about ten minutes and nothing in it deletes your memory.

Commands are written as you would type them, with `supercharged-memory` on your
`PATH`. If it is not, type the full path to the executable instead.

## What changes

- **The command.** Every `python3 "<repo>/scripts/<name>.py"` call becomes
  `supercharged-memory <command>`. The instructions in `~/.claude/CLAUDE.md` are
  rewritten to say so; you replace the old block with the new one (step 5).
- **Where configuration lives.** A file named `.env` beside the executable replaces
  the `env` entries in `~/.claude/settings.json`. The executable finds it from its own
  location, whatever directory it is run from. Through a symlink, it is the
  directory of the file the link points to.
- **The backup job.** It runs `supercharged-memory backup` instead of
  `bash scripts/supercharged-memory-backup.sh`.
- **The schema gains one table**, `instructions`, added by `update -y` (step 4) after
  a backup. Nothing existing is altered.
- **Where things default to.** The Python scripts kept dumps in `<repo>/backups`. The
  executable has no repository, so without a setting it would use
  `~/.local/share/supercharged-memory/backups`. Step 2 passes your existing directory so
  nothing moves.
- **Breaking changes are expected in 2.0.** The executable does not imitate script
  behaviour. Output formats, exit codes and a few flags differ; the instructions you
  install in step 5 describe the executable, not the scripts.

## What stays the same

- **The database is the same file at the same path.** It is not moved, converted or
  re-created. `init` adopts it: it opens it only to count the memories and never writes
  to it. The executable and the Python scripts both open it through Turso with
  multiprocess WAL, so Claude Code's `turso` MCP server keeps working unchanged.
- **The schema and every row.** `semantic_memory`, `episodic_memory`, the embeddings,
  `topic_keywords`, the coworker and eval tables. Only the `instructions` table is new.
- **The embedding model.** `bge-m3`, 1024 dimensions, through Ollama.
- **Your episodic mode** (`every-prompt`, `major-actions`, `major-events`, `manual`),
  which you hand to `init` in step 2.
- **The Python scripts stay in the repository.** Nothing in this move deletes or edits
  them, and you should not delete them either; they are the way back (see Rollback).
  A later release removes them.

## Before you start

1. **Take a backup with the Python script, and keep a copy.** From the repository
   root:

   ```
   bash scripts/supercharged-memory-backup.sh
   ```

   It prints `[backup] ok: <file>`. The dump is the only undo for the database. The
   executable names its dumps exactly like the script (`<date>-supercharged-memory.sql.gz`),
   so a dump it takes later the same day replaces this file. Copy the one the script
   just wrote somewhere outside the backup directory.
2. **Copy `~/.claude/CLAUDE.md` and `~/.claude/settings.json` aside.** You edit both.
3. **Write down five values.** You need them in step 2 and to verify:
   - the database path: the `SUPERCHARGED_MEMORY_TURSO_PATH` entry under `env` in
     `~/.claude/settings.json`; with no such entry it is
     `~/.local/share/turso/supercharged-memory.db`;
   - the backup directory: the `BACKUP_DIR` entry there, else `backups` inside the
     repository;
   - the episodic mode: the line `Episodic policy on this machine — ` followed by the
     mode in backticks, inside the managed block in `~/.claude/CLAUDE.md`;
   - `RECALL_ALPHA`, if the `env` block sets it;
   - the memory count: `python3 scripts/recall.py --status` prints `READY <n>` (or
     `DEGRADED <n>` when Ollama is down). Keep `<n>`.

## Steps

Do them in this order. A step that fails changes nothing past it; fix it and repeat it.

### 1. Put the executable in a permanent place

Under a stable name, outside a cloud-synced folder, `Downloads` and temporary
directories. `init` warns about each of those. For example:

```
mkdir -p ~/.local/bin
mv supercharged-memory ~/.local/bin/supercharged-memory
chmod +x ~/.local/bin/supercharged-memory
```

The `.env` is written next to it, and the instructions will contain this path. If you
move or rename the executable later, move the `.env` with it and run `update`. On
Windows the file is `supercharged-memory.exe`; the commands are the same.

### 2. Adopt the database

Run `init` with your three values. Without `-y` it only prints a plan:

```
supercharged-memory init --db-path <database path> --backup-dir <backup directory> --episodic-mode <mode>
```

Read the plan. It should say, for your database:

```
  exists: a memory database (<n> memories) — adopted as it is; opened only to count them, never written
```

and, for the instructions:

```
  the instructions' headline is present — left untouched (init never rewrites text).
  It is a legacy block (synced-at <sha>). The cutover note:
```

(The note that follows is a short version of this file.) A dry run ends with
`Ask the user to confirm`. If it lists a missing Ollama or `tursodb`, those are optional;
see Optional. If it says `exists but is not a memory database`, `--db-path` is wrong;
nothing was written. Then repeat the same command with `-y`:

```
supercharged-memory init --db-path <database path> --backup-dir <backup directory> --episodic-mode <mode> -y
```

It prints `Applied:` and `wrote <dir>/.env: added SUPERCHARGED_MEMORY_TURSO_PATH, BACKUP_DIR,
SUPERCHARGED_MEMORY_VERSION, EMBEDDINGS`. `init` does not touch `~/.claude/CLAUDE.md`
here, and it does not touch the database.

### 3. Take the entries out of Claude Code's settings

A real environment variable beats the `.env` on every run. Claude Code puts the `env`
block of `~/.claude/settings.json` into every tool call, so entries left there silently
override the `.env`.

If `RECALL_ALPHA` was set there, first add the same line to the `.env` beside the
executable (`RECALL_ALPHA=<value>`). Then delete `SUPERCHARGED_MEMORY_TURSO_PATH`,
`BACKUP_DIR` and `RECALL_ALPHA` from the `env` block of `~/.claude/settings.json`.
`PYTHONUTF8` (Windows) is not read by the executable; leave it if you may go back to the
scripts. Restart Claude Code so its tool calls lose the old variables, then check:

```
supercharged-memory config
```

Every line for `SUPERCHARGED_MEMORY_TURSO_PATH`, `BACKUP_DIR` and the `SUPERCHARGED_MEMORY_VERSION`
must end in `[.env]` (`RECALL_ALPHA` too, if you set it). A line ending in `[env]` is
still coming from the environment; a variable named in the `init` plan as "the real
environment sets ..." is the one to find.

### 4. Check what `update` finds, then apply the schema change

```
supercharged-memory update
```

It is a dry run. Expect `1 schema migration(s) pending`, the migration `instructions (0.1.0)`,
and, for CLAUDE.md, `a legacy block (synced-at <sha>)` with a diff against the new
render. It ends with `Ask the user to confirm`. Then:

```
supercharged-memory update -y
```

It backs the database up first (into your backup directory), then creates the table in
one transaction:

```
  migration instructions (0.1.0): applied — table `instructions` exists
```

`update` never writes `~/.claude/CLAUDE.md`. That is step 5.

### 5. Replace the Python-era managed block

The block in `~/.claude/CLAUDE.md` starts with

```
<!-- BEGIN agentic-memory (managed by install-claude-md.sh) -->
<!-- supercharged-memory: synced-at <sha> -->
```

and ends with `<!-- END agentic-memory -->`. Print the new block:

```
supercharged-memory update --print-block
```

In `~/.claude/CLAUDE.md`, replace everything from the `BEGIN` line through the `END`
line, both included, with that output. Whatever you wrote outside the two markers
stays as it is. The new block starts with
`<!-- BEGIN agentic-memory (managed by supercharged-memory) -->` and
`<!-- supercharged-memory: rendered-by <version> -->`, and holds your episodic mode.

Do not run `install-claude-md.sh` again. It renders the Python-era instructions and
cannot fill the executable's placeholders. Starting a Claude Code session after this
step makes the agent use the executable.

### 6. Move the scheduled backup

Disable the old job: the launchd job (macOS) or the crontab entry (Linux) that runs
`scripts/supercharged-memory-backup.sh`. Two jobs would write the same file names into
the same directory. Then run `init` again without `-y`; it changes nothing and prints a
backup schedule for your system under `Backup schedule`: a systemd user timer, a launchd
plist, a scheduled task or a cron line, each running `supercharged-memory backup`. Install the one that fits.

```
supercharged-memory init
supercharged-memory backup
```

`backup` prints the dump's path and a per-table count and ends with
`backup: gzip verified, <n> bytes of SQL inside`. It keeps the 3 newest daily and 4
newest weekly dumps, as the script did.

## How to verify

Start a new Claude Code session, or run these in a shell.

```
supercharged-memory status
```

must print `READY <n>`, with the same `<n>` you noted before starting (memories added
since then add to it). With Ollama off or the model missing it prints `DEGRADED <n>`
and a line saying why (`no embeddings: ...`); the database is fine. A `schema: ...
pending` line means step 4 did not finish. `MISSING` means a wrong path, not lost data:
`supercharged-memory recall --candidates` lists other databases and dumps it can find.

```
supercharged-memory update
```

must end with
`Up to date: the .env and this binary are both <version>, and no schema migration is pending.`
and report `instructions current: the block matches this binary's render`.

```
supercharged-memory recall "<a phrase from a memory you know is stored>"
```

must list that memory. `supercharged-memory config` still shows every value from `.env`
(step 3). The agent's own check is the first line of a new session: it runs
`supercharged-memory status` where it used to run `recall.py --status`.

## What is optional

Nothing below is required; the executable runs without any of it and says what is
missing.

- **Ollama and `bge-m3`.** You already have them. Without them recall is keyword-only
  and new memories are stored without a vector until `supercharged-memory embed --missing`
  fills them in. `init` writes `EMBEDDINGS=off` or `on` from a probe; edit the `.env` to
  change it.
- **`tursodb`.** The executable has its own Turso engine and never calls it. It is only
  for ad-hoc SQL, together with the MCP server.
- **The `turso` MCP server.** Your existing registration keeps working. To make it
  read-only, so writes go through the executable, re-register it; the `init` plan
  prints the exact command, which has this shape:

  ```
  claude mcp remove turso --scope user
  claude mcp add turso --scope user -- <path to tursodb> <database path> --mcp --readonly --experimental-multiprocess-wal
  ```

## Rollback

The Python scripts are still in the repository and were not changed, so going back
needs no download.

1. Put the copy of `~/.claude/CLAUDE.md` back, or paste the old block between its
   markers again, and the `SUPERCHARGED_MEMORY_TURSO_PATH`, `BACKUP_DIR` and
   `RECALL_ALPHA` entries back into the `env` block of `~/.claude/settings.json`.
   Restart Claude Code. Without the path entry the scripts look at
   `~/.local/share/turso/supercharged-memory.db`.
2. Switch the backup job back to the script, and remove the new one.
3. The database needs nothing: the only schema change is the extra `instructions`
   table, which the scripts ignore. Memories stored through the executable without a
   vector show up in `supercharged-memory status` as `unembedded: ...`; run
   `supercharged-memory embed --missing` with Ollama up before you go back, so the
   scripts find a vector on every row.
4. To go back to the data as it was before the move, restore the dump from "Before you
   start" into a fresh file (the target must not exist), with either tool, then point
   `SUPERCHARGED_MEMORY_TURSO_PATH` (and the `turso` MCP registration) at it. This
   discards what was stored since.

   ```
   supercharged-memory restore --dump <the copied dump> --out <a new database file>
   python3 scripts/restore.py --dump <the copied dump> --out <a new database file>
   ```

   Both count every table against the dump and exit non-zero on a mismatch. The
   executable's ends with `RESTORE OK — every row in the dump is present in <file>`.
   Neither replaces the database in use: you point at the new file yourself.
