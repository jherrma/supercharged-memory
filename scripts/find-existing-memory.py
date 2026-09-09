#!/usr/bin/env python3
"""Report pre-existing, file-based Claude memory that is NOT in this system yet.

Read-only and dependency-free on purpose: it needs neither the database nor
Ollama, so SETUP.md can call it before either is proven working, and a user can
run it to decide whether a migration is worth doing at all.

Emits JSON. Every count and total in the report covers `kind: memory` files only
-- an index and a CLAUDE.md body are context, not things to import, so a user who
merely has a CLAUDE.md is not told they have memory to migrate.

The interesting fields per file are `chars` (against the 2000-char MAX_TEXT the
writer enforces), `scope` (`global` or `project`), `project_dir` (the
`projects/<slug>` directory name -- Claude Code's mangled cwd, NOT a work-item id
for `remember.py --project`) and `kind`:

  memory   a memory file -- one fact, the unit a semantic row is written from
  index    a pointer list (MEMORY.md); its LINES are links to memory files, so
           importing it as a fact stores a table of contents instead of content
  claude   whatever sits in CLAUDE.md OUTSIDE this system's managed block, i.e.
           the instructions and index a user curated before installing this

Scope is deliberately the user's own memory: $CLAUDE_CONFIG_DIR (default
~/.claude). Repo-checked-in CLAUDE.md files are excluded -- they belong to the
repo, are already loaded per session, and are not the user's to migrate.

Both memory directories are walked RECURSIVELY (they nest -- `team/` is shared
memory) for `*.md` only, which is the one thing the built-in memory writes; see
MEMORY_SUFFIX and SKIP_DIRS.
"""
import argparse, json, os, sys
from pathlib import Path

# Written by install-claude-md.sh. Content between these markers is this system's
# own rendered instructions, so it is not "existing memory" to migrate.
BEGIN = "<!-- BEGIN agentic-memory (managed by install-claude-md.sh) -->"
END = "<!-- END agentic-memory -->"
MAX_TEXT = 2000        # keep in step with memlib.MAX_TEXT

# `.md` ONLY, deliberately: the built-in file-based memory writes nothing else.
# Claude Code's memory-tool permission gate is literally `path.endswith(".md")`
# under the memory directory (verified against the 2.1.266 CLI binary,
# 2026-09-09), and its own prune pass describes itself as deleting "`.md` files
# inside the memory directory only". Widening this to every file would report
# whatever else a user parked there as memory to import.
MEMORY_SUFFIX = "*.md"
# Subdirectories of a memory directory that are NOT memory. Claude Code calls
# these "protected subdirectories like `.git` or `agents`" and excludes them
# from its own memory writes/prunes: `agents/` holds subagent definitions, `.git`
# a work tree. `team/` is deliberately NOT here -- it is repo-shared memory, and
# it is the reason this walk has to be recursive at all.
SKIP_DIRS = {".git", "agents"}


def config_dir():
    return Path(os.environ.get("CLAUDE_CONFIG_DIR") or (Path.home() / ".claude"))


def outside_managed_block(text):
    """CLAUDE.md with this system's managed block removed."""
    if BEGIN not in text:
        return text
    head, rest = text.split(BEGIN, 1)
    tail = rest.split(END, 1)[1] if END in rest else ""
    return head + tail


def entry(path, kind, chars, scope, project=None):
    return {"path": str(path), "kind": kind, "chars": chars,
            "over_max_text": chars > MAX_TEXT,
            # Which memory a file is: `global` applies everywhere, `project` only
            # when cwd matches. A worker must not have to re-derive that from the
            # path, and the M1 scope choice is unanswerable without it.
            "scope": scope,
            # The `projects/<slug>` directory name, verbatim. It is Claude Code's
            # mangled cwd, NOT a tracking-tool work-item id -- so it is reported
            # for context and must never be passed to `remember.py --project`.
            "project_dir": project}


def read_text(path):
    """File content, or None if it cannot be read. Never raises: this is a
    read-only pre-flight probe that SETUP.md runs before anything else, so one
    unreadable file (mode 000, a stale symlink) must degrade, not traceback."""
    try:
        return path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return None


def memory_files(base):
    """Every `.md` file under `base`, recursively, sorted, minus SKIP_DIRS.

    Recursive on purpose: memory directories nest (Claude Code's own `team/`
    subdirectory is shared memory), and a single-level `memory/*.md` glob left
    `memory/nested/deep.md` out of the report entirely -- so SETUP.md Step 7
    said "nothing to do" about memory that exists. `rglob` does not follow
    symlinked directories, so a symlink loop cannot hang the probe."""
    out = []
    for f in sorted(base.rglob(MEMORY_SUFFIX)):
        if SKIP_DIRS.intersection(f.relative_to(base).parts[:-1]):
            continue
        out.append(f)
    return out


def scan():
    root = config_dir()
    found, sources, unreadable = [], [], []

    claude_md = root / "CLAUDE.md"
    if claude_md.is_file():
        text = read_text(claude_md)
        if text is None:
            unreadable.append(str(claude_md))
        else:
            body = outside_managed_block(text).strip()
            if body:
                found.append(entry(claude_md, "claude", len(body), "global"))
                sources.append("CLAUDE.md (outside the managed block)")

    # Global memory, then per-project memory. Same shape, different scope: a
    # project-scoped file is only loaded when cwd matches, so its facts are
    # narrower and that has to survive the move.
    projects = root / "projects"
    groups = [("global memory file", root / "memory",
               memory_files(root / "memory"), "global"),
              ("project memory file", projects,
               [f for d in sorted(projects.glob("*/memory"))
                for f in memory_files(d)], "project")]
    for label, base, files, scope in groups:
        n = 0
        for f in files:
            if not f.is_file():
                continue          # a directory named *.md, or a broken symlink
            text = read_text(f)
            if text is None:
                unreadable.append(str(f))
                continue
            kind = "index" if f.name.upper() == "MEMORY.MD" else "memory"
            # ~/.claude/projects/<slug>/memory/[<sub>/...]<file>.md -- the slug is
            # the first component under `projects/`, at any nesting depth. It is
            # NOT `f.parent.parent.name`, which reads "memory" for a nested file.
            project = (f.relative_to(projects).parts[0]
                       if scope == "project" else None)
            found.append(entry(f, kind, len(text.strip()), scope, project))
            # `kind: memory` only, so this human-readable line cannot disagree
            # with `n_memory_files`/`n_global`/`n_project` below. Counting an
            # index here reported "2 global memory files" next to
            # `n_memory_files: 1`.
            if kind == "memory":
                n += 1
        if n:
            sources.append(f"{n} {label}{'' if n == 1 else 's'} under {base}")

    return root, found, sources, unreadable


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--quiet", action="store_true",
                    help="print only whether anything was found (exit 0 = yes, 1 = no)")
    a = ap.parse_args()

    root, found, sources, unreadable = scan()
    # `memory` files ONLY. An index is a pointer list, and the CLAUDE.md body is
    # instructions -- counting either as a memory file made a plain CLAUDE.md
    # ("Always use pnpm") read as one file to migrate, which is what SETUP.md
    # Step 7 gates on, and put a normal-length CLAUDE.md in `over_max_text` as a
    # memory file the user must split.
    memories = [f for f in found if f["kind"] == "memory"]
    report = {
        "config_dir": str(root),
        "n_files": len(found),
        "n_memory_files": len(memories),
        "n_index_files": len([f for f in found if f["kind"] == "index"]),
        "n_claude_files": len([f for f in found if f["kind"] == "claude"]),
        "n_global": len([f for f in memories if f["scope"] == "global"]),
        "n_project": len([f for f in memories if f["scope"] == "project"]),
        "projects": sorted({f["project_dir"] for f in memories if f["project_dir"]}),
        "total_chars": sum(f["chars"] for f in memories),
        "over_max_text": [f["path"] for f in memories if f["over_max_text"]],
        "unreadable": unreadable,
        "sources": sources,
        "files": found,
        "note": ("counts and totals cover `kind: memory` only. An index file is a "
                 "pointer list and a `claude` entry is instructions -- read both "
                 "for context, import neither as a fact. A memory file over "
                 f"{MAX_TEXT} chars must be split or condensed, never truncated."),
    }
    if a.quiet:
        print("found" if memories else "nothing")
        sys.exit(0 if memories else 1)
    print(json.dumps(report, indent=1))


if __name__ == "__main__":
    main()
