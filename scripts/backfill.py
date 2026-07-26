#!/usr/bin/env python3
"""Backfill the memory DB from a directory of text/Markdown files (fresh start).
Each file becomes ONE memory (must be <= 2000 chars; larger files are reported
and skipped — split them yourself). Category defaults to `reference`; topic =
file stem; keywords = file stem words. NEVER import files containing PII.

  backfill.py --dir ~/notes
  backfill.py --dir ~/notes --glob '*.txt' --project 869abc --category project
"""
import argparse, re, sys
sys.dont_write_bytecode = True
from pathlib import Path
sys.path.insert(0, str(Path(__file__).parent))
import memlib as M  # noqa: E402
from remember import store_memory  # noqa: E402


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--dir", required=True)
    p.add_argument("--table", choices=["semantic", "episodic"], default="semantic")
    p.add_argument("--category", default="reference",
                   help="semantic category (NOT baseline — that needs explicit confirmation)")
    p.add_argument("--project"); p.add_argument("--model", default="unknown")
    p.add_argument("--glob", default="*.md")
    a = p.parse_args()

    M.require_db()
    d = Path(a.dir).expanduser()
    if not d.is_dir():
        sys.exit(f"not a directory: {d}")
    files = sorted(f for f in d.rglob(a.glob) if f.is_file())
    if not files:
        sys.exit(f"no files matching {a.glob} under {d}")

    done = skipped = 0
    for f in files:
        text = f.read_text(errors="replace").strip()
        if not text:
            continue
        if len(text) > M.MAX_TEXT:
            print(f"SKIP {f.name}: {len(text)} chars > {M.MAX_TEXT} cap — split it manually")
            skipped += 1
            continue
        kw = ", ".join(re.findall(r"\w{3,}", f.stem.lower())) or None
        args = dict(topic=f.stem, project=a.project, source="backfill",
                    model=a.model, keywords=kw, force=True)
        if a.table == "semantic":
            args["category"] = a.category
        else:
            args.update(event_type="note", importance="routine")
        store_memory(a.table, text, **args)
        print(f"stored {f.name}")
        done += 1
    print(f"backfilled {done} file(s); skipped {skipped}")


if __name__ == "__main__":
    main()
