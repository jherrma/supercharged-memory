#!/usr/bin/env python3
"""Recall memories. Hybrid ranking: rows are scored

    score = cosine_distance - RECALL_ALPHA * keyword_credit    (lower is better)

where keyword_credit is the IDF-weighted share of the query's tokens present in the
memory, normalised to 0..1. IDF is measured per query against the same rows the
search ranks, so common words demote themselves and no stopword list is needed.
The tokens carried into the score are the 8 RAREST in the query, not the first 8.

RECALL_ALPHA (default 0.15) is corpus-calibrated, not universal — re-measure it with
investigations/eval-harness.py after an embedding-model change or a big topic shift.

This replaced an "ORDER BY kw DESC, dist ASC" lexicographic sort, which measured
WORSE than using no keyword layer at all. See
investigations/2026-08-18-recall-keyword-layer.md for the numbers.

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
import argparse, math, os, re, sys
sys.dont_write_bytecode = True
from pathlib import Path
sys.path.insert(0, str(Path(__file__).parent))
import memlib as M  # noqa: E402

MAX_CANDIDATES = 24     # bound the df probe; longer queries than this are rare
MAX_TOKENS = 8          # tokens carried into the score, chosen by IDF
ALPHA = float(os.environ.get("RECALL_ALPHA", "0.15"))


def candidate_tokens(query):
    """Every distinct word token, in query order, capped. Deliberately no length
    filter and no stopword list — IDF demotes common words on its own, and the old
    len>=4 rule was throwing away sql/wal/api/ef/pr."""
    seen, out = set(), []
    for t in re.findall(r"\w+", query.lower()):
        if t not in seen:
            seen.add(t)
            out.append(t)
    # If the cap has to bite, cut the least promising rather than the last-typed:
    # real IDF isn't known yet, but digit-bearing and longer tokens are the cheap
    # proxy for "rare". Order is otherwise irrelevant — token_weights re-sorts by IDF.
    if len(out) > MAX_CANDIDATES:
        out.sort(key=lambda t: (not any(c.isdigit() for c in t), -len(t)))
    return out[:MAX_CANDIDATES]


def token_weights(cands, table, base):
    """IDF per candidate token, measured against exactly the rows this search will
    rank (same base predicate), then keep the MAX_TOKENS rarest. One extra scan."""
    if not cands:
        return [], {}
    sums = ", ".join(
        f"SUM(CASE WHEN lower(memory_text) LIKE {M.like_lit(t)} THEN 1 ELSE 0 END)"
        for t in cands)
    out = M.exec_sql(f"SELECT {sums}, COUNT(*) FROM {table}_memory WHERE {base};",
                     mode="list").strip().splitlines()
    vals = [int(x) for x in out[0].split("|")] if out else []
    if len(vals) != len(cands) + 1:          # unexpected shape: fall back to flat weights
        toks = cands[:MAX_TOKENS]
        return toks, {t: 1.0 for t in toks}
    n = vals[-1]
    w = {t: math.log((n + 1) / (df + 1)) for t, df in zip(cands, vals[:-1])}
    return sorted(cands, key=lambda t: -w[t])[:MAX_TOKENS], w


def kw_expr(toks, w):
    """IDF-weighted share of the query's tokens found in the row, normalised to 0..1
    so ALPHA means the same thing regardless of how many tokens the query has."""
    if not toks:
        return "0"
    total = sum(w[t] for t in toks) or 1.0
    terms = " + ".join(
        f"(CASE WHEN lower(memory_text) LIKE {M.like_lit(t)} THEN {w[t]:.6f} ELSE 0 END)"
        for t in toks)
    return f"(({terms}) / {total:.6f})"


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
    cands = candidate_tokens(query)
    proj = f" AND project = {M.q(project)}" if project else ""
    have_ollama = M.ollama_up()
    vlit = M.fmt_vec(M.embed(query)) if have_ollama else None
    for t in (["semantic", "episodic"] if table == "both" else [table]):
        base = ("superseded_by IS NULL AND retired_at IS NULL" if t == "semantic" else "1=1") + proj
        if coworker_id is not None:
            base += (f" AND (id NOT IN (SELECT memory_id FROM memory_coworkers WHERE memory_table={M.q(t)}) "
                    f"OR id IN (SELECT memory_id FROM memory_coworkers WHERE memory_table={M.q(t)} AND coworker_id={coworker_id}))")
        toks, w = token_weights(cands, t, base)
        kw = kw_expr(toks, w)
        meta = "category" if t == "semantic" else "event_type || '/' || importance"
        if have_ollama:
            # embedding IS NOT NULL: vector_distance_cos raises "Invalid vector type"
            # on a NULL, which would take down the whole query over one bad row.
            sql = (f"SELECT round(vector_distance_cos(embedding,{vlit}),4) AS dist, "
                   f"round({kw},3) AS kw, "
                   f"round(vector_distance_cos(embedding,{vlit}) - {ALPHA}*({kw}),4) AS score, "
                   f"created_at, {meta} AS meta, project, topic, memory_text "
                   f"FROM {t}_memory WHERE {base} AND embedding IS NOT NULL "
                   f"ORDER BY score ASC LIMIT {k};")
            print(f"===== {t} (top {k}; score = dist - {ALPHA}*kw, lower is better; "
                  f"kw = IDF-weighted keyword share 0..1) =====")
        else:                                    # DEGRADED: keyword-only
            sql = (f"SELECT round({kw},3) AS kw, created_at, {meta} AS meta, project, topic, memory_text "
                   f"FROM {t}_memory WHERE {base} AND ({kw}) > 0 "
                   f"ORDER BY kw DESC, created_at DESC LIMIT {k};")
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
