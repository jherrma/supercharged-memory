# Supercharged Memory

A **local, embedded, long-term memory for AI coding agents** (built for [Claude Code](https://claude.com/claude-code)), shipped as **one native binary**.

Your agent's text is stored in a local [Turso](https://github.com/tursodatabase/turso) (SQLite-compatible) database and, when a local embedding model is available, embedded as a vector, so it recalls memories by *meaning* — with an IDF-weighted keyword credit so exact ids and error strings still surface. Everything runs on your machine: no cloud, no API cost, no data leaving your laptop. The database is the single source of truth; a daily job backs it up locally, and a multiprocess WAL lets several agent instances share it at once.

## Features

- **Semantic recall** — hybrid vector + keyword search over everything the agent has learned.
- **Two memory types** — timeless, revisable *semantic* facts and time-anchored, append-only *episodic* events.
- **Instructions on demand** — directives you want followed when you say so ("use instruction X"), kept apart from memory and never loaded at session start.
- **Configurable episodic policy** — pick, at setup, how aggressively events are logged (every prompt → only when you ask).
- **Fully local & private** — an optional local embedding model (Ollama) and a local database; nothing is sent to a cloud service.
- **Multi-instance safe** — several agent sessions can share one database concurrently.
- **Graceful degradation** — with no embedding model, recall falls back to keyword-only, new memories are stored without an embedding, and the DB stays fully usable; `embed --missing` catches up later.
- **Sleep & deep sleep** — user-triggered consolidation: sift events into facts, then (deep) purge what you agree is dead, merge near-duplicates, and derive patterns and trends across the whole event log. All of it read by subagents, so the cost doesn't land in your session's context.
- **Coworkers** — optional named AI personas with scoped memory and trust-gated autonomy.
- **Backups built in** — validated, gzipped dumps with daily + weekly retention, and a counted restore.

## Install

1. Download the one binary for your platform and put it in a permanent place under a stable name, for example `~/.local/bin/supercharged-memory`. The `.env` it uses is read from beside the binary, and the instructions it installs name its path.
2. Start a Claude Code session and tell the agent to **set up the memory system**. It runs the binary's own runbook (`runbook setup`), which drives `init`:
   - `init` with no flag is a dry run: it prints the plan (database path, `.env` values, the CLAUDE.md block, the state of the optional dependencies, a backup schedule) and writes nothing. The agent relays it, you confirm, and `init -y` applies it.
   - It creates the database from the schema embedded in the binary (or adopts an existing one), writes the `.env`, and appends the memory instructions to `~/.claude/CLAUDE.md` between managed markers. It never overwrites a database or a differing `.env` value.

Or by hand: `supercharged-memory init`, read the plan, `supercharged-memory init -y`, restart your session.

## Dependencies

**None of them is required.** The binary carries the database engine, the schema, the instructions and the runbooks. Two things are optional and recommended:

### Ollama + an embedding model — recommended

[Ollama](https://ollama.com) serves the embedding model locally.

```bash
brew install ollama && brew services start ollama   # macOS; see the runbook for Linux and Windows
ollama pull bge-m3                                  # multilingual, 1024-dim
```

By default it is expected at `http://localhost:11434`. **bge-m3** is the default model; one embedding model per database — the writer refuses to mix models in one DB.

Without it the system still works, and says what it skips: `status` reads `DEGRADED n` and gives the reason; recall is **keyword-only**; a new memory is stored **without an embedding** and the near-duplicate check is text-only (weaker); clustering for sleep's compaction and pattern mining, and the recall-quality harness, need vectors and refuse. When Ollama arrives, `embed --missing` fills in every row stored meanwhile, and switches `EMBEDDINGS=off` in the `.env` back on once nothing is left.

### Turso (`tursodb`) — recommended, for ad-hoc reads

Turso 0.7.0+ — **0.7.2 is what this project targets**. The binary never calls `tursodb`; it only helps you: registered as a Claude Code MCP server named `turso`, it gives the agent broad ad-hoc reads that the fixed commands (`recall`, `list`, `show`, `coworkers --load`, …) do not offer. Register it **read-only**, so writes can only go through the binary:

```bash
claude mcp add turso --scope user -- <path to tursodb> <database path> --mcp --readonly --experimental-multiprocess-wal
```

`init` prints this command, filled in. On Windows add `--vfs experimental_win_iocp` beside the WAL flag (see [Concurrency](#concurrency--locking)). A read-write registration works, but then "writes go through the binary" is only a convention (advisory).

## Configuration

Settings come from the real environment, then the `.env` beside the binary, then a built-in default — in that order of **precedence**. `supercharged-memory config` prints every setting, its value and where it came from; a real environment variable that overrides a differing `.env` value says so. A key the binary does not know in the `.env` is warned about, never fatal (a `.env` written by a newer binary must not brick an older one). Relative paths in the `.env` are refused.

| Key | Default | Notes |
|-----|---------|-------|
| `SUPERCHARGED_MEMORY_TURSO_PATH` | `${XDG_DATA_HOME:-~/.local/share}/turso/supercharged-memory.db` | The live DB. **Keep it local — never in a cloud-synced folder**; cloud sync corrupts live SQLite. Written by `init`. |
| `BACKUP_DIR` | `${XDG_DATA_HOME:-~/.local/share}/supercharged-memory/backups` | Where `backup` writes its dumps. Written by `init`. |
| `SUPERCHARGED_MEMORY_VERSION` | unset | The version that last ran `init`/`update`; `update` compares it with the binary's. |
| `EMBEDDINGS` | `on` | `off` = never contact Ollama. `init` writes `off` when it finds none; `embed --missing` switches it back. |
| `OLLAMA_URL` | `http://localhost:11434` | Ollama endpoint. |
| `EMBED_MODEL` | `bge-m3` | Embedding model; one per DB. |
| `RECALL_ALPHA` | `0.15` | Keyword weight in recall ranking. Corpus-calibrated — see [Hybrid recall](#how-it-works). |
| `TURSO_VFS` | `experimental_win_iocp` on Windows, else none | IO backend paired with the multiprocess WAL. Empty or `none` drops it. |
| `TURSO_BIN` | `~/.turso/tursodb` | Only for the MCP registration `init` prints; the binary itself needs no `tursodb`. |
| `SUPERCHARGED_MEMORY_EVAL_DIR` | `<db parent>/eval` | Query-embedding cache of the eval harness. Derived data; the cases themselves live in the DB. |

`CLAUDE_CONFIG_DIR` (the real environment only) moves Claude Code's own directory, which `init`, `update` and `find-existing-memory` honour.

### Episodic memory policy

Chosen at setup (`init --episodic-mode <mode>`) and written into the agent's instructions, this controls how aggressively **episodic** events are logged (it does not affect semantic facts, gotchas, or corrections — those are always stored autonomously):

| Mode | Behavior |
|------|----------|
| `every-prompt` | Store an episodic note for every prompt / turn. |
| `major-actions` | Store every substantive action; skip quick questions and clarifications. |
| `major-events` | Store only major events — feature done, bug resolved, decision, milestone, incident. *(Default.)* |
| `manual` | Store episodic memory only when you explicitly ask. |

Change it later by having the agent replace the managed block with `update --print-block --episodic-mode <mode>`.

## Migrating existing file-based memory

If you were running Claude Code with ordinary file-based memory before installing this — a curated `~/.claude/CLAUDE.md`, a `~/.claude/memory/*.md` set, project-scoped `~/.claude/projects/<slug>/memory/*.md` — none of it is in the database, and `init`'s candidate check will not find it: that looks for a Turso DB or a backup dump, which is a different thing.

The `migrate` runbook (`runbook migrate`) imports those files as semantic memory and then runs the deep-sleep phases that apply (compaction, the required topic-index rebuild, the Verify pass — imported memory is old by definition). Setup offers it; you can also ask for it later ("migrate my memory").

- **`find-existing-memory`** — a read-only scan, needing neither the database nor Ollama. It reports per file its `kind` (`memory` / `index` / `claude` / `empty`), `scope` (`global` / `project`) and char count. Counts of what there is to import cover `kind: memory` only. Everything it leaves out lands in a reported field instead of vanishing — `over_max_text` (flagged against the **1800-char effective budget**, not the raw 2000-char cap, because `remember` appends `--keywords` into the same field the cap counts), `empty`, `unreadable` and `excluded_dirs`. The runbook gates on the computed `nothing_to_do`, not on `n_memory_files`.
- **Additive, not reversible.** No source file is deleted or edited, and every row carries `source='migration'` plus a `file_reference` back to its file. There is no bulk undo of the imported rows: the source files are always the way back, and a pre-import backup restores a database that already held rows.
- **Classification runs in subagents**, the same contract the sleep runbooks use; the orchestrator never reads the corpus in bulk.

## Staying up to date

The runtime state lives *outside* the binary — the database, the `.env`, `~/.claude/CLAUDE.md` — so replacing the binary does not update a machine on its own, and some releases need a step. Replace the binary **in place** (same path, same name), then tell the agent to **update the memory system**: it runs `update`, relays what it prints, asks, and applies `update -y`. Nothing is applied without asking.

`update` (a dry run) compares the version the `.env` records with the binary's and prints, in order: every **migration note** between the two versions; the **schema migrations** the database lacks (decided by asking the database, never by trusting the `.env`); and what to change in CLAUDE.md — a diff of the installed block against the one this binary would write, including any local hand-edit inside the markers that replacing the block would destroy. `update -y` backs up the database before the first migration, applies each migration in a transaction of its own, and records the new version in the `.env` only once all succeeded. It never writes CLAUDE.md: the agent replaces the managed block with `update --print-block` once you agree. It refuses a downgrade.

### The block's stamp

`init` writes the version that rendered the block as the first line inside the managed block:

```
<!-- BEGIN agentic-memory (managed by supercharged-memory) -->
<!-- supercharged-memory: rendered-by 0.1.0 -->
```

`update` does not trust the stamp alone: it renders the template again with this machine's own values (binary path, episodic mode) and compares the **content**, so a hand-edit or a moved binary shows up even when the version matches.

### `migrations/` — breaking-change notes

Every release that breaks an existing install ships a note in `migrations/<version>/NOTE.md` (and, for a schema change, the SQL beside it). Each is **embedded in the binary** and printed by `update` for the versions between the installed one and the running one. A note has three parts: **What broke**, **How to resolve** (concrete steps, in order) and **Verification** (what to run afterwards and what output proves it worked). A new schema migration also gets an entry with a probe — a query the database answers when the migration is already applied — in the binary's table, and a test fails until both agree. Notes are keyed by version, so a note can land in the same commit as the change.

The older `migration-steps/` directory is frozen history: sha-keyed notes written for the Python-script install. A machine still on that layout does not apply them for 2.0: it follows [MIGRATING-FROM-PYTHON.md](MIGRATING-FROM-PYTHON.md).

## Usage

The agent drives everything through the binary; you rarely type these yourself. Run `supercharged-memory --help` for the full list.

```bash
supercharged-memory status                       # MISSING | EMPTY | DEGRADED n | READY n (+ reason, + unembedded count)
supercharged-memory recall "how do I run a single test?" [--table semantic|episodic|both] [--project <id>] [--k N] [--coworker <name>]
supercharged-memory recall --baseline            # always-apply rules, loaded every session
supercharged-memory recall --topics [word]       # which subjects have memory at all
supercharged-memory recall --candidates          # other memory DBs/backups found on this machine

supercharged-memory remember --table semantic --topic <t> --category <c> \
    --keywords "id, error string, synonyms" --source <s> --model <your-model-id> --text "..."
supercharged-memory remember --table semantic --supersedes <old-id> ...   # revise a fact
supercharged-memory remember --table episodic --event-type bug_fix --importance notable ...

supercharged-memory list --table semantic --current --keywords    # metadata, never the text
supercharged-memory show --table semantic 12,40                   # whole rows
supercharged-memory instruct --topic <name> --text "..."          # store an instruction
supercharged-memory instructions --get <name>                     # use instruction <name>
supercharged-memory backfill --dir <path> [--glob '*.md'] [--category project]   # bulk import, one file = one memory
supercharged-memory runbook [name]               # print a procedure; no name lists them
```

The Python scripts under `scripts/` (and `investigations/eval-harness.py`) remain in the repository as the **reference implementation** of the first version, and as the legacy path for a machine installed from a checkout. Nothing the instructions or runbooks say points at them.

### Session start & availability

On session start the agent runs `status` first:

- **`MISSING`** — nothing at the configured path → **stop and ask the user** (see below).
- **`EMPTY`** — DB exists but holds no memories → check `recall --candidates`, then offer to **backfill** from a directory.
- **`DEGRADED n`** — no embeddings (Ollama down, the model not pulled, or `EMBEDDINGS=off`; the next line says which) → still load baseline, recall is keyword-only, stores still work (saved without an embedding). `n` is as below.
- **`ERROR`** — fall back to the agent's built-in memory store.
- **`READY n`** — load baseline and proceed. `n` is a liveness signal, not a corpus size: it counts *all* `semantic_memory` rows (superseded and retired included) plus *all* `episodic_memory` rows. Never compare it against a current-rows count such as `sleep --staleness`'s `n_current`, and never diff it across time — the DB is shared, so it moves on its own.

An `unembedded: …` line under either state counts the active rows still waiting for `embed --missing`.

### Never lose a database by accident

The most likely cause of `MISSING` is a **wrong path**, not lost data — and the two are indistinguishable unless you look. An agent that responds by creating a fresh DB silently strands the real one; an agent that restores a backup over a live DB loses everything since that backup. So nothing in this system ever creates, restores, or overwrites a database on its own:

- No command creates a database except `init` (and `restore`, into a fresh file you name); every other one refuses a missing database rather than creating it.
- `status` on `MISSING` prints the configured path, **where that path came from** (real environment vs `.env` vs built-in default), and any other memory DB or backup it found — each with its memory count.
- `recall --candidates` runs that search on demand, at any time.
- The agent instructions require it to work through *wrong path → candidate DB → ask the user* before restore is even considered, and creating an empty DB is the last resort with explicit confirmation.
- `restore` never writes over the configured database, not even with `--force`.

A path that silently defaults is the root of this whole failure mode, so the path lives in the `.env` beside the binary, written by `init`, where every session — interactive or not — finds it. A path in a shell profile alone is not enough: the agent's shell never sources it.

## How it works

One Rust binary; the commands are thin layers over a `Store` that owns every SQL statement.

- **`remember`** — one memory = one row (no chunking). Folds `--keywords` into the text, embeds it when it can, inserts. Guards: baseline needs `--confirm-baseline`; semantic refuses a near-duplicate (cosine < 0.10 with embeddings, a normalised text match without) unless `--force` (episodic is exempt — events recur); `--supersedes <id>` inserts a revision and marks the old row superseded in one transaction; refuses a table already embedded with a different model; refuses an empty or malformed `--created-at` (`YYYY-MM-DD HH:MM:SS`, or a bare `YYYY-MM-DD` normalised to midnight — anything else is kept verbatim by SQLite and read as NULL by every date function); refuses text over the 2000-character cap, naming how much of it is the appended keyword line.
- **`recall`** — hybrid search, scored `dist - RECALL_ALPHA * kw` (lower wins): brute-force `vector_distance_cos` minus an IDF-weighted keyword credit, normalised to 0..1. IDF is measured per query against the rows being ranked, so common words demote themselves — there is no stopword list, and IDF applies from 5 rows (below that every query token weighs the same) — and the 8 *rarest* query tokens are the ones that count. With no embeddings it ranks by keywords alone and says why; rows stored without a vector are listed in a section of their own. Also `--baseline`, `--topics`, `--candidates`, `--count`. `RECALL_ALPHA` is corpus-calibrated, not universal: it trades cosine-distance units against keyword credit, so re-measure it (`eval --sweep`) after an embedding-model change or a large topic shift.
- **`list` / `show`** — the read surface the runbooks need without a SQL client: filtered metadata, and whole rows (embedding never).
- **`instruct` / `instructions`** — directives kept in a table of their own; never loaded at session start, never read by sleep.
- **`embed --missing`** — embeds the active rows that have none, resumably, batch by batch.
- **`sleep`** — the mechanical primitives a sleep pass needs, with no judgment of its own: `--mark-processed`, `--retire`, `--rebuild-topics`, plus deep sleep's `--purge` (guarded hard delete), `--cluster`, `--contradiction-candidates`, `--staleness` and `--verify-candidates` (read-only JSON).
- **`eval`** — the recall-quality harness: validate the authored eval set, score the shipped ranking, sweep `RECALL_ALPHA`, author and repoint cases.
- **`backup` / `restore`** — a verified dump and a counted restore (below).
- **`init` / `update` / `config` / `runbook` / `find-existing-memory`** — the machine-level commands described above.
- **`coworkers`** — manage AI personas (below).

## Database schema

Tables are in `schema.sql` (embedded in the binary; `init` creates them, and `update`'s migrations add what a later release introduced). `semantic_memory`, `episodic_memory` and `instructions` each hold one row per entry and embed `memory_text` into `embedding` for brute-force `vector_distance_cos` search. Hard caps are enforced via `CHECK` (SQLite ignores declared VARCHAR sizes): `memory_text` ≤ 2000; `project`/`topic`/`source`/`model`/`embed_model` ≤ 128. Enums are `CHECK`-enforced. **Keywords are not a column** — they live inside `memory_text` (appended by `remember --keywords`), so they're both embedded and searchable. An `embedding` of `NULL` is a row stored without one.

### `semantic_memory` — facts, timeless, revisable

`id`, `created_at`, `updated_at`, `project` (NULL = global; a tracking-tool work-item id), `topic`, `category` ∈ `baseline|user|feedback|project|reference|pattern`, `source`, `model`, `embed_model`, `memory_text`, `file_reference`, `embedding F32_BLOB(1024)`, `superseded_by` (current truth = `WHERE superseded_by IS NULL`), `retired_at` (soft-delete set by a sleep pass — current truth also requires `retired_at IS NULL`).

`baseline` = must-always-apply rules, loaded every session start (`recall --baseline`), not by search. **Storing a baseline memory requires explicit user confirmation** (`--confirm-baseline`). None are seeded by default.

`pattern` = **derived**, written only by a deep sleep pass: a recurrence or trend found across episodic events, carrying the episodic ids it came from so the claim can be re-checked rather than trusted. Revised through the normal supersede chain when the count changes.

A hard `DELETE` happens in exactly one place — deep sleep's purge gate, on rows the user selects, after a backup (see *Deep sleep*). Everywhere else, obsolescence is `retired_at` or a supersede.

### `episodic_memory` — events, time-anchored, append-only

`id`, `created_at` (event time), `project`, `topic`, `event_type` ∈ `project_start|bug_fix|feature_complete|decision|milestone|incident|note`, `importance` ∈ `routine|notable|major`, `source`, `model`, `embed_model`, `memory_text`, `file_reference`, `embedding F32_BLOB(1024)`, `processed_at` (set once a sleep pass has sifted this row; NULL = not yet processed).

### `instructions` — directives, on demand

Same shape as a semantic row, with `retired_at` as the only lifecycle (`instruct --replaces <id>` retires the old row in the same transaction as the insert). Looked up with `instructions`, never loaded at session start.

## Coworkers

Optional named AI personas with scoped memory and trust-gated autonomy. The `new-coworker` runbook (`runbook new-coworker`) explains how to construct a coherent personality. Three tables, no changes to `semantic_memory`/`episodic_memory`: `coworkers` (name/expertise/personality/trust_level/active), `memory_coworkers` (many-to-many — untagged = global, tagged = visible only when that coworker is loaded), `appraisals` (one current row per coworker, history via `superseded_by`).

- **`coworkers`** — `--add`, `--appraise`, `--set-trust`, `--retire`/`--reactivate`, and the reads: `--list` and `--load <name>` (the coworker, its current appraisal and the current feedback tagged to it since).
- **`remember --coworker name[,name...]`** — tag a memory to one or more coworkers instead of leaving it global; dedup scopes to that coworker's visible set.
- **`recall --coworker name`** — scope a semantic search to memories visible to that coworker (untagged ∪ tagged-to-them).
- `trust_level` (`supervised|trusted|autonomous`) never loosens a global baseline rule — it's a hard floor regardless of which coworker is active.

## Sleep cycle

A user-triggered ("sleep" / "go to sleep") consolidation pass — never scheduled automatically. The procedure is the `sleep` runbook (`runbook sleep`); no new memory tables, just three additions.

The instructions deliberately carry only a **one-line pointer** to that runbook, not a summary of the procedure — sleep happens rarely, so its steps are read on demand and cost nothing in the sessions that never sleep. Same reasoning for backups. Keep the *policy* in the template (user-triggered only) and the *procedure* in the runbook.

- `episodic_memory.processed_at` — sleep sifts unprocessed rows, keeping only ones where something was actually learned (discarding bare event sequences like "x happened, then y happened"), promotes the kept ones into `semantic_memory` (via `remember`, same as any other store/supersede), then stamps `processed_at` on every row it looked at — kept or discarded — so nothing gets re-sifted forever.
- `semantic_memory.retired_at` — sleep also sweeps current semantic memory for near-duplicates to consolidate (`remember --supersedes`) and obsolete facts to retire. Retirement is **soft-delete only** (`sleep --retire`, sets `retired_at`) — same never-hard-delete philosophy as coworkers' `active=0`. "Obsolete" has no fixed rule; ask the user when it's not clear-cut.
- `topic_keywords` — a curated topic → keywords index, rebuilt wholesale each sleep (`sleep --rebuild-topics`: full DELETE + re-INSERT, never accumulated). **Loaded in full at every session start** (`recall --topics`, same slot as `--baseline`) so a session knows *what topics have memory* without having to guess a search term first. This puts pressure back on sleep's consolidation step to keep the topic count bounded — `recall --topics` warns past 50 topics as a nudge to merge harder next sleep, since that many would cost real context budget every session.

### Where the reading happens: subagents

Both sleep runbooks push all bulk reading into **subagents**. The orchestrating agent queries skinny metadata (`list`: ids, topics, dates) and never holds `memory_text` in bulk — that cost scales with the corpus, which is precisely what a memory system is supposed to grow. Workers get self-contained prompts (their ids, the `show` command, the judgment rule, the exact `remember` call) and report one line per row.

Who may write is split on risk: episodic-sift workers write via `remember` themselves (high volume, low stakes), while compaction and pattern workers only *propose* and the user approves in one batch. `--mark-processed` stays with the orchestrator and covers only ids a worker actually reported back — a worker that dies leaves its rows unprocessed, which is the recoverable state.

## Deep sleep

A second, deeper pass — the `deep-sleep` runbook, reached by saying "deep sleep" or by answering yes to the offer at the end of a normal sleep. Also user-triggered only. It does the three things a normal pass deliberately doesn't:

| Phase | What it does |
|-------|--------------|
| D0 | Normal sleep (prerequisite) + `status`. `DEGRADED` does not stop it: the phases that need stored vectors (D3, D4's map stage, D6 scoring) are skipped, and the report says so, unless `embed --missing` can run first. |
| D1 | Backup. Mandatory: D2 is the only operation in this system that destroys a memory. |
| D2 | **Purge gate.** Lists every superseded/retired semantic row and hard-deletes *the ones the user selects*. No default policy — asked every run. |
| D3 | **Compaction.** Clusters all current semantic memory and merges same-topic near-duplicates. |
| D4 | **Pattern mining.** Derives recurrences and trends from the whole episodic log. |
| D5 | Rebuild `topic_keywords`, report. |
| D6 | **Recall check.** Validate `eval_cases`, report `recall@1/@5/MRR` vs the last `eval_runs` row, propose up to 3 new cases, and — only on a flagged regression — sweep `RECALL_ALPHA` and *ask* whether to change it. |
| D6.5 | **Memory-quality metrics.** Age profile of current rows plus an adjudicated contradiction rate — the two numbers ranking metrics cannot express. Reported, never persisted. |
| D7 | **Verify.** Lists current rows quoting a checkable artifact (a path, a command and its flags, an env var or `.env` key, a version) and has workers check each one against the machine as it stands. Staleness is invisible to retrieval metrics. |

D6 runs after D2 and D3 for a reason: those two are what break an eval set. A purged row is gone and a merged row is superseded, so a case pointing at either looks exactly like a ranking regression while being nothing of the sort — validation has to happen after them, not before. D7 is the one phase that follows D6, and only because it must: it is the sole remaining phase that can retire a row, so putting it earlier would reintroduce the very problem D6's placement solves. The price is that a retirement in D7 has to be followed by re-running `eval --validate`.

Two things a consolidation pass structurally cannot do are split out into their own phases. **D6.5** reports the memory-quality metrics that ranking cannot express: the age profile of current rows (`sleep --staleness`) and a contradiction rate adjudicated by workers over a mechanically-built shortlist (`sleep --contradiction-candidates`, which reuses the `--cluster` grouping at a tighter 0.15 threshold — just above `remember`'s own 0.10 dedup cutoff, because two rows that disagree about the same subject are near-identical in vector space without being duplicates). **D7** is the Verify stage: staleness of a stored fact is invisible to retrieval metrics, since a row can rank first and still name a renamed flag, so `sleep --verify-candidates` lists current rows quoting a checkable artifact, oldest first with artifact-class count as tiebreak, and workers check each artifact against the machine. Both primitives are read-only and decide nothing; retiring or superseding stays a user call, exactly as in D2 and D3.

Ordering candidates by how *often* a row is actually recalled would be the better signal — a frequently-retrieved stale row is the worst case — but the only way to know that is to increment a counter on every search, which makes `recall` a writer on the read path. Keeping recall a pure reader won that trade; age plus artifact density is the accepted proxy.

The eval set is **runtime state, not repo content** — its `expect` values are live row ids — so it lives in the DB, in `eval_cases` (authored cases, soft-deleted via `retired_at`) and `eval_runs` (one row per run). That puts it inside the dump, which is deliberate: cases are authored and cannot be regenerated from the corpus, and a past corpus cannot be re-measured. The only file left on disk is the query-embedding cache `<db parent>/eval/qvec-<model>.json` — pure derived data. There is no auto-generated eval set: an LLM writing a query while looking at the row it should retrieve produces lexical overlap a real user never produces, which would bias every future alpha upward. Cases are drafted from a row's `topic` and `keywords` only, never its text, and the user approves each one. The schema stores each target's `created_at` at authoring time (`expect_stamps`), because row ids are reused after a purge and the stamp is the only thing that catches a case silently re-pointing at an unrelated memory.

`RECALL_ALPHA` is never changed unattended. The harness reports the recall@5 plateau and whether the configured value still sits inside it; if it doesn't, the agent asks, and a change goes into the `.env`.

**Purge** (`sleep --purge <ids> --confirm-purge`) is the sole exception to the never-hard-delete rule, and it is fenced: it refuses a current row, an unknown id, a missing `--confirm-purge`, any id whose deletion would strand a surviving row that points at it, and any run where the newest backup is not strictly newer than the database's last write (and does not decompress to its end). It also clears the rows' `memory_coworkers` entries — that table has no FK on `memory_id`, so an orphan would silently re-scope a future memory that reuses the id. Episodic memory is never purged, and the `instructions` table is outside sleep entirely. Pass all ids in **one** call: a purge is itself a write, so a second call in the same pass fails the backup gate.

**Clustering** (`sleep --cluster [--table semantic|episodic] [--threshold N]`) is mechanical — no LLM, no new embeddings. It runs pairwise `vector_distance_cos` over the vectors already stored and emits connected components as JSON, plus any rows with no embedding, listed separately rather than silently skipped (and a refusal when none has one). Semantic defaults to `0.22`, episodic to `0.25` — both measured against a real ~230-row corpus, not guessed. The useful range is much tighter than intuition suggests, because connected components **chain**: if a–b are close and b–c are close, a and c land in the same cluster even when they have nothing to do with each other. On that corpus `0.22` yields ~11 pairs and triples, `0.30` already produces a 33-row blob, and `0.35` collapses 154 of ~200 rows into one "cluster". The command therefore refuses to present that as a finding: when the largest cluster exceeds 12 rows *and* a quarter of all clustered rows, the JSON carries a `warning` telling you to lower the threshold. Merges then apply via `remember --supersedes <id1,id2,id3>` — one insert, all listed rows pointed at it, one transaction.

**Pattern mining** is two-stage, because there are two kinds of pattern and no single worker can see both. A map stage partitioned by *episodic embedding cluster* — not by exact `topic`, since episodic topics are near-unique per ticket and exact grouping would yield all singletons — catches within-topic recurrence ("that service restarted four times"). Then one reduce worker reads only the tiny per-row digests and catches cross-topic shapes ("a whole class of bug keeps coming back") that a topic-scoped worker structurally cannot see. A pattern needs ≥3 supporting events, a *trend* claim also ≥2 distinct calendar months, and its `evidence_ids` go into the memory text so a later session can verify the claim instead of trusting it.

### `topic_keywords` — curated topic index, derived (not a source of truth)

`topic` (primary key), `keywords`, `updated_at`. Deliberately unlinked to `semantic_memory`/`episodic_memory` — no FK, no embedding, filtered by `recall --topics <word>`.

## Concurrency & locking

`tursodb` takes an **exclusive** file lock — a second process can't even read — **unless every opener enables the multiprocess WAL**. The binary opens the database that way on every command, and the recommended MCP registration passes `--experimental-multiprocess-wal`: multiple agent instances share the DB, reads run concurrently, and writes serialize. A process that opens it without the setting is refused. The setting is **experimental** — that's the trade for concurrency.

On Windows the WAL needs a VFS beside it (`--vfs experimental_win_iocp`), or the open fails with `experimental multiprocess WAL is not supported by the active IO backend`. The binary pairs them itself (`TURSO_VFS`, see the table above); a `tursodb` command you type by hand, and the MCP registration, need both flags. Dropping the WAL flag is not an alternative — that is what re-introduces the exclusive lock.

## Backup & restore

- **Backup** — `backup` writes a **verified**, gzipped SQL dump `YYYY-MM-DD-supercharged-memory.sql.gz` into `BACKUP_DIR` from one read snapshot (writers are never blocked, so it runs while a session is open) and prints its path on stdout. Verification re-counts every table against the rows written and decompresses the gzip it wrote, in-process; nothing replaces an older dump until the new one passed. Retains **3 daily + 4 weekly** (Monday) copies. It needs no `tursodb`. It refuses a missing, empty or non-database file, so a bad path cannot rotate the real dumps out.
- **Scheduling** — `init` prints the schedule for your platform (a systemd user timer, launchd, Task Scheduler or cron) that runs `<binary> backup` daily; it never installs one. A scheduled run uses the same `.env` as every other command.
- **Restore** — into a fresh DB file, counted:
  ```bash
  supercharged-memory restore --out /path/to/new.db            # newest dump in BACKUP_DIR by default
  supercharged-memory restore --dump <file>.sql.gz --out /path/to/new.db
  ```
  It reads this binary's dumps and tursodb's own `.dump`, `.sql` and `.sql.gz` alike, splits them with the engine's own parser, and prints a per-table `in dump` vs `restored` comparison; any mismatch, rejected statement or cut-off dump exits non-zero and leaves nothing at `--out`. It refuses a target that exists (`--force` replaces one, only after the restore is verified) and never the configured database or its sidecar files. **A restore that was not counted is not a restore.** Restoring is never done without the user's explicit ok, and switching to the restored file is a separate step (`SUPERCHARGED_MEMORY_TURSO_PATH` in the `.env`).

  **Do not pipe a dump into `tursodb`**, and do not use its `.read`. The CLI splits its input on **line** boundaries, but `memory_text` contains newlines, so most of a dump's lines are continuations *inside string literals* — in one measured dump, 4378 of 5034. The first multi-line `INSERT` ends its statement mid-value and everything after it is parsed as garbage until something happens to parse again. The result is an arbitrary **fraction** of the rows (98 of 389 in the observed case) together with a misleading `Parse error: table 'eval_cases' does not exist` — a partial restore that looks like it mostly worked.
- **New machine:** install the binary, `restore` the latest dump into a fresh file, then `init --db-path <that file>` adopts it and writes the `.env` and the instructions.

## Data protection

**Never store PII — anywhere.** Anonymize before calling `remember`/`backfill`. Store pointers (ticket/case ids, role labels) instead of personal data. This applies to the memory text *and* anything sent to Ollama to embed.

## License

[GNU General Public License v3.0](LICENSE).
