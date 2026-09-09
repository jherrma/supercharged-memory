# SETUP — machine setup runbook

**This file is an instruction set for Claude Code.** When the user asks you to
"run SETUP.md" (or similar), execute the steps below in order. It sets up a
machine to use this memory system: verify/install the two dependencies (Turso and
Ollama), ensure the embedding model is present, gather two configuration choices
(where to store the database, and the episodic-memory policy), then activate the
agent instructions.

Work from the repository root. Report the outcome of each step in one line. Stop
and surface the problem if any step fails — do not continue past a failed
dependency.

> **On Windows, run every `python3` in this file as `python`, and add `--vfs
> experimental_win_iocp` to every `tursodb` command in it.** There is no
> `python3` on Windows: the name is a Microsoft Store alias stub that prints
> `Python was not found` **and exits 0**, so a command reads as a successful,
> empty result and the agent reports work it never did. And
> `--experimental-multiprocess-wal` on its own is refused by Windows' default IO
> backend (`experimental multiprocess WAL is not supported by the active IO
> backend`), so a `tursodb` line without the VFS does nothing at all — pair the
> two flags, never drop the WAL one. See [Windows](#windows) below.

## Rules

- **Ask before installing anything.** Before running any install command, tell
  the user exactly what you're about to install and how, and wait for their
  confirmation. Never install a dependency that is already present.
- **Never use `sudo`** unless the user explicitly approves it for a specific command.
- Detect the platform first (`uname -s`) and pick install commands accordingly.
  The commands below are for macOS/Homebrew and generic Linux; adapt if the
  user's environment differs, and confirm your chosen command with them.
- **On Windows, read [Windows](#windows) first and use the commands there.**
  Enough of this runbook changes on Windows that following the POSIX path verbatim
  fails, and two of the failures are silent — they look like success. `uname -s`
  reports `MINGW64_NT-*` under Git Bash.

## Step 1 — Turso (`tursodb`)

Check whether it's installed:

```bash
command -v tursodb || ls "$HOME/.turso/tursodb" 2>/dev/null
```

- **Found** → report the path and version (`tursodb --version`), continue.
- **Not found** → tell the user you want to install Turso with the command below
  and **ask for confirmation first**. Only after they agree:

  ```bash
  curl -sSL tur.so/install | sh
  ```

  Then re-check that `tursodb` is on `PATH` (or at `~/.turso/tursodb`). If the
  installer added it to a shell profile, the current shell may need
  `export PATH="$HOME/.turso:$PATH"` for this session.

  **On Windows** that `sh` installer is the wrong artifact. Turso publishes a
  PowerShell installer and an MSVC build; use them (no admin rights needed, it
  installs per-user into `%USERPROFILE%\.turso`):

  ```powershell
  irm https://github.com/tursodatabase/turso/releases/download/v0.7.2/turso_cli-installer.ps1 -OutFile "$env:TEMP\turso-install.ps1"
  powershell -NoProfile -ExecutionPolicy Bypass -File "$env:TEMP\turso-install.ps1"
  ```

  Pin the version to whatever is current — check the release list first, the URL
  above is not a moving `latest` alias. The installer does **not** add
  `~/.turso` to `PATH` for the running shell; call the binary by full path, or
  prepend it. Note the binary is `tursodb.exe`, but you never need to write the
  extension: both Windows `CreateProcess` and Git Bash append it, so the
  `~/.turso/tursodb` path the scripts default to resolves as-is.

## Step 2 — Ollama

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

  Then start it (if needed) and confirm `curl -sf http://localhost:11434/api/version`
  responds.

  **On Windows**, install it with winget and do not look for a service manager:

  ```powershell
  winget install --id Ollama.Ollama --accept-package-agreements --accept-source-agreements
  ```

  The installer starts the server itself (an `ollama` process plus an `ollama app`
  tray process) and registers it to run at login, so there is no
  `brew services start` equivalent to run and nothing to add to a profile. It
  lands in `%LOCALAPPDATA%\Programs\Ollama`, which the installer puts on the
  user `PATH` — but not on the `PATH` of already-running shells, so a session
  started before the install must call it by full path.

## Step 3 — Embedding model (bge-m3)

Check whether the model is already pulled:

```bash
ollama list | grep -q bge-m3 && echo "present" || echo "missing"
```

- **Present** → continue.
- **Missing** → this is a download (no confirmation needed unless the user has
  asked to approve every step). Pull it:

  ```bash
  ollama pull bge-m3
  ```

This is the default embedding model (multilingual, 1024-dim). If the user has
overridden `EMBED_MODEL`, pull that model instead.

## Step 4 — Choose where to store the database

**Ask the user to paste an absolute path** for the live Turso database file, then
hold onto it as `SUPERCHARGED_MEMORY_TURSO_PATH` for the rest of this runbook. Guidance to give them:

- It's a single SQLite file — end the path with a filename.
- **It must be a local path, never inside a cloud-synced folder** (iCloud Drive,
  Dropbox, OneDrive, Google Drive) — cloud sync corrupts a live SQLite file.
- The parent directory will be created if missing.

Where it conventionally goes, by platform — offer the matching one if the user
just wants a default:

| Platform | Conventional location |
|---|---|
| Linux | `${XDG_DATA_HOME:-~/.local/share}/turso/supercharged-memory.db` |
| macOS | `~/.local/share/turso/supercharged-memory.db` (what the code falls back to; `~/Library/Application Support/turso/` is the platform-native spot if the user prefers it) |
| Windows | `%LOCALAPPDATA%\turso\supercharged-memory.db`, i.e. `C:/Users/<you>/AppData/Local/turso/supercharged-memory.db` |

`memlib.py` computes its built-in default as `$XDG_DATA_HOME/turso/...`, falling
back to `~/.local/share/turso/...`. That resolves on Windows too — to
`C:\Users\<you>\.local\share\turso` — but it is not where a Windows user or any
other Windows tool would look, so set the path explicitly there rather than
accepting the default.

The cloud-sync rule bites hardest on Windows: `Documents`, `Desktop` and
`Pictures` are silently redirected into OneDrive on most managed machines, so a
path that reads as local may not be. `%LOCALAPPDATA%` is never redirected, which
is the other reason to prefer it. Check with `echo $env:USERPROFILE` against
`echo $env:OneDrive` if unsure.

Do not proceed with a path that sits under an obvious cloud-sync directory —
flag it and ask for another.

Persist the choice so the scripts and future sessions agree on it (they read the
`SUPERCHARGED_MEMORY_TURSO_PATH` env var, defaulting otherwise). Detect the
user's login shell and pick the matching profile file:

- **zsh** (`$SHELL` ends in `zsh`) → `~/.zshrc`
- **bash** (`$SHELL` ends in `bash`) → `~/.bashrc` (or `~/.bash_profile` on macOS)
- other/unknown → ask the user which profile file to use.

**Ask before editing the profile**, then append the export (idempotently — don't
add a second line if one is already there) and also set it for the current
session:

```bash
# pick PROFILE for the detected shell, e.g. PROFILE="$HOME/.zshrc" or "$HOME/.bashrc"
grep -q 'export SUPERCHARGED_MEMORY_TURSO_PATH=' "$PROFILE" 2>/dev/null \
  || printf '\n# supercharged-memory: live Turso DB location\nexport SUPERCHARGED_MEMORY_TURSO_PATH="<pasted-path>"\n' >> "$PROFILE"
export SUPERCHARGED_MEMORY_TURSO_PATH="<pasted-path>"     # also set it for this session
```

If the user declines to edit a profile, set it only for this session and tell
them they'll need to export it themselves in future shells.

**On Windows** there is usually no profile to edit — `$PROFILE`
(`Documents\WindowsPowerShell\Microsoft.PowerShell_profile.ps1`) does not exist by
default, and creating one under `Documents` puts it in OneDrive's path on a
managed machine. Prefer the durable, shell-independent store instead:

```powershell
[Environment]::SetEnvironmentVariable('SUPERCHARGED_MEMORY_TURSO_PATH', '<pasted-path>', 'User')
$env:SUPERCHARGED_MEMORY_TURSO_PATH = '<pasted-path>'   # and for this session
```

That reaches every future shell, PowerShell and Git Bash alike. It does **not**
reach already-running processes, this session's Claude Code included — which is
why the `settings.json` step below is the one that actually matters.

**Also add it to `~/.claude/settings.json`** — this is not optional, and a profile
export alone is *not* enough. Claude Code runs its Bash tool in a **non-interactive**
shell, and `~/.zshrc` / `~/.bashrc` are only sourced for *interactive* shells. Without
this the agent's own `recall.py --status` reads the fallback default path, reports
`MISSING`, and offers to restore a backup over a perfectly healthy database. Merge the
`env` key into the existing JSON (don't overwrite the file):

```json
{
  "env": {
    "SUPERCHARGED_MEMORY_TURSO_PATH": "<pasted-path>"
  }
}
```

Verify with `jq -e '.env.SUPERCHARGED_MEMORY_TURSO_PATH' ~/.claude/settings.json`.

**On Windows, add `"PYTHONUTF8": "1"` to the same `env` block.** Every script here
prints em-dashes, and a Windows console inherits the legacy OEM codepage (commonly
`cp850` or `cp437`) in which `U+2014` is undefined — so Python raises
`UnicodeEncodeError` and the script dies on its first line of output. `memlib.py`
reconfigures its own streams to survive this, but `PYTHONUTF8=1` fixes it for the
whole interpreter, including any script that prints before importing `memlib`.

## Step 5 — Choose the episodic-memory policy

Episodic memory is the append-only log of *events*. Ask the user how aggressively
they want it stored, and record their choice as one of these keys (this governs
episodic only — semantic facts, gotchas, and corrections are always stored
autonomously either way):

1. **`every-prompt`** — store an episodic note for *every* prompt / turn.
2. **`major-actions`** — recommended, store every substantive action, but skip quick questions
   and clarifications.
3. **`major-events`** — store only major events: a feature completed, a bug
   resolved, a decision, a milestone or incident. *(Default / recommended.)*
4. **`manual`** — store episodic memory only when the user explicitly asks.

Map their answer (1–4 or the name) to the matching key and hold it as
`EPISODIC_MODE`.

## Step 6 — Activate the agent instructions

Render the template into `~/.claude/CLAUDE.md` with the two choices baked in
(idempotent — safe to re-run):

```bash
SUPERCHARGED_MEMORY_TURSO_PATH="<pasted-path>" EPISODIC_MODE="<chosen-key>" bash scripts/install-claude-md.sh
```

Report the `BASE_PATH`, `SUPERCHARGED_MEMORY_TURSO_PATH`, `EPISODIC_MODE` and `PY`
it echoes back.

**On Windows**, run it from Git Bash (it is a bash script; there is no PowerShell
port) and check the two values it resolved for you:

- `PY` must be `python`, not `python3`. The script picks this from `uname -s`;
  `PYTHON_BIN` overrides it. See [Windows](#windows) for why `python3` is not
  merely absent but actively harmful here.
- `BASE_PATH` must be a `C:/...` path, not `/c/...`. The script runs `cygpath -m`
  to convert, because the value is baked into `~/.claude/CLAUDE.md` verbatim and a
  `/c/...` path is resolvable only from Git Bash — a session driving PowerShell
  could not open any script it names. Pass `BASE_PATH` explicitly if you want it
  pointed somewhere other than the checkout you are running from.

Then set up the database. **Never create one without checking for an existing one
first** — on a re-run, a second machine, or after a path change, a fresh empty DB
silently strands memory the user already has:

```bash
# This gate is only worth as much as the interpreter that runs it, so prove the
# interpreter first: the Store stub prints an advert and exits 0 (see Windows).
python3 --version 2>&1 | grep -q '^Python 3' \
  || echo "STOP: wrong interpreter — re-run this step with 'python'"
python3 scripts/recall.py --candidates    # any DB/backup elsewhere?
```

- **No output, or output that is not a `configured path :` line** → the script
  never ran. Read this as *unknown*, **never** as "no candidates": that
  misreading is what strands a user's real memory behind a fresh empty DB.
  `--candidates` unconditionally prints `configured path : <path>` as its first
  line, so its absence means the interpreter is wrong (on Windows, `python`) or
  the path is. Fix it and re-run before creating anything.
- **A `CANDIDATE DB` is listed** → do NOT create anything. Show the user the path
  and its memory count and ask whether that is their real memory. If yes, point
  `SUPERCHARGED_MEMORY_TURSO_PATH` at it (settings.json + profile + MCP) instead of
  creating a new one.
- **Only a `CANDIDATE BACKUP` is listed** → ask whether to restore it rather than
  start empty:
  `python3 scripts/restore.py --dump "<newest-backup>" --out "$SUPERCHARGED_MEMORY_TURSO_PATH"`
  Handles `.sql` and `.sql.gz`, and prints a per-table `in dump` vs `restored` count —
  read it, and treat a mismatch as a failed restore. Never pipe the dump into `tursodb`
  instead: that silently restores only a fraction of the rows.
- **Nothing found, and the file doesn't exist** → confirm with the user that they
  are starting from zero, then create it:

  ```bash
  [ -f "$SUPERCHARGED_MEMORY_TURSO_PATH" ] || tursodb "$SUPERCHARGED_MEMORY_TURSO_PATH" --experimental-multiprocess-wal < schema.sql
  ```

  **On Windows add `--vfs experimental_win_iocp`** — without it this fails with
  `experimental multiprocess WAL is not supported by the active IO backend` and
  creates nothing:

  ```bash
  [ -f "$SUPERCHARGED_MEMORY_TURSO_PATH" ] || tursodb "$SUPERCHARGED_MEMORY_TURSO_PATH" --experimental-multiprocess-wal --vfs experimental_win_iocp < schema.sql
  ```

Never pipe `schema.sql` into a path that already has a file — verify with `[ -f ]`
as above. Finish with:

```bash
python3 scripts/recall.py --status     # expect EMPTY (fresh) or READY n
```

## Step 7 — Offer to create coworkers

Once setup succeeds, offer to create a few dedicated AI coworkers (named personas
with scoped memory and trust-gated autonomy). Present it as optional and suggest a
couple of concrete starters, for example:

- **Reviewer** — a meticulous code reviewer. Expertise: reviewing diffs for bugs,
  edge cases, and regressions. Personality: blunt, detail-obsessed, flags what
  breaks rather than what's nice; grudging praise only when a change is airtight.
- **Architect** — a software architect. Expertise: system design, module
  boundaries, trade-offs, and long-term maintainability. Personality: asks "what
  does this cost us in a year?", pushes back on premature complexity and leaky
  abstractions, weighs alternatives before committing.

Other ideas to mention if the user wants more: a **Security** reviewer, a
**Testing/QA** specialist, a **Docs** editor, or a **Performance** analyst.

If the user wants one, construct a coherent personality using the Big-Five method
in `NEW-COWORKER.md` (don't freehand a grab-bag of adjectives), then create it:

```bash
python3 scripts/coworkers.py --add --name <Name> \
  --expertise "<what they review/advise on>" \
  --personality "<tone, biases, what they push back on>"
```

Then document them as `coworkers/<name>.md` (template in `coworkers/README.md`).
New coworkers default to `supervised` trust until you appraise them. To use one in
a session, the user tells the agent "load <Name>."


## Windows

Verified on Windows 11 (26100), Git Bash from Git for Windows, PowerShell 5.1,
Python 3.14, `tursodb` 0.7.2 (the targeted version, also verified on Linux),
Ollama 0.33.3. Everything in this runbook works
there, but four things differ and two of them fail *silently* — they look like
success, which is worse than an error.

**Run this runbook from Git Bash.** The scripts are bash and Python; only
`install-claude-md.sh` and the backup script are shell, and neither has a
PowerShell port. Use PowerShell for the two installers above and nothing else.

### 1. `--experimental-multiprocess-wal` needs a VFS here (loud failure)

Windows' default IO backend refuses the flag outright:

```
Error: Invalid argument supplied: experimental multiprocess WAL is not supported
by the active IO backend for '<db>'
```

Pair it with `--vfs experimental_win_iocp`, which `tursodb --help` names for
exactly this. **Do not "fix" this by dropping the flag** — it is what stops
tursodb taking an exclusive lock, so dropping it breaks concurrent sessions
instead (see `CLAUDE.md`, Concurrency). Every opener needs both: the schema
load, the MCP registration, and each script.

The scripts handle this themselves — `memlib.py` builds `OPEN_ARGS` from
`sys.platform` (`win32`, plus `msys`/`cygwin` for an MSYS2 or Cygwin Python), and
the backup script from `uname -s`. `TURSO_VFS` overrides the detection in both
directions: a name switches the backend, and `TURSO_VFS=none` (or empty) drops
`--vfs` altogether — which is what a tursodb that supports multiprocess WAL
natively on Windows, or that renames the backend, will need.

**Every `tursodb` command you run by hand needs both flags yourself**, and the
runbooks hand you several: Step 6 (schema load) and the MCP registration under
*Done* in this file, the worker read command and the MCP fallback in `SLEEP.md`,
the worker read command in `DEEP-SLEEP.md`, and whatever a `migration-steps/`
note tells `UPDATE.md` to run — those notes are dated records of a past migration,
written before Windows was supported, and are deliberately not retrofitted. Each
runbook now says this in the note at its top; where this file writes the command
out, the Windows form is given inline.

### 2. `python3` is a trap, not a missing command (silent failure)

Windows has no `python3`. The name is taken by a Microsoft Store alias stub that
prints `Python was not found` **and exits 0**. So:

```bash
python3 scripts/recall.py --status     # prints a Store advert, exit code 0, no output
```

reads as a *successful, empty* status. A session that trusts it concludes the
memory DB is empty and can go on to offer restoring a backup over a database that
was never broken. Use `python`. `install-claude-md.sh` renders `python` into
`~/.claude/CLAUDE.md` automatically on Windows (`PYTHON_BIN` overrides it), but
that covers only the session prefix.

**It does not cover the runbooks.** This file, `SLEEP.md`, `DEEP-SLEEP.md` and
`UPDATE.md` all spell commands `python3`, and an agent *executes* those lines —
so `sleep.py --mark-processed`, `--purge … --confirm-purge` and
`… | sleep.py --rebuild-topics` would each print the Store advert, exit 0, and be
reported as done with nothing written. All four files carry the same note at the
top, covering both this and the `--vfs` pairing above: read every `python3` in
the file as `python`, and add `--vfs experimental_win_iocp` to every `tursodb`
command in it.

The worst instance is Step 6's `recall.py --candidates`, the gate behind "never
create a DB without checking for an existing one first" — under the stub it
prints nothing, and *nothing* reads as "no candidates found", so the next step
builds a fresh empty DB and strands the user's real memory. That is why Step 6
proves the interpreter before running the gate and tells you to treat a missing
`configured path :` line as unknown rather than empty.

Check which one you have before trusting any script output:

```bash
python --version        # expect: Python 3.x
python3 --version       # if this advertises the Store, never use it again
```

### 3. Console codepage kills script output (loud, but confusing)

The scripts print em-dashes. A Windows console inherits the legacy OEM codepage —
`chcp` shows `850` or `437` on a default machine — where `U+2014` has no encoding,
so Python raises `UnicodeEncodeError` and the script dies mid-report. It is not a
data problem and the DB is fine.

`memlib.py` reconfigures `stdout`/`stderr` to UTF-8 with `errors="replace"` on
import, so anything going through it survives. Set `PYTHONUTF8=1` in
`~/.claude/settings.json` as well (Step 4) to cover the whole interpreter.

### 4. Paths: mixed form, and OneDrive

Use the mixed form — `C:/Users/you/...` — everywhere you write a path into config.
Git Bash accepts it, PowerShell accepts it, and `tursodb.exe` accepts it, whereas
an MSYS `/c/Users/...` path only works from Git Bash. `install-claude-md.sh`
converts `BASE_PATH` with `cygpath -m` for this reason.

Put the database under `%LOCALAPPDATA%`, not in a user folder. On a managed
Windows machine `Documents`, `Desktop` and `Pictures` are redirected into
OneDrive, so a path that reads as local is cloud-synced — and cloud sync corrupts
a live SQLite file. `%LOCALAPPDATA%` is never redirected.

Two things you do *not* have to worry about: the `.exe` extension (both
`CreateProcess` and Git Bash append it, so the scripts' default
`~/.turso/tursodb` resolves), and `PATH` for the MCP server (register it with
absolute paths, as Step 6 does, and it does not need one).

### Not covered

The launchd backup job in the README is macOS-only. The Windows equivalent is a
Task Scheduler entry running `bash scripts/supercharged-memory-backup.sh` through
Git Bash; the script itself works on Windows, only the scheduling is missing.
That is out of scope here.

## Done

Summarize what was installed vs. already present, the chosen `SUPERCHARGED_MEMORY_TURSO_PATH` and
`EPISODIC_MODE`, and any coworkers created. Then tell the user:

- Register Turso as a Claude Code MCP server named `turso`, using the same path:
  `tursodb "$SUPERCHARGED_MEMORY_TURSO_PATH" --mcp --experimental-multiprocess-wal` (if not done yet).

  On Windows, register it with absolute paths and the IOCP VFS, and confirm it
  reports `Connected`:

  ```bash
  claude mcp add turso --scope user -- "C:/Users/<you>/.turso/tursodb.exe" "<pasted-path>" --mcp --experimental-multiprocess-wal --vfs experimental_win_iocp
  claude mcp get turso
  ```

  Use `claude mcp`, never hand-edit `~/.claude.json`. Absolute paths matter
  because the MCP server is spawned without the user's interactive `PATH`, so
  `~/.turso` will not be on it.
- **Restart the Claude Code session** to pick up the newly installed
  `~/.claude/CLAUDE.md`.
- To change the episodic policy later, re-run Step 6 with a different
  `EPISODIC_MODE`; to move the database, update the `SUPERCHARGED_MEMORY_TURSO_PATH` export and the MCP
  registration (see the README for restore/backfill options).
