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
- **Sleep & deep sleep** — user-triggered consolidation: sift events into facts, then (deep) purge what you agree is dead, merge near-duplicates, and derive patterns and trends across the whole event log. All of it read by subagents, so the cost doesn't land in your session's context.
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
   export SUPERCHARGED_MEMORY_TURSO_PATH="${XDG_DATA_HOME:-$HOME/.local/share}/turso/supercharged-memory.db"   # your choice
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
| `SUPERCHARGED_MEMORY_TURSO_PATH` | `${XDG_DATA_HOME:-~/.local/share}/turso/supercharged-memory.db` | The live DB. **Keep it local — never in a cloud-synced folder**; cloud sync corrupts live SQLite. |
| `TURSO_BIN` | `~/.turso/tursodb` | Path to the `tursodb` binary. |
| `OLLAMA_URL` | `http://localhost:11434` | Ollama endpoint. |
| `EMBED_MODEL` | `bge-m3` | Embedding model; one per DB. |
| `BACKUP_DIR` | `./Backups` | Where daily dumps are written. |

`install-claude-md.sh` reads two more env vars and bakes them into the rendered
`~/.claude/CLAUDE.md` (they are template placeholders, not runtime script config):

| Variable | Default | Notes |
|----------|---------|-------|
| `SUPERCHARGED_MEMORY_TURSO_PATH` | `${XDG_DATA_HOME:-~/.local/share}/turso/supercharged-memory.db` | Written into the instructions so the agent restores to the right path. Keep it in sync with the `SUPERCHARGED_MEMORY_TURSO_PATH` the scripts use. |
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

## Staying up to date

The runtime state lives *outside* this repo — the database, `~/.claude/CLAUDE.md`,
`~/.claude/settings.json` — so pulling new commits does not update a machine on its
own, and some commits need a manual step. `instructions/UPDATE.md` is the runbook
that closes that gap. In a session:

> update the memory system

It reads the sync stamp, pulls, applies any pending migration notes, shows you what
changed in the instructions, and re-renders `~/.claude/CLAUDE.md`. Nothing is applied
without asking.

### The sync stamp

`install-claude-md.sh` writes the repo commit it rendered from as the first line
inside the managed block:

```
<!-- BEGIN agentic-memory (managed by install-claude-md.sh) -->
<!-- supercharged-memory: synced-at 9f3c1ab… -->
```

That one line is the whole ledger — the repo's git history supplies the rest, so
there is no separate state file to drift out of sync. `git log <stamp>..HEAD` yields
both the template changes worth explaining and the migrations still pending. Two
special values: a `-dirty` suffix means the install was rendered from an uncommitted
working tree, and `unknown` means `BASE_PATH` was not a git work tree (a plain file
copy). An install predating this feature has no stamp at all; UPDATE.md then lists
every migration note and asks which ones already apply, rather than guessing a base
commit.

### `migration-steps/` — breaking-change notes

Every change that breaks an existing install ships a note here: schema changes,
script CLI changes, renamed env vars or paths, changed instruction contracts,
anything needing a manual step. One file per change,
`migration-steps/YYYY-MM-DD-<slug>.md`:

| Part | Purpose |
|------|---------|
| `commit: <sha>` (first line) | The commit that **introduced** the break. UPDATE.md classifies notes with `git merge-base --is-ancestor`, so a missing or unreachable sha is reported as malformed instead of silently applied. |
| **What broke** | Schema / CLI / instruction contract / on-disk runtime state. |
| **How to resolve** | Concrete commands, in order. |
| **Verification** | What to run afterwards, and what output proves it worked. |

Because a commit cannot reference its own sha, **the note lands in the commit right
after the breaking one** and points back at it — pushed together, so no `git pull`
can leave a machine between the break and its instructions.

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
- **`sleep.py`** — the mechanical write primitives a sleep pass needs, no judgment of
  its own: `--mark-processed`, `--retire`, `--rebuild-topics`, plus deep sleep's
  `--purge` (guarded hard delete) and `--cluster` (read-only similarity grouping).
- **`supercharged-memory-backup.sh`** — the daily backup (below).
- **`install-claude-md.sh`** — render the template into `~/.claude/CLAUDE.md`, stamping
  the repo commit it rendered from (see *Staying up to date*).
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
work-item id), `topic`, `category` ∈
`baseline|user|feedback|project|reference|pattern`, `source`, `model`,
`embed_model`, `memory_text`, `file_reference`, `embedding F32_BLOB(1024)`,
`superseded_by` (current truth = `WHERE superseded_by IS NULL`), `retired_at`
(soft-delete set by a sleep pass — current truth also requires
`retired_at IS NULL`).

`baseline` = must-always-apply rules, loaded every session start
(`recall.py --baseline`), not by search. **Storing a baseline memory requires
explicit user confirmation** (`--confirm-baseline`). None are seeded by default.

`pattern` = **derived**, written only by a deep sleep pass: a recurrence or trend
found across episodic events, carrying the episodic ids it came from so the claim
can be re-checked rather than trusted. Revised through the normal supersede chain
when the count changes.

A hard `DELETE` happens in exactly one place — deep sleep's purge gate, on rows
the user selects, after a backup (see *Deep sleep* above). Everywhere else,
obsolescence is `retired_at` or a supersede.

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

### Where the reading happens: subagents

Both sleep runbooks push all bulk reading into **subagents**. The orchestrating
agent queries skinny metadata (ids, topics, dates) and never holds `memory_text`
in bulk — that cost scales with the corpus, which is precisely what a memory
system is supposed to grow. Workers get self-contained prompts (their ids, the
read command, the judgment rule, the exact `remember.py` call) and report one line
per row.

Who may write is split on risk: episodic-sift workers write via `remember.py`
themselves (high volume, low stakes), while compaction and pattern workers only
*propose* and the user approves in one batch. `--mark-processed` stays with the
orchestrator and covers only ids a worker actually reported back — a worker that
dies leaves its rows unprocessed, which is the recoverable state.

## Deep sleep

A second, deeper pass — `instructions/DEEP-SLEEP.md`, reached by saying "deep
sleep" or by answering yes to the offer at the end of a normal sleep. Also
user-triggered only. It does the three things a normal pass deliberately doesn't:

| Phase | What it does |
|-------|--------------|
| D0 | Normal sleep (prerequisite) + `recall.py --status`; stops on `DEGRADED` — nothing can be embedded with Ollama down. |
| D1 | Backup. Mandatory: D2 is the only operation in this system that destroys a memory. |
| D2 | **Purge gate.** Lists every superseded/retired semantic row and hard-deletes *the ones the user selects*. No default policy — asked every run. |
| D3 | **Compaction.** Clusters all current semantic memory and merges same-topic near-duplicates. |
| D4 | **Pattern mining.** Derives recurrences and trends from the whole episodic log. |
| D5 | Rebuild `topic_keywords`, report. |

**Purge** (`sleep.py --purge <ids> --confirm-purge`) is the sole exception to the
never-hard-delete rule, and it is fenced: it refuses a current row, an unknown id,
a missing `--confirm-purge`, any id whose deletion would strand a surviving row
that points at it, and any run where the newest backup is older than the DB file.
It also clears the rows' `memory_coworkers` entries — that table has no FK on
`memory_id`, so an orphan would silently re-scope a future memory that reuses the
id. Episodic memory is never purged. Pass all ids in **one** call: a purge is
itself a write, so a second call in the same pass fails the backup gate.

**Clustering** (`sleep.py --cluster [--table semantic|episodic] [--threshold N]`)
is mechanical — no LLM, no new embeddings. It runs pairwise
`vector_distance_cos` over the vectors already stored and emits connected
components as JSON, plus any rows with no embedding, listed separately rather than
silently skipped. Semantic defaults to `0.22`, episodic to `0.25` — both measured
against a real ~230-row corpus, not guessed. The useful range is much tighter than
intuition suggests, because connected components **chain**: if a–b are close and
b–c are close, a and c land in the same cluster even when they have nothing to do
with each other. On this corpus `0.22` yields ~11 pairs and triples, `0.30`
already produces a 33-row blob, and `0.35` collapses 154 of ~200 rows into one
"cluster". The script therefore refuses to present that as a finding: when the
largest cluster exceeds 12 rows *and* a quarter of all clustered rows, the JSON
carries a `warning` telling you to lower the threshold. Merges then apply via
`remember.py --supersedes <id1,id2,id3>` — one insert, all listed rows pointed at
it, one transaction.

**Pattern mining** is two-stage, because there are two kinds of pattern and no
single worker can see both. A map stage partitioned by *episodic embedding
cluster* — not by exact `topic`, since episodic topics are near-unique per ticket
and exact grouping would yield all singletons — catches within-topic recurrence
("that service restarted four times"). Then one reduce worker reads only the tiny
per-row digests and catches cross-topic shapes ("a whole class of bug keeps coming
back") that a topic-scoped worker structurally cannot see. A pattern needs ≥3
supporting events, a *trend* claim also ≥2 distinct calendar months, and its
`evidence_ids` go into the memory text so a later session can verify the claim
instead of trusting it.

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

- **Daily backup** — schedule `scripts/supercharged-memory-backup.sh` (e.g. a
  launchd/cron job) to produce a **validated** gzipped SQL dump
  `YYYY-MM-DD-supercharged-memory.sql.gz` in `Backups/` (concurrent-safe reader;
  checks non-empty + has INSERTs + gzip intact). Retains **3 daily + 4 weekly**
  (Monday) copies, and runs even while a session is open.
- **Manual backup:** `bash scripts/supercharged-memory-backup.sh` — this is also what
  the agent runs when the user asks for a backup (`CLAUDE.md.template` points at
  the script by absolute path). Dumps always land in this repo's `Backups/`, the
  `BACKUP_DIR` default; `recall.py --candidates` looks there for restorable dumps,
  so keep them together rather than scattering them per-machine. Taking a backup is
  additive and non-destructive; **restoring is neither** — never restore without the
  user's explicit ok.
- **Restore** (into a fresh DB file). Decompress with whatever your platform ships —
  `gunzip -c` exists on both macOS and Linux, macOS also has `gzcat`, Linux also has
  `zcat` (Linux's `gzip` package installs no `gzcat`, so a copy-pasted `gzcat` fails
  there even though the archive is fine):
  ```bash
  gunzip -c "$(ls -1t Backups/*-supercharged-memory*.sql.gz | head -1)" \
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
