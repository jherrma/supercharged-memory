#!/usr/bin/env python3
"""Phase 3: score a candidate embedding model against the WHOLE memory corpus.

The 32 authored eval_cases cannot resolve a model comparison — the shipped ranking
already sits at recall@5 = 0.97 there and one case is 3.1 pp. So the primary metric
comes from ground truth that already exists in the database:

  Tier A  known-item, n~577 : query = a row's `topic`, target = that row.
                              Legitimate because `topic` is NEVER embedded —
                              remember.py appends only --keywords into memory_text.
  Tier B  supersede pairs   : query = superseded row's topic, target = current row.
                              Same subject, independently reworded weeks apart.
  Tier C  pattern -> events : query = a `pattern` row's topic, targets = the episodic
                              ids it cites ("Derived from episodic ids: ..."). Cross-topic.
  Tier D  authored cases    : eval_cases. Realism guard / veto — A-C all query by
                              topic, which is not how anyone actually searches.

Primary score is PURE COSINE: the keyword layer is model-independent and only
compresses the differences being measured. --blend also reports the shipped
`dist - RECALL_ALPHA*kw` ranking, which is what a user actually experiences.

    python3 investigations/whole-db-eval.py --db scratch/qwen3.db \
        --model qwen3-embedding:0.6b --dim 1024 [--blend]
"""
import argparse, hashlib, json, os, re, subprocess, sys, time, urllib.request
sys.dont_write_bytecode = True
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "scripts"))
import memlib as M  # noqa: E402

K = 10
SEM_BASE = "superseded_by IS NULL AND retired_at IS NULL"
EPI_BASE = "1=1"
MARK = "@@Q"


def sql(db, statements):
    """Pipe a chunk of statements. Every statement must be ONE physical line: the
    CLI splits piped input on line boundaries."""
    p = subprocess.run([M.TURSO, str(db), M.FLAG, "-q", "-m", "list"],
                       input="\n".join(statements) + "\n",
                       capture_output=True, text=True, encoding="utf-8", timeout=900)
    if p.returncode != 0 or "error" in p.stderr.lower():
        sys.exit(f"tursodb failed rc={p.returncode}: {p.stderr.strip()[:300]}"
                 f" | {p.stdout.strip()[-300:]}")
    return p.stdout


def rows(db, query):
    out = sql(db, [query])
    return [l.split("|") for l in out.split("\n") if l.strip()]


# ---------------------------------------------------------------- embeddings
_cache, _cache_path, _dirty = {}, None, False


def cache_open(model):
    global _cache, _cache_path
    d = Path(os.environ.get("SUPERCHARGED_MEMORY_EVAL_DIR",
                            str(Path(M.DB).parent / "eval")))
    d.mkdir(parents=True, exist_ok=True)
    safe = "".join(c if c.isalnum() or c in "-._" else "_" for c in model)
    _cache_path = d / f"vec-{safe}.json"
    try:
        _cache = json.loads(_cache_path.read_text())
    except Exception:
        _cache = {}


def cache_save():
    if _dirty and _cache_path:
        _cache_path.write_text(json.dumps(_cache))


def embed(text, model, dim):
    global _dirty
    key = hashlib.sha256(text.encode()).hexdigest()
    if key in _cache:
        return _cache[key]
    req = urllib.request.Request(f"{M.OLLAMA}/api/embed",
        data=json.dumps({"model": model, "input": text}).encode(),
        headers={"Content-Type": "application/json"})
    v = json.load(urllib.request.urlopen(req, timeout=300))["embeddings"][0]
    if len(v) != dim:
        sys.exit(f"{model} returned dim {len(v)}, expected {dim}")
    _cache[key] = v
    _dirty = True
    return v


# ---------------------------------------------------------------- case sets
def build_cases(db):
    """Every case: (tier, table, query_text, set(target_ids))."""
    cases = []
    for t, base in (("semantic", SEM_BASE), ("episodic", EPI_BASE)):
        for r in rows(db, f"SELECT id, replace(replace(topic,char(10),' '),'|','/') "
                          f"FROM {t}_memory WHERE {base} AND topic IS NOT NULL "
                          f"AND length(topic) > 0;"):
            if len(r) >= 2 and r[1].strip():
                cases.append(("A", t, r[1], {int(r[0])}))

    for r in rows(db, "SELECT id, superseded_by, "
                      "replace(replace(topic,char(10),' '),'|','/') "
                      "FROM semantic_memory WHERE superseded_by IS NOT NULL "
                      "AND topic IS NOT NULL AND length(topic) > 0;"):
        if len(r) >= 3 and r[2].strip():
            cases.append(("B", "semantic", r[2], {int(r[1])}))

    live_epi = {int(r[0]) for r in rows(db, "SELECT id FROM episodic_memory;")}
    for r in rows(db, "SELECT id, replace(replace(topic,char(10),' '),'|','/'), "
                      "replace(replace(memory_text,char(10),' '),'|','/') "
                      "FROM semantic_memory WHERE category='pattern' AND "
                      f"{SEM_BASE} AND topic IS NOT NULL;"):
        if len(r) < 3:
            continue
        m = re.search(r"[Dd]erived from episodic ids?:\s*([0-9,\s]+)", r[2])
        if not m:
            continue
        ids = {int(x) for x in re.findall(r"\d+", m.group(1))} & live_epi
        if ids and r[1].strip():
            cases.append(("C", "episodic", r[1], ids))

    for r in rows(db, "SELECT memory_table, replace(replace(query,char(10),' '),'|','/'), "
                      "expect_ids, class FROM eval_cases WHERE retired_at IS NULL;"):
        if len(r) >= 3:
            cases.append((f"D:{r[3] if len(r) > 3 else '?'}", r[0], r[1],
                          {int(x) for x in r[2].split(",") if x.strip()}))
    return cases


# ---------------------------------------------------------------- ranking
def rank_all(db, cases, model, dim, alpha=None):
    """Returns a list of ranked id-lists, one per case. alpha=None -> pure cosine."""
    import recall as R
    results = [None] * len(cases)
    order = sorted(range(len(cases)), key=lambda i: cases[i][1])
    t0 = time.time()
    for start in range(0, len(order), 40):
        stmts, idxs = [], []
        for i in order[start:start + 40]:
            tier, table, qtext, _ = cases[i]
            base = SEM_BASE if table == "semantic" else EPI_BASE
            v = M.fmt_vec(embed(qtext, model, dim))
            expr = f"vector_distance_cos(embedding,{v})"
            if alpha is not None:
                toks, w = R.token_weights(R.candidate_tokens(qtext), table, base)
                expr += f" - {alpha}*({R.kw_expr(toks, w)})"
            stmts.append(f"SELECT '{MARK}{i}';")
            stmts.append(f"SELECT id FROM {table}_memory WHERE {base} AND "
                         f"embedding IS NOT NULL ORDER BY {expr} ASC LIMIT {K};")
            idxs.append(i)
        out = sql(db, stmts)
        cur = None
        for line in out.split("\n"):
            line = line.strip()
            if not line:
                continue
            if line.startswith(MARK):
                cur = int(line[len(MARK):])
                results[cur] = []
            elif cur is not None and line.lstrip("-").isdigit():
                results[cur].append(int(line))
        cache_save()
        print(f"  ranked {min(start+40, len(order))}/{len(order)} "
              f"({(min(start+40,len(order)))/(time.time()-t0):.1f}/s)", flush=True)
    return results


def score(cases, ranked):
    agg = {}
    for (tier, _t, _q, targets), got in zip(cases, ranked):
        got = got or []
        pos = next((j + 1 for j, i in enumerate(got) if i in targets), None)
        # dict.fromkeys, not a tuple: for a tier with no class suffix (A/B/C)
        # `tier` and `tier.split(':')[0]` are the same key and would double-count n.
        for key in dict.fromkeys((tier, tier.split(":")[0], "ALL")):
            a = agg.setdefault(key, [0, 0, 0, 0.0])
            a[0] += 1
            if pos == 1:
                a[1] += 1
            if pos and pos <= 5:
                a[2] += 1
            if pos:
                a[3] += 1.0 / pos
    return agg


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--db", required=True)
    ap.add_argument("--model", required=True)
    ap.add_argument("--dim", type=int, required=True)
    ap.add_argument("--blend", action="store_true",
                    help="also rank with the shipped dist - RECALL_ALPHA*kw formula")
    ap.add_argument("--alpha", type=float, default=0.15)
    ap.add_argument("--json-out")
    a = ap.parse_args()

    M.DB = a.db                       # recall.py's helpers read memlib.DB
    cache_open(a.model)
    cases = build_cases(a.db)
    counts = {}
    for c in cases:
        counts[c[0].split(":")[0]] = counts.get(c[0].split(":")[0], 0) + 1
    print(f"model={a.model} dim={a.dim} db={a.db}")
    print(f"cases: {len(cases)}  " + "  ".join(f"{k}={v}" for k, v in sorted(counts.items())))

    report = {"model": a.model, "dim": a.dim, "n": len(cases)}
    for label, alpha in (("cosine", None),) + ((("blend", a.alpha),) if a.blend else ()):
        print(f"\n--- ranking [{label}] ---", flush=True)
        agg = score(cases, rank_all(a.db, cases, a.model, a.dim, alpha))
        report[label] = {k: {"n": v[0], "r1": v[1] / v[0], "r5": v[2] / v[0],
                             "mrr": v[3] / v[0]} for k, v in agg.items()}
        print(f"\n{label:>8}  {'tier':<16} {'n':>4} {'R@1':>7} {'R@5':>7} {'MRR@10':>7}")
        for k in sorted(agg):
            n, r1, r5, mrr = agg[k]
            print(f"{'':>8}  {k:<16} {n:>4} {r1/n:>7.4f} {r5/n:>7.4f} {mrr/n:>7.4f}")
    cache_save()
    if a.json_out:
        Path(a.json_out).write_text(json.dumps(report, indent=1))
        print(f"\nwrote {a.json_out}")


if __name__ == "__main__":
    main()
