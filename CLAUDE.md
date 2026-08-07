# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this repo is

The **source-controlled bootstrap** for a local, Turso-only long-term memory system for AI coding agents (Claude Code). This repo holds the scripts, schema, and CLAUDE.md template — **not** the runtime state. At runtime the pieces live *outside* the repo:

- **The DB** (default `${XDG_DATA_HOME:-~/.local/share}/turso/supercharged-memory.db`, XDG-conformant; override with `SUPERCHARGED_MEMORY_TURSO_PATH`) — the single source of truth. Must be **local only, never in a cloud-synced folder** (cloud sync corrupts live SQLite).
- **The active instructions** (`~/.claude/CLAUDE.md`) — rendered from `CLAUDE.md.template` and installed between managed markers by `install-claude-md.sh`.
- **Backups** — a local `Backups/` folder written by a daily launchd job.

So editing `CLAUDE.md.template` or `scripts/*` here changes behavior only after re-running the installer / the next session. There is **no build, lint, or test tooling** — the scripts are plain Python 3 stdlib (no dependencies) plus bash.

`README.md` is the canonical design doc. `CLAUDE.md.template` is what agents actually follow at runtime — keep it and `README.md` consistent when changing behavior.

## Machine setup

`instructions/SETUP.md` is an executable runbook: when the user says "run SETUP.md" (or asks to set up this machine), follow that file step by step. It verifies/installs the two dependencies (asking before any install), ensures the `bge-m3` model is pulled, and runs `install-claude-md.sh`. Don't improvise a setup flow — use that file.

## Sleep cycle

`instructions/SLEEP.md` is an executable runbook: when the user says "sleep" or "go to sleep", follow it step by step — never run it on a schedule, only when told. It sifts unprocessed episodic memory for actual lessons (discarding bare event sequences), promotes the rest into semantic memory, consolidates/retires overlapping or obsolete semantic facts (soft-delete via `scripts/sleep.py --retire`, never a hard `DELETE`), then rebuilds `topic_keywords` — the agent derives the topic/keyword groupings itself (judgment stays with the LLM); `scripts/sleep.py --rebuild-topics` is a dumb atomic DELETE+INSERT on top. `topic_keywords` is loaded in full at every session start (`recall.py --topics`, same slot as `--baseline`), so keep its row count bounded by consolidating hard — it costs context every session, not just when queried.

## Keeping an install in sync — and breaking changes

`instructions/UPDATE.md` is an executable runbook: when the user says "update" (or asks to sync this machine with the repo), follow it step by step. It reads the **sync stamp** — `<!-- supercharged-memory: synced-at <sha> -->`, written into the managed block by `install-claude-md.sh` — pulls, applies whatever migration notes landed since that commit, then re-renders the template. The repo's git history is the ledger; there is no separate state file.

**Any change that breaks an existing install must ship a note in `migration-steps/`.** That means a schema change, a script CLI/flag change, a renamed env var or path, a changed instruction contract, or anything the user must run by hand. The runtime state lives *outside* this repo (the DB, `~/.claude/CLAUDE.md`, `~/.claude/settings.json`), so a commit here can leave a machine broken with no way to find out what happened.

One markdown file per breaking change, `migration-steps/YYYY-MM-DD-<slug>.md`:

- `commit:` — the git sha that **introduced** the break, as the first line. UPDATE.md classifies notes with `git merge-base --is-ancestor`, so a note without a resolvable sha is reported as malformed rather than applied.
- **What broke** — schema / CLI / instruction contract / on-disk runtime state
- **How to resolve** — concrete commands, in order
- **Verification** — what to run afterwards, and what output proves it worked

A commit cannot reference its own sha, so **the note goes in the commit immediately after** the breaking one and points back at it. Push both together — otherwise a `git pull` can land a machine between the break and its instructions.

## Runtime dependencies (not installed by this repo)

- **tursodb** (Turso 0.7.1, Rust SQLite rewrite w/ native vectors) — `curl -sSL tur.so/install | sh`. Registered as the `turso` MCP server for ad-hoc SQL. Its `current_database` MCP tool misreports `:memory: (default)` for a CLI-opened DB ([upstream #8061](https://github.com/tursodatabase/turso/issues/8061)) — verify with a real `SELECT`, never `open_database`.
- **Ollama** at `localhost:11434` running the **bge-m3** embedding model (`ollama pull bge-m3`), 1024-dim.

## Common commands

All scripts honor env overrides `TURSO_BIN`, `SUPERCHARGED_MEMORY_TURSO_PATH`, `BACKUP_DIR`, `OLLAMA_URL`, `EMBED_MODEL` (defaults in `scripts/memlib.py`).

```bash
# Activation — render template into ~/.claude/CLAUDE.md (idempotent; re-run after any edit)
bash scripts/install-claude-md.sh

# Session start (the agent runs this first): MISSING | EMPTY | DEGRADED n | ERROR | READY n
python3 scripts/recall.py --status
python3 scripts/recall.py --baseline            # rules to load every session
python3 scripts/recall.py --topics              # topic_keywords index, load every session
python3 scripts/recall.py --candidates          # other memory DBs/backups — ALWAYS check before creating one

# Recall (hybrid vector + keyword)
python3 scripts/recall.py "<query>" [--table semantic|episodic|both] [--project <id>] [--k N] [--coworker <name>]

# Store one memory (see remember.py docstring for the full flag set)
python3 scripts/remember.py --table semantic --topic <t> --category <c> --keywords "..." --source <s> --model <id> --text "..."
python3 scripts/remember.py --table semantic --supersedes <old-id> ...   # revise a fact
python3 scripts/remember.py --table episodic --event-type <e> --importance <i> ...

# Bulk import a directory (fresh start; one file = one memory)
python3 scripts/backfill.py --dir <path> [--glob '*.md'] [--category project]

# Coworkers (writes only; reads are ad-hoc SQL via the turso MCP)
python3 scripts/coworkers.py --add --name <n> --expertise "..." --personality "..."
python3 scripts/coworkers.py --appraise <name> --trust <level> --text "..."

# Sleep (mechanical write primitives; see instructions/SLEEP.md for the full procedure)
python3 scripts/sleep.py --mark-processed <id1,id2,...>   # episodic_memory.processed_at
python3 scripts/sleep.py --retire <id>                     # soft-delete a semantic memory
echo '[{"topic":"...", "keywords":"..."}]' | python3 scripts/sleep.py --rebuild-topics

# Backup / restore / rebuild
bash scripts/supercharged-memory-backup.sh
gzcat "$(ls -1t Backups/*-supercharged-memory*.sql.gz | head -1)" | tursodb "$SUPERCHARGED_MEMORY_TURSO_PATH" --experimental-multiprocess-wal   # restore into a FRESH db file
tursodb "$SUPERCHARGED_MEMORY_TURSO_PATH" --experimental-multiprocess-wal < schema.sql   # rebuild empty schema — PIPE it, don't pass as arg
python3 scripts/seed.py                          # empty by default; add SEM/EPI entries first
```

## Architecture

**`scripts/memlib.py` is the shared core** every script imports — config/env, `embed()` (asserts 1024 dims), vector literal formatting, SQL escaping (`q`, `like_lit`), and `exec_sql()`, the one tursodb runner: it detects errors on **stderr only** (so row data on stdout can't false-trigger) and retries with backoff on `busy|locked`. Everything else is a thin CLI on top of it.

**Two memory tables (`schema.sql`), one row per memory, no chunking:**
- `semantic_memory` — timeless facts, **revisable** via a supersede chain (`superseded_by IS NULL` = current truth) and soft-deletable via `retired_at` (set only by `sleep.py --retire`; current truth also requires `retired_at IS NULL`). Category ∈ `baseline|user|feedback|project|reference`.
- `episodic_memory` — time-anchored events, **append-only** (no dedup — events recur). `processed_at` marks whether a sleep pass has already sifted a row.
- `baseline` rows are loaded every session (not by search) and encode must-always-apply rules.

**Sleep layer** — user-triggered consolidation (`instructions/SLEEP.md`), no new tables for episodic/semantic, just the two columns above plus one unlinked `topic_keywords(topic PK, keywords, updated_at)` table: a curated topic → keywords index, rebuilt wholesale each sleep (`sleep.py --rebuild-topics`, DELETE+re-INSERT). The agent derives topics/keywords itself from current semantic memory — the script only does the atomic write. `CLAUDE.md.template` points every session at this table (substring `LIKE`, ad-hoc SQL) so "no memory found" isn't mistaken for "nothing to look for."

**Hybrid recall** (`recall.py`): brute-force `vector_distance_cos` ranked with a keyword boost = count of meaningful query tokens found in the text (stopwords dropped, digit-bearing id/error tokens always kept). If Ollama is down, it **degrades gracefully to keyword-only** — the DB stays usable, but new memories can't be stored (no embeddings).

**Coworkers layer** — named personas with scoped memory + trust-gated autonomy, added without touching the two memory tables: `coworkers`, `memory_coworkers` (many-to-many; **no rows = global/visible-to-all**), `appraisals` (one current row per coworker, same supersede pattern). `coworkers.py` does writes only; loading/listing a coworker is ad-hoc SQL via the turso MCP.

## Invariants — do not break these

- **`--experimental-multiprocess-wal` on EVERY opener** (MCP, scripts, backup). tursodb takes an exclusive file lock otherwise — a process without the flag is *refused*, and one without it would block all readers. This is what lets multiple Claude instances share the DB (concurrent reads, serialized writes). It's experimental — that's the trade.
- **One embedding model per DB** (bge-m3, 1024-dim, recorded in `embed_model`). Mixing models makes cosine meaningless; `remember.py` refuses a table that already holds another model. To switch models you must rebuild + re-embed.
- **Keywords are not a column** — `remember.py --keywords` appends them into `memory_text` so they're both embedded and LIKE-searchable.
- **Length caps are enforced by `CHECK`, not VARCHAR** (SQLite ignores declared sizes): `memory_text` ≤ 2000; most metadata ≤ 128.
- **Never store PII** — anonymize before `remember.py`/`backfill.py`; store pointers (ticket ids, role labels). Applies to the text *and* anything sent to Ollama to embed.
- **`CLAUDE.md.template` is loaded into context every session — keep it terse.** Prefer short imperative lines over prose; anything an agent needs only occasionally belongs in `README.md`/`instructions/`, which the template points at. The installer renders only the *active* episodic mode into `{{EPISODIC_RULE}}`, so adding a mode means adding its one-line rule to the `case` in `install-claude-md.sh` as well as the validation list.
- **Rare on-request procedures get a pointer, not a summary.** Sleep and backup live under the template's "On user request only" heading as one line each — an absolute `{{BASE_PATH}}` path to `instructions/SLEEP.md` / `scripts/supercharged-memory-backup.sh`, nothing about *how* they work. The steps are then read only in the rare session that actually sleeps. Keep the *policy* (user-triggered only, never scheduled/proactive) in the template, since an agent must know that without opening the file, and keep the *procedure* in the runbook. Don't let a summary creep back in.
- **`BASE_PATH` in `CLAUDE.md.template` is machine-specific** — the one value to update on a new laptop (the installer defaults it to the repo's parent dir).
- When rebuilding schema, **pipe** `schema.sql` — `tursodb "$(cat schema.sql)"` fails because the leading `--` comment parses as a CLI flag.
- Don't hand-edit `superseded_by` — use `remember.py --supersedes` (semantic) / the appraisal flow (coworkers), which insert + supersede in one transaction.
- **Never hard-delete a memory** — `retired_at` (semantic) is soft-delete only, set via `sleep.py --retire`; same never-delete philosophy as coworkers' `active=0`. Ask the user when obsolescence isn't clear-cut.
- **Never create, restore, or overwrite a DB unprompted.** `MISSING` almost always means a wrong path, not lost data. Run `recall.py --candidates`, show the user what was found, and ask — creating an empty DB strands the real one, and restoring a backup over a live DB loses everything since that backup. `require_db()` enforces the no-auto-create half; the rest is instruction-level in `CLAUDE.md.template`.
- **`SUPERCHARGED_MEMORY_TURSO_PATH` must live in `~/.claude/settings.json` under `env`** — Claude Code's Bash tool is non-interactive and never sources `~/.zshrc`/`~/.bashrc`, so a profile export alone leaves the scripts on the default path. Default is XDG: `${XDG_DATA_HOME:-~/.local/share}/turso/supercharged-memory.db`.
- `sleep.py --rebuild-topics` is a **full replace**, not an upsert — always pass the complete current topic set on stdin, or you'll silently drop the topics you omit.
- **The sync stamp is load-bearing** — `install-claude-md.sh` writes `<!-- supercharged-memory: synced-at <sha> -->` as the first line inside the managed block, and `instructions/UPDATE.md` is built entirely on reading it back. Don't drop it, move it outside the markers, or change its wording without updating UPDATE.md's `grep`. A `-dirty` suffix means the install was rendered from uncommitted work; `unknown` means `BASE_PATH` wasn't a git work tree.
- **A breaking change ships its `migration-steps/` note in the NEXT commit** — a commit can't contain its own sha, and the note's `commit:` anchor is what makes it verifiable. Push the pair together so no `git pull` lands between the break and its instructions.
