# Supercharged Memory

A bootstrap for **local, embedded, long-term memory for AI coding agents** (built for [Claude Code](https://claude.com/claude-code)).

Your agent's text is embedded with a local model and stored as a vector in a local [Turso](https://github.com/tursodatabase/turso) (SQLite-compatible) database, so it recalls memories by *meaning* — with a keyword boost so exact ids and error strings still surface. Everything runs on your machine: no cloud, no API cost, no data leaving your laptop. The database is the single source of truth; a daily job backs it up locally, and a multiprocess flag lets several agent instances share it at once.

## Features

- **Semantic recall** — hybrid vector + keyword search over everything the agent has learned.
- **Two memory types** — timeless, revisable *semantic* facts and time-anchored, append-only *episodic* events.
- **Configurable episodic policy** — pick, at setup, how aggressively events are logged (every prompt → only when you ask).
- **Fully local & private** — a local embedding model (Ollama) and a local database; nothing is sent to a cloud service.
- **Multi-instance safe** — several agent sessions can share one database concurrently.
- **Graceful degradation** — if the embedding model is offline, recall falls back to keyword-only and the DB stays usable.
- **Coworkers** — optional named AI personas with scoped memory and trust-gated autonomy.
- **Backups built in** — validated, gzipped daily dumps with daily + weekly retention.

## Dependencies

Neither dependency is bundled — install both before setup.

### Turso (`tursodb`)

Turso 0.7.0+ — the Rust rewrite of SQLite with native vector support.

```bash
curl -sSL tur.so/install | sh
```

`tursodb` is always opened with `--experimental-multiprocess-wal` (see [Concurrency](#concurrency--locking)).

### Ollama + an embedding model

[Ollama](https://ollama.com) serves the embedding model locally.

```bash
# macOS
brew install ollama
brew services start ollama

# Pull the embedding model (multilingual, 1024-dim)
ollama pull bge-m3
```

By default it must be reachable at `http://localhost:11434`. **bge-m3** is the default model; one embedding model per database — the writer refuses to mix models in the same DB.

### Runtime

Python 3 (standard library only — no `pip install` needed) and bash.

## Setup

### Guided (recommended)

`instructions/SETUP.md` is a runbook Claude Code can execute for you. In a session
started in this repo, just say **"run SETUP.md"** — the agent verifies/installs the
two dependencies (asking before each install), pulls the embedding model, asks
**where to store the database** and **which episodic-memory policy** to use,
activates the instructions, creates the DB, and finally offers to set up coworkers.

### Manual

1. Install the dependencies above.
2. Clone this repository somewhere local (not a cloud-synced folder — see below).
3. Choose where the live database file lives and export it (keep it local; add the
   export to your shell profile so every session and script agrees):

   ```bash
   export SUPERCHARGED_MEMORY_TURSO_PATH="${XDG_DATA_HOME:-$HOME/.local/share}/turso/agent-foundations.db"   # your choice
   ```
4. Register Turso as a Claude Code MCP server named `turso`, launched as
   `tursodb "$SUPERCHARGED_MEMORY_TURSO_PATH" --mcp --experimental-multiprocess-wal`.
5. Activate the memory instructions in your agent, baking in your database path and
   episodic policy:

   ```bash
   SUPERCHARGED_MEMORY_TURSO_PATH="$SUPERCHARGED_MEMORY_TURSO_PATH" EPISODIC_MODE=major-events bash scripts/install-claude-md.sh
   ```

   This renders `CLAUDE.md.template` into `~/.claude/CLAUDE.md` between managed
   markers (idempotent — safe to re-run after any edit), substituting `BASE_PATH`,
   `SUPERCHARGED_MEMORY_TURSO_PATH`, and `EPISODIC_MODE`. Restart your session to pick it up.
6. Create the database and check availability (the scripts refuse to
   silently create an empty DB, so build the schema first):

   ```bash
   tursodb "$SUPERCHARGED_MEMORY_TURSO_PATH" --experimental-multiprocess-wal < schema.sql
   python3 scripts/recall.py --status     # MISSING | EMPTY | DEGRADED n | READY n
   ```

### Configuration

Scripts read these environment variables (defaults in `scripts/memlib.py`):

| Variable | Default | Notes |
|----------|---------|-------|
| `SUPERCHARGED_MEMORY_TURSO_PATH` | `${XDG_DATA_HOME:-~/.local/share}/turso/agent-foundations.db` | The live DB. **Keep it local — never in a cloud-synced folder**; cloud sync corrupts live SQLite. |
| `TURSO_BIN` | `~/.turso/tursodb` | Path to the `tursodb` binary. |
| `OLLAMA_URL` | `http://localhost:11434` | Ollama endpoint. |
| `EMBED_MODEL` | `bge-m3` | Embedding model; one per DB. |
| `BACKUP_DIR` | `./Backups` | Where daily dumps are written. |

`install-claude-md.sh` reads two more env vars and bakes them into the rendered
`~/.claude/CLAUDE.md` (they are template placeholders, not runtime script config):

| Variable | Default | Notes |
|----------|---------|-------|
| `SUPERCHARGED_MEMORY_TURSO_PATH` | `${XDG_DATA_HOME:-~/.local/share}/turso/agent-foundations.db` | Written into the instructions so the agent restores to the right path. Keep it in sync with the `SUPERCHARGED_MEMORY_TURSO_PATH` the scripts use. |
| `EPISODIC_MODE` | `major-events` | Episodic-storage policy (see below). Validated to one of the four keys. |
| `BASE_PATH` | repo root | Points at this repo; the installer fills it in automatically — update it on a new machine. |

### Episodic memory policy

Chosen at setup and written into the agent's instructions, this controls how
aggressively **episodic** events are logged (it does not affect semantic facts,
gotchas, or corrections — those are always stored autonomously):

| `EPISODIC_MODE` | Behavior |
|-----------------|----------|
| `every-prompt` | Store an episodic note for every prompt / turn. |
| `major-actions` | Store every substantive action; skip quick questions and clarifications. |
| `major-events` | Store only major events — feature done, bug resolved, decision, milestone, incident. *(Default.)* |
| `manual` | Store episodic memory only when you explicitly ask. |

Change it later by re-running `install-claude-md.sh` with a different
`EPISODIC_MODE`.

## Usage

Memory operations go through the helper scripts in `scripts/` (a 1024-float
vector is clumsy to inline into MCP calls). The Turso MCP server is for ad-hoc
SQL and inspection only — revisions go through the scripts.

```bash
# Recall (hybrid vector + keyword; add --coworker to scope to a persona)
python3 scripts/recall.py "how do I run a single test?" [--table semantic|episodic|both] [--project <id>] [--k N]
python3 scripts/recall.py --baseline            # always-apply rules, loaded every session
python3 scripts/recall.py --status              # availability check
python3 scripts/recall.py --candidates          # other memory DBs/backups found on this machine

# Store one memory (see the script docstrings for the full flag set)
python3 scripts/remember.py --table semantic --topic <t> --category <c> \
    --keywords "id, error string, synonyms" --source <s> --model <your-model-id> --text "..."
python3 scripts/remember.py --table semantic --supersedes <old-id> ...   # revise a fact
python3 scripts/remember.py --table episodic --event-type bug_fix --importance notable ...

# Bulk import a directory as a fresh start (one file = one memory)
python3 scripts/backfill.py --dir <path> [--glob '*.md'] [--category project]
```

### Session start & availability

On session start the agent runs `recall.py --status` first:

- **`MISSING`** — nothing at the configured path → **stop and ask the user** (see below).
- **`EMPTY`** — DB exists but holds no memories → check `--candidates`, then offer to **backfill** from a directory.
- **`DEGRADED n`** — Ollama down, DB fine → still load baseline and use keyword-only recall, but don't store new memories.
- **`ERROR`** — fall back to the agent's built-in memory store.
- **`READY n`** — load baseline and proceed.

### Never lose a database by accident

The most likely cause of `MISSING` is a **wrong path**, not lost data — and the two
are indistinguishable unless you look. An agent that responds by creating a fresh DB
silently strands the real one; an agent that restores a backup over a live DB loses
everything since that backup. So nothing in this system ever creates, restores, or
overwrites a database on its own:

- `memlib.require_db()` refuses to let any script auto-create an empty DB.
- `--status MISSING` prints the configured path, **where that path came from**
  (env var vs. built-in default), and any other memory DB or backup it found —
  each with its memory count.
- `--candidates` runs that search on demand, at any time.
- The agent instructions require it to work through *wrong path → candidate DB →
  ask the user* before restore is even considered, and creating an empty DB is
  the last resort with explicit confirmation.

A path that silently defaults is the root of this whole failure mode, so note that
**`SUPERCHARGED_MEMORY_TURSO_PATH` must be set where Claude Code can see it** —
`~/.claude/settings.json` under `env`. A shell profile alone is not enough: the
agent's Bash tool runs non-interactively and never sources `~/.zshrc` or `~/.bashrc`.

## How it works

`scripts/memlib.py` is the shared core every script imports: config, embedding
(with a dimension assert), compact vector literals, SQL escaping, and a robust
`tursodb` runner (stderr-scoped error detection + busy backoff). The rest are
thin CLIs on top:

- **`remember.py`** — one memory = one row (no chunking). Folds `--keywords` into
  the text, embeds it, inserts. Guards: baseline needs `--confirm-baseline`;
  semantic refuses a near-duplicate (cosine < 0.10) unless `--force` (episodic is
  exempt — events recur); `--supersedes <id>` inserts a revision and marks the old
  row superseded in one call; refuses a DB embedded with a different model.
- **`recall.py`** — hybrid search: `vector_distance_cos` (brute force) plus a
  keyword boost equal to the count of meaningful query tokens found in the text
  (stopwords dropped, ids/error strings kept). Degrades to keyword-only if Ollama
  is down. Also `--baseline`, `--status`, `--count`.
- **`backfill.py`** — import a directory of files for a fresh start (skips files over the cap).
- **`seed.py`** — scaffold for a one-time bootstrap load (empty by default).
- **`agent-foundations-backup.sh`** — the daily backup (below).
- **`install-claude-md.sh`** — render the template into `~/.claude/CLAUDE.md`.
- **`coworkers.py`** — manage AI personas (below).

## Database schema

Two tables (see `schema.sql`), one row per memory. Both embed `memory_text` into
`embedding` and search by brute-force `vector_distance_cos`. Hard caps are
enforced via `CHECK` (SQLite ignores declared VARCHAR sizes): `memory_text` ≤
2000; `project`/`topic`/`source`/`model`/`embed_model` ≤ 128. Enums are
`CHECK`-enforced. **Keywords are not a column** — they live inside `memory_text`
(appended by `remember.py --keywords`), so they're both embedded and searchable.

### `semantic_memory` — facts, timeless, revisable

`id`, `created_at`, `updated_at`, `project` (NULL = global; a tracking-tool
work-item id), `topic`, `category` ∈ `baseline|user|feedback|project|reference`,
`source`, `model`, `embed_model`, `memory_text`, `file_reference`,
`embedding F32_BLOB(1024)`, `superseded_by` (current truth =
`WHERE superseded_by IS NULL`), `retired_at` (soft-delete set by a sleep pass —
current truth also requires `retired_at IS NULL`; never a hard `DELETE`).

`baseline` = must-always-apply rules, loaded every session start
(`recall.py --baseline`), not by search. **Storing a baseline memory requires
explicit user confirmation** (`--confirm-baseline`). None are seeded by default.

### `episodic_memory` — events, time-anchored, append-only

`id`, `created_at` (event time), `project`, `topic`, `event_type` ∈
`project_start|bug_fix|feature_complete|decision|milestone|incident|note`,
`importance` ∈ `routine|notable|major`, `source`, `model`, `embed_model`,
`memory_text`, `file_reference`, `embedding F32_BLOB(1024)`, `processed_at`
(set once a sleep pass has sifted this row; NULL = not yet processed).

## Coworkers

Optional named AI personas with scoped memory and trust-gated autonomy — full
design in `docs/2026-07-23-ai-coworkers-design.md`, and see `NEW-COWORKER.md`
for how to construct a coherent personality. Three tables, no changes to
`semantic_memory`/`episodic_memory`: `coworkers` (name/expertise/personality/
trust_level/active), `memory_coworkers` (many-to-many — untagged = global,
tagged = visible only when that coworker is loaded), `appraisals` (one current
row per coworker, history via `superseded_by`).

- **`scripts/coworkers.py`** — writes only: `--add`, `--appraise`, `--set-trust`,
  `--retire`/`--reactivate`. Reads (list coworkers, load a coworker's state) are
  ad-hoc SQL via the Turso MCP — no embedding involved.
- **`remember.py --coworker name[,name...]`** — tag a memory to one or more
  coworkers instead of leaving it global; dedup scopes to that coworker's visible set.
- **`recall.py --coworker name`** — scope a semantic search to memories visible to
  that coworker (untagged ∪ tagged-to-them).
- `trust_level` (`supervised|trusted|autonomous`) never loosens a global baseline
  rule — it's a hard floor regardless of which coworker is active.

## Sleep cycle

A user-triggered ("sleep" / "go to sleep") consolidation pass — never
scheduled automatically. Full procedure in `instructions/SLEEP.md`; no new
memory tables, just three additions.

`CLAUDE.md.template` deliberately carries only a **one-line pointer** to that
file, not a summary of the procedure — sleep happens rarely, so its steps are
read on demand and cost nothing in the sessions that never sleep. The pointer is
an absolute path baked in at install time (`{{BASE_PATH}}`), so it resolves
without the agent hunting for the repo. Same reasoning for backups (below).
Keep the *policy* in the template (user-triggered only) and the *procedure* here.

- `episodic_memory.processed_at` — sleep sifts unprocessed rows, keeping only
  ones where something was actually learned (discarding bare event sequences
  like "x happened, then y happened"), promotes the kept ones into
  `semantic_memory` (via `remember.py`, same as any other store/supersede),
  then stamps `processed_at` on every row it looked at — kept or discarded —
  so nothing gets re-sifted forever.
- `semantic_memory.retired_at` — sleep also sweeps current semantic memory for
  near-duplicates to consolidate (`remember.py --supersedes`) and obsolete
  facts to retire. Retirement is **soft-delete only** (`scripts/sleep.py
  --retire`, sets `retired_at`) — same never-hard-delete philosophy as
  coworkers' `active=0`. "Obsolete" has no fixed rule; ask the user when it's
  not clear-cut.
- `topic_keywords` (new table, see schema below) — a curated topic → keywords
  index, rebuilt wholesale each sleep (`scripts/sleep.py --rebuild-topics`:
  full DELETE + re-INSERT, never accumulated). **Loaded in full at every
  session start** (`recall.py --topics`, same slot as `--baseline`) so a
  session knows *what topics have memory* without having to guess a search
  term first. This puts pressure back on sleep's consolidation step to keep
  the topic count bounded — `recall.py --topics` warns past 50 topics as a
  nudge to merge harder next sleep, since that many would cost real context
  budget every session.

### `topic_keywords` — curated topic index, derived (not a source of truth)

`topic` (primary key), `keywords`, `updated_at`. Deliberately unlinked to
`semantic_memory`/`episodic_memory` — no FK, no embedding, substring-searched
via plain `LIKE`.

## Concurrency & locking

`tursodb` takes an **exclusive** file lock — a second process can't even read —
**unless every opener passes `--experimental-multiprocess-wal`**. With the flag
on all openers (MCP, scripts, backup): multiple agent instances share the DB,
reads run concurrently, and writes serialize (a clash returns `database is busy`;
scripts retry with backoff). A process without the flag is refused. The flag is
**experimental** — that's the trade for concurrency.

## Backup & restore

- **Daily backup** — schedule `scripts/agent-foundations-backup.sh` (e.g. a
  launchd/cron job) to produce a **validated** gzipped SQL dump
  `YYYY-MM-DD-agent-foundations.sql.gz` in `Backups/` (concurrent-safe reader;
  checks non-empty + has INSERTs + gzip intact). Retains **3 daily + 4 weekly**
  (Monday) copies, and runs even while a session is open.
- **Manual backup:** `bash scripts/agent-foundations-backup.sh` — this is also what
  the agent runs when the user asks for a backup (`CLAUDE.md.template` points at
  the script by absolute path). Dumps always land in this repo's `Backups/`, the
  `BACKUP_DIR` default; `recall.py --candidates` looks there for restorable dumps,
  so keep them together rather than scattering them per-machine. Taking a backup is
  additive and non-destructive; **restoring is neither** — never restore without the
  user's explicit ok.
- **Restore** (into a fresh DB file):
  ```bash
  gzcat "$(ls -1t Backups/*-agent-foundations*.sql.gz | head -1)" \
    | tursodb "$SUPERCHARGED_MEMORY_TURSO_PATH" --experimental-multiprocess-wal
  ```
- **Rebuild empty schema** — **pipe** the file; don't pass it as a SQL argument
  (`tursodb "$(cat schema.sql)"` fails: the leading `--` comment parses as a CLI flag):
  ```bash
  tursodb "$SUPERCHARGED_MEMORY_TURSO_PATH" --experimental-multiprocess-wal < schema.sql
  python3 scripts/seed.py    # empty by default — add entries first if you want a seeded start
  ```
- **New machine:** run `instructions/SETUP.md` (or manually: install `tursodb` +
  Ollama, `ollama pull bge-m3`, choose/export `SUPERCHARGED_MEMORY_TURSO_PATH`, restore the latest dump,
  register the MCP with the flag, and re-run `install-claude-md.sh` with your
  `SUPERCHARGED_MEMORY_TURSO_PATH` and `EPISODIC_MODE`).

## Data protection

**Never store PII — anywhere.** Anonymize before calling
`remember.py`/`backfill.py`. Store pointers (ticket/case ids, role labels)
instead of personal data. This applies to the memory text *and* anything sent to
Ollama to embed.

## License

[GNU General Public License v3.0](LICENSE).
