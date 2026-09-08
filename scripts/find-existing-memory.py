#!/usr/bin/env python3
"""Report pre-existing, file-based Claude memory that is NOT in this system yet.

Read-only and dependency-free on purpose: it needs neither the database nor
Ollama, so SETUP.md can call it before either is proven working, and a user can
run it to decide whether a migration is worth doing at all.

Emits JSON. The interesting fields per file are `chars` (against the 2000-char
MAX_TEXT the writer enforces) and `kind`:

  memory   a memory file -- one fact, the unit a semantic row is written from
  index    a pointer list (MEMORY.md); its LINES are links to memory files, so
           importing it as a fact stores a table of contents instead of content
  claude   whatever sits in CLAUDE.md OUTSIDE this system's managed block, i.e.
           the instructions and index a user curated before installing this

Scope is deliberately the user's own memory: $CLAUDE_CONFIG_DIR (default
~/.claude). Repo-checked-in CLAUDE.md files are excluded -- they belong to the
repo, are already loaded per session, and are not the user's to migrate.
"""
import argparse, json, os, sys
from pathlib import Path

# Written by install-claude-md.sh. Content between these markers is this system's
# own rendered instructions, so it is not "existing memory" to migrate.
BEGIN = "<!-- BEGIN agentic-memory (managed by install-claude-md.sh) -->"
END = "<!-- END agentic-memory -->"
MAX_TEXT = 2000        # keep in step with memlib.MAX_TEXT


def config_dir():
    return Path(os.environ.get("CLAUDE_CONFIG_DIR") or (Path.home() / ".claude"))


def outside_managed_block(text):
    """CLAUDE.md with this system's managed block removed."""
    if BEGIN not in text:
        return text
    head, rest = text.split(BEGIN, 1)
    tail = rest.split(END, 1)[1] if END in rest else ""
    return head + tail


def entry(path, kind, chars):
    return {"path": str(path), "kind": kind, "chars": chars,
            "over_max_text": chars > MAX_TEXT}


def scan():
    root = config_dir()
    found, sources = [], []

    claude_md = root / "CLAUDE.md"
    if claude_md.is_file():
        body = outside_managed_block(claude_md.read_text(encoding="utf-8", errors="replace")).strip()
        if body:
            found.append(entry(claude_md, "claude", len(body)))
            sources.append("CLAUDE.md (outside the managed block)")

    # Global memory, then per-project memory. Same shape, different scope: a
    # project-scoped file is only loaded when cwd matches, so its facts are
    # narrower and the `project` column is where that belongs.
    globs = [("global memory files", root / "memory", root.glob("memory/*.md")),
             ("project memory files", root / "projects", root.glob("projects/*/memory/*.md"))]
    for label, base, it in globs:
        n = 0
        for f in sorted(it):
            kind = "index" if f.name.upper() == "MEMORY.MD" else "memory"
            found.append(entry(f, kind, len(f.read_text(encoding="utf-8", errors="replace").strip())))
            n += 1
        if n:
            sources.append(f"{n} {label} under {base}")

    return root, found, sources


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--quiet", action="store_true",
                    help="print only whether anything was found (exit 0 = yes, 1 = no)")
    a = ap.parse_args()

    root, found, sources = scan()
    memories = [f for f in found if f["kind"] != "index"]
    report = {
        "config_dir": str(root),
        "n_files": len(found),
        "n_memory_files": len(memories),
        "n_index_files": len(found) - len(memories),
        "total_chars": sum(f["chars"] for f in memories),
        "over_max_text": [f["path"] for f in memories if f["over_max_text"]],
        "sources": sources,
        "files": found,
        "note": ("index files are pointer lists, not facts -- read them to find "
                 "the memory files, do not import them as memories. A file over "
                 f"{MAX_TEXT} chars must be split or condensed, never truncated."),
    }
    if a.quiet:
        print("found" if memories else "nothing")
        sys.exit(0 if memories else 1)
    print(json.dumps(report, indent=1))


if __name__ == "__main__":
    main()
