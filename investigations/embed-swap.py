#!/usr/bin/env python3
"""Build a scratch copy of the memory DB re-embedded with a candidate model.

Phase 2 of investigations/2026-08-18-embedding-model-candidates.md. NEVER writes to
the live DB — it only reads it, and refuses to run if --out resolves to the live path.

    python3 investigations/embed-swap.py --model qwen3-embedding:0.6b --dim 1024 \
        --out /path/to/scratch/qwen3.db

Rows are copied verbatim (ids, created_at, every scalar column) and only `embedding`
and `embed_model` are replaced, so eval_cases.expect_ids / expect_stamps stay valid.
"""
import argparse, json, os, subprocess, sys, time, urllib.request
sys.dont_write_bytecode = True
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "scripts"))
import memlib as M  # noqa: E402

MEM_TABLES = ["semantic_memory", "episodic_memory"]
COPY_TABLES = MEM_TABLES + ["eval_cases", "topic_keywords"]


def run_file(db, sql_text):
    """Pipe a multi-statement script into tursodb. Piping (not argv) is what keeps
    2000-char memory_text plus a 1024-float vector literal off the command line."""
    p = subprocess.run([M.TURSO, str(db), M.FLAG], input=sql_text,
                       capture_output=True, text=True, encoding="utf-8", timeout=900)
    if p.returncode != 0 or "error" in p.stderr.lower():
        sys.exit(f"tursodb failed rc={p.returncode}\n  stderr: {p.stderr.strip()[:600]}"
                 f"\n  stdout: {p.stdout.strip()[-600:]}")
    return p.stdout


def run_batched(db, stmts, size=25):
    """tursodb reads piped statements fine, but a single 4 MB script fails with an
    empty stderr, so feed it in chunks and name the chunk that breaks."""
    for i in range(0, len(stmts), size):
        chunk = stmts[i:i + size]
        try:
            run_file(db, "\n".join(chunk) + "\n")
        except SystemExit as e:
            sys.exit(f"{e}\n  first stmt of failing chunk: {chunk[0][:200]}")


def read_rows(db, table):
    """json_object round-trips newlines, quotes and pipes safely; the default
    pipe-delimited output does not, and memory_text contains all three."""
    # `embedding` is excluded on purpose: json_object() cannot serialise an
    # F32_BLOB and fails the whole statement. It is regenerated anyway.
    cols = [c for c in columns(db, table) if c != "embedding"]
    args = ",".join(f"'{c}',{c}" for c in cols)
    p = subprocess.run([M.TURSO, str(db), M.FLAG, "-q", "-m", "list",
                        f"SELECT json_object({args}) FROM {table};"],
                       capture_output=True, text=True, encoding="utf-8", timeout=180)
    if p.returncode != 0 or "error" in p.stderr.lower():
        sys.exit(f"read {table} failed: rc={p.returncode} {p.stderr.strip()[:300]}")
    # split("\n"), NOT splitlines(): splitlines() also breaks on \r, \x0b, \x0c and
    # U+2028/U+2029, and at least one memory_text contains such a character — it
    # cuts that row's JSON in half and yields "Unterminated string".
    return [json.loads(l) for l in p.stdout.split("\n") if l.strip().startswith("{")]


def qm(v):
    """SQL string literal that survives being PIPED into tursodb.

    The CLI splits piped input on line boundaries, so a literal newline inside a
    quoted string ends the statement mid-value and the next line is parsed as SQL
    ("expected ... but found 'ALL_SERVICES'"). Splicing char(10)/char(13) back in
    keeps every statement on one physical line.
    """
    if v is None or v == "":
        return "NULL"
    lit = str(v).replace("'", "''")
    for ch, code in (("\r", 13), ("\n", 10)):
        lit = lit.replace(ch, f"' || char({code}) || '")
    return "'" + lit + "'"


def columns(db, table):
    p = subprocess.run([M.TURSO, str(db), M.FLAG, "-q", "-m", "list",
                        f"SELECT name FROM pragma_table_info('{table}');"],
                       capture_output=True, text=True, encoding="utf-8", timeout=60)
    return [c for c in p.stdout.split() if c]


def embed(text, model, dim):
    req = urllib.request.Request(f"{M.OLLAMA}/api/embed",
        data=json.dumps({"model": model, "input": text}).encode(),
        headers={"Content-Type": "application/json"})
    v = json.load(urllib.request.urlopen(req, timeout=300))["embeddings"][0]
    if len(v) != dim:
        sys.exit(f"{model} returned dim {len(v)}, expected {dim}")
    return v


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--model", required=True)
    ap.add_argument("--dim", type=int, required=True)
    ap.add_argument("--out", required=True)
    a = ap.parse_args()

    src, out = Path(M.DB), Path(a.out)
    if not src.exists():
        sys.exit(f"live DB not found at {src}")
    if out.resolve() == src.resolve():
        sys.exit("refusing to write to the live DB")
    out.parent.mkdir(parents=True, exist_ok=True)
    if out.exists():
        out.unlink()
    for side in ("-wal", "-shm"):
        p = Path(str(out) + side)
        if p.exists():
            p.unlink()

    schema = (Path(__file__).resolve().parent.parent / "schema.sql").read_text()
    if a.dim != 1024:
        schema = schema.replace("F32_BLOB\n(1024)", f"F32_BLOB\n({a.dim})")
        schema = schema.replace("F32_BLOB(1024)", f"F32_BLOB({a.dim})")
    run_file(out, schema)

    total_embedded, t0 = 0, time.time()
    for table in COPY_TABLES:
        rows = read_rows(src, table)
        stmts = []
        for r in rows:
            r.pop("embedding", None)
            if table in MEM_TABLES:
                r["embed_model"] = a.model
            cols = [c for c in r if r[c] is not None]
            vals = [str(r[c]) if isinstance(r[c], (int, float)) else qm(r[c])
                    for c in cols]
            if table in MEM_TABLES:
                cols.append("embedding")
                vals.append(M.fmt_vec(embed(r["memory_text"], a.model, a.dim)))
                total_embedded += 1
                if total_embedded % 50 == 0:
                    print(f"  embedded {total_embedded} "
                          f"({total_embedded/(time.time()-t0):.1f}/s)", flush=True)
            stmts.append(f"INSERT INTO {table} ({','.join(cols)}) "
                         f"VALUES ({','.join(vals)});")
        if stmts:
            run_batched(out, stmts)
        print(f"{table}: {len(rows)} rows", flush=True)

    print(f"done: {total_embedded} embeddings in {time.time()-t0:.0f}s -> {out}")


if __name__ == "__main__":
    main()
