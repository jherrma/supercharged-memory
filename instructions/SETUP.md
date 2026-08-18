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

## Rules

- **Ask before installing anything.** Before running any install command, tell
  the user exactly what you're about to install and how, and wait for their
  confirmation. Never install a dependency that is already present.
- **Never use `sudo`** unless the user explicitly approves it for a specific command.
- Detect the platform first (`uname -s`) and pick install commands accordingly.
  The commands below are for macOS/Homebrew and generic Linux; adapt if the
  user's environment differs, and confirm your chosen command with them.

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

- It's a single SQLite file — end the path with a filename. The default is the
  XDG-conformant `${XDG_DATA_HOME:-~/.local/share}/turso/supercharged-memory.db`;
  offer that if they just want one.
- **It must be a local path, never inside a cloud-synced folder** (iCloud Drive,
  Dropbox, OneDrive, Google Drive) — cloud sync corrupts a live SQLite file.
- The parent directory will be created if missing.

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

Report the `BASE_PATH`, `SUPERCHARGED_MEMORY_TURSO_PATH`, and `EPISODIC_MODE` it echoes back.

Then set up the database. **Never create one without checking for an existing one
first** — on a re-run, a second machine, or after a path change, a fresh empty DB
silently strands memory the user already has:

```bash
python3 scripts/recall.py --candidates    # any DB/backup elsewhere?
```

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

## Done

Summarize what was installed vs. already present, the chosen `SUPERCHARGED_MEMORY_TURSO_PATH` and
`EPISODIC_MODE`, and any coworkers created. Then tell the user:

- Register Turso as a Claude Code MCP server named `turso`, using the same path:
  `tursodb "$SUPERCHARGED_MEMORY_TURSO_PATH" --mcp --experimental-multiprocess-wal` (if not done yet).
- **Restart the Claude Code session** to pick up the newly installed
  `~/.claude/CLAUDE.md`.
- To change the episodic policy later, re-run Step 6 with a different
  `EPISODIC_MODE`; to move the database, update the `SUPERCHARGED_MEMORY_TURSO_PATH` export and the MCP
  registration (see the README for restore/backfill options).
