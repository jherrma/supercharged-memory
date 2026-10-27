# SETUP — machine setup runbook

**This file is an instruction set for Claude Code.** When the user asks to "set up
the memory system" (or similar), execute the steps below in order. The binary is
all a machine needs: `init` creates the database, writes the `.env` beside the
binary and installs the agent instructions. Ollama (semantic recall) and tursodb
(broad ad-hoc reads) are **optional, recommended** — set up without them, and add
them later.

Report the outcome of each step in one line. Stop and surface the problem if a
step fails.

## Rules

- **Ask before installing anything.** Before running any install command, tell
  the user exactly what you're about to install and how, and wait for their
  confirmation. Never install a dependency that is already present.
- **Never use `sudo`** unless the user explicitly approves it for a specific command.
- Detect the platform first (`uname -s`) and pick install commands accordingly.
  The commands below are for macOS/Homebrew and generic Linux; adapt if the
  user's environment differs, and confirm your chosen command with them.
- **On Windows, read [Windows](#windows) first.** Run these steps from Git Bash.
- **Never create, restore or overwrite a database on your own.** `init` adopts an
  existing one and creates one only when the file is absent; it never writes over
  a file that already holds a database.

## Step 1 — Put the binary where it will stay

The instructions `init` writes and the `.env` it creates are tied to the binary's
path, and the `.env` lives beside the binary. So the binary must sit in a
permanent place under a stable name before `init` runs (for example
`~/.local/bin/supercharged-memory`). If `init` warns that the binary is in a
temporary or download directory, move or rename it, then run `init` from there.
Never move it afterwards without running `{{EXE}} update` (it detects the move).

## Step 2 — `init`: the plan, then apply

Run the dry run. It writes nothing:

```bash
{{EXE}} init
```

Relay the plan to the user in a few lines: the database path, the `.env` values it
would write, the CLAUDE.md block it would append, the Ollama and tursodb state,
and the backup schedule it proposes. The plan ends with `Ask the user to confirm`.

Two choices are the user's, not yours:

- **Where the database lives.** Paste an absolute path or accept the default. It
  is a single SQLite file and **must be local, never inside a cloud-synced folder**
  (iCloud Drive, Dropbox, OneDrive, Google Drive): cloud sync corrupts a live
  SQLite file. Pass a different path with `--db-path <file>`; an existing memory
  database there is adopted, not overwritten. Also `--backup-dir <dir>` for where
  `backup` writes its dumps.
- **The episodic-memory policy**, with `--episodic-mode <mode>`. It governs the
  append-only log of *events* only; semantic facts, gotchas and corrections are
  always stored autonomously either way:
  1. `every-prompt` — a note for every prompt / turn.
  2. `major-actions` — every substantive action, not quick questions.
  3. `major-events` — only a feature completed, a bug resolved, a decision, a
     milestone or an incident. *(Default.)*
  4. `manual` — only when the user explicitly asks.

If the plan lists a `Missing` section, it names each optional dependency that is
absent and exactly what the user loses without it. **Read it to the user.** `-y`
is their acknowledgement of every item in it, so do not run `-y` until they have
heard it and agreed to continue without, or until you have done Step 3.

Then apply, with the same flags as the dry run plus `-y`:

```bash
{{EXE}} init -y
```

`init` never overwrites a differing `.env` value, never rewrites existing text in
CLAUDE.md (it appends the instructions only when their headline is absent), and
stops at a database that is not a memory database.

- **`MISSING` or a candidate database listed** → the plan says so. Show the user
  the path and its memory count and ask whether that is their real memory; if
  yes, re-run with `--db-path` pointing at it. Never start empty beside a real
  one.
- **Only a backup is found** → ask whether to restore it rather than start empty:
  `{{EXE}} restore --dump "<newest backup>" --out "<new database path>"`. It prints
  a per-table `in dump` vs `restored` count — read it, and treat a mismatch as a
  failed restore. Then run `init --db-path "<new database path>"`.
- **A CLAUDE.md that already has a block written by the legacy installer** →
  `init` leaves it untouched and prints the cutover note; follow it.

## Step 3 — Ollama and the embedding model (optional, recommended)

Without Ollama the system still works: recall is keyword-only, new memories are
stored without an embedding with a text-only duplicate check, and `status` reads
`DEGRADED n` with its reason. Offer it, and say what it buys: semantic recall,
the vector duplicate guard, clustering for sleep's compaction and pattern
mining, and the recall-quality harness.

Check whether it's installed and running:

```bash
command -v ollama && curl -sf http://localhost:11434/api/version
```

- **Installed and responding** → continue.
- **Installed but not responding** → start it, then continue:
  - macOS: `brew services start ollama`
  - Linux: `ollama serve` (run in the background) or start the system service.
- **Not installed** → tell the user how you'll install it and **ask for
  confirmation first**. Only after they agree:
  - macOS: `brew install ollama` then `brew services start ollama`
  - Linux: `curl -fsSL https://ollama.com/install.sh | sh`
  - Windows (winget; the installer starts the server and registers it at login, so
    there is no service to start): `winget install --id Ollama.Ollama
    --accept-package-agreements --accept-source-agreements`

Then the model — a download, no confirmation needed unless the user asked to
approve every step:

```bash
ollama list | grep -q bge-m3 && echo "present" || ollama pull bge-m3
```

This is the default embedding model (multilingual, 1024-dim); if the user set
`EMBED_MODEL` in the `.env`, pull that one instead. A database holds one model
only.

If `init -y` already ran without Ollama, it wrote `EMBEDDINGS=off` into the `.env`.
Once Ollama and the model are there, fill in what was stored meanwhile; the
command switches the setting back on when nothing is left unembedded:

```bash
{{EXE}} embed --missing
```

Check `{{EXE}} status`: `READY n` with no `unembedded:` line means done.

## Step 4 — tursodb and the read-only MCP server (optional, recommended)

Everything the instructions need goes through this binary. A `turso` MCP server
adds broad ad-hoc reads (queries no command offers); without it only the fixed
retrieval commands (`recall`, `list`, `show`, `coworkers --load`, …) remain.
Register it **read-only**, so writes can only go through the binary.

```bash
command -v tursodb || ls "$HOME/.turso/tursodb" 2>/dev/null
```

- **Found** → continue.
- **Not found** → tell the user you want to install it and **ask first**. Only then:
  `curl -sSL tur.so/install | sh` (macOS, Linux). On Windows see [Windows](#windows).
  The installer may need `export PATH="$HOME/.turso:$PATH"` for this session.

Then register the server with the exact command the `init` plan printed. It has
this shape, with absolute paths (the server is spawned without the user's
interactive `PATH`):

```bash
claude mcp add turso --scope user -- <path to tursodb> <database path> --mcp --readonly --experimental-multiprocess-wal
```

`--readonly` makes the server's write tools fail; `--experimental-multiprocess-wal`
must stay, or the server would lock the binary out of the database. Use
`claude mcp`, never hand-edit `~/.claude.json`. Check with `claude mcp get turso`.
A registration without `--readonly` still works, but "writes go through the
binary" is then only a convention (advisory): say so to the user.

The server's `current_database` tool reports `:memory: (default)` even when it is
attached correctly
([upstream #8061](https://github.com/tursodatabase/turso/issues/8061)); confirm with
a real `SELECT`, never with that tool.

## Step 5 — Check, then offer to migrate existing memory

```bash
{{EXE}} status      # expect EMPTY (fresh) or READY n; DEGRADED n without Ollama
{{EXE}} config      # every setting, its value, and where it came from
```

`config` must show the database path coming from the `.env` (or the default). A
value sourced from the real environment overrides the `.env` on every run.

`init` and `status` do not look for the *other* kind of memory a user may already
have: ordinary file-based memory — a curated `~/.claude/CLAUDE.md`, a
`~/.claude/memory/*.md` set, project-scoped `~/.claude/projects/<slug>/memory/`.
Check whether there is anything to migrate. Read-only, and needs neither the
database nor Ollama:

```bash
{{EXE}} find-existing-memory
```

**Gate on `nothing_to_do`, never on `n_memory_files` alone.** That count covers
what can be imported; `unreadable` and `excluded_dirs` hold what the probe could
not or would not read, and nothing else reports those — so a config dir whose only
memory file is mode 000 comes back `n_memory_files: 0` with a populated
`unreadable`.

- **`nothing_to_do` is `true`** → say so in one line and continue. A `CLAUDE.md`
  on its own counts as `n_claude_files`, not as memory: instructions are not
  facts. Same for an empty `.md` file (`empty` / `n_empty_files`).
- **`nothing_to_do` is `false` but `n_memory_files` is 0** → do **not** say nothing
  to do. Report each `unreadable` / `excluded_dirs` entry's `path` and `reason`
  (plus `n_md` for an excluded directory) and let the user decide: fix the
  permission and re-run the probe, or accept the skip. Do not fix anything on
  their behalf.
- **`n_memory_files` is above 0** → report `n_memory_files`, `total_chars` and
  `over_max_text` (each entry carries the file, its `chars` and a `reason`),
  plus any non-empty `unreadable` / `excluded_dirs` / `empty`. Then **ask whether
  to migrate it into the database**. On yes, print and follow
  `{{EXE}} runbook migrate`. On no, continue; the files keep working as they did.

The migration is additive: it never deletes or edits a source file, and every row
it writes is tagged `source='migration'` with a `file_reference` back to its file.
It is not one-click reversible — there is no bulk undo of the imported rows, and
the way back is the source files — so do not offer it as one.

One thing to flag either way: `init` appended a managed block to
`~/.claude/CLAUDE.md`, so anything the user already had in that file sits alongside
it. If that content is a second set of memory instructions, both are live and
neither mentions the other. `runbook migrate` (M5) handles it, and leaves the
decision to the user.

## Step 6 — The daily backup

`init` printed a schedule for the platform (systemd user timer, launchd, Task
Scheduler or cron) that runs `{{EXE}} backup` daily. It never installs one. Ask
the user, and only then set it up. `backup` takes no arguments and writes a
verified, gzipped dump into `BACKUP_DIR` (from the `.env`), keeping 3 daily and
4 weekly copies. Run it once by hand to prove it works:

```bash
{{EXE}} backup
```

## Step 7 — Offer to create coworkers

Offer a few dedicated AI coworkers (named personas with scoped memory and
trust-gated autonomy). Present it as optional and suggest a couple of concrete
starters, for example:

- **Reviewer** — a meticulous code reviewer. Expertise: reviewing diffs for bugs,
  edge cases, and regressions. Personality: blunt, detail-obsessed, flags what
  breaks rather than what's nice; grudging praise only when a change is airtight.
- **Architect** — a software architect. Expertise: system design, module
  boundaries, trade-offs, and long-term maintainability. Personality: asks "what
  does this cost us in a year?", pushes back on premature complexity and leaky
  abstractions, weighs alternatives before committing.

Other ideas if the user wants more: a **Security** reviewer, a **Testing/QA**
specialist, a **Docs** editor, or a **Performance** analyst.

If the user wants one, construct a coherent personality with the Big-Five method
in `{{EXE}} runbook new-coworker` (don't freehand a grab-bag of adjectives), then
create it:

```bash
{{EXE}} coworkers --add --name <Name> \
  --expertise "<what they review/advise on>" \
  --personality "<tone, biases, what they push back on>"
```

New coworkers start `supervised` until you appraise them. To use one in a
session, the user tells the agent "load <Name>."

## Done

Summarize what was installed vs. already present, the database path, the episodic
mode, which optional dependencies are still missing (and what that costs), and any
coworkers created. Then tell the user:

- **Restart the Claude Code session** to pick up the newly installed
  `~/.claude/CLAUDE.md`.
- To change the episodic policy later, edit the policy line in the managed block
  with the user (or have `{{EXE}} update --print-block --episodic-mode <mode>`
  print the block for that mode and replace the old one). To move the database,
  set `SUPERCHARGED_MEMORY_TURSO_PATH` in the `.env` beside the binary and re-register
  the MCP server on the new path.
- To add Ollama or tursodb later, re-run this runbook's Step 3 / Step 4.

## Windows

Run everything from **Git Bash**. Use PowerShell only for the installers below.

**Install tursodb.** The `sh` installer is the wrong artifact. Turso publishes a
PowerShell installer (no admin rights, installs per-user into `%USERPROFILE%\.turso`):

```powershell
irm https://github.com/tursodatabase/turso/releases/download/v0.7.2/turso_cli-installer.ps1 -OutFile "$env:TEMP\turso-install.ps1"
powershell -NoProfile -ExecutionPolicy Bypass -File "$env:TEMP\turso-install.ps1"
```

Pin the version to whatever is current: the URL is not a moving `latest` alias.
The installer does not add `~/.turso` to the running shell's `PATH`; call the
binary by full path. You never need to write the `.exe` extension.

**`--experimental-multiprocess-wal` needs a VFS here.** Windows' default IO backend
refuses the flag outright (`experimental multiprocess WAL is not supported by the
active IO backend`). Pair it with `--vfs experimental_win_iocp` on every `tursodb`
command you run by hand and on the MCP registration. Do not "fix" this by dropping
the WAL flag: it is what stops tursodb taking an exclusive lock. The binary pairs
them itself (`TURSO_VFS` in the `.env` overrides; `none` drops `--vfs`).
Register the server with absolute paths:

```bash
claude mcp add turso --scope user -- "C:/Users/<you>/.turso/tursodb.exe" "<database path>" --mcp --readonly --experimental-multiprocess-wal --vfs experimental_win_iocp
claude mcp get turso
```

**Paths: mixed form, and OneDrive.** Write paths as `C:/Users/you/...`: Git Bash,
PowerShell and `tursodb.exe` all accept it, whereas `/c/Users/...` only works in
Git Bash. `init` writes the binary's path in that form. Put the database under
`%LOCALAPPDATA%` (`C:/Users/<you>/AppData/Local/turso/supercharged-memory.db`), not
in a user folder: on a managed machine `Documents`, `Desktop` and `Pictures` are
redirected into OneDrive, so a path that reads as local is cloud-synced, and
cloud sync corrupts a live SQLite file. `%LOCALAPPDATA%` is never redirected.
Check with `echo $env:USERPROFILE` against `echo $env:OneDrive` if unsure. Set the
path explicitly there rather than taking the default.

**Console codepage.** A Windows console inherits a legacy OEM codepage (`chcp`
shows `850` or `437`) in which the em-dash in the binary's output has no encoding,
so it can print as garbage. Not verified on Windows: the output is UTF-8; if it
looks wrong, run `chcp 65001` first. It is a display problem, not a data problem.

**Scheduling the backup** on Windows is a Task Scheduler entry running
`{{EXE}} backup`; the `init` plan prints it.
