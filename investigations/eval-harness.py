#!/usr/bin/env python3
"""Recall-ranking evaluation harness for supercharged-memory.

Modes:
  --validate            check every eval case still points at rows that exist and are
                        current truth; propose replacements where a supersede chain
                        moved the target. Run after deep sleep's D2 purge / D3 merges.
  --report              score the SHIPPED recall.py ranking at the current
                        RECALL_ALPHA, append to history.jsonl, diff vs the last run.
  --sweep A,B,C         score the shipped formula across alpha values, print the
                        plateau and whether the configured alpha still sits in it.
  --variants            developer comparison: frozen legacy ranker vs pure cosine vs
                        IDF-lexicographic vs blended. Used to justify the 2026-08-18
                        change; keep for the embedding-model investigation.

Cases live in the DB (`eval_cases`) and runs in `eval_runs`, so the .dump backup
covers both — they are AUTHORED artifacts that cannot be regenerated from the corpus.
Only the query-embedding cache stays on disk (pure derived data): <db parent>/eval,
override with SUPERCHARGED_MEMORY_EVAL_DIR.

  --import DIR          one-shot migration of a legacy eval.jsonl / history.jsonl
                        into eval_cases / eval_runs. Idempotent.

Never writes to semantic_memory / episodic_memory.
"""
import argparse, datetime, importlib.util, json, math, os, re, sys
from pathlib import Path

SCRIPTS = os.environ.get("SM_SCRIPTS",
                         str(Path(__file__).resolve().parent.parent / "scripts"))
sys.path.insert(0, SCRIPTS)
import memlib as M  # noqa: E402

_spec = importlib.util.spec_from_file_location("rc", os.path.join(SCRIPTS, "recall.py"))
rc = importlib.util.module_from_spec(_spec); _spec.loader.exec_module(rc)

EVAL_DIR = Path(os.environ.get("SUPERCHARGED_MEMORY_EVAL_DIR",
                               str(Path(M.DB).parent / "eval")))
QVEC_F = EVAL_DIR / "qvec.json"      # derived cache only — everything else is in the DB

BASE = {"semantic": "superseded_by IS NULL AND retired_at IS NULL", "episodic": "1=1"}

# ---- FROZEN legacy ranker (recall.py as of 2026-08-18, before the blend change).
# Inlined on purpose: importing recall.py's tokens() would make the baseline track
# the new code and silently destroy the comparison this harness exists to make.
LEGACY_STOP = {
    "the","and","for","you","with","this","that","are","was","not","but","can",
    "how","why","what","when","does","should","into","from","your","have","has",
    "will","would","about","them","then","there","here","which","were","been",
    "und","der","die","das","den","dem","ein","eine","ist","wie","was","mit",
    "für","nicht","auch","oder","aber","wird","sich","dass","man","noch","nur",
}


def legacy_tokens(query):
    out = []
    for t in re.findall(r"\w+", query.lower()):
        if any(c.isdigit() for c in t):
            out.append(t)
        elif len(t) >= 4 and t not in LEGACY_STOP:
            out.append(t)
    seen, uniq = set(), []
    for t in out:
        if t not in seen:
            seen.add(t); uniq.append(t)
    return uniq[:8]


# ---------------- data -----------------------------------------------------
def load_cases():
    out = M.exec_sql("SELECT id, class, memory_table, expect_ids, expect_stamps, query "
                     "FROM eval_cases WHERE retired_at IS NULL ORDER BY id;", mode="list")
    cases = []
    for line in out.splitlines():
        if not line.strip():
            continue
        # query goes LAST so a '|' inside it cannot shift the other fields
        cid, cls, tbl, ids, stamps, q = line.split("|", 5)
        cases.append({"id": cid, "class": cls, "table": tbl, "query": q,
                      "expect": [int(x) for x in ids.split(",") if x],
                      "stamps": stamps.split(",")})
    if not cases:
        sys.exit("no eval cases in the DB (table eval_cases is empty).\n"
                 "  Migrate a legacy file set with --import <dir>, or author cases in\n"
                 "  deep sleep D6.3. Do NOT auto-generate them from memory_text — a query\n"
                 "  written from the row it should retrieve biases every future alpha.")
    return cases


def import_files(src):
    """One-shot migration of eval.jsonl / history.jsonl into the DB."""
    src = Path(src)
    cases_f, hist_f = src / "eval.jsonl", src / "history.jsonl"
    if not cases_f.exists():
        sys.exit(f"no eval.jsonl in {src}")
    added = skipped = 0
    for line in cases_f.read_text().splitlines():
        if not line.strip():
            continue
        c = json.loads(line)
        if M.exec_sql(f"SELECT 1 FROM eval_cases WHERE id={M.q(c['id'])};", mode="list").strip():
            skipped += 1
            continue
        stamps = []
        for rid in c["expect"]:
            got = M.exec_sql(f"SELECT created_at FROM {c['table']}_memory WHERE id={rid};",
                             mode="list").strip()
            if not got:
                sys.exit(f"case {c['id']}: target row {rid} does not exist in "
                         f"{c['table']}_memory — fix the file before importing.")
            stamps.append(got.splitlines()[0])
        M.exec_sql(
            "INSERT INTO eval_cases (id, class, memory_table, query, expect_ids, expect_stamps) "
            f"VALUES ({M.q(c['id'])}, {M.q(c['class'])}, {M.q(c['table'])}, {M.q(c['query'])}, "
            f"{M.q(','.join(str(x) for x in c['expect']))}, {M.q(','.join(stamps))});")
        added += 1
    runs = 0
    if hist_f.exists():
        for line in hist_f.read_text().splitlines():
            if not line.strip():
                continue
            h = json.loads(line)
            dup = M.exec_sql(f"SELECT 1 FROM eval_runs WHERE ran_at={M.q(h['date'])} "
                             f"AND alpha={h['alpha']} AND n_cases={h['n_cases']} "
                             f"AND r1={h['r1']} AND r5={h['r5']} AND mrr={h['mrr']};",
                             mode="list").strip()
            if dup:                      # re-import must not duplicate the baseline
                continue
            M.exec_sql("INSERT INTO eval_runs (ran_at, alpha, embed_model, n_cases, r1, r5, mrr) "
                       f"VALUES ({M.q(h['date'])}, {h['alpha']}, {M.q(h['embed_model'])}, "
                       f"{h['n_cases']}, {h['r1']}, {h['r5']}, {h['mrr']});")
            runs += 1
    print(f"imported {added} case(s) ({skipped} already present), {runs} run(s).")
    print("The source files are now redundant — the DB is the source of truth and the "
          ".dump backup covers it. qvec.json stays where it is (derived cache).")


def corpus(table):
    sql = (f"SELECT id, replace(replace(replace(lower(memory_text),char(10),' '),"
           f"char(13),' '),'|',' ') FROM {table}_memory WHERE {BASE[table]};")
    out = {}
    for line in M.exec_sql(sql, mode="list").splitlines():
        if "|" in line:
            i, _, t = line.partition("|")
            out[int(i)] = t
    return out


def dists(table, vlit):
    sql = (f"SELECT id, round(vector_distance_cos(embedding,{vlit}),6) FROM {table}_memory "
           f"WHERE {BASE[table]} AND embedding IS NOT NULL;")
    out = {}
    for line in M.exec_sql(sql, mode="list").splitlines():
        if "|" in line:
            i, _, d = line.partition("|")
            out[int(i)] = float(d)
    return out


def qvecs(cases):
    cache = json.loads(QVEC_F.read_text()) if QVEC_F.exists() else {}
    missing = [c["query"] for c in cases if c["query"] not in cache]
    if missing and not M.ollama_up():
        sys.exit("Ollama is down and some queries are not cached — cannot score.")
    for q in missing:
        cache[q] = M.embed(q)
    if missing:
        EVAL_DIR.mkdir(parents=True, exist_ok=True)
        QVEC_F.write_text(json.dumps(cache))
    return cache


# ---------------- validation ----------------------------------------------
def survivor(row_id):
    """Follow a semantic supersede chain to the current row, or None if the chain
    ends retired / the row is gone."""
    seen, cur = set(), row_id
    while cur is not None and cur not in seen:
        seen.add(cur)
        out = M.exec_sql(f"SELECT COALESCE(superseded_by,-1), CASE WHEN retired_at IS NULL "
                         f"THEN 0 ELSE 1 END FROM semantic_memory WHERE id={cur};",
                         mode="list").strip()
        if not out:
            return None
        nxt, retired = (int(x) for x in out.splitlines()[0].split("|"))
        if nxt == -1:
            return None if retired else cur
        cur = nxt
    return None


def validate(cases):
    print(f"eval_cases: {len(cases)} live case(s) in {M.DB}\n")
    problems = 0
    for c in cases:
        t = c["table"]
        alive, notes = [], []
        for rid, stamp in zip(c["expect"], c["stamps"]):
            row = M.exec_sql(f"SELECT created_at FROM {t}_memory WHERE id={rid} AND {BASE[t]};",
                             mode="list").strip()
            if not row:
                surv = survivor(rid) if t == "semantic" else None
                notes.append(f"{rid} -> {surv} (superseded; repoint)" if surv
                             else f"{rid} -> GONE (purged or retired; remove the case)")
            elif row.splitlines()[0] != stamp:
                # rowid reuse: `id INTEGER PRIMARY KEY` has no AUTOINCREMENT, so a
                # purged high id comes back on the next insert pointing at a
                # completely unrelated memory. This is the only way to see it.
                notes.append(f"{rid} -> ID REUSED (created_at {row.splitlines()[0]} != "
                             f"{stamp}); the case now points at a different memory")
            else:
                alive.append(rid)
        if not notes:
            continue
        problems += 1
        state = "NO VALID TARGET LEFT" if not alive else f"{len(alive)} target(s) still valid"
        print(f"  {c['id']:<6} [{c['class']}] {state}")
        for n in notes:
            print(f"           {n}")
    if problems:
        print(f"\n{problems} case(s) need attention. A case whose targets all went GONE or "
              f"got REUSED should be RETIRED\n(UPDATE eval_cases SET retired_at=CURRENT_TIMESTAMP "
              f"WHERE id='...'), not repointed at a\nloosely-related row — a repointed case "
              f"quietly changes what the metric measures.")
    else:
        print("  all cases point at live, current rows with matching timestamps.")
    return problems


# ---------------- ranking variants -----------------------------------------
def idf_map(toks, texts):
    n = len(texts)
    return {t: math.log((n + 1) / (sum(1 for x in texts if t in x) + 1)) for t in toks}


def rank_shipped(query, rows, texts_by_id, table, alpha):
    """Exactly what recall.py does, via recall.py's own functions."""
    toks, w = rc.token_weights(rc.candidate_tokens(query), table, BASE[table])
    if not toks:
        return sorted(rows, key=lambda i: rows[i])
    total = sum(w[t] for t in toks) or 1.0
    def kw(i):
        return sum(w[t] for t in toks if t in texts_by_id[i]) / total
    return sorted(rows, key=lambda i: rows[i] - alpha * kw(i))


def rank_legacy(query, rows, texts_by_id, table, alpha):
    toks = legacy_tokens(query)
    def kw(i): return sum(1 for t in toks if t in texts_by_id[i])
    return sorted(rows, key=lambda i: (-kw(i), rows[i]))


def rank_vector(query, rows, texts_by_id, table, alpha):
    return sorted(rows, key=lambda i: rows[i])


def rank_idf_lex(query, rows, texts_by_id, table, alpha):
    toks = legacy_tokens(query)
    w = idf_map(toks, list(texts_by_id.values()))
    def kw(i): return sum(w[t] for t in toks if t in texts_by_id[i])
    return sorted(rows, key=lambda i: (-kw(i), rows[i]))


# ---------------- scoring ---------------------------------------------------
def score_all(cases, ranker, alpha, texts, dcache):
    r1 = r5 = mrr = 0.0
    per, misses = {}, []
    for c in cases:
        t = c["table"]
        ranked = ranker(c["query"], dcache[(t, c["query"])], texts[t], t, alpha)
        exp = set(c["expect"])
        hit1 = 1.0 if ranked[:1] and ranked[0] in exp else 0.0
        hit5 = 1.0 if any(i in exp for i in ranked[:5]) else 0.0
        rr = next((1.0 / p for p, i in enumerate(ranked[:10], 1) if i in exp), 0.0)
        r1 += hit1; r5 += hit5; mrr += rr
        s = per.setdefault(c["class"], [0.0, 0])
        s[0] += hit5; s[1] += 1
        if not hit5:
            misses.append((c["id"], c["query"][:44], ranked[:3]))
    n = len(cases)
    return {"r1": r1 / n, "r5": r5 / n, "mrr": mrr / n,
            "per_class": {k: v[0] / v[1] for k, v in per.items()}, "misses": misses}


def prep(cases):
    texts = {t: corpus(t) for t in ("semantic", "episodic")}
    qv = qvecs(cases)
    dcache = {}
    for c in cases:
        key = (c["table"], c["query"])
        if key not in dcache:
            dcache[key] = dists(c["table"], M.fmt_vec(qv[c["query"]]))
    return texts, dcache


def fmt(name, r, classes):
    return (f"{name:<26}{r['r1']:>6.2f}{r['r5']:>6.2f}{r['mrr']:>6.2f}  "
            + "".join(f"{r['per_class'].get(c, float('nan')):>11.2f}" for c in classes))


def header(classes):
    h = f"{'variant':<26}{'R@1':>6}{'R@5':>6}{'MRR':>6}  " + "".join(f"{c[:9]:>11}" for c in classes)
    return h + "\n" + "-" * len(h)


# ---------------- modes -----------------------------------------------------
def report(cases, texts, dcache, classes):
    r = score_all(cases, rank_shipped, rc.ALPHA, texts, dcache)
    print(f"shipped recall.py @ RECALL_ALPHA={rc.ALPHA}   ({len(cases)} cases, "
          f"noise floor {100.0/len(cases):.1f}pp per case)\n")
    print(header(classes)); print(fmt("current", r, classes))
    prev = None
    last = M.exec_sql("SELECT ran_at, alpha, n_cases, r1, r5, mrr FROM eval_runs "
                      "ORDER BY id DESC LIMIT 1;", mode="list").strip()
    if last:
        d, a, n, x1, x5, xm = last.splitlines()[0].split("|")
        prev = {"date": d, "alpha": a, "n_cases": int(n),
                "r1": float(x1), "r5": float(x5), "mrr": float(xm)}
    if prev:
        d = {k: r[k] - prev[k] for k in ("r1", "r5", "mrr")}
        print(f"\nvs {prev['date']} (alpha {prev['alpha']}, {prev['n_cases']} cases): "
              f"R@1 {d['r1']:+.2f}  R@5 {d['r5']:+.2f}  MRR {d['mrr']:+.2f}")
        if prev["n_cases"] != len(cases):
            print("  NOTE: case count changed — the deltas are not like-for-like.")
        if d["r5"] <= -2.0 / len(cases):
            print("  REGRESSION: recall@5 dropped by more than one case. Check the eval "
                  "set with --validate first (purged/superseded targets look like a\n"
                  "  ranking regression), then consider --sweep.")
    M.exec_sql("INSERT INTO eval_runs (alpha, embed_model, n_cases, r1, r5, mrr) VALUES ("
               f"{rc.ALPHA}, {M.q(M.EMBED_MODEL)}, {len(cases)}, "
               f"{r['r1']}, {r['r5']}, {r['mrr']});")
    if r["misses"]:
        print("\nmisses @5:")
        for mid, q, top in r["misses"]:
            print(f"  {mid}  {q:<46} top3={top}")
    return r


def sweep(cases, texts, dcache, classes, alphas):
    print(header(classes))
    results = {}
    for a in alphas:
        results[a] = score_all(cases, rank_shipped, a, texts, dcache)
        print(fmt(f"alpha={a}", results[a], classes))
    best_r5 = max(r["r5"] for r in results.values())
    plateau = [a for a in alphas if results[a]["r5"] >= best_r5 - 1e-9]
    # Within the recall@5 plateau, break the tie on MRR — but the plateau itself is
    # the recommendation; picking its argmax alone would be optimising noise.
    mid = max(plateau, key=lambda a: (results[a]["mrr"], -abs(a - rc.ALPHA))) if plateau else rc.ALPHA
    print(f"\nbest recall@5 {best_r5:.2f} on alpha {plateau}")
    print(f"best-in-plateau: {mid}   configured RECALL_ALPHA: {rc.ALPHA}")
    if rc.ALPHA in plateau:
        print("VERDICT: configured alpha is still inside the plateau — no change needed.")
    else:
        print(f"VERDICT: configured alpha is OUTSIDE the plateau.\n"
              f"  ASK THE USER whether to change RECALL_ALPHA to {mid}. Do not change it\n"
              f"  unattended — show this table, and note one case is "
              f"{100.0/len(cases):.1f}pp so a one-case gap is noise.")
    return results


def variants(cases, texts, dcache, classes, alphas):
    print(header(classes))
    for name, fn in (("V0_legacy_lexicographic", rank_legacy), ("V_vector_only", rank_vector),
                     ("V1_idf_lexicographic", rank_idf_lex)):
        print(fmt(name, score_all(cases, fn, None, texts, dcache), classes))
    for a in alphas:
        print(fmt(f"V3_shipped_blend a={a}", score_all(cases, rank_shipped, a, texts, dcache), classes))


def main():
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--validate", action="store_true")
    p.add_argument("--report", action="store_true")
    p.add_argument("--sweep", metavar="A,B,C")
    p.add_argument("--variants", action="store_true")
    p.add_argument("--import", dest="import_dir", metavar="DIR")
    a = p.parse_args()
    M.require_db()
    if a.import_dir:
        import_files(a.import_dir)
        return
    cases = load_cases()

    if a.validate:
        sys.exit(1 if validate(cases) else 0)

    classes = sorted({c["class"] for c in cases})
    texts, dcache = prep(cases)
    if a.sweep:
        sweep(cases, texts, dcache, classes, [float(x) for x in a.sweep.split(",")])
    elif a.variants:
        variants(cases, texts, dcache, classes,
                 [0.10, 0.15, 0.20] if not a.sweep else [])
    else:
        report(cases, texts, dcache, classes)


main()
