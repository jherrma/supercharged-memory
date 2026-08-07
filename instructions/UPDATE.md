# UPDATE — sync this machine with the repo

**This file is an instruction set for Claude Code.** When the user says "update
the memory system", "run UPDATE.md", or similar, execute the steps below in
order. It brings a machine that was set up earlier back in line with `master`:
applies any **breaking-change migrations** that landed since, then re-renders
`~/.claude/CLAUDE.md` from the current template.

Work from the repository root. Report each step in one line. **Ask before
applying anything** — this runbook reads and reports on its own, but never
changes the machine without the user's ok.

Two variables used throughout:

- `TARGET` — the installed instructions, default `~/.claude/CLAUDE.md`
- **sync stamp** — the line `<!-- supercharged-memory: synced-at <sha> -->` that
  `install-claude-md.sh` writes into the managed block. It records the repo commit
  this machine was last installed from, and it is the only ledger this runbook
  needs: the repo's own git history supplies everything else.

## Step 1 — Read the sync stamp

```bash
TARGET="${TARGET:-$HOME/.claude/CLAUDE.md}"
grep -o 'synced-at [0-9a-f]\{7,40\}\(-dirty\)\?' "$TARGET" || echo "no stamp"
```

- **A sha** → hold it as `STAMP`. Continue.
- **`<sha>-dirty`** → the install was rendered from an uncommitted working tree.
  Use the sha as `STAMP` but tell the user: anything that was uncommitted then is
  invisible to the comparison below.
- **`unknown`** → the install came from a copy that wasn't a git work tree. Treat
  it as "no stamp".
- **No stamp / no managed block** → the machine was installed before stamping
  existed (or never installed). Do **not** guess a base commit. Say so, and in
  Step 4 list *every* migration note instead of a computed subset, asking the user
  which already apply. `install-claude-md.sh` will write a stamp at the end, so
  this only happens once.

## Step 2 — Update the checkout

```bash
git status --porcelain          # must be empty
git rev-parse HEAD              # note the old HEAD
git pull --ff-only
git rev-parse HEAD              # note the new HEAD
```

If `git status --porcelain` is **not** empty, stop and show the user the dirty
files — pulling over local edits is their call, not yours. If the pull is not a
fast-forward, stop and report; do not merge or rebase to force it.

Report old → new HEAD, and the commit subjects in between:

```bash
git log --oneline "$STAMP"..HEAD
```

If `STAMP` equals `HEAD` and no migrations are pending, tell the user there is
nothing to update and stop here.

## Step 3 — Find pending migrations

Breaking changes each ship a note in `migration-steps/` anchored to the commit
that introduced them (see the repo's `CLAUDE.md` for the authoring rule). A note
is **pending** when its commit is in history now but was not yet installed:

```bash
for f in migration-steps/*.md; do
  [ -e "$f" ] || continue
  sha="$(grep -m1 -oE '^commit:[[:space:]]*[0-9a-f]{7,40}' "$f" | grep -oE '[0-9a-f]{7,40}')"
  [ -n "$sha" ] || { echo "MALFORMED $f (no commit: line)"; continue; }
  if ! git merge-base --is-ancestor "$sha" HEAD 2>/dev/null; then
    echo "NOT-IN-HISTORY $f ($sha)"          # note references an unknown commit
  elif git merge-base --is-ancestor "$sha" "$STAMP" 2>/dev/null; then
    echo "ALREADY-INSTALLED $f ($sha)"       # skip silently
  else
    echo "PENDING $f ($sha)"
  fi
done
```

`merge-base --is-ancestor` is used rather than parsing `git log` output so a note
whose commit is unreachable (rewritten history, wrong sha) surfaces as
`NOT-IN-HISTORY` instead of being silently treated as pending.

Report a `MALFORMED` or `NOT-IN-HISTORY` note to the user — it is a bug in the
note, not something to work around.

## Step 4 — Apply pending migrations

Read each pending note in full, **oldest commit first**
(`git log --format='%ct %H' --reverse` to order them, or just sort by the note's
date-prefixed filename — the two agree by convention).

For each note, in order:

1. Show the user its **What broke** and **How to resolve** sections in a couple of
   lines, and ask whether to apply it.
2. On approval, run the note's resolution commands exactly as written. If the note
   touches the database, run
   `bash scripts/supercharged-memory-backup.sh` first — a migration that rewrites
   a table has no undo other than that dump.
3. Run the note's **Verification** section and show its output. Do not proceed to
   the next note if verification fails; stop and report.

If Step 1 found no stamp, list every note in `migration-steps/` here instead, with
its commit and one-line summary, and ask the user which ones this machine still
needs — some will already have been applied by hand.

## Step 5 — Diff the installed instructions against the template

Re-render the template with **this machine's own settings** and compare it to what
is installed, so the user sees what would actually change:

```bash
# The values this machine was installed with:
EPISODIC_MODE="$(grep -o 'Episodic policy on this machine — `[a-z-]*`' "$TARGET" \
                 | grep -o '`[a-z-]*`' | tr -d '`')"
DB="$(jq -r '.env.SUPERCHARGED_MEMORY_TURSO_PATH // empty' ~/.claude/settings.json)"

TMP="$(mktemp -d)"
cp "$TARGET" "$TMP/preview.md"          # copy FIRST — see note below
TARGET="$TMP/preview.md" EPISODIC_MODE="$EPISODIC_MODE" \
  SUPERCHARGED_MEMORY_TURSO_PATH="$DB" bash scripts/install-claude-md.sh >/dev/null
diff -u "$TARGET" "$TMP/preview.md" || true
```

**Render into a copy of the installed file, never into an empty temp file.** The
installer only replaces the managed block and leaves everything else alone, so
rendering into a fresh file makes every line the user keeps *outside* the markers
show up as a deletion — burying the changes that matter in noise.

If either value comes back empty, ask the user rather than falling back to a
default — installing with the wrong `EPISODIC_MODE` silently changes how much gets
stored, and the wrong DB path is how an agent ends up reporting `MISSING` against a
healthy database.

Summarize the diff as a compact bullet list of *behavior* changes (new rule, changed
threshold, new pointer), not a line-by-line dump. Two things to read carefully in it:

- The **stamp line always differs** — that is the point, not a change worth reporting.
- Content present in the installed file but absent from the render, **inside** the
  markers, is a **local hand-edit that re-installing will destroy**. Surface each one
  and ask whether to port it into `CLAUDE.md.template` first (then re-render), or drop
  it deliberately. Never silently overwrite one.

## Step 6 — Re-install

Ask for the go-ahead, then:

```bash
EPISODIC_MODE="$EPISODIC_MODE" SUPERCHARGED_MEMORY_TURSO_PATH="$DB" \
  bash scripts/install-claude-md.sh
```

It replaces the managed block and writes the new stamp. Any content the user has
outside the markers is left untouched.

## Step 7 — Report

One summary: old → new HEAD, migrations applied (or none pending), what changed in
the instructions, and the new stamp. Then tell the user to **restart the Claude Code
session** to pick up the new `~/.claude/CLAUDE.md`.

If any migration touched the database, finish with
`python3 scripts/recall.py --status` and show the result.
