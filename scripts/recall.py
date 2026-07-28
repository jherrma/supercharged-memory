#!/usr/bin/env python3
"""Recall memories. Hybrid ranking: query keywords (meaningful tokens matched in
a memory's text) boost it above pure cosine order — boost is the COUNT of distinct
matching tokens, so more overlap ranks higher (not a binary any-match tie).
Stopwords are dropped; digit-bearing tokens (ids/codes) always count.

If Ollama is down, recall degrades to keyword-only (no embedding needed).

--coworker NAME scopes the search to memories visible to that coworker
(untagged + tagged to them) — for actual semantic search, not for loading a
coworker's current profile (that's ad-hoc SQL, see README.md - Coworkers).

  recall.py "should I commit automatically?"
  recall.py "869e7xzp6" --table episodic --k 3
  recall.py "null check pattern" --coworker jeff
  recall.py --baseline        # baseline memories (pure SQL, works without Ollama)
  recall.py --topics          # topic_keywords index (pure SQL, load every session)
  recall.py --status          # MISSING | EMPTY | DEGRADED n | READY n
  recall.py --candidates      # other memory DBs/backups found — check before creating one
  recall.py --count
"""
import argparse, re, sys
sys.dont_write_bytecode = True
from pathlib import Path
sys.path.insert(0, str(Path(__file__).parent))
import memlib as M  # noqa: E402

STOP = {
    "the","and","for","you","with","this","that","are","was","not","but","can",
    "how","why","what","when","does","should","into","from","your","have","has",
    "will","would","about","them","then","there","here","which","were","been",
    "und","der","die","das","den","dem","ein","eine","ist","wie","was","mit",
    "für","nicht","auch","oder","aber","wird","sich","dass","man","noch","nur",
}


def tokens(query):
    out = []
    for t in re.findall(r"\w+", query.lower()):
        if any(c.isdigit() for c in t):        # ids / codes: keep regardless
            out.append(t)
        elif len(t) >= 4 and t not in STOP:
            out.append(t)
    # dedupe, preserve order, cap
    seen, uniq = set(), []
    for t in out:
        if t not in seen:
            seen.add(t); uniq.append(t)
    return uniq[:8]


def kw_expr(toks):
    if not toks:
        return "0"
    return " + ".join(
        f"(CASE WHEN lower(memory_text) LIKE {M.like_lit(t)} THEN 1 ELSE 0 END)"
        for t in toks)


def status():
    if not M.db_exists():
        # Print the path and any DB/backup found elsewhere: a wrong path (unset
        # env var) looks identical to real data loss unless we say so.
        print("MISSING")
        for line in M.missing_report():
            print("  " + line)
        return
    try:
        n = int(M.scalar("SELECT (SELECT count(*) FROM semantic_memory) + "
                         "(SELECT count(*) FROM episodic_memory);"))
    except Exception as e:
        print(f"ERROR {e}"); return
    if not M.ollama_up():
        print(f"DEGRADED {n}")           # DB fine, embeddings unavailable
    else:
        print("EMPTY" if n == 0 else f"READY {n}")


def candidates():
    """List memory DBs / backups found outside the configured path.

    Answers "is my real memory somewhere else?" before anything creates,
    restores, or writes into the wrong database.
    """
    dbs, backups = M.find_candidates()
    print(f"configured path : {M.DB}"
          f"{' (exists)' if M.db_exists() else ' (MISSING)'}")
    for path, n in dbs:
        print(f"CANDIDATE DB    : {path} ({n} memories)")
    for b in backups[:5]:
        print(f"CANDIDATE BACKUP: {b}")
    if not dbs and not backups:
        print("no other database or backup found in the usual locations")
    else:
        print("Ask the user before switching, restoring, or overwriting anything.")


def baseline():
    M.require_db()
    print("===== baseline (load every session, follow for the whole session) =====")
    print(M.exec_sql("SELECT topic, memory_text FROM semantic_memory "
                     "WHERE category='baseline' AND superseded_by IS NULL "
                     "AND retired_at IS NULL ORDER BY created_at;"))


def topics():
    M.require_db()
    n = M.scalar("SELECT count(*) FROM topic_keywords;")
    print(f"===== topic index (load every session; {n} topic(s); rebuilt by sleep) =====")
    print(M.exec_sql("SELECT topic, keywords FROM topic_keywords ORDER BY updated_at DESC;"))
    if n and int(n) > 50:
        print(f"NOTE: {n} topics is a lot to hold in context every session — "
              "consider consolidating harder next sleep (merge overlapping topics).")


def search(query, table, k, project, coworker):
    M.require_db()
    coworker_id = M.resolve_coworker(coworker) if coworker else None
    toks = tokens(query)
    kw = kw_expr(toks)
    proj = f" AND project = {M.q(project)}" if project else ""
    have_ollama = M.ollama_up()
    vlit = M.fmt_vec(M.embed(query)) if have_ollama else None
    for t in (["semantic", "episodic"] if table == "both" else [table]):
        base = ("superseded_by IS NULL AND retired_at IS NULL" if t == "semantic" else "1=1") + proj
        if coworker_id is not None:
            base += (f" AND (id NOT IN (SELECT memory_id FROM memory_coworkers WHERE memory_table={M.q(t)}) "
                    f"OR id IN (SELECT memory_id FROM memory_coworkers WHERE memory_table={M.q(t)} AND coworker_id={coworker_id}))")
        meta = "category" if t == "semantic" else "event_type || '/' || importance"
        if have_ollama:
            sql = (f"SELECT round(vector_distance_cos(embedding,{vlit}),4) AS dist, "
                   f"({kw}) AS kw, created_at, {meta} AS meta, project, topic, memory_text "
                   f"FROM {t}_memory WHERE {base} ORDER BY kw DESC, dist ASC LIMIT {k};")
            print(f"===== {t} (top {k}; kw = # matching keywords, boosted) =====")
        else:                                    # DEGRADED: keyword-only
            sql = (f"SELECT ({kw}) AS kw, created_at, {meta} AS meta, project, topic, memory_text "
                   f"FROM {t}_memory WHERE {base} AND ({kw}) > 0 ORDER BY kw DESC, created_at DESC LIMIT {k};")
            print(f"===== {t} (Ollama down — keyword-only, top {k}) =====")
        print(M.exec_sql(sql))


def main():
    p = argparse.ArgumentParser()
    p.add_argument("query", nargs="?")
    p.add_argument("--table", choices=["semantic", "episodic", "both"], default="both")
    p.add_argument("--project"); p.add_argument("--k", type=int, default=5)
    p.add_argument("--coworker", help="scope search to memories visible to this coworker")
    p.add_argument("--baseline", action="store_true")
    p.add_argument("--topics", action="store_true", help="load the topic_keywords index (load every session, like --baseline)")
    p.add_argument("--status", action="store_true")
    p.add_argument("--candidates", action="store_true",
                   help="list memory DBs/backups found outside the configured path")
    p.add_argument("--count", action="store_true")
    a = p.parse_args()
    if a.status:
        status(); return
    if a.candidates:
        candidates(); return
    if a.count:
        M.require_db(); print(M.scalar("SELECT (SELECT count(*) FROM semantic_memory) + "
                                       "(SELECT count(*) FROM episodic_memory);")); return
    if a.baseline:
        baseline(); return
    if a.topics:
        topics(); return
    if not a.query:
        sys.exit("provide a query, or use --baseline / --status / --count")
    search(a.query, a.table, a.k, a.project, a.coworker)


if __name__ == "__main__":
    main()
