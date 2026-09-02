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
--purge <ids> --confirm-purge    HARD-DELETE superseded/retired semantic rows the
                                  user explicitly selected (deep sleep only — see
                                  ../instructions/DEEP-SLEEP.md). The one place in
                                  this system that deletes a memory, hence four
                                  refusals: a current row, a dangling supersede
                                  target, a missing --confirm-purge, or no backup
                                  newer than the DB. Also clears the row's
                                  memory_coworkers scoping rows (that table has no
                                  FK on memory_id, so orphans would silently
                                  re-attach to a future memory reusing the id).

--cluster [--table T]            Read-only: group rows by embedding similarity and
        [--threshold N]           print JSON on stdout. Mechanical, no LLM — pairwise
                                  vector_distance_cos below the threshold, then
                                  connected components. Feeds deep sleep's
                                  compaction (semantic, tight) and its pattern-mining
                                  map stage (episodic, looser). Rows with no
                                  embedding are listed separately, never dropped.

--verify-candidates              Read-only: current rows that quote a CHECKABLE
        [--table T] [--limit N]   artifact (a script path, a CLI flag, an env var,
                                  a version) and are therefore the rows a Verify
                                  pass should re-check for staleness — oldest
                                  first, artifact-class count as tiebreak. Prints
                                  JSON; decides nothing. Feeds deep sleep's D7.
                                  Class PRESENCE is computed in SQL, never by
                                  pulling memory_text — the orchestrator must not
                                  hold text in bulk, and memory_text contains
                                  newlines that line-mode output cannot carry.

--contradiction-candidates       Read-only: pairs of CURRENT rows close enough in
        [--table T]               vector space to be about the same subject, and
        [--threshold N]           therefore able to contradict each other. Reuses
                                  --cluster's grouping at a tighter default
                                  (0.15 — just above remember.py's 0.10 dedup
                                  threshold), then expands clusters to pairs.
                                  Shortlist only; an LLM decides whether a pair
                                  actually disagrees. Feeds D6.5.

--staleness [--table T]          Read-only: age profile of current rows in four
                                  buckets, plus the oldest current row. Pure SQL.
                                  Reported, never persisted — eval_runs has fixed
                                  columns and this work adds no schema. Feeds D6.5.

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

# --cluster defaults, measured against a real ~230-row bge-m3 corpus, NOT guessed.
# Clusters are connected components, so distance chains: a-b close, b-c close puts
# a and c together even if a and c are unrelated. That makes the useful range much
# tighter than intuition suggests. Semantic: 0.10 finds nothing (that's remember.py's
# own duplicate threshold), 0.22 gives ~11 pairs/triples, 0.30 already yields a
# 33-row blob and 0.35 collapses 154 of ~200 rows into one. Episodic runs slightly
# looser since it only needs rows related enough to read together, but 0.40 there
# swallows the entire table.
DEFAULT_THRESHOLD = {"semantic": 0.22, "episodic": 0.25}

# A "cluster" bigger than this is chaining, not a group of near-duplicates — the
# script says so instead of handing back a blob that looks like a finding.
BLOB_MIN, BLOB_SHARE = 12, 0.25

TOTAL_CHAR_CAP = 500  # hard cap on topic_keywords' total content size (topic+keywords
                      # across all rows) — it's loaded into every session's context,
                      # not queried on demand, so this bounds a fixed per-session cost.

# --verify-candidates: what counts as a CHECKABLE artifact. Each entry is a SQL
# predicate over memory_text, evaluated in the DB on purpose — the orchestrator
# must never pull memory_text in bulk (see instructions/DEEP-SLEEP.md, Subagent
# contract), and memory_text contains newlines, which line-mode output cannot
# carry back safely. So the script reports WHICH CLASSES a row mentions and lets
# a worker read the row itself to extract the actual strings.
# Each pattern is deliberately NARROW. Measured on the live corpus while building
# this: the loose first drafts matched 502 of 561 rows (89%), which makes the
# artifact-density tiebreak meaningless — "--" alone hits prose dashes, a run of
# six capitals hits ordinary emphasis (ALWAYS, NEVER, WHAT WAS ACTUALLY BAD), and
# '[0-9].[0-9]' hits any decimal including scores like 0.96875. So: a flag must be
# '--' followed by a lowercase letter, an env var must carry an underscore between
# capitals, and a version needs two dots or a 'v' prefix.
ARTIFACT_CLASSES = {
    "paths":    ("memory_text LIKE '%scripts/%' OR memory_text LIKE '%instructions/%' "
                 "OR memory_text LIKE '%investigations/%' OR memory_text LIKE '%migration-steps/%'"),
    "flags":    "memory_text GLOB '*--[a-z]*'",
    "env":      "memory_text GLOB '*[A-Z][A-Z]_[A-Z]*'",
    "versions": ("memory_text GLOB '*[0-9].[0-9].[0-9]*' "
                 "OR memory_text GLOB '*v[0-9].[0-9]*'"),
}

# Contradictions live just ABOVE remember.py's own duplicate threshold (0.10):
# two rows about the same subject that state different things are near-identical
# in vector space but not duplicates. 0.22 (the merge-candidate default) is far
# too loose for this. NOT yet measured on this corpus — override with --threshold
# and record what worked.
CONTRADICTION_THRESHOLD = 0.15


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


def _newest_backup():
    """(path, mtime) of the newest dump in BACKUP_DIR, or (None, 0)."""
    try:
        dumps = sorted(Path(M.BACKUP_DIR).glob("*supercharged-memory*.sql.gz"),
                       key=lambda f: f.stat().st_mtime, reverse=True)
    except OSError:
        return None, 0
    return (dumps[0], dumps[0].stat().st_mtime) if dumps else (None, 0)


def purge(ids_csv, confirm):
    """Hard-delete non-current semantic rows. Deep sleep's D2 only."""
    ids = [int(i.strip()) for i in ids_csv.split(",") if i.strip()]
    if not ids:
        sys.exit("refused: no ids given.")
    ids = sorted(set(ids))
    if not confirm:
        sys.exit(f"refused: --purge hard-DELETEs {len(ids)} memory row(s) — irreversible except "
                 "from a backup. Re-run with --confirm-purge once the user has picked the ids.")
    M.require_db()

    # A dump newer than the DB is the only undo this operation has.
    dump, dump_mtime = _newest_backup()
    db_mtime = Path(M.DB).stat().st_mtime
    if dump is None:
        sys.exit(f"refused: no backup found in {M.BACKUP_DIR}. Run "
                 "scripts/supercharged-memory-backup.sh first — a purge has no other undo.")
    if dump_mtime < db_mtime:
        sys.exit(f"refused: newest backup ({dump.name}) is older than the database itself, so it "
                 "does not contain everything you are about to delete. Run "
                 "scripts/supercharged-memory-backup.sh first.\n"
                 "  (Any write ages the DB past the dump — including a previous --purge. Pass "
                 "ALL selected ids in ONE --purge call rather than one call per id.)")

    id_list = ",".join(str(i) for i in ids)
    found = {int(l) for l in M.exec_sql(
        f"SELECT id FROM semantic_memory WHERE id IN ({id_list});", mode="list").split()}
    missing = [i for i in ids if i not in found]
    if missing:
        sys.exit(f"refused: no semantic row with id {','.join(str(i) for i in missing)}.")

    current = [int(l) for l in M.exec_sql(
        f"SELECT id FROM semantic_memory WHERE id IN ({id_list}) "
        "AND superseded_by IS NULL AND retired_at IS NULL;", mode="list").split()]
    if current:
        sys.exit(f"refused: id {','.join(str(i) for i in sorted(current))} is CURRENT truth "
                 "(neither superseded nor retired). --purge only removes rows already replaced "
                 "or retired; retire it first if it really is obsolete.")

    # A surviving row pointing at a purged one would be left with a dangling
    # superseded_by, breaking the revision chain it documents.
    dangling = M.exec_sql(
        f"SELECT id FROM semantic_memory WHERE superseded_by IN ({id_list}) "
        f"AND id NOT IN ({id_list});", mode="list").split()
    if dangling:
        sys.exit(f"refused: row(s) {','.join(dangling)} are superseded BY a row in this purge "
                 "list, so deleting it would strand them. Purge the whole chain together, or "
                 "leave these targets in place.")

    mc = M.scalar(f"SELECT count(*) FROM memory_coworkers WHERE memory_table='semantic' "
                  f"AND memory_id IN ({id_list});") or "0"
    M.exec_sql("BEGIN; "
               f"DELETE FROM memory_coworkers WHERE memory_table='semantic' AND memory_id IN ({id_list}); "
               f"DELETE FROM semantic_memory WHERE id IN ({id_list}); COMMIT;")
    print(f"purged {len(ids)} semantic row(s): {id_list}")
    print(f"cleared {mc} memory_coworkers row(s)")
    print(f"backup verified: {dump}")


def cluster(table, threshold, emit=True):
    """Connected components over pairwise cosine distance. Read-only, no LLM.

    Returns the result dict so other primitives can re-use the grouping
    (--contradiction-candidates) instead of duplicating the SQL; `emit` controls
    whether it also prints, so --cluster's output is byte-identical to before.
    """
    M.require_db()
    tbl = f"{table}_memory"

    def preds(alias):
        # semantic clusters current truth only; episodic clusters every row —
        # processed_at means "sifted for lessons", not "pattern-checked".
        p = [f"{alias}embedding IS NOT NULL"]
        if table == "semantic":
            p += [f"{alias}superseded_by IS NULL", f"{alias}retired_at IS NULL"]
        return " AND ".join(p)

    pairs = M.exec_sql(
        f"SELECT a.id, b.id FROM {tbl} a JOIN {tbl} b ON a.id < b.id "
        f"WHERE {preds('a.')} AND {preds('b.')} "
        f"AND vector_distance_cos(a.embedding, b.embedding) < {float(threshold)};",
        mode="list")

    parent = {}
    def find(x):
        parent.setdefault(x, x)
        while parent[x] != x:
            parent[x] = parent[parent[x]]
            x = parent[x]
        return x
    for line in pairs.splitlines():
        line = line.strip()
        if not line:
            continue
        a, b = (int(v) for v in line.split("|"))
        ra, rb = find(a), find(b)
        if ra != rb:
            parent[rb] = ra

    groups = {}
    for node in list(parent):
        groups.setdefault(find(node), []).append(node)
    clusters = [{"cluster": n + 1, "ids": sorted(g)} for n, g in
                enumerate(sorted(groups.values(), key=lambda g: (-len(g), min(g))))]

    missing_where = "embedding IS NULL"
    if table == "semantic":
        missing_where += " AND superseded_by IS NULL AND retired_at IS NULL"
    no_embedding = [int(l) for l in M.exec_sql(
        f"SELECT id FROM {tbl} WHERE {missing_where};", mode="list").split()]

    out = {"table": table, "threshold": float(threshold),
           "clusters": clusters, "no_embedding": no_embedding}
    clustered = sum(len(c["ids"]) for c in clusters)
    biggest = max((len(c["ids"]) for c in clusters), default=0)
    if biggest > max(BLOB_MIN, BLOB_SHARE * clustered):
        out["warning"] = (
            f"largest cluster holds {biggest} of {clustered} clustered rows — at this "
            f"threshold distance is chaining unrelated rows together (a-b close, b-c "
            f"close pulls in a-c). Re-run with a lower --threshold before treating "
            f"these as merge candidates.")
    if emit:
        print(json.dumps(out, indent=1))
    return out


def contradiction_candidates(table, threshold):
    """Read-only: pairs of current rows close enough to possibly CONTRADICT.

    Issue #10 item E. A contradiction is two current rows about the same subject
    that state different things — near-identical in vector space, but not
    duplicates. This finds the shortlist mechanically and judges nothing: an LLM
    adjudicates only these pairs, so the cost does not scale with the corpus.

    Reuses --cluster's grouping at a tighter threshold, then expands each cluster
    into pairs, because "which two rows disagree" is the question a judge can
    actually answer.
    """
    grouped = cluster(table, threshold, emit=False)
    pairs = []
    for c in grouped["clusters"]:
        ids = c["ids"]
        for i in range(len(ids)):
            for j in range(i + 1, len(ids)):
                pairs.append({"ids": [ids[i], ids[j]]})
    out = {"table": table, "threshold": float(threshold), "n_pairs": len(pairs),
           "pairs": pairs, "no_embedding": grouped["no_embedding"],
           "note": "candidates only — a pair here shares a subject; whether the two "
                   "rows CONTRADICT is a judgment call, not a distance."}
    if "warning" in grouped:
        out["warning"] = grouped["warning"]
    print(json.dumps(out, indent=1))


def staleness(table):
    """Read-only: age profile of current rows. Pure SQL, no LLM, no embeddings.

    Issue #10 item E. Reported, never persisted: eval_runs has fixed columns and
    adding one would be a schema change this work deliberately avoids, so there
    is no trend baseline for this number yet — compare it by eye across passes.
    """
    M.require_db()
    tbl = f"{table}_memory"
    live_sql = ("superseded_by IS NULL AND retired_at IS NULL"
                if table == "semantic" else "1=1")
    rows = M.exec_sql(
        "SELECT CASE "
        "  WHEN julianday('now') - julianday(created_at) < 30  THEN '0-30d' "
        "  WHEN julianday('now') - julianday(created_at) < 90  THEN '30-90d' "
        "  WHEN julianday('now') - julianday(created_at) < 180 THEN '90-180d' "
        "  ELSE '180d+' END AS bucket, count(*) "
        f"FROM {tbl} WHERE {live_sql} GROUP BY bucket;", mode="list")
    buckets = {"0-30d": 0, "30-90d": 0, "90-180d": 0, "180d+": 0}
    for line in rows.splitlines():
        line = line.strip()
        if not line:
            continue
        name, n = line.split("|", 1)
        buckets[name.strip()] = int(n.strip())
    total = sum(buckets.values())
    oldest = M.scalar(f"SELECT min(created_at) FROM {tbl} WHERE {live_sql};") or ""
    print(json.dumps({"table": table, "n_current": total, "buckets": buckets,
                      "oldest_current": oldest,
                      "note": "age is not wrongness — an old row about a stable fact is "
                              "fine. Read this next to D7's stale count, not alone."},
                     indent=1))


def verify_candidates(table, limit):
    """Read-only: current rows quoting a checkable artifact, oldest first.

    Issue #10 item A. Staleness cannot be found by ranking — a row can rank
    first and still name a flag that was renamed months ago — so this lists what
    a Verify pass should re-check and in what order. It decides nothing:
    retiring stays a user call via --retire.

    Priority is age first (staleness risk grows with age), then how many
    artifact CLASSES the row mentions as a tiebreak. There is deliberately no
    retrieval-frequency signal: adding one would make recall.py a writer on
    every query, which is a worse trade than a coarser ordering here.
    """
    M.require_db()
    tbl = f"{table}_memory"
    live = ["1=1"]
    if table == "semantic":
        live = ["superseded_by IS NULL", "retired_at IS NULL"]
    live_sql = " AND ".join(live)

    flags = ", ".join(f"({pred}) AS has_{name}" for name, pred in ARTIFACT_CLASSES.items())
    any_artifact = " OR ".join(f"({pred})" for pred in ARTIFACT_CLASSES.values())

    # topic is selected LAST so that a stray '|' inside it can only split the
    # tail, which is re-joined below rather than silently dropping a column.
    rows = M.exec_sql(
        f"SELECT id, created_at, "
        f"CAST(julianday('now') - julianday(created_at) AS INTEGER) AS age_days, "
        f"{flags}, topic FROM {tbl} "
        f"WHERE {live_sql} AND ({any_artifact}) ORDER BY created_at ASC;",
        mode="list")

    names = list(ARTIFACT_CLASSES)
    candidates = []
    for line in rows.splitlines():
        line = line.strip()
        if not line:
            continue
        parts = line.split("|", 3 + len(names))
        rid, created_at, age_days = parts[0], parts[1], parts[2]
        present = [n for n, v in zip(names, parts[3:3 + len(names)]) if v.strip() == "1"]
        topic = parts[3 + len(names)] if len(parts) > 3 + len(names) else ""
        candidates.append({"id": int(rid), "topic": topic, "created_at": created_at,
                           "age_days": int(age_days or 0),
                           "artifacts": present, "artifact_classes": len(present)})

    candidates.sort(key=lambda c: (c["created_at"], -c["artifact_classes"]))
    n_current = int(M.scalar(f"SELECT count(*) FROM {tbl} WHERE {live_sql};") or 0)
    out = {"table": table, "n_current": n_current, "n_candidates": len(candidates),
           "n_no_artifacts": n_current - len(candidates), "limit": limit,
           "candidates": candidates[:limit] if limit else candidates}
    print(json.dumps(out, indent=1))


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
    p.add_argument("--purge", metavar="ID[,ID...]",
                   help="hard-delete superseded/retired semantic rows (deep sleep; needs --confirm-purge)")
    p.add_argument("--confirm-purge", dest="confirm_purge", action="store_true",
                   help="required acknowledgement that --purge is irreversible")
    p.add_argument("--cluster", action="store_true",
                   help="print embedding-similarity clusters as JSON (read-only)")
    p.add_argument("--table", choices=["semantic", "episodic"], default="semantic",
                   help="--cluster: which table (default semantic)")
    p.add_argument("--contradiction-candidates", dest="contradiction_candidates",
                   action="store_true",
                   help="print pairs of current rows close enough to possibly contradict (read-only)")
    p.add_argument("--staleness", action="store_true",
                   help="print the age profile of current rows (read-only)")
    p.add_argument("--verify-candidates", dest="verify_candidates", action="store_true",
                   help="print current rows quoting a checkable artifact, oldest first (read-only)")
    p.add_argument("--limit", type=int, default=0,
                   help="--verify-candidates: cap the list (0 = all)")
    p.add_argument("--threshold", type=float,
                   help="--cluster: max cosine distance within a cluster "
                        f"(default {DEFAULT_THRESHOLD['semantic']} semantic / "
                        f"{DEFAULT_THRESHOLD['episodic']} episodic; lower = tighter). "
                        f"--contradiction-candidates: default {CONTRADICTION_THRESHOLD}")
    a = p.parse_args()
    if a.mark_processed:
        mark_processed(a.mark_processed)
    elif a.retire is not None:
        retire(a.retire)
    elif a.rebuild_topics:
        rebuild_topics()
    elif a.purge:
        purge(a.purge, a.confirm_purge)
    elif a.cluster:
        cluster(a.table, a.threshold if a.threshold is not None else DEFAULT_THRESHOLD[a.table])
    elif a.verify_candidates:
        verify_candidates(a.table, a.limit)
    elif a.contradiction_candidates:
        contradiction_candidates(
            a.table, a.threshold if a.threshold is not None else CONTRADICTION_THRESHOLD)
    elif a.staleness:
        staleness(a.table)
    else:
        sys.exit("provide one of --mark-processed / --retire / --rebuild-topics / "
                 "--purge / --cluster / --verify-candidates / "
                 "--contradiction-candidates / --staleness")


if __name__ == "__main__":
    main()
