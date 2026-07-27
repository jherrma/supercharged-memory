#!/usr/bin/env python3
"""Mechanical write primitives for a sleep pass (see ../instructions/SLEEP.md
for the full procedure). Promoting episodic content into semantic memory, and
consolidating semantic memory, both reuse remember.py (--table semantic and
--supersedes) — this script only covers what remember.py doesn't:

--mark-processed <id1,id2,...>   stamp episodic_memory.processed_at (kept AND
                                  discarded rows alike — a sifted row is done
                                  either way).
--retire <id>                    soft-delete a semantic memory: sets
                                  retired_at, never a hard DELETE. Refuses on
                                  an unknown/already-superseded/already-retired id.
--rebuild-topics                 atomically replaces topic_keywords wholesale
                                  (DELETE + re-INSERT, never accumulated) from
                                  a JSON array on stdin:
                                  [{"topic": "...", "keywords": "..."}, ...]
                                  This script does NOT derive the topics or
                                  keywords itself — it's a dumb mechanical
                                  write. The agent reads current semantic
                                  memory, groups it by topic, and composes the
                                  keyword list per topic (judgment/meaning
                                  stays with the LLM, same split as everywhere
                                  else in this system); this script only does
                                  the atomic replace.

                                  HARD CAP: the whole table is loaded into
                                  every session's context (recall.py
                                  --topics), so total size across ALL rows is
                                  capped at TOTAL_CHAR_CAP chars — refuses the
                                  write over that, rather than silently
                                  bloating every future session. Drop or
                                  merge topics and retry; this script won't
                                  pick which ones for you.
"""
import argparse, json, sys
sys.dont_write_bytecode = True
from pathlib import Path
sys.path.insert(0, str(Path(__file__).parent))
import memlib as M  # noqa: E402

TOTAL_CHAR_CAP = 500  # hard cap on topic_keywords' total content size (topic+keywords
                      # across all rows) — it's loaded into every session's context,
                      # not queried on demand, so this bounds a fixed per-session cost.


def mark_processed(ids_csv):
    ids = [int(i.strip()) for i in ids_csv.split(",") if i.strip()]
    if not ids:
        sys.exit("refused: no ids given.")
    M.require_db()
    id_list = ",".join(str(i) for i in ids)
    M.exec_sql(f"UPDATE episodic_memory SET processed_at=datetime('now') "
              f"WHERE id IN ({id_list});")
    print(f"marked {len(ids)} episodic row(s) processed")


def retire(memory_id):
    M.require_db()
    cur = M.scalar(f"SELECT count(*) FROM semantic_memory WHERE id={int(memory_id)} "
                   "AND superseded_by IS NULL AND retired_at IS NULL;")
    if not cur or int(cur) != 1:
        sys.exit(f"refused: --retire {memory_id}: no current (un-superseded, "
                 "non-retired) semantic row with that id.")
    M.exec_sql(f"UPDATE semantic_memory SET retired_at=datetime('now'), "
              f"updated_at=datetime('now') WHERE id={int(memory_id)};")
    print(f"retired semantic memory {memory_id}")


def rebuild_topics():
    M.require_db()
    rows = json.load(sys.stdin)
    if not isinstance(rows, list):
        sys.exit("refused: stdin must be a JSON array of {topic, keywords} objects.")
    total = sum(len(r["topic"]) + len(r["keywords"]) for r in rows)
    if total > TOTAL_CHAR_CAP:
        by_size = sorted(rows, key=lambda r: len(r["topic"]) + len(r["keywords"]), reverse=True)
        biggest = ", ".join(f"{r['topic']} ({len(r['topic']) + len(r['keywords'])})" for r in by_size[:5])
        sys.exit(f"refused: {len(rows)} topics total {total} chars, over the "
                 f"{TOTAL_CHAR_CAP}-char cap (this table loads into every session's "
                 f"context). Drop or merge topics and retry. Largest: {biggest}")
    delete = "DELETE FROM topic_keywords;"
    if rows:
        values = ", ".join(
            f"({M.q(r['topic'])}, {M.q(r['keywords'])}, datetime('now'))" for r in rows)
        insert = f" INSERT INTO topic_keywords (topic, keywords, updated_at) VALUES {values};"
    else:
        insert = ""
    M.exec_sql("BEGIN; " + delete + insert + " COMMIT;")
    print(f"rebuilt topic_keywords: {len(rows)} topic(s)")


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--mark-processed", dest="mark_processed", metavar="ID[,ID...]")
    p.add_argument("--retire", type=int, metavar="ID")
    p.add_argument("--rebuild-topics", action="store_true",
                   help="reads a JSON array of {topic, keywords} from stdin")
    a = p.parse_args()
    if a.mark_processed:
        mark_processed(a.mark_processed)
    elif a.retire is not None:
        retire(a.retire)
    elif a.rebuild_topics:
        rebuild_topics()
    else:
        sys.exit("provide one of --mark-processed / --retire / --rebuild-topics")


if __name__ == "__main__":
    main()
