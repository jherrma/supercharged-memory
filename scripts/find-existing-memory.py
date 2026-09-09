#!/usr/bin/env python3
"""Report pre-existing, file-based Claude memory that is NOT in this system yet.

Read-only and dependency-free on purpose: it needs neither the database nor
Ollama, so SETUP.md can call it before either is proven working, and a user can
run it to decide whether a migration is worth doing at all.

Emits JSON. Every count and total that says how much there is to IMPORT covers
`kind: memory` files only -- an index and a CLAUDE.md body are context, not things
to import, so a user who merely has a CLAUDE.md is not told they have memory to
migrate. `n_index_files`/`n_claude_files`/`n_empty_files` are labelled by the kind
they count, and `files` lists every entry of every kind.

Nothing this probe skips is dropped silently -- SETUP.md Step 7 gates on it, so a
skip that appears in no field reads as "you have no memory there". Whatever is not
counted lands in `over_max_text`, `empty`, `unreadable` (a file that could not be
read, a broken symlink, a directory that could not be listed, a symlink loop) or
`excluded_dirs` (a protected subdirectory of a memory root, or a symlink pointing
above it).

The interesting fields per file are `chars` (against the 2000-char MAX_TEXT the
writer enforces), `scope` (`global` or `project`), `project_dir` (the
`projects/<slug>` directory name -- Claude Code's mangled cwd, NOT a work-item id
for `remember.py --project`) and `kind`:

  memory   a memory file -- one fact, the unit a semantic row is written from
  index    a pointer list (MEMORY.md); its LINES are links to memory files, so
           importing it as a fact stores a table of contents instead of content
  claude   whatever sits in CLAUDE.md OUTSIDE this system's managed block, i.e.
           the instructions and index a user curated before installing this
  empty    a `.md` file with nothing in it after stripping. `remember.py` exits
           `refused: --text is empty` on such a file, so there is nothing to
           import: it is reported, and counted as memory nowhere

Scope is deliberately the user's own memory: $CLAUDE_CONFIG_DIR (default
~/.claude). Repo-checked-in CLAUDE.md files are excluded -- they belong to the
repo, are already loaded per session, and are not the user's to migrate.

Both memory directories are walked RECURSIVELY (they nest -- `team/` is shared
memory) for `.md` only, case-insensitively, which is the one thing the built-in
memory writes; see MEMORY_SUFFIX and SKIP_DIRS.
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
# Matched case-INSENSITIVELY: a `*.md` glob is case-sensitive on Linux, so
# `notes.MD` and `other.Md` were invisible to the whole report.
MEMORY_SUFFIX = ".md"
# Subdirectories of a memory directory that are NOT memory. Claude Code calls
# these "protected subdirectories like `.git` or `agents`" and excludes them
# from its own memory writes/prunes: `agents/` holds subagent definitions, `.git`
# a work tree. `team/` is deliberately NOT here -- it is repo-shared memory, and
# it is the reason this walk has to be recursive at all.
#
# Matched at the memory ROOT only, and reported in `excluded_dirs` when it hits.
# Matching the name at any depth dropped `memory/<topic>/agents/*.md` -- a topic
# directory that merely shares the name -- with no trace in the report.
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
    """`(content, None)`, or `(None, reason)` if it cannot be read. Never raises:
    this is a read-only pre-flight probe that SETUP.md runs before anything else,
    so one unreadable file (mode 000, a stale symlink) must degrade, not
    traceback. The reason is carried so `unreadable` says WHY, not just which."""
    try:
        return path.read_text(encoding="utf-8", errors="replace"), None
    except OSError as e:
        return None, e.strerror or str(e)


def count_md(d):
    """How many `.md` files an excluded directory holds, or None if unknown.
    Reported alongside the exclusion so a user whose *topic* directory happens to
    be named `agents` can see there is content in there, without this probe
    offering a subagent definition as memory to import."""
    try:
        return sum(1 for f in d.rglob("*")
                   if f.name.lower().endswith(MEMORY_SUFFIX) and f.is_file())
    except OSError:
        return None


def walk_memory(base, seen=None):
    """`(files, issues, excluded)` for one memory root.

    files     candidate `.md` paths (case-insensitive suffix), sorted
    issues    one `{"path", "reason"}` per thing NOT walked: a directory that
              could not be listed, a symlink loop. These feed `unreadable`.
    excluded  one `{"path", "n_md", "reason"}` per directory left out on
              purpose: a SKIP_DIRS name at the memory root, or a symlink to an
              ancestor of it. Skipped deliberately, reported all the same.

    Recursive on purpose: memory directories nest (Claude Code's own `team/`
    subdirectory is shared memory), and a single-level `memory/*.md` glob left
    `memory/nested/deep.md` out of the report entirely -- so SETUP.md Step 7
    said "nothing to do" about memory that exists.

    `os.walk` rather than `rglob` because `rglob` loses two whole classes of
    file with no trace in any field: it swallows a directory-level `OSError`
    (a mode-000 `memory/noaccess/` and everything in it vanished from the
    report AND from `unreadable`), and it never descends a symlinked directory
    (`memory/linkdir -> ../elsewhere` vanished the same way). Symlinks are
    followed here, with `(st_dev, st_ino)` bookkeeping shared across every root
    in one scan, so a loop -- and any second path to a directory already walked
    -- is pruned and reported instead of hanging the probe or reporting the same
    file twice under two names.

    A symlink can also point UPWARDS. `memory/x -> ..` (or `-> ~`) makes the
    walk leave the memory tree and report the whole directory above it as
    memory, `CLAUDE.md` and other projects' memory included, which is how
    following symlinks at all can be worse than not. Such a link is pruned and
    listed in `excluded_dirs`."""
    files, issues, excluded = [], [], []
    if not base.is_dir():
        return files, issues, excluded
    base_real = base.resolve()
    seen = set() if seen is None else seen

    def onerror(err):
        # os.walk drops these silently unless it is handed a callback.
        issues.append({"path": str(getattr(err, "filename", None) or base),
                       "reason": f"directory could not be listed: "
                                 f"{getattr(err, 'strerror', None) or err}"})

    for dirpath, dirnames, filenames in os.walk(base, onerror=onerror,
                                                followlinks=True):
        d = Path(dirpath)
        try:
            st = d.stat()
        except OSError as e:
            issues.append({"path": dirpath,
                           "reason": f"directory could not be stat'ed: "
                                     f"{e.strerror or e}"})
            dirnames[:] = []
            continue
        real = d.resolve()
        if real in base_real.parents:   # `base` is never in its own parents
            # A symlink pointing at an ancestor of this memory root. Walking it
            # reports everything above the memory directory as memory.
            excluded.append({"path": dirpath, "n_md": None,
                             "reason": f"symlink to {real}, an ancestor of the "
                                       "memory root -- not walked"})
            dirnames[:] = []
            continue
        key = (st.st_dev, st.st_ino)
        if key in seen:                 # followlinks=True can revisit a directory
            issues.append({"path": dirpath,
                           "reason": "symlink loop or repeated directory -- "
                                     "walked once, not again"})
            dirnames[:] = []
            continue
        seen.add(key)
        if d == base:                   # protected subdirectories: ROOT level only
            for name in sorted(set(dirnames) & SKIP_DIRS):
                dirnames.remove(name)
                excluded.append({"path": str(d / name), "n_md": count_md(d / name),
                                 "reason": "protected subdirectory of a memory "
                                           "root, not memory"})
        dirnames.sort()
        files.extend(d / f for f in filenames
                     if f.lower().endswith(MEMORY_SUFFIX))
    return sorted(files), issues, excluded


def scan():
    root = config_dir()
    found, sources, unreadable = [], [], []

    claude_md = root / "CLAUDE.md"
    if claude_md.is_file():
        text, why = read_text(claude_md)
        if text is None:
            unreadable.append({"path": str(claude_md), "reason": why})
        else:
            body = outside_managed_block(text).strip()
            if body:
                found.append(entry(claude_md, "claude", len(body), "global"))
                sources.append("CLAUDE.md (outside the managed block)")

    # Global memory, then per-project memory. Same shape, different scope: a
    # project-scoped file is only loaded when cwd matches, so its facts are
    # narrower and that has to survive the move.
    projects = root / "projects"
    # One `seen` set for every root: a project memory directory symlinked at the
    # global one (or at another project's) is then reported as a repeat instead
    # of having its files counted twice under two scopes.
    seen = set()
    g_files, g_issues, excluded_dirs = walk_memory(root / "memory", seen)
    p_files, p_issues = [], []
    for d in sorted(projects.glob("*/memory")):
        f_, i_, e_ = walk_memory(d, seen)
        p_files += f_
        p_issues += i_
        excluded_dirs += e_
    unreadable += g_issues + p_issues

    groups = [("global memory file", root / "memory", g_files, "global"),
              ("project memory file", projects, p_files, "project")]
    for label, base, files, scope in groups:
        n = 0
        for f in files:
            if not f.is_file():
                # A broken symlink (`broken.md -> /nowhere`) or a directory named
                # *.md. `continue` alone made real-looking memory vanish from the
                # report with nothing to notice it by.
                unreadable.append({"path": str(f), "reason":
                                   "not a regular file (broken symlink, or a "
                                   "directory named *.md)"})
                continue
            text, why = read_text(f)
            if text is None:
                unreadable.append({"path": str(f), "reason": why})
                continue
            # An empty file is its own kind: `remember.py` refuses an empty
            # --text, so reporting it as an importable memory of `chars: 0`
            # promised a row that cannot be written.
            if f.name.upper() == "MEMORY.MD":
                kind = "index"
            else:
                kind = "memory" if text.strip() else "empty"
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

    return root, found, sources, unreadable, excluded_dirs


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--quiet", action="store_true",
                    help="print only whether anything was found (exit 0 = yes, 1 = no)")
    a = ap.parse_args()

    root, found, sources, unreadable, excluded_dirs = scan()
    # `memory` files ONLY. An index is a pointer list, and the CLAUDE.md body is
    # instructions -- counting either as a memory file made a plain CLAUDE.md
    # ("Always use pnpm") read as one file to migrate, which is what SETUP.md
    # Step 7 gates on, and put a normal-length CLAUDE.md in `over_max_text` as a
    # memory file the user must split.
    memories = [f for f in found if f["kind"] == "memory"]
    report = {
        "config_dir": str(root),
        # No aggregate "n_files": one number covering every kind read as the
        # count of memory to import, contradicting the docstring and the three
        # per-kind counts below. Every count here says which kind it counts.
        "n_memory_files": len(memories),
        "n_index_files": len([f for f in found if f["kind"] == "index"]),
        "n_claude_files": len([f for f in found if f["kind"] == "claude"]),
        "n_empty_files": len([f for f in found if f["kind"] == "empty"]),
        "n_global": len([f for f in memories if f["scope"] == "global"]),
        "n_project": len([f for f in memories if f["scope"] == "project"]),
        "projects": sorted({f["project_dir"] for f in memories if f["project_dir"]}),
        "total_chars": sum(f["chars"] for f in memories),
        "over_max_text": [f["path"] for f in memories if f["over_max_text"]],
        "empty": [f["path"] for f in found if f["kind"] == "empty"],
        "unreadable": unreadable,
        "excluded_dirs": excluded_dirs,
        "sources": sources,
        "files": found,
        "note": ("counts and totals of memory to import cover `kind: memory` only. "
                 "An index file is a pointer list and a `claude` entry is "
                 "instructions -- read both for context, import neither as a fact. "
                 f"A memory file over {MAX_TEXT} chars must be split or condensed, "
                 "never truncated. Nothing is skipped silently: report `unreadable` "
                 "(unreadable file or directory, broken symlink, symlink loop), "
                 "`excluded_dirs` (protected subdirectory of a memory root) and "
                 "`empty` (nothing to import -- the writer refuses an empty "
                 "--text) as gaps."),
    }
    if a.quiet:
        print("found" if memories else "nothing")
        sys.exit(0 if memories else 1)
    print(json.dumps(report, indent=1))


if __name__ == "__main__":
    main()
