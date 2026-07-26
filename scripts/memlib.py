#!/usr/bin/env python3
"""Shared helpers for the agent-foundations memory scripts.

Central place for: DB/Ollama config, embedding (with dim assert), compact vector
literals, SQL escaping, and a robust tursodb runner (stderr-scoped error
detection + busy backoff). Import as `memlib`.
"""
import json, os, re, subprocess, sys, time, urllib.request
from pathlib import Path

TURSO = os.environ.get("TURSO_BIN", str(Path.home() / ".turso/tursodb"))
DB = os.environ.get("SUPERCHARGED_MEMORY_TURSO_PATH", str(Path.home() / "Documents/turso/agent-foundations.db"))
OLLAMA = os.environ.get("OLLAMA_URL", "http://localhost:11434")
EMBED_MODEL = os.environ.get("EMBED_MODEL", "bge-m3")
FLAG = "--experimental-multiprocess-wal"
DIM = 1024
MAX_TEXT = 2000


def db_exists():
    return Path(DB).exists()


def require_db():
    if not db_exists():
        sys.exit(f"memory DB missing at {DB} — restore a backup or run seed.py. "
                 "(Refusing to let a probe silently create an empty DB.)")


def ollama_up():
    try:
        urllib.request.urlopen(f"{OLLAMA}/api/version", timeout=5)
        return True
    except Exception:
        return False


def embed(text):
    req = urllib.request.Request(f"{OLLAMA}/api/embed",
        data=json.dumps({"model": EMBED_MODEL, "input": text}).encode(),
        headers={"Content-Type": "application/json"})
    v = json.load(urllib.request.urlopen(req, timeout=120))["embeddings"][0]
    if len(v) != DIM:
        sys.exit(f"embedding dim {len(v)} != expected {DIM} (model {EMBED_MODEL}); refusing to insert.")
    return v


def fmt_vec(v):
    # F32 keeps ~7 significant digits; %.7g halves the SQL length vs repr(float).
    return "vector32('[" + ",".join(f"{x:.7g}" for x in v) + "]')"


def q(v):
    return "NULL" if v in (None, "") else "'" + str(v).replace("'", "''") + "'"


def like_lit(tok):
    tok = tok.replace("'", "''").replace("%", "").replace("_", "")
    return "'%" + tok + "%'"


def exec_sql(sql, mode="line"):
    """Run one statement via tursodb. Error detection is stderr-scoped so row
    data on stdout can't false-trigger it. Retries with backoff on busy/locked."""
    for attempt in range(6):
        r = subprocess.run([TURSO, DB, FLAG, "-q", "-m", mode, sql],
                           capture_output=True, text=True)
        if re.search(r"busy|locked", r.stderr, re.I):
            time.sleep(0.3 * (attempt + 1))
            continue
        if r.returncode != 0 or re.search(r"error", r.stderr, re.I):
            raise RuntimeError(r.stderr.strip() or r.stdout.strip() or "unknown tursodb error")
        return r.stdout
    raise RuntimeError("database busy after retries")


def scalar(sql):
    out = exec_sql(sql, mode="list").strip()
    return out.splitlines()[0].strip() if out else ""


def resolve_coworker(name):
    cid = scalar(f"SELECT id FROM coworkers WHERE name={q(name)};")
    if not cid:
        sys.exit(f"refused: no coworker named '{name}'.")
    return int(cid)
