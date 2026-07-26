# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this repo is

The **source-controlled bootstrap** for a local, Turso-only long-term memory system for AI coding agents (Claude Code). This repo holds the scripts, schema, and CLAUDE.md template — **not** the runtime state. At runtime the pieces live *outside* the repo:

- **The DB** (`~/Documents/turso/agent-foundations.db`) — the single source of truth. Must be **local only, never in a cloud-synced folder** (cloud sync corrupts live SQLite).
- **The active instructions** (`~/.claude/CLAUDE.md`) — rendered from `CLAUDE.md.template` and installed between managed markers by `install-claude-md.sh`.
- **Backups** — a local `Backups/` folder written by a daily launchd job.

So editing `CLAUDE.md.template` or `scripts/*` here changes behavior only after re-running the installer / the next session. There is **no build, lint, or test tooling** — the scripts are plain Python 3 stdlib (no dependencies) plus bash.

`README.md` is the canonical design doc; `docs/2026-07-23-ai-coworkers-design.md` covers the coworkers layer. `CLAUDE.md.template` is what agents actually follow at runtime — keep it and `README.md` consistent when changing behavior.

## Machine setup

`instructions/SETUP.md` is an executable runbook: when the user says "run SETUP.md" (or asks to set up this machine), follow that file step by step. It verifies/installs the two dependencies (asking before any install), ensures the `bge-m3` model is pulled, and runs `install-claude-md.sh`. Don't improvise a setup flow — use that file.

## Runtime dependencies (not installed by this repo)

- **tursodb** (Turso 0.7.0, Rust SQLite rewrite w/ native vectors) — `curl -sSL tur.so/install | sh`. Registered as the `turso` MCP server for ad-hoc SQL.
- **Ollama** at `localhost:11434` running the **bge-m3** embedding model (`ollama pull bge-m3`), 1024-dim.

## Common commands

All scripts honor env overrides `TURSO_BIN`, `SUPERCHARGED_MEMORY_TURSO_PATH`, `BACKUP_DIR`, `OLLAMA_URL`, `EMBED_MODEL` (defaults in `scripts/memlib.py`).

```bash
# Activation — render template into ~/.claude/CLAUDE.md (idempotent; re-run after any edit)
bash scripts/install-claude-md.sh

# Session start (the agent runs this first): MISSING | EMPTY | DEGRADED n | ERROR | READY n
python3 scripts/recall.py --status
python3 scripts/recall.py --baseline            # rules to load every session

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

# Backup / restore / rebuild
bash scripts/agent-foundations-backup.sh
gzcat "$(ls -1t Backups/*-agent-foundations*.sql.gz | head -1)" | tursodb "$SUPERCHARGED_MEMORY_TURSO_PATH" --experimental-multiprocess-wal   # restore into a FRESH db file
tursodb "$SUPERCHARGED_MEMORY_TURSO_PATH" --experimental-multiprocess-wal < schema.sql   # rebuild empty schema — PIPE it, don't pass as arg
python3 scripts/seed.py                          # empty by default; add SEM/EPI entries first
```

## Architecture

**`scripts/memlib.py` is the shared core** every script imports — config/env, `embed()` (asserts 1024 dims), vector literal formatting, SQL escaping (`q`, `like_lit`), and `exec_sql()`, the one tursodb runner: it detects errors on **stderr only** (so row data on stdout can't false-trigger) and retries with backoff on `busy|locked`. Everything else is a thin CLI on top of it.

**Two memory tables (`schema.sql`), one row per memory, no chunking:**
- `semantic_memory` — timeless facts, **revisable** via a supersede chain (`superseded_by IS NULL` = current truth). Category ∈ `baseline|user|feedback|project|reference`.
- `episodic_memory` — time-anchored events, **append-only** (no dedup — events recur).
- `baseline` rows are loaded every session (not by search) and encode must-always-apply rules.

**Hybrid recall** (`recall.py`): brute-force `vector_distance_cos` ranked with a keyword boost = count of meaningful query tokens found in the text (stopwords dropped, digit-bearing id/error tokens always kept). If Ollama is down, it **degrades gracefully to keyword-only** — the DB stays usable, but new memories can't be stored (no embeddings).

**Coworkers layer** — named personas with scoped memory + trust-gated autonomy, added without touching the two memory tables: `coworkers`, `memory_coworkers` (many-to-many; **no rows = global/visible-to-all**), `appraisals` (one current row per coworker, same supersede pattern). `coworkers.py` does writes only; loading/listing a coworker is ad-hoc SQL via the turso MCP.

## Invariants — do not break these

- **`--experimental-multiprocess-wal` on EVERY opener** (MCP, scripts, backup). tursodb takes an exclusive file lock otherwise — a process without the flag is *refused*, and one without it would block all readers. This is what lets multiple Claude instances share the DB (concurrent reads, serialized writes). It's experimental — that's the trade.
- **One embedding model per DB** (bge-m3, 1024-dim, recorded in `embed_model`). Mixing models makes cosine meaningless; `remember.py` refuses a table that already holds another model. To switch models you must rebuild + re-embed.
- **Keywords are not a column** — `remember.py --keywords` appends them into `memory_text` so they're both embedded and LIKE-searchable.
- **Length caps are enforced by `CHECK`, not VARCHAR** (SQLite ignores declared sizes): `memory_text` ≤ 2000; most metadata ≤ 128.
- **Never store PII** — anonymize before `remember.py`/`backfill.py`; store pointers (ticket ids, role labels). Applies to the text *and* anything sent to Ollama to embed.
- **`BASE_PATH` in `CLAUDE.md.template` is machine-specific** — the one value to update on a new laptop (the installer defaults it to the repo's parent dir).
- When rebuilding schema, **pipe** `schema.sql` — `tursodb "$(cat schema.sql)"` fails because the leading `--` comment parses as a CLI flag.
- Don't hand-edit `superseded_by` — use `remember.py --supersedes` (semantic) / the appraisal flow (coworkers), which insert + supersede in one transaction.
