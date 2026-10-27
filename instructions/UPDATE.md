# UPDATE — bring this machine up to date with the binary

**This file is an instruction set for Claude Code.** When the user says "update the
memory system", "I replaced the binary", or similar, execute the steps below in
order. Updating means: the user replaced the binary with a newer one, and the
`.env`, the database and `~/.claude/CLAUDE.md` have to catch up with it.

Report each step in one line. **Ask before applying anything** — `update` without
`-y` only reads and reports; it never changes the machine.

How it knows where the machine stands: the `.env` beside the binary records the
version that last updated it (`SUPERCHARGED_MEMORY_VERSION`), and the managed block
in CLAUDE.md starts with the line `<!-- supercharged-memory: rendered-by <version> -->`.
Migration notes are keyed by version, and which schema migrations are pending is
asked of the database itself.

## Step 1 — Replace the binary in place

The user downloads the new binary. It must **replace the old one at the same path
and under the same name**: the `.env` sits beside the binary, and the instructions
in CLAUDE.md name its path. If it landed elsewhere, move it over the old one. If
the path or name must change, say so; `update` detects the moved binary and
reports it (Step 3).

## Step 2 — The dry run

```bash
{{EXE}} update
```

It compares the version the `.env` records with this binary's and prints, in
order:

- every migration note between the two versions, oldest first;
- the schema migrations the database lacks;
- what to change in CLAUDE.md: either "instructions current", or the diff between
  the installed block and the one this binary would write;
- a last line, `Ask the user to confirm` (something is pending) or `Up to date`.

It refuses a downgrade (a `.env` written by a newer binary) and a machine without
a `.env` (run `{{EXE}} runbook setup` instead, which uses `init`).

On `Up to date`, say so and stop.

## Step 3 — Relay and ask

Show the user, in a few lines each:

1. **Each migration note** — what broke, how to resolve it, how to verify. A
   step a note asks for by hand is yours to do once the user agrees; `-y` does
   not do it for you.
2. **The schema migrations** pending, which `-y` applies after a backup.
3. **The CLAUDE.md change**, as a short list of *behavior* changes (new rule,
   changed threshold, new pointer), not a line-by-line diff. Two things to read
   carefully:
   - The stamp line differs on every update: that is the point, not a change to
     report.
   - Content present in the installed block but absent from the render, **inside**
     the markers, is a **local hand-edit that replacing the block destroys**.
     `update` lists each one. Surface it and ask whether to keep it (add it
     outside the markers) or drop it deliberately. Never silently overwrite one.
   - A moved binary: if the commands in the block name another path than this
     binary's, the diff shows it; replacing the block fixes it.

If the report says the block's episodic mode cannot be read, **ask the user which
mode** and re-run with `--episodic-mode <mode>`. Never assume the default: the
wrong one silently changes how much gets stored.

Ask for the go-ahead before Step 4.

## Step 4 — Apply

```bash
{{EXE}} update -y
```

It backs up the database before the first migration (the dump is the only undo),
applies each migration in a transaction of its own, and records this binary's
version in the `.env` only once all of them succeeded. If one fails it stops,
names the statement, and leaves the version alone: report it and do not go on.
Then do whatever hand steps the notes asked for, and run each note's
**Verification**, showing its output.

`update` never writes CLAUDE.md. Once the user has agreed, replace the managed
block yourself: from the `<!-- BEGIN agentic-memory` line through the
`<!-- END agentic-memory -->` line, with exactly what this prints, and leave
everything outside the markers alone:

```bash
{{EXE}} update --print-block
```

(Add `--episodic-mode <mode>` when Step 3 needed it.)

## Step 5 — Report

One summary: old → new version, notes relayed, migrations applied (or none), what
changed in the instructions, and the backup's path. Then tell the user to
**restart the Claude Code session** to pick up the new `~/.claude/CLAUDE.md`.

Finish with `{{EXE}} status` and show the result, and with `{{EXE}} update` once
more: it must say `Up to date`.

## A machine installed from a repository checkout (legacy)

Before the binary existed, this system was installed by cloning a repository and
running an installer script. Such a machine is recognisable by the first line of
the managed block in CLAUDE.md:

```
<!-- supercharged-memory: synced-at <sha> -->
```

and by commands that start with an interpreter and a path into the checkout. The
binary does **not** rewrite that block: `init` and `update` find it, leave it
untouched, and print the cutover note instead (`update -y` refuses). Relay that
note and follow it step by step with the user; in short it:

1. keeps the binary in a permanent place under a stable name;
2. runs `init` with `--db-path` pointing at the **existing** database, `--backup-dir`
   at the existing dumps and `--episodic-mode` as the block has it, so the database
   is adopted and the `.env` written, and the old block is left alone;
3. removes the environment variables the old setup put into Claude Code's
   settings, because a real environment variable overrides the `.env` on every
   run, and checks with `{{EXE}} config` that every value now comes from the `.env`;
4. replaces the old block with what `{{EXE}} update --print-block` prints;
5. runs `{{EXE}} update -y` for the schema migration;
6. points the scheduled backup job at `{{EXE}} backup`.

The Python scripts may stay in the checkout; nothing the instructions say points
at them any more.
