# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this repo is

The source of a **native Rust binary** — a local, Turso-only long-term memory system for AI coding agents (Claude Code) — together with what the binary embeds: the schema, the `CLAUDE.md` template and the runbooks. At runtime the pieces live *outside* the repo:

- **The DB** (default `${XDG_DATA_HOME:-~/.local/share}/turso/supercharged-memory.db`) — the single source of truth. Must be **local only, never in a cloud-synced folder** (cloud sync corrupts live SQLite).
- **The `.env` beside the binary** — the machine's settings (database path, backup directory, embeddings on/off, …). Written by `init`; read before every command.
- **The active instructions** (`~/.claude/CLAUDE.md`) — rendered from `CLAUDE.md.template` with the binary's own path and the chosen episodic mode, appended once by `init` between managed markers, and replaced by the agent from `update --print-block` when `update` reports a change.
- **Backups** — dumps in `BACKUP_DIR`, written by `backup` (scheduled daily by the user's own timer).

`CLAUDE.md.template`, `instructions/*.md`, `schema.sql` and `migrations/` are compiled into the binary (`include_str!`), so editing them changes behavior only in the next build, and on a machine only after `update` and the agent's replacement of the managed block. `README.md` is the canonical design doc for humans; keep it consistent with the template when you change behavior.

The Python scripts in `scripts/` (and `investigations/eval-harness.py`) remain in the repo as the **reference implementation** of the first version and as the legacy path for a machine installed from a checkout. They are not maintained alongside the binary, and nothing in the template or the runbooks points at them. Do not extend them.

## Build and test

```bash
cargo build
cargo test --no-fail-fast       # unit tests + tests/*.rs end-to-end tests against the built binary
cargo clippy --all-targets      # zero warnings
cargo fmt --check
```

Build for the host only. `build.sh` produces the release matrix and is run by whoever publishes a release, not during development. Tests use temp directories and path overrides: never point one at the live database, the real `~/.claude`, a real `.env`, or `backups/`, and never at a real Ollama (the tests inject a fake embedder). `cli::tests::code()` runs commands against the *real* environment, so a usage check for a command that opens the database must be a parse-only test.

Code written with test-driven development: the failing test comes first, and a documentation change is checked by a test that fails on the old text (`src/runbook.rs` tests the embedded texts, `tests/docs.rs` this file and the README).

## Machine setup

`instructions/SETUP.md` is the setup runbook, printed by `supercharged-memory runbook setup`: when the user asks to set up the memory system, follow it step by step. It is built around `init` (dry run → relay → `init -y`); Ollama + bge-m3 and tursodb + the read-only MCP are optional, recommended steps. Don't improvise a setup flow — use that runbook.

## Sleep cycle

`instructions/SLEEP.md` (`runbook sleep`) is the procedure for "sleep" or "go to sleep": follow it step by step, never run it on a schedule, only when told. It sifts unprocessed episodic memory for actual lessons (discarding bare event sequences), promotes the rest into semantic memory, consolidates/retires overlapping or obsolete semantic facts (soft-delete via `sleep --retire`, never a hard `DELETE`), then rebuilds `topic_keywords` — the agent derives the topic/keyword groupings itself (judgment stays with the LLM); `sleep --rebuild-topics` is a dumb atomic DELETE+INSERT on top. `topic_keywords` is loaded in full at every session start (`recall --topics`, same slot as `--baseline`), so keep its row count bounded by consolidating hard — it costs context every session, not just when queried.

## Deep sleep

`instructions/DEEP-SLEEP.md` (`runbook deep-sleep`) is the second runbook, reached by "deep sleep" or by answering yes to the offer at the end of sleep. It does the three things a normal pass deliberately doesn't: **D2** hard-deletes superseded/retired semantic rows *the user selects* (`sleep --purge`, after a mandatory backup in D1), **D3** clusters *all* current semantic memory (`sleep --cluster`, mechanical `vector_distance_cos` + connected components — no LLM) and merges same-topic near-duplicates via `remember --supersedes <csv>`, and **D4** mines the whole episodic log for recurrences and trends, storing each as a `category='pattern'` semantic row carrying the episodic ids it was derived from. **D6** then checks recall quality itself: it validates the eval set (D2 and D3 are what break it — purged and superseded rows look like ranking regressions), reports `recall@1/@5/MRR` for the shipped ranking against the last `eval_runs` row, proposes at most three new eval cases, and — only if the regression check fired — sweeps `RECALL_ALPHA` and **asks** the user whether to change it. The eval set lives in the DB (`eval_cases`, `eval_runs`) so the dump covers it — the cases are authored and cannot be regenerated, and a past corpus cannot be re-measured — and is edited only through `eval` (`--add-case`, `--repoint`, `--retire-case`). `eval_cases.expect_stamps` records each target's `created_at` at authoring time: `id INTEGER PRIMARY KEY` is a rowid alias with **no AUTOINCREMENT**, so a purged high id is handed to the next insert, and the stamp is the only thing that catches a case silently re-pointing at an unrelated memory. Only the query-embedding cache stays on disk. **D6.5** adds the two memory-quality numbers that ranking metrics cannot express — staleness distribution and an adjudicated contradiction rate — both reported, never persisted. **D7** is the Verify half of Keep/Remove/Move/Verify: `sleep --verify-candidates` lists current rows that quote a checkable artifact (path, command and flag, env var or `.env` key, version), oldest first, and workers check each artifact against `<exe> <cmd> --help` and the machine as it is today. It runs last so it cannot invalidate the eval set before D6 measures it — the price being that any retirement there must be followed by `eval --validate`. There is deliberately no retrieval-frequency signal: ordering by true "hotness" would make `recall` a writer on every query, and keeping recall a pure reader was the better trade.

D4 is two-stage on purpose: a map stage partitioned by *episodic embedding cluster* (not exact `topic` — episodic topics are near-unique per ticket, so exact grouping finds nothing) catches within-topic recurrence, and one reduce worker over tiny per-row digests catches cross-topic "class of bug" shapes that a topic-scoped worker structurally cannot see.

Both runbooks push all bulk reading into **subagents**: the orchestrator queries skinny metadata (`list`: ids, topics, dates) and never holds `memory_text` in bulk, since that cost scales with the corpus. Sift workers write via `remember` themselves; compaction and pattern workers only propose, and the user approves in one batch. `sleep --mark-processed` stays with the orchestrator and covers only ids a worker actually reported — a dead worker leaves its rows unprocessed, which is the recoverable state.

## Migrating file-based memory

`instructions/MIGRATE-EXISTING-MEMORY.md` (`runbook migrate`) brings ordinary file-based Claude memory — a curated `~/.claude/CLAUDE.md`, `~/.claude/memory/*.md`, project-scoped `~/.claude/projects/<slug>/memory/*.md` — into the database. `find-existing-memory` is its read-only probe: no DB, no Ollama, so setup can run it before either is proven working. It reports per file a `kind` (`memory` / `index` / `claude` / `empty`), a `scope` (`global` / `project`) and a char count against an effective budget of `MAX_TEXT` minus a 200-char keyword reserve (the writer appends `--keywords` into the same field the 2000-char CHECK counts), and every count of memory *to import* covers `kind: memory` only. Nothing it skips is dropped silently: `over_max_text`, `empty`, `unreadable` and `excluded_dirs` are reported fields, and the gate is the computed `nothing_to_do`, not four fields a reader has to combine.

Classification runs in **subagents** on the same contract as the sleep runbooks. Every imported row is tagged `source='migration'` with a `file_reference` back to its file — the resume position for an interrupted import, and the audit trail afterwards. The import is **additive, not reversible** (no bulk undo; the untouched source files are the way back, M2's pre-import dump restores a database that already held rows, and on `EMPTY` there is no dump because nothing is at risk), and a D3 merge in the follow-up phases keeps its inputs, so the database remains the audit trail.

## Keeping an install in sync — and breaking changes

`instructions/UPDATE.md` (`runbook update`) is the runbook for "update": the user replaces the binary in place; the agent runs `update`, relays its notes, migrations and CLAUDE.md diff, asks, then runs `update -y`, and replaces the managed block with `update --print-block`. The `.env` records the installed version (`SUPERCHARGED_MEMORY_VERSION`); the repo's version in `Cargo.toml` is the other end. The block's stamp `<!-- supercharged-memory: rendered-by <version> -->` is informational: `update` compares the block's **content** against a fresh render.

**Any change that breaks an existing install must ship a note in `migrations/<version>/`.** That means a schema change, a CLI/flag change, a renamed `.env` key or path, a changed instruction contract, or anything the user must do by hand. The runtime state lives *outside* this repo (the DB, the `.env`, `~/.claude/CLAUDE.md`), so a release can leave a machine broken with no way to find out what happened.

- `migrations/<version>/NOTE.md` — **What broke**, **How to resolve** (concrete steps, in order), **Verification** (what to run afterwards, and what output proves it worked). Embedded in the binary, printed by `update` for the versions between the `.env`'s and the binary's.
- A schema change also ships its SQL (`migrations/<version>/<id>.sql`, idempotent) and an entry with a **probe** in `src/update/migrations.rs` (`NOTES`/`MIGRATIONS`); the probe is what the database is asked, never the `.env`. A test fails until the directory and the tables agree. The version must not exceed `Cargo.toml`'s. The same SQL must be in `schema.sql`, so a database fresh from `init` holds every migration.
- Notes are keyed by **version**, so a note can land in the same commit as the change. The old rule — the note goes in the *next* commit, anchored to the breaking commit's sha — applied to the sha-keyed `migration-steps/` directory, which is now **frozen history** (notes for the Python-script install, applied by machines still on that layout). A machine installed from a checkout is moved onto the binary by the cutover note that `init` and `update` print for a legacy block (`<!-- supercharged-memory: synced-at <sha> -->`).

## Runtime dependencies (optional)

Nothing is required. Both are recommended, and each missing one is listed by `init` with what the user loses:

- **Ollama** at `localhost:11434` running the **bge-m3** embedding model (`ollama pull bge-m3`), 1024-dim. Without it: keyword-only recall, rows stored without an embedding (`embedding IS NULL`), text-only dedup, `status` = `DEGRADED n`; `embed --missing` catches up.
- **tursodb** (Turso 0.8.3 — the targeted version; 0.7.0+ is the floor) registered as the **read-only** `turso` MCP server for ad-hoc reads. The binary itself never calls it. Its `current_database` MCP tool misreports `:memory: (default)` for a CLI-opened DB ([upstream #8061](https://github.com/tursodatabase/turso/issues/8061)) — verify with a real `SELECT`, never `open_database`.

## Common commands

All commands read the settings in `config` (`SUPERCHARGED_MEMORY_TURSO_PATH`, `BACKUP_DIR`, `SUPERCHARGED_MEMORY_VERSION`, `EMBEDDINGS`, `OLLAMA_URL`, `EMBED_MODEL`, `RECALL_ALPHA`, `TURSO_VFS`, `TURSO_BIN`, `SUPERCHARGED_MEMORY_EVAL_DIR`): real environment > the `.env` beside the binary > built-in default. `supercharged-memory --help` lists every command; the ones a maintainer needs:

```bash
supercharged-memory config                       # every setting, its value, its source
supercharged-memory init [-y]                    # first run: dry-run plan, then apply
supercharged-memory update [-y | --print-block]  # after replacing the binary
supercharged-memory status                       # MISSING | EMPTY | DEGRADED n | ERROR | READY n
supercharged-memory recall "<query>" [--table semantic|episodic|both] [--project <id>] [--k N]
supercharged-memory recall --baseline | --topics [word] | --candidates
supercharged-memory remember --table semantic --topic <t> --category <c> --keywords "..." --source <s> --model <id> --text "..."
supercharged-memory remember --table semantic --supersedes <id1,id2,id3> ...   # revise a fact / merge N into 1
supercharged-memory embed --missing              # fill in embeddings stored without one
supercharged-memory list --table <t> [filters]   # skinny metadata; show --table <t> <ids> prints whole rows
supercharged-memory sleep --mark-processed <ids> | --retire <id> | --rebuild-topics | --purge <ids> --confirm-purge
supercharged-memory sleep --cluster | --verify-candidates | --contradiction-candidates | --staleness
supercharged-memory eval --validate | --report | --sweep a,b,c | --add-case ... | --repoint <case> <id>
supercharged-memory coworkers --list | --load <name> | --add ... | --appraise <name> --trust <level> --text "..."
supercharged-memory instruct --topic <t> --text "..."   # instructions --get <topic> | --list
supercharged-memory backfill --dir <path>        # bulk import, one file = one memory
supercharged-memory backup                       # verified dump into BACKUP_DIR, path on stdout
# restore into a FRESH db file. NEVER pipe a dump into tursodb — it splits input on line
# boundaries, memory_text contains newlines, and you get an arbitrary FRACTION of the rows
# plus a misleading "table ... does not exist". restore splits with the engine's own parser
# and COUNTS every table against the dump; it exits non-zero on a mismatch.
supercharged-memory restore --out /path/to/new.db [--dump <file>.sql.gz]
supercharged-memory runbook [setup|update|sleep|deep-sleep|migrate|new-coworker]
```

## Architecture

**`src/main.rs` → `cli.rs`** — one clap definition; `execute()` resolves the configuration before **every** command (a malformed `.env` is a refusal for all of them), then dispatches. Exit codes: 0 ok, 1 failed (`error:`), 2 refused (`refused:`; the mechanical guards), 64 usage.

**`store/`** is the one place SQL lives: the `Store` trait and its Turso implementation (`store/turso.rs`; `dump.rs`, `restore.rs`, `migrate.rs`, `candidates.rs` beside it). The Turso driver opens the file with the multiprocess WAL on, and **every** command goes through it, so concurrent sessions share the DB (concurrent reads, serialized writes). The rest are thin commands on top: `remember/`, `recall/`, `rows.rs` (`list`/`show`), `instructions/`, `embed/` (the `Embedder` trait, Ollama, `embed --missing`), `sleep/`, `eval/`, `coworkers.rs`, `backfill.rs`, `backup.rs`, `restore.rs`, `init/`, `update/`, `find_existing.rs`, `status.rs`, `runbook.rs`, `claude_md.rs`, `config/` (`.env` parser, resolver, precedence).

**Tables (`schema.sql`), one row per memory, no chunking:**
- `semantic_memory` — timeless facts, **revisable** via a supersede chain (`superseded_by IS NULL` = current truth) and soft-deletable via `retired_at` (set only by `sleep --retire`; current truth also requires `retired_at IS NULL`). Category ∈ `baseline|user|feedback|project|reference|pattern`.
- `episodic_memory` — time-anchored events, **append-only** (no dedup — events recur). `processed_at` marks whether a sleep pass has already sifted a row.
- `instructions` — directives the user wants followed on demand; `retired_at` only; never loaded at session start, never read by sleep.
- `baseline` rows are loaded every session (not by search) and encode must-always-apply rules.
- `topic_keywords` — the curated topic index, rebuilt wholesale each sleep; `coworkers`, `memory_coworkers`, `appraisals`; `eval_cases`, `eval_runs`.

**Embeddings are optional.** `embedding` and `embed_model` are NULL for a row stored without them. `recall` ranks the embedded rows by `vector_distance_cos - RECALL_ALPHA * kw` and lists the unembedded rows by keywords in a section of their own; `remember` falls back to a text-only duplicate check; `status` says `DEGRADED n` with the reason. Anything that reads stored vectors (`sleep --cluster`, the eval scoring modes) refuses rather than answer from an empty set.

**Hybrid recall** (`recall/`): rows are scored `vector_distance_cos - RECALL_ALPHA * kw`, lowest first, where `kw` is the IDF-weighted share of the query's tokens present in the row, normalised to 0..1. IDF is computed per query against exactly the rows being ranked, so common words demote themselves — there is **no stopword list and no minimum token length**, and the tokens carried into the score are the 8 *rarest* in the query, not the first 8. A subset whose rows all contain a token weighs it 0, so any split ranking must weigh tokens over the whole scope. This replaced an `ORDER BY kw DESC, dist ASC` lexicographic sort, which measured **worse than having no keyword layer at all** (R@5 0.84 vs 0.94 for pure cosine on a 32-case eval set). `RECALL_ALPHA` is **corpus-calibrated, not universal** — it trades cosine-distance units against keyword credit, and the useful range depends on how tightly that corpus's distances cluster. Re-measure with `eval --sweep` after an embedding-model change or a large topic shift.

**Eval layer** — `eval_cases` + `eval_runs`, written only by `eval` (and so by deep sleep D6). `eval_cases` holds authored `(query, expect_ids, expect_stamps)` triples soft-deleted via `retired_at`; `eval_runs` is the append-only regression baseline. Deliberately no FK from `expect_ids` to the memory tables — a child table would be one more thing `sleep --purge` must clean, whereas a dangling id in a TEXT column is inert and is exactly what `eval --validate` looks for. The eval tables are part of `schema.sql`; no migration creates them in a database that predates them (decided; the live database has them).

**Coworkers layer** — named personas with scoped memory + trust-gated autonomy, added without touching the two memory tables: `coworkers`, `memory_coworkers` (many-to-many; **no rows = global/visible-to-all**), `appraisals` (one current row per coworker, same supersede pattern). `coworkers` is the only writer, and `--list`/`--load` are its reads.

**Runbooks and the template are text with one placeholder.** `{{EXE}}` is replaced by the binary's own path at print time (`runbook`) and render time (`init`, `update`), written as a command line writes it (`"…"` around whitespace, mixed `C:/` on Windows) — so the text writes `{{EXE}} <cmd>`, never `"{{EXE}}"`. `{{EPISODIC_MODE}}`/`{{EPISODIC_RULE}}` exist in the template only. Any other placeholder left unfilled is a refusal, not an installed broken command.

## Invariants — do not break these

- **The multiprocess WAL on EVERY opener** (the binary's `Store`, the recommended MCP registration `--experimental-multiprocess-wal`). tursodb takes an exclusive file lock otherwise — a process without it is *refused*, and one without it would block all readers. It's experimental — that's the trade. **On Windows it must be paired with a VFS** (`--vfs experimental_win_iocp`) or the open is refused with `experimental multiprocess WAL is not supported by the active IO backend`; the binary pairs them itself via `TURSO_VFS`, but every `tursodb` line a doc or runbook hands an agent to execute needs both flags written out. *(Changed: it used to be a rule about the Python scripts and the backup script.)*
- **One embedding model per DB** (bge-m3, 1024-dim, recorded in `embed_model`). Mixing models makes cosine meaningless; `remember` refuses a table that already holds another model, and `instruct` and `embed --missing` check all three tables. To switch models you must rebuild + re-embed.
- **Keywords are not a column** — `remember --keywords` appends them into `memory_text` so they're both embedded and searchable.
- **Length caps are enforced by `CHECK`, not VARCHAR** (SQLite ignores declared sizes): `memory_text` ≤ 2000; most metadata ≤ 128.
- **Never store PII** — anonymize before `remember`/`backfill`; store pointers (ticket ids, role labels). Applies to the text *and* anything sent to Ollama to embed.
- **`CLAUDE.md.template` is loaded into context every session — keep it terse.** Prefer short imperative lines over prose; anything an agent needs only occasionally belongs in `README.md`/`instructions/`, which the template points at with `runbook <name>`. A test caps its length. The renderer fills only the *active* episodic mode into `{{EPISODIC_RULE}}`, so adding a mode means adding its one-line rule to `claude_md::EpisodicMode` as well as its name.
- **Rare on-request procedures get a pointer, not a summary.** Sleep, deep sleep, update and migrate live under the template's "On user request only" heading as one line each — a command (`runbook sleep`, `backup`, `update`), nothing about *how* they work. The steps are then read only in the rare session that actually sleeps. Keep the *policy* (user-triggered only, never scheduled/proactive) in the template, since an agent must know that without opening the runbook, and keep the *procedure* in the runbook. Don't let a summary creep back in.
- **A text that hands the agent a command must name one that exists.** `src/runbook.rs` checks every `{{EXE}} <command> --flag` in the template and the runbooks against the clap definition, and that no embedded text points at the checkout, a Python script or an interpreter. Add a command or rename a flag, and the texts that name it fail until they are updated.
- **`{{EXE}}` comes from `current_exe()`, not from a path baked in by hand.** *(Changed: `BASE_PATH` was machine-specific and had to be updated on a new laptop; a binary needs no checkout.)* `update` detects a moved binary by comparing the commands in the installed block with the current path.
- **The schema is applied by `init` and `update`.** *(Changed: it used to be piped into tursodb by hand.)* `init` creates a database from the embedded `schema.sql` only when the file is absent; `update` runs the migrations the database lacks, each probe-decided. Never pass or pipe the schema to `tursodb` by hand, and never hand-create a database on a user's machine.
- Don't hand-edit `superseded_by` — use `remember --supersedes` (semantic) / the appraisal flow (coworkers), which insert + supersede in one transaction.
- **Never hard-delete a memory outside deep sleep's purge gate** — `retired_at` (semantic) is soft-delete only, set via `sleep --retire`; same never-delete philosophy as coworkers' `active=0`. Ask the user when obsolescence isn't clear-cut. The single exception is `sleep --purge`, which deletes **only** superseded/retired semantic rows the user explicitly selected in deep sleep's D2, only with `--confirm-purge`, and only with a verified backup strictly newer than the database's last write. Episodic memory is never purged. Pass all ids in ONE `--purge` call — a purge is itself a write, so the second call in a pass fails the backup gate.
- **Never create, restore, or overwrite a DB unprompted.** `MISSING` almost always means a wrong path, not lost data. Run `recall --candidates`, show the user what was found, and ask — creating an empty DB strands the real one, and restoring a backup over a live DB loses everything since that backup. Every command but `init` (and `restore`, into a fresh file) refuses a missing database; the rest is instruction-level in `CLAUDE.md.template`.
- **The database path lives in the `.env` beside the binary** — `SUPERCHARGED_MEMORY_TURSO_PATH`, written by `init`, found by every command however it is launched. *(Changed: it used to have to live in `~/.claude/settings.json` under `env`, because Claude Code's Bash tool never sources a shell profile.)* A real environment variable still overrides the `.env`, which is why the legacy cutover tells the user to remove the old ones; `config` shows where each value came from.
- `sleep --rebuild-topics` is a **full replace**, not an upsert — always pass the complete current topic set on stdin, or you'll silently drop the topics you omit.
- **The block's content is what `update` verifies.** *(Changed: the `synced-at <sha>` stamp was the ledger and `UPDATE.md` was built entirely on reading it back.)* The stamp is now `rendered-by <version>` and informational; `init` appends the block only when the template's headline is absent, and `update` diffs the installed block against a fresh render, so a hand-edit or a moved binary shows even when the version matches. `block::MODE_ANCHOR` (the `Episodic policy on this machine — ` line) must stay in the template: `update` reads the mode back from it. The `claude_md::HEADLINE` is the template's first line.
- **`init` recovers nothing from the old block, because it needs nothing.** *(Changed: `install-claude-md.sh` read `EPISODIC_MODE` and `BASE_PATH` back out of the block before defaulting, because it re-rendered from a possibly-throwaway checkout.)* The binary's path is `current_exe()` and the mode is read from the block by `update` or passed as `--episodic-mode`; `init` never rewrites text that is already in CLAUDE.md.
- **A breaking change ships its note in `migrations/<version>/`**, in the same commit if you like — see *Keeping an install in sync*. The sha-anchored, next-commit rule is retired with `migration-steps/`.
- **MCP writes are advisory.** The `turso` MCP is registered `--readonly` by the plan `init` prints, so writes can only go through the binary; a user who registers it read-write makes "writes go through the CLI" a convention, and the template says so in those words. Nothing can enforce it from here.
- **Every rule gets a mechanical enforcer, or is explicitly marked advisory.** This is already the de facto pattern — `sleep --rebuild-topics` refuses an over-cap topic set, `remember` refuses a near-duplicate and refuses a `--created-at` that is not `YYYY-MM-DD[ HH:MM:SS]`, `restore` counts every table against the dump, `purge` has several separate refusals. A rule that lives only in prose is a rule that silently stops being followed, so when adding one, either add the check that enforces it or write "advisory" next to it on purpose.
