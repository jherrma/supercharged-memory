#!/usr/bin/env python3
"""Shared helpers for the supercharged-memory scripts.

Central place for: DB/Ollama config, embedding (with dim assert), compact vector
literals, SQL escaping, and a robust tursodb runner (stderr-scoped error
detection + busy backoff). Import as `memlib`.
"""
import json, os, re, subprocess, sys, time, urllib.request
from pathlib import Path

TURSO = os.environ.get("TURSO_BIN", str(Path.home() / ".turso/tursodb"))
DB_ENV = "SUPERCHARGED_MEMORY_TURSO_PATH"
# XDG Base Directory spec: state that survives and is not cache goes under
# $XDG_DATA_HOME, which defaults to ~/.local/share.
XDG_DATA_HOME = Path(os.environ.get("XDG_DATA_HOME") or (Path.home() / ".local/share"))
DB_DEFAULT = str(XDG_DATA_HOME / "turso/supercharged-memory.db")
DB = os.environ.get(DB_ENV, DB_DEFAULT)
DB_FROM_ENV = DB_ENV in os.environ
BACKUP_DIR = os.environ.get("BACKUP_DIR", str(Path(__file__).resolve().parent.parent / "Backups"))
OLLAMA = os.environ.get("OLLAMA_URL", "http://localhost:11434")
EMBED_MODEL = os.environ.get("EMBED_MODEL", "bge-m3")
FLAG = "--experimental-multiprocess-wal"
DIM = 1024
MAX_TEXT = 2000

# Places a live DB plausibly lives. Searched (non-recursively) only when the
# configured path turns up empty, to tell "you pointed me somewhere wrong" apart
# from "your memory is genuinely gone".
SEARCH_DIRS = [
    Path(DB).parent,
    XDG_DATA_HOME / "turso",
    Path.home() / ".local/share/turso",
    Path.home() / "turso",
    Path.home() / ".turso",
]


def db_exists():
    return Path(DB).exists()


def _count_memories(db_path):
    """Row count for a candidate DB, or None if it isn't a readable memory DB."""
    try:
        r = subprocess.run(
            [TURSO, str(db_path), FLAG, "-q", "-m", "list",
             "SELECT (SELECT count(*) FROM semantic_memory) + "
             "(SELECT count(*) FROM episodic_memory);"],
            capture_output=True, text=True, timeout=15)
        if r.returncode != 0 or re.search(r"error", r.stderr, re.I):
            return None
        return int(r.stdout.strip().splitlines()[0])
    except Exception:
        return None


def find_candidates():
    """Existing memory DBs and backups found anywhere but the configured path.

    Returns (dbs, backups): dbs is a list of (path, memory_count) sorted with the
    richest first; backups is a list of backup paths, newest first. Used to warn
    before anything creates or overwrites a database.
    """
    configured = str(Path(DB).resolve())
    seen, dbs = set(), []
    for d in SEARCH_DIRS:
        try:
            if not d.is_dir():
                continue
            for f in sorted(d.glob("*.db")):
                rp = str(f.resolve())
                if rp in seen or rp == configured:
                    continue
                seen.add(rp)
                n = _count_memories(f)
                if n is not None:
                    dbs.append((rp, n))
        except OSError:
            continue
    dbs.sort(key=lambda t: -t[1])
    try:
        backups = sorted(Path(BACKUP_DIR).glob("*supercharged-memory*.sql.gz"),
                         key=lambda f: f.stat().st_mtime, reverse=True)
    except OSError:
        backups = []
    return dbs, [str(b) for b in backups]


def missing_report():
    """Human/agent-readable lines explaining a missing DB and what else exists."""
    src = f"{DB_ENV} env var" if DB_FROM_ENV else f"built-in default (no {DB_ENV} set)"
    lines = [f"configured path : {DB}", f"path came from  : {src}"]
    dbs, backups = find_candidates()
    for path, n in dbs:
        lines.append(f"CANDIDATE DB    : {path} ({n} memories)")
    for b in backups[:3]:
        lines.append(f"CANDIDATE BACKUP: {b}")
    if not dbs and not backups:
        lines.append("no other database or backup found in the usual locations")
    lines.append("DO NOT create or overwrite a database — ask the user first.")
    return lines


def require_db():
    if not db_exists():
        sys.exit("memory DB missing — refusing to silently create an empty one.\n"
                 + "\n".join("  " + l for l in missing_report()))


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
