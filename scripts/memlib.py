#!/usr/bin/env python3
"""Shared helpers for the supercharged-memory scripts.

Central place for: DB/Ollama config, embedding (with dim assert), compact vector
literals, SQL escaping, and a robust tursodb runner (failure detected on the exit
status, on stderr, and on a stdout that is nothing but a diagnostic;
phrase-scoped busy backoff). Import as `memlib`.
"""
import json, os, re, subprocess, sys, time, urllib.request
from pathlib import Path

TURSO = os.environ.get("TURSO_BIN", str(Path.home() / ".turso/tursodb"))

# Every script in this repo prints em-dashes, and a Windows console inherits the
# legacy OEM codepage (cp850/cp437 on a default machine) where U+2014 is undefined
# -- so the default is a hard UnicodeEncodeError on the first line of output, not
# mojibake. Force UTF-8 with errors="replace": a wrong glyph on a legacy console
# beats a traceback, and nothing here should die over a dash. No-op off Windows.
# sys.platform is "msys"/"cygwin" under an MSYS2 or Cygwin Python, and Git Bash is
# the shell this repo's Windows path recommends -- so gate on all three, matching
# supercharged-memory-backup.sh's `uname -s` test (MINGW*|MSYS*|CYGWIN*).
IS_WINDOWS = sys.platform in ("win32", "msys", "cygwin")

if IS_WINDOWS:
    for _stream in (sys.stdout, sys.stderr):
        try:
            _stream.reconfigure(encoding="utf-8", errors="replace")
        except (AttributeError, ValueError):
            pass   # already-wrapped or non-reconfigurable stream: leave it alone

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
# Windows' default IO backend refuses multiprocess WAL outright ("experimental
# multiprocess WAL is not supported by the active IO backend"), which makes every
# script here fail on open. tursodb's own --help names the fix: pair the flag with
# the IOCP backend. Dropping the flag instead is not an option -- it is what keeps
# concurrent openers from being refused (see CLAUDE.md, Concurrency).
# TURSO_VFS overrides in both directions: name a different backend, or set it to
# "none" (or empty) to drop --vfs altogether -- what a future tursodb supporting
# multiprocess WAL natively on Windows, or renaming the backend, will need.
_VFS_ENV = os.environ.get("TURSO_VFS")
if _VFS_ENV is None:
    VFS = "experimental_win_iocp" if IS_WINDOWS else ""
else:
    VFS = "" if _VFS_ENV.strip().lower() in ("", "none") else _VFS_ENV.strip()
# Splat this everywhere tursodb is opened, so no call site can drift.
OPEN_ARGS = [FLAG] + (["--vfs", VFS] if VFS else [])
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
] + [Path(os.environ[_v]) / "turso" for _v in ("LOCALAPPDATA", "APPDATA")
     if os.environ.get(_v)]   # SETUP.md's recommended Windows location


def db_exists():
    return Path(DB).exists()


def _count_memories(db_path):
    """Row count for a candidate DB, or None if it isn't a readable memory DB."""
    try:
        r = subprocess.run(
            [TURSO, str(db_path), *OPEN_ARGS, "-q", "-m", "list",
             "SELECT (SELECT count(*) FROM semantic_memory) + "
             "(SELECT count(*) FROM episodic_memory);"],
            capture_output=True, text=True, encoding="utf-8", errors="replace",
            timeout=15)
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


# The two contention messages, matched as PHRASES. The bare words "busy" and
# "locked" are not usable: a failed run echoes the offending SQL back (miette caret
# diagram) plus any rows already scanned, so a query text or a row body holding
# either word would turn a hard SQL error into a 6-attempt stall -- and
# `recall.py "database is locked"` is a search this corpus invites.
BUSY_RE = re.compile(r"database is (?:busy|locked)", re.I)
# Shapes tursodb reports a diagnostic in. Verified on 0.7.1 AND 0.7.2 on Linux by
# driving the binary directly (a SELECT against a missing table, a syntax error, a
# CHECK and a NOT NULL violation, a reserved `__turso_internal_` name, a bad --vfs,
# an unopenable path) -- identical output on both versions:
#   stdout  "  × Parse error: no such table: nope"      <- miette, unicode theme
#   stdout  "  × Parse error: Object name reserved ..." followed by
#           "  │ __turso_internal_seq_foo"              <- long message, wrapped
#   stdout  "Error: Runtime error: CHECK constraint failed: ... (19)"  <- no bullet
#   stderr  "Error: Invalid argument supplied: no such VFS: ..."       <- open/CLI
# Note where the word "error" sits: a parse error reads "× Parse error:", i.e.
# MID-LINE, so a line-initial "error:" alternative can never match one. Only the
# "Error:"-prefixed runtime/CLI shape starts with it.
# The alternatives beyond the observed glyphs are deliberate breadth, not observed
# behaviour:
#  - "x" and "|": miette has an ASCII theme it selects when it cannot detect
#    unicode support, which is what a Windows console on codepage 850/437 (the
#    environment instructions/SETUP.md documents) is. NOT reproduced here -- on
#    Linux the bullet stayed U+00D7 under LC_ALL=C, LANG=C, TERM=dumb and
#    NO_COLOR=1 -- so this is a cheap precaution against an unverified
#    Windows-only rendering, and the cost of being wrong about it is that
#    `database is busy` raises instead of backing off.
#  - the wrap glyphs, because a long contention message can push the phrase itself
#    onto the continuation line, where a bullet-only match misses it.
# Kept anchored: only a line-initial glyph-plus-space or "Error:" counts, so the
# echoed SQL of a miette caret diagram (" 1 │ SELECT ...", number first) and
# ordinary row bodies do not read as diagnostics.
ERR_LINE_RE = re.compile(r"^\s*(?:[×x]\s|[│|]\s|Error:)")


def _is_busy(stderr, stdout):
    """True only when tursodb itself reported contention, never for row/SQL text."""
    if BUSY_RE.search(stderr):
        return True
    return any(BUSY_RE.search(ln) for ln in stdout.splitlines() if ERR_LINE_RE.match(ln))


def _stdout_reports_failure(stdout):
    """True when stdout carries a tursodb diagnostic AND NOTHING ELSE.

    Needed because tursodb puts SQL-level errors on stdout and leaves stderr
    EMPTY (verified on 0.7.1 and 0.7.2), so for those `exec_sql` would otherwise
    be leaning on the exit status alone.

    Deliberately structural rather than textual. A diagnostic is plain text and
    this corpus stores tursodb's error messages as memories, so a SUCCESSFUL read
    can print lines that are byte-identical to a real diagnostic -- verified by
    storing a row quoting "  × Parse error: ..." and "Error: Runtime error:
    database is busy (5)" and reading it back: rc=0, empty stderr, both lines in
    stdout. What separates the two is not the line, it is everything around it: a
    failed statement prints its diagnostic and no rows, a successful write prints
    nothing at all, and a successful read prints at least one line that is not
    diagnostic-shaped (in the default -m line mode, always "<column> = ..."
    first). So: at least one line, and every non-empty line a diagnostic.

    Conservative on purpose -- it does not fire on a miette caret diagram (a
    syntax error's frame lines are not diagnostic-shaped), which the exit status
    covers on both versions tested.
    """
    lines = [ln for ln in stdout.splitlines() if ln.strip()]
    return bool(lines) and all(ERR_LINE_RE.match(ln) for ln in lines)


def exec_sql(sql, mode="line"):
    """Run one statement via tursodb.

    A failure is a non-zero exit, an error on stderr, or a stdout that holds a
    tursodb diagnostic and nothing else (see _stdout_reports_failure -- tursodb
    reports SQL-level errors on stdout with an EMPTY stderr). Of those failures
    only a `database is busy/locked` phrase retries, with backoff; everything else
    raises RuntimeError.
    """
    last = ""
    for attempt in range(6):
        r = subprocess.run([TURSO, DB, *OPEN_ARGS, "-q", "-m", mode, sql],
                           capture_output=True, text=True,
                           encoding="utf-8", errors="replace")
        # All three signals, because none covers the others: SQL-level failures put
        # their diagnostic on stdout with an empty stderr, while open/CLI failures
        # put it on stderr with an empty stdout. Both currently also exit non-zero
        # (0.7.1 and 0.7.2 -- including the reserved-name parse error recorded in
        # scripts/restore.py, which exits 1 as an argv statement and piped alike),
        # so the stdout arm is what would catch an exit-0 report rather than hand
        # the error text back as a successful result.
        failed = (r.returncode != 0
                  or bool(re.search(r"error", r.stderr, re.I))
                  or _stdout_reports_failure(r.stdout))
        # The busy/locked probe reads stdout only ONCE the run has failed, and only
        # its diagnostic lines. Scanning a successful run's stdout would let a row
        # body containing "database is busy" (a memory this corpus invites) re-run
        # a write that had already landed. Retrying is safe however the failure was
        # detected, exit code or not: contention is reported *instead of* running
        # the statement, so there is no landed write for the retry to duplicate.
        if failed:
            last = r.stderr.strip() or r.stdout.strip() or "unknown tursodb error"
        if _is_busy(r.stderr, r.stdout if failed else ""):
            time.sleep(0.3 * (attempt + 1))
            continue
        if failed:
            raise RuntimeError(last)
        return r.stdout
    # Report what tursodb actually said -- a bare "busy after retries" hides
    # which statement lost which lock.
    raise RuntimeError(f"database busy after retries: {last}")


def scalar(sql):
    out = exec_sql(sql, mode="list").strip()
    return out.splitlines()[0].strip() if out else ""


def resolve_coworker(name):
    cid = scalar(f"SELECT id FROM coworkers WHERE name={q(name)};")
    if not cid:
        sys.exit(f"refused: no coworker named '{name}'.")
    return int(cid)
