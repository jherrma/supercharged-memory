#!/usr/bin/env python3
"""Store ONE memory (no chunking). Hard cap 2000 chars (schema CHECK + here).

Guards: baseline needs --confirm-baseline; refuses a near-duplicate (cosine <
0.10) unless --force; refuses if the table already holds rows embedded with a
different model (mixed vector spaces break recall). NEVER pass PII — anonymize.

--coworker NAME[,NAME...] tags the memory to one or more coworkers (see
coworkers.py) instead of leaving it global. Dedup then scopes its
near-duplicate search to memories visible to that coworker set, not the
whole table.
"""
import argparse, sys
sys.dont_write_bytecode = True                       # no __pycache__ in a synced folder
from pathlib import Path
sys.path.insert(0, str(Path(__file__).parent))
import memlib as M                                   # noqa: E402

DUP_DIST = 0.10


def store_memory(table, text, *, topic=None, project=None, category=None,
                 event_type=None, importance=None, source=None, model=None,
                 keywords=None, file_reference=None, created_at=None,
                 confirm_baseline=False, force=False, supersedes=None,
                 coworker=None):
    text = text.strip()
    if keywords:                                     # keywords live INSIDE the text
        text = f"{text}\n\nKeywords: {keywords}".strip()
    if category == "baseline" and not confirm_baseline:
        sys.exit("refused: 'baseline' memories load every session and need the user's "
                 "explicit confirmation. Ask first, then pass --confirm-baseline.")
    if len(text) > M.MAX_TEXT:
        sys.exit(f"refused: memory is {len(text)} chars; hard cap {M.MAX_TEXT}. "
                 "Tighten it or split into separate memories.")
    M.require_db()
    if supersedes:
        if table != "semantic":
            sys.exit("--supersedes is only valid for semantic memories.")
        # Validate EVERY id before writing anything: a merge of N memories into one
        # must not half-apply, leaving some old rows current and some superseded.
        id_list = ",".join(str(i) for i in supersedes)
        found = {int(l) for l in M.exec_sql(
            f"SELECT id FROM semantic_memory WHERE id IN ({id_list}) "
            "AND superseded_by IS NULL AND retired_at IS NULL;", mode="list").split()}
        bad = [i for i in supersedes if i not in found]
        if bad:
            sys.exit(f"refused: --supersedes {','.join(str(i) for i in bad)}: no current "
                     "(un-superseded, non-retired) semantic row with that id — nothing "
                     "revised, nothing stored.")

    coworker_ids = []
    if coworker:
        for name in [n.strip() for n in coworker.split(",") if n.strip()]:
            coworker_ids.append(M.resolve_coworker(name))

    other = M.scalar(f"SELECT count(*) FROM {table}_memory "
                     f"WHERE embed_model IS NOT NULL AND embed_model <> {M.q(M.EMBED_MODEL)};")
    if other and int(other) > 0:
        sys.exit(f"refused: {table}_memory has rows embedded with a different model than "
                 f"'{M.EMBED_MODEL}'. Mixing vector spaces makes cosine meaningless — "
                 "re-embed the DB (rebuild) before switching models.")

    if not M.ollama_up():
        sys.exit(f"refused: Ollama not reachable at {M.OLLAMA} — can't embed. "
                 "Start it (brew services start ollama), then retry.")
    vlit = M.fmt_vec(M.embed(text))

    # Dedup only for semantic; episodic is append-only (events recur legitimately).
    if table == "semantic" and not force and not supersedes:
        visible = ""
        if coworker_ids:
            ids_csv = ",".join(str(i) for i in coworker_ids)
            visible = (" AND (id NOT IN (SELECT memory_id FROM memory_coworkers WHERE memory_table='semantic') "
                      f"OR id IN (SELECT memory_id FROM memory_coworkers WHERE memory_table='semantic' AND coworker_id IN ({ids_csv})))")
        # embedding IS NOT NULL: vector_distance_cos raises "Invalid vector type" on a
        # NULL embedding, so one such row would make every insert fail, not just skew.
        near = M.scalar(f"SELECT round(vector_distance_cos(embedding,{vlit}),4) "
                        f"FROM semantic_memory WHERE superseded_by IS NULL AND retired_at IS NULL "
                        f"AND embedding IS NOT NULL{visible} ORDER BY 1 LIMIT 1;")
        if near not in ("", "NULL") and float(near) < DUP_DIST:
            sys.exit(f"refused: a near-duplicate already exists (cosine {near} < {DUP_DIST}). "
                     "Use --supersedes <id> to replace it, or --force to add anyway.")

    row = dict(project=project, topic=topic, source=source,
              model=model, embed_model=M.EMBED_MODEL, memory_text=text,
              file_reference=file_reference)
    if table == "semantic":
        row["category"] = category
    else:
        row["event_type"] = event_type
        row["importance"] = importance
    if created_at:
        row["created_at"] = created_at
        if table == "semantic":
            row["updated_at"] = created_at
    cols = list(row.keys()) + ["embedding"]
    vals = [M.q(row[c]) for c in row] + [vlit]
    insert = f"INSERT INTO {table}_memory ({', '.join(cols)}) VALUES ({', '.join(vals)});"

    # Multiple coworker tags go in ONE multi-row INSERT: last_insert_rowid()
    # only updates when an INSERT completes, so every VALUES row in a single
    # statement still sees the semantic/episodic insert's id, not a prior
    # join row's id.
    join_insert = ""
    if coworker_ids:
        values = ", ".join(f"({M.q(table)}, last_insert_rowid(), {cid})" for cid in coworker_ids)
        join_insert = f" INSERT INTO memory_coworkers (memory_table, memory_id, coworker_id) VALUES {values};"

    if supersedes:
        # Order matters: UPDATE doesn't touch last_insert_rowid(), so
        # join_insert can safely come after it and still see the new row's id.
        # One UPDATE covers all superseded ids, so an N->1 merge is atomic.
        id_list = ",".join(str(i) for i in supersedes)
        M.exec_sql("BEGIN; " + insert +
                   " UPDATE semantic_memory SET superseded_by=last_insert_rowid(), "
                   f"updated_at=datetime('now') WHERE id IN ({id_list});" +
                   join_insert + " COMMIT;")
    elif join_insert:
        M.exec_sql("BEGIN; " + insert + join_insert + " COMMIT;")
    else:
        M.exec_sql(insert)
    return 1


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--table", choices=["semantic", "episodic"], required=True)
    p.add_argument("--text", required=True)
    p.add_argument("--topic"); p.add_argument("--project")
    p.add_argument("--keywords"); p.add_argument("--source"); p.add_argument("--model")
    p.add_argument("--file-reference", dest="file_reference")
    p.add_argument("--created-at", dest="created_at")
    p.add_argument("--category")                        # semantic
    p.add_argument("--event-type", dest="event_type")  # episodic
    p.add_argument("--importance")                     # episodic
    p.add_argument("--confirm-baseline", action="store_true")
    p.add_argument("--force", action="store_true", help="store even if a near-duplicate exists")
    p.add_argument("--supersedes",
                   help="comma-separated id(s) of the semantic row(s) this replaces: inserts the "
                        "new row and marks all of them superseded_by it, in one transaction. "
                        "Several ids = merging N memories into one (sleep's consolidation).")
    p.add_argument("--coworker", help="comma-separated coworker name(s) to scope this memory to")
    a = p.parse_args()
    supersedes = None
    if a.supersedes:
        try:
            supersedes = [int(s.strip()) for s in a.supersedes.split(",") if s.strip()]
        except ValueError:
            sys.exit(f"refused: --supersedes '{a.supersedes}' is not a comma-separated id list.")
        if not supersedes:
            sys.exit("refused: --supersedes given with no ids.")
    store_memory(a.table, a.text, topic=a.topic, project=a.project, category=a.category,
                 event_type=a.event_type, importance=a.importance, source=a.source,
                 model=a.model, keywords=a.keywords, file_reference=a.file_reference,
                 created_at=a.created_at, confirm_baseline=a.confirm_baseline, force=a.force,
                 supersedes=supersedes, coworker=a.coworker)
    print(f"stored {a.table} memory ({a.topic or '-'})")


if __name__ == "__main__":
    main()
