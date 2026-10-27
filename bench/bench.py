#!/usr/bin/env python3
"""Benchmark the binary against the Python reference, on the same workloads.

    cargo build --release                # the binary measured: host architecture only
    cargo build --release --example bench-measure   # the measuring launcher
    python3 bench/bench.py               # every workload, report on stdout
    python3 bench/bench.py --out report.md --only status,recall-query

Run on demand, never from `cargo test`. Every run happens on a throwaway
machine under a temporary directory: its own HOME, XDG and Claude config
directories, a `.env` beside a copy of the binary, a backup directory, and an
explicit allow-list of environment variables for every child, so nothing of
the real machine leaks in and nothing is written outside it. Ollama is a fake
one inside this process, on an ephemeral port, answering with the same
deterministic vectors as the end-to-end suite (`tests/e2e/vectors.rs`); the
keyword-only workloads point at a port that refuses. A real Ollama is never
contacted.

Each workload runs N times per implementation on copies of the same fixture,
alternating which side goes first; the first run of each side is a warm-up and
is discarded. Measured per run: wall-clock time (monotonic, spawn to exit), user
and system CPU time and peak resident memory. On Linux and macOS they come from
the child's own resource usage, collected by `bench/measure.rs`, a small native
launcher (wait4/getrusage of the children it reaps): measured from this
interpreter directly, a child's peak memory would read as at least the
interpreter's own, because exec keeps the spawning process's high-water mark.
The reference's CPU and memory figures include the tursodb processes it starts
and reaps (CPU summed, memory the largest of them), because that is what
running it costs. On Windows only the wall time and the working-set peak are
reported, and the CPU figures say they are unavailable (this branch is not
verified).

Without the tursodb CLI the reference cannot run: its side of every workload is
skipped and named in the report. Nothing is ever installed.

    python3 bench/bench.py --self-test   # the statistics and the fixture builder
    python3 bench/bench.py --vector TEXT # the fake Ollama's vector, as JSON
"""
import argparse
import datetime
import json
import os
import platform
import re
import shutil
import sqlite3
import struct
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

sys.dont_write_bytecode = True

REPO = Path(__file__).resolve().parent.parent
DIM = 1024
M64 = (1 << 64) - 1
PATH = "/usr/local/bin:/usr/bin:/bin"
BACKUP_ERR_LINE = "ERR=/tmp/supercharged-memory-backup.err"

# --------------------------------------------------------------------------
# The fake Ollama's vectors: the same function as tests/e2e/vectors.rs.


def fnv1a(data):
    h = 0xCBF29CE484222325
    for b in data:
        h = ((h ^ b) * 0x100000001B3) & M64
    return h


_WORDS = {}


def word(token):
    """The -1/0/1 vector of one word: two bits per component, 32 per draw."""
    if token in _WORDS:
        return _WORDS[token]
    state, out = fnv1a(token), []
    for _ in range(DIM // 32):
        state = (state + 0x9E3779B97F4A7C15) & M64
        z = state
        z = ((z ^ (z >> 30)) * 0xBF58476D1CE4E5B9) & M64
        z = ((z ^ (z >> 27)) * 0x94D049BB133111EB) & M64
        z ^= z >> 31
        out.extend((-1, 0, 1, 0)[(z >> (2 * j)) & 3] for j in range(32))
    _WORDS[token] = out
    return out


def vector(text):
    """Bag of words over runs of ASCII letters and digits, lowercased."""
    tokens = re.findall(rb"[a-z0-9]+", text.encode("utf-8").lower())
    if not tokens:
        return [1] + [0] * (DIM - 1)
    v = [0] * DIM
    for t in tokens:
        v = [a + b for a, b in zip(v, word(t))]
    return v


def blob(text):
    return struct.pack(f"<{DIM}f", *vector(text))


# --------------------------------------------------------------------------
# The fake Ollama itself.

MODELS = ("bge-m3", "other-model")


class _Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def _send(self, code, obj):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == "/api/tags":
            self._send(200, {"models": [{"name": f"{m}:latest"} for m in MODELS]})
        elif self.path == "/api/version":
            self._send(200, {"version": "0.0.0-fake"})
        else:
            self._send(404, {"error": "not found"})

    def do_POST(self):
        if self.path != "/api/embed":
            return self._send(404, {"error": "not found"})
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))))
        model = str(body.get("model", "")).removesuffix(":latest")
        if model not in MODELS:
            return self._send(404, {"error": f'model "{model}" not found, try pulling it first'})
        self._send(200, {"embeddings": [[float(x) for x in vector(body["input"])]]})


def start_ollama():
    server = ThreadingHTTPServer(("127.0.0.1", 0), _Handler)
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, f"http://127.0.0.1:{server.server_address[1]}"


def refusing_url():
    """A port the system handed out and that was closed again at once."""
    import socket
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return f"http://127.0.0.1:{port}"


# --------------------------------------------------------------------------
# Fixtures.


def strip_sql_comments(sql):
    """Drop `--` comments outside string literals. sqlite3 keeps a CREATE
    statement's text verbatim, and the reference's restore.py splits a dump at
    every `;` outside quotes -- including one inside such a comment."""
    out, i, n, quoted = [], 0, len(sql), False
    while i < n:
        c = sql[i]
        if quoted:
            out.append(c)
            if c == "'":
                quoted = False
        elif c == "'":
            quoted = True
            out.append(c)
        elif sql.startswith("--", i):
            while i < n and sql[i] != "\n":
                i += 1
            continue
        else:
            out.append(c)
        i += 1
    return "".join(out)


VOCAB = (
    "turso wal lock reader writer busy retry backoff checkpoint dump restore backup "
    "retention weekly daily embedding vector ollama model dimension cosine distance "
    "recall ranking keyword idf token query session baseline topic index sleep "
    "episodic semantic supersede retire purge cluster threshold contradiction stale "
    "verify artifact flag path version binary script python rust cargo clippy fmt "
    "test suite gate refusal exit code prefix stderr stdout runbook template init "
    "update migration schema table column row text cap keywords coworker trust "
    "appraisal feedback instruction release deploy tag commit push branch merge "
    "review finding severity handover counts benchmark host median spread memory "
    "corpus fixture deterministic machine environment allow list home config "
    "directory file symlink unreadable empty probe import export gzip literal "
    "statement parser transaction rollback commit wal frame page header engine"
).split()


def splitmix_stream(seed):
    state = seed & M64
    while True:
        state = (state + 0x9E3779B97F4A7C15) & M64
        z = state
        z = ((z ^ (z >> 30)) * 0xBF58476D1CE4E5B9) & M64
        z = ((z ^ (z >> 27)) * 0x94D049BB133111EB) & M64
        yield z ^ (z >> 31)


def generated_texts(n, seed):
    """n deterministic texts of 12 to 23 words from VOCAB."""
    rnd = splitmix_stream(seed)
    for _ in range(n):
        k = 12 + next(rnd) % 12
        words = [VOCAB[next(rnd) % len(VOCAB)] for _ in range(k)]
        yield " ".join(words).capitalize() + "."


def build_fixture(path, embed_all=False, semantic=0, episodic=0, unembedded=0):
    """schema.sql + tests/e2e/fixture.sql, then generated rows, then the
    vectors, then `unembedded` generated semantic rows without one."""
    path = Path(path)
    for suffix in ("", "-wal", "-shm", "-tshm"):
        Path(str(path) + suffix).unlink(missing_ok=True)
    con = sqlite3.connect(path)
    con.execute("PRAGMA journal_mode=WAL")
    con.executescript(strip_sql_comments((REPO / "schema.sql").read_text(encoding="utf-8")))
    con.executescript((REPO / "tests/e2e/fixture.sql").read_text(encoding="utf-8"))
    day = datetime.datetime(2025, 1, 1)
    for i, text in enumerate(generated_texts(semantic, 1)):
        con.execute(
            "INSERT INTO semantic_memory (created_at, updated_at, project, topic, category, source, "
            "model, embed_model, memory_text) VALUES (?, ?, ?, ?, ?, 'generated', 'bench', 'bge-m3', ?)",
            ((day + datetime.timedelta(minutes=i)).strftime("%Y-%m-%d %H:%M:%S"),) * 2
            + (f"proj-{i % 10}", text.split()[0].lower(), ("reference", "project", "user", "feedback")[i % 4], text),
        )
    for i, text in enumerate(generated_texts(episodic, 2)):
        con.execute(
            "INSERT INTO episodic_memory (created_at, project, topic, event_type, importance, source, "
            "model, embed_model, memory_text) VALUES (?, ?, ?, 'note', 'routine', 'generated', 'bench', 'bge-m3', ?)",
            ((day + datetime.timedelta(minutes=i)).strftime("%Y-%m-%d %H:%M:%S"),
             f"proj-{i % 10}", text.split()[0].lower(), text),
        )
    only = "" if embed_all else " WHERE embed_model IS NOT NULL"
    for table in ("semantic_memory", "episodic_memory", "instructions"):
        rows = con.execute(f"SELECT id, memory_text FROM {table}{only}").fetchall()
        con.executemany(
            f"UPDATE {table} SET embedding = ?, embed_model = 'bge-m3' WHERE id = ?",
            [(blob(text), rid) for rid, text in rows],
        )
    for i, text in enumerate(generated_texts(unembedded, 3)):
        con.execute(
            "INSERT INTO semantic_memory (created_at, updated_at, topic, category, source, model, memory_text) "
            "VALUES (?, ?, ?, 'reference', 'generated', 'bench', ?)",
            ((day + datetime.timedelta(minutes=i)).strftime("%Y-%m-%d %H:%M:%S"),) * 2
            + (f"unembedded-{i}", f"{text} Row {i}."),
        )
    con.commit()
    con.close()
    return path


def counts(path):
    con = sqlite3.connect(path)
    try:
        return {t: con.execute(f"SELECT count(*) FROM {t}").fetchone()[0]
                for t in ("semantic_memory", "episodic_memory", "instructions")}
    finally:
        con.close()


# --------------------------------------------------------------------------
# Machines and measuring.


class Machine:
    """One side's throwaway machine: directories, a database, and for the
    binary a copy of it with its `.env`."""

    def __init__(self, base, name, side, binary, ollama, dead, tursodb):
        self.root = Path(base) / name
        self.side, self.binary, self.ollama, self.dead, self.tursodb = side, binary, ollama, dead, tursodb
        for d in ("home", "data/turso", "config", "claude", "backups", "tmp", "app", "tools"):
            (self.root / d).mkdir(parents=True, exist_ok=True)
        self.db = self.root / "data/turso/supercharged-memory.db"
        if side == "binary":
            exe = self.exe()
            try:
                os.link(binary, exe)
            except OSError:
                shutil.copy2(binary, exe)
            (self.root / "app/.env").write_text(
                f"SUPERCHARGED_MEMORY_TURSO_PATH={self.db}\nBACKUP_DIR={self.root / 'backups'}\n"
                f"OLLAMA_URL={ollama}\n", encoding="utf-8")
        else:
            script = (REPO / "scripts/supercharged-memory-backup.sh").read_text(encoding="utf-8")
            if script.count(BACKUP_ERR_LINE) != 1:
                sys.exit("the reference backup script no longer writes its error file where expected")
            (self.root / "tools/supercharged-memory-backup.sh").write_text(
                script.replace(BACKUP_ERR_LINE, 'ERR="$TMPDIR/supercharged-memory-backup.err"'),
                encoding="utf-8")

    def exe(self):
        return self.root / "app" / ("supercharged-memory.exe" if os.name == "nt" else "supercharged-memory")

    def use(self, fixture):
        """A fresh byte copy of a fixture as the database."""
        for suffix in ("", "-wal", "-shm", "-tshm"):
            Path(str(self.db) + suffix).unlink(missing_ok=True)
        if fixture is not None:
            shutil.copyfile(fixture, self.db)

    def env(self, refused=False, extra=None):
        at = lambda rel: str(self.root / rel)  # noqa: E731
        env = {
            "HOME": at("home"), "XDG_DATA_HOME": at("data"), "XDG_CONFIG_HOME": at("config"),
            "CLAUDE_CONFIG_DIR": at("claude"), "TMPDIR": at("tmp"), "PATH": PATH,
            "LANG": "C.UTF-8", "TZ": "UTC", "OLLAMA_URL": self.dead if refused else self.ollama,
            "SUPERCHARGED_MEMORY_TURSO_PATH": str(self.db), "BACKUP_DIR": at("backups"),
        }
        if self.side == "reference":
            env["TURSO_BIN"] = str(self.tursodb)
            env["PYTHONDONTWRITEBYTECODE"] = "1"
        env.update(extra or {})
        return env

    def argv(self, args):
        args = [a.replace("{ROOT}", str(self.root)) for a in args]
        if self.side == "binary":
            return [str(self.exe())] + args
        sub, rest = args[0], args[1:]
        if sub == "backup":
            return ["bash", str(self.root / "tools/supercharged-memory-backup.sh")]
        if sub == "python-startup":
            return [sys.executable, "-c", "pass"]
        script = {
            "status": "scripts/recall.py", "recall": "scripts/recall.py",
            "remember": "scripts/remember.py", "sleep": "scripts/sleep.py",
            "coworkers": "scripts/coworkers.py", "eval": "investigations/eval-harness.py",
            "restore": "scripts/restore.py", "backfill": "scripts/backfill.py",
            "find-existing-memory": "scripts/find-existing-memory.py",
        }[sub]
        return [sys.executable, "-B", str(REPO / script)] + (["--status"] if sub == "status" else rest)


def _peak_working_set_kib(handle):
    import ctypes
    from ctypes import wintypes

    class Counters(ctypes.Structure):
        _fields_ = [("cb", wintypes.DWORD), ("PageFaultCount", wintypes.DWORD)] + [
            (n, ctypes.c_size_t) for n in (
                "PeakWorkingSetSize", "WorkingSetSize", "QuotaPeakPagedPoolUsage",
                "QuotaPagedPoolUsage", "QuotaPeakNonPagedPoolUsage", "QuotaNonPagedPoolUsage",
                "PagefileUsage", "PeakPagefileUsage")]
    c = Counters()
    c.cb = ctypes.sizeof(Counters)
    ctypes.windll.psapi.GetProcessMemoryInfo(int(handle), ctypes.byref(c), c.cb)
    return c.PeakWorkingSetSize / 1024


def measure(argv, env, cwd, stdin=None, launcher=None):
    """One run: (wall s, user s or None, sys s or None, peak RSS KiB or None,
    exit code, stderr text). Output goes to files, so a talkative child can
    never block on a full pipe.

    With `launcher` (bench/measure.rs) the command is started and measured by
    that small native process: spawned from this interpreter directly, a
    child's ru_maxrss is at least the interpreter's own peak, because exec
    keeps the spawning process's memory high-water mark."""
    out_dir = Path(cwd) / "tmp"
    out_path, err_path = out_dir / "bench.out", out_dir / "bench.err"
    with open(out_path, "wb") as out, open(err_path, "wb") as err:
        stdin_f = None
        if stdin is not None:
            (out_dir / "bench.in").write_text(stdin, encoding="utf-8")
            stdin_f = open(out_dir / "bench.in", "rb")
        try:
            if launcher is not None:
                report = out_dir / "bench.json"
                report.unlink(missing_ok=True)
                done = subprocess.run([str(launcher), str(report), *argv], env=env, cwd=cwd, stdout=out,
                                      stderr=err, stdin=stdin_f or subprocess.DEVNULL)
                if done.returncode != 0:
                    raise RuntimeError(f"the measuring launcher failed ({done.returncode}): {err_path.read_text()}")
                r = json.loads(report.read_text(encoding="utf-8"))
                return (r["wall"], r["user"], r["sys"], r["rss_kib"], r["code"],
                        err_path.read_text(encoding="utf-8", errors="replace"))
            start = time.perf_counter()
            p = subprocess.Popen(argv, env=env, cwd=cwd, stdout=out, stderr=err,
                                 stdin=stdin_f or subprocess.DEVNULL)
            if hasattr(os, "wait4"):
                _, status, ru = os.wait4(p.pid, 0)
                wall = time.perf_counter() - start
                p.returncode = os.waitstatus_to_exitcode(status)
                rss = ru.ru_maxrss / (1024 if sys.platform == "darwin" else 1)
                user, system = ru.ru_utime, ru.ru_stime
            else:
                p.wait()
                wall = time.perf_counter() - start
                user = system = None
                try:
                    rss = _peak_working_set_kib(p._handle)
                except Exception:
                    rss = None
        finally:
            if stdin_f:
                stdin_f.close()
    return wall, user, system, rss, p.returncode, err_path.read_text(encoding="utf-8", errors="replace")


# --------------------------------------------------------------------------
# Statistics and the report.


def percentile(xs, p):
    """Linear interpolation between the closest ranks."""
    xs = sorted(xs)
    if not xs:
        return None
    k = (len(xs) - 1) * p / 100
    lo = int(k)
    hi = min(lo + 1, len(xs) - 1)
    return xs[lo] + (xs[hi] - xs[lo]) * (k - lo)


def summary(xs):
    xs = [x for x in xs if x is not None]
    if not xs:
        return None
    return {"n": len(xs), "median": percentile(xs, 50), "p95": percentile(xs, 95),
            "min": min(xs), "max": max(xs)}


MEASURES = (("wall", "wall ms", 1000), ("user", "user CPU ms", 1000),
            ("sys", "system CPU ms", 1000), ("rss", "peak RSS MiB", 1 / 1024))


def fmt(x, digits=1):
    return "n/a" if x is None else f"{x:,.{digits}f}"


def ratio(a, b):
    if a is None or b is None or b == 0:
        return "n/a"
    return f"{a / b:.2f}"


def _stat(samples, key, scale):
    s = summary([x[key] for x in samples])
    return None if s is None else {k: (v * scale if k != "n" else v) for k, v in s.items()}


def render(results, host, sizes, fixtures, method):
    lines = ["# Benchmark: the binary against the Python reference", ""]
    lines += ["Ratios are binary / reference on the median: below 1.00 the binary is faster or smaller.", ""]
    lines += ["## Host", "", "| | |", "|---|---|"]
    lines += [f"| {k} | {v} |" for k, v in host.items()]
    lines += ["", "## Sizes", "", "| | |", "|---|---|"]
    lines += [f"| {k} | {v} |" for k, v in sizes.items()]
    lines += ["", "## Fixtures", "", "| | |", "|---|---|"]
    lines += [f"| {k} | {v} |" for k, v in fixtures.items()]
    lines += ["", "## Method", ""] + [f"- {m}" for m in method]
    lines += ["", "## Summary", "",
              "| workload | samples (binary + reference) | wall median binary | wall median reference | wall ratio "
              "| RSS median binary | RSS median reference | RSS ratio |",
              "|---|---|---|---|---|---|---|---|"]
    for r in results:
        b = _stat(r["binary"], "wall", 1000)
        p = _stat(r["reference"], "wall", 1000) if r["reference"] else None
        br = _stat(r["binary"], "rss", 1 / 1024)
        pr = _stat(r["reference"], "rss", 1 / 1024) if r["reference"] else None
        lines.append(
            f"| {r['name']} | {len(r['binary'])} + {len(r['reference'])} | {fmt(b and b['median'])} ms "
            f"| {fmt(p and p['median'])}{' ms' if p else ''} | {ratio(b and b['median'], p and p['median'])} "
            f"| {fmt(br and br['median'])} MiB | {fmt(pr and pr['median'])}{' MiB' if pr else ''} "
            f"| {ratio(br and br['median'], pr and pr['median'])} |")
    lines += ["", "## Per workload", ""]
    for r in results:
        lines += [f"### {r['name']}", "", f"binary: `{r['binary_cmd']}`  ",
                  f"reference: {('`' + r['reference_cmd'] + '`') if r['reference_cmd'] else r['no_reference']}  ",
                  f"samples: {len(r['binary'])} binary, {len(r['reference'])} reference (after one warm-up each)", "",
                  "| measure | binary median | binary p95 | binary spread | reference median | reference p95 "
                  "| reference spread | ratio |", "|---|---|---|---|---|---|---|---|"]
        for key, label, scale in MEASURES:
            b = _stat(r["binary"], key, scale)
            p = _stat(r["reference"], key, scale) if r["reference"] else None
            spread = lambda s: "n/a" if s is None else f"{fmt(s['min'])} – {fmt(s['max'])}"  # noqa: E731
            lines.append(
                f"| {label} | {fmt(b and b['median'])} | {fmt(b and b['p95'])} | {spread(b)} "
                f"| {fmt(p and p['median'])} | {fmt(p and p['p95'])} | {spread(p)} "
                f"| {ratio(b and b['median'], p and p['median'])} |")
        lines.append("")
    compared = [r for r in results if r["reference"]]
    faster = [r["name"] for r in compared
              if _stat(r["binary"], "wall", 1)["median"] < _stat(r["reference"], "wall", 1)["median"]]
    slower = [r["name"] for r in compared if r["name"] not in faster]
    lines += ["## Reading, mechanically", "",
              f"- compared workloads: {len(compared)}; the binary's median wall time is lower on "
              f"{len(faster)} and higher on {len(slower)}" + (f" ({', '.join(slower)})" if slower else ""),
              ]
    with_rss = [r for r in compared if _stat(r["binary"], "rss", 1) and _stat(r["reference"], "rss", 1)]
    smaller = [r["name"] for r in with_rss
               if _stat(r["binary"], "rss", 1)["median"] < _stat(r["reference"], "rss", 1)["median"]]
    lines.append(f"- peak memory: lower on {len(smaller)} of {len(with_rss)}"
                 + (f" (higher on {', '.join(r['name'] for r in with_rss if r['name'] not in smaller)})"
                    if len(smaller) < len(with_rss) else ""))
    skipped = [r["name"] for r in results if not r["reference"] and r["reference_cmd"]]
    if skipped:
        lines.append(f"- SKIPPED reference side: {', '.join(skipped)}")
    return "\n".join(lines) + "\n"


def first_line(argv, env=None):
    try:
        out = subprocess.run(argv, capture_output=True, text=True, timeout=30, env=env or {"PATH": PATH})
        return (out.stdout or out.stderr).strip().splitlines()[0]
    except (OSError, IndexError, subprocess.SubprocessError):
        return None


def host_info(binary, tursodb):
    cpu = platform.processor() or platform.machine()
    try:
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.startswith("model name"):
                cpu = line.split(":", 1)[1].strip()
                break
    except OSError:
        pass
    ram = "unknown"
    try:
        kib = next(int(l.split()[1]) for l in Path("/proc/meminfo").read_text().splitlines()
                   if l.startswith("MemTotal:"))
        ram = f"{kib / 1024 / 1024:.1f} GiB"
    except (OSError, StopIteration):
        if hasattr(os, "sysconf"):
            try:
                ram = f"{os.sysconf('SC_PAGE_SIZE') * os.sysconf('SC_PHYS_PAGES') / 2**30:.1f} GiB"
            except (ValueError, OSError):
                pass
    os_name = platform.platform()
    try:
        pretty = next(l.split("=", 1)[1].strip('"') for l in Path("/etc/os-release").read_text().splitlines()
                      if l.startswith("PRETTY_NAME="))
        os_name = f"{pretty}; {os_name}"
    except (OSError, StopIteration):
        pass
    lock = (REPO / "Cargo.lock").read_text(encoding="utf-8")
    turso = re.search(r'name = "turso"\nversion = "([^"]+)"', lock)
    sha = first_line(["git", "-C", str(REPO), "rev-parse", "--short", "HEAD"], os.environ) or "unknown"
    dirty = subprocess.run(["git", "-C", str(REPO), "status", "--porcelain", "--untracked-files=no"],
                           capture_output=True, text=True).stdout.strip()
    return {
        "CPU": cpu,
        "cores": os.cpu_count(),
        "RAM": ram,
        "OS / kernel": os_name,
        "Python (reference)": f"{platform.python_version()} ({sys.executable})",
        "Rust": first_line(["rustc", "--version"], os.environ) or "rustc not on PATH",
        "Turso": f"binary links turso {turso.group(1) if turso else 'unknown'}; "
                 f"the reference drives {first_line([str(tursodb), '--version']) if tursodb else 'no tursodb'}",
        "binary measured": f"{binary} ({first_line([str(binary), '--version'])})",
        "code": f"{sha}{' with uncommitted changes to tracked files' if dirty else ''}",
        "load average at start": " ".join(f"{x:.2f}" for x in os.getloadavg()) if hasattr(os, "getloadavg")
        else "unavailable",
    }


# --------------------------------------------------------------------------
# Workloads.


class Workload:
    def __init__(self, name, args, *, reference=True, fixture="embedded", fresh=False, refused=False,
                 stdin=None, expect=0, setup=None, runs="small", why=None, env=None):
        self.name, self.args, self.reference, self.fixture, self.fresh = name, args, reference, fixture, fresh
        self.refused, self.stdin, self.expect, self.setup, self.runs = refused, stdin, expect, setup, runs
        self.why, self.env = why, env


def _tree(m):
    c = m.root / "claude"
    for rel, text in (("CLAUDE.md", "# My notes\n\nAlways answer briefly.\n"),
                      ("memory/MEMORY.md", "- [a](a.md) one fact\n"),
                      ("memory/a.md", "A fact worth keeping.\n"), ("memory/team/B.MD", "A shared fact.\n"),
                      ("memory/big.md", "y" * 1850),
                      ("projects/-home-user-repo/memory/p.md", "A project fact.\n")):
        (c / rel).parent.mkdir(parents=True, exist_ok=True)
        (c / rel).write_text(text, encoding="utf-8")


def _notes(m):
    d = m.root / "notes"
    for rel, text in (("wal-notes.md", "Readers never block the multiprocess writer in tursodb.\n"),
                      ("sub/retry_policy.md", "Retry three times with a growing backoff.\n"),
                      ("big.md", "z" * 2100), ("other.md", "Benchmarks name their host and sample counts.\n")):
        (d / rel).parent.mkdir(parents=True, exist_ok=True)
        (d / rel).write_text(text, encoding="utf-8")


def _empty_backups(m):
    shutil.rmtree(m.root / "backups", ignore_errors=True)
    (m.root / "backups").mkdir()


def _restore_target(m):
    for suffix in ("", "-wal", "-shm", "-tshm"):
        Path(str(m.root / "restored.db") + suffix).unlink(missing_ok=True)


def _fresh_dump(m):
    """A dump this side made, copied in after the database copy, so it is
    newer than the database: the purge gate's precondition."""
    _empty_backups(m)
    shutil.copyfile(m.saved_dump, m.root / "backups" / m.saved_dump.name)


def _bare(m):
    (m.root / "app/.env").unlink(missing_ok=True)


def _versioned(m):
    with open(m.root / "app/.env", "a", encoding="utf-8") as f:
        f.write("SUPERCHARGED_MEMORY_VERSION=0.1.0\n")


def _versioned_once(m):
    env = (m.root / "app/.env").read_text(encoding="utf-8")
    if "SUPERCHARGED_MEMORY_VERSION" not in env:
        _versioned(m)


TOPICS = json.dumps([{"topic": "turso", "keywords": "wal, busy"}, {"topic": "rust", "keywords": "clippy, fmt"}])

REMEMBER = ["--source", "self-observed", "--model", "bench"]

WORKLOADS = [
    Workload("startup", ["--version"], why="process start: the binary's --version against `python3 -c pass`"),
    Workload("status", ["status"]),
    Workload("status-degraded", ["status"], refused=True),
    Workload("config", ["config"], reference=False, why="no reference: configuration was environment variables"),
    Workload("recall-baseline", ["recall", "--baseline"]),
    Workload("recall-topics", ["recall", "--topics"]),
    Workload("recall-count", ["recall", "--count"]),
    Workload("recall-query", ["recall", "wal lock reader"]),
    Workload("recall-project", ["recall", "turso wal checkpoint", "--project", "proj-alpha"]),
    Workload("recall-keyword-only", ["recall", "wal lock reader"], refused=True),
    Workload("recall-large", ["recall", "checkpoint wal reader lock"], fixture="large", runs="large"),
    Workload("recall-large-keyword-only", ["recall", "checkpoint wal reader lock"], fixture="large",
             runs="large", refused=True),
    Workload("remember-semantic", ["remember", "--table", "semantic", "--topic", "bench", "--category", "reference",
                                   "--keywords", "gamma, delta", "--text",
                                   "A brand new fact about gamma rays and delta waves."] + REMEMBER, fresh=True),
    Workload("remember-episodic", ["remember", "--table", "episodic", "--topic", "bench", "--event-type", "note",
                                   "--importance", "routine", "--text", "An event about zeta functions."] + REMEMBER,
             fresh=True),
    Workload("remember-without-ollama", ["remember", "--table", "semantic", "--topic", "bench", "--category",
                                         "reference", "--text", "A fact stored while Ollama is down."] + REMEMBER,
             fresh=True, refused=True, reference=False,
             why="no reference: the reference refuses to store without Ollama"),
    Workload("embed-missing", ["embed", "--missing"], reference=False, fixture="unembedded", fresh=True,
             runs="embed", why="no reference: rows without a vector did not exist there"),
    Workload("find-existing-memory", ["find-existing-memory"], fixture=None, setup=_tree),
    Workload("init-dry-run", ["init", "--db-path", "{ROOT}/data/turso/supercharged-memory.db", "--backup-dir",
                              "{ROOT}/backups", "--episodic-mode", "major-actions"], reference=False, fixture=None,
             setup=_bare, why="no reference: replaced the checkout's installer"),
    Workload("update-dry-run", ["update"], reference=False, setup=_versioned_once,
             why="no reference: replaced the checkout's installer"),
    Workload("backup", ["backup"], setup=_empty_backups),
    Workload("restore", ["restore", "--out", "{ROOT}/restored.db"], setup=_restore_target),
    Workload("sleep-mark-processed", ["sleep", "--mark-processed", "3,4"], fresh=True),
    Workload("sleep-retire", ["sleep", "--retire", "14"], fresh=True),
    Workload("sleep-rebuild-topics", ["sleep", "--rebuild-topics"], fresh=True, stdin=TOPICS),
    Workload("sleep-cluster", ["sleep", "--cluster"]),
    Workload("sleep-staleness", ["sleep", "--staleness"]),
    Workload("sleep-contradictions", ["sleep", "--contradiction-candidates"]),
    Workload("sleep-verify", ["sleep", "--verify-candidates"]),
    Workload("sleep-purge", ["sleep", "--purge", "4,6", "--confirm-purge"], fresh=True, setup=_fresh_dump),
    Workload("coworkers-add", ["coworkers", "--add", "--name", "cy", "--expertise", "test design",
                               "--personality", "calm"], fresh=True),
    Workload("coworkers-appraise", ["coworkers", "--appraise", "ada", "--trust", "autonomous", "--text",
                                    "Third review: autonomous on recovery work."], fresh=True),
    Workload("coworkers-load", ["coworkers", "--load", "ada"], reference=False,
             why="no reference: a coworker was loaded with ad-hoc SQL"),
    Workload("eval-validate", ["eval", "--validate"], expect=1),
    Workload("eval-sweep", ["eval", "--sweep", "0.0,0.15,0.3"]),
    Workload("eval-variants", ["eval", "--variants"]),
    Workload("eval-report", ["eval", "--report"], fresh=True),
    Workload("instruct", ["instruct", "--topic", "deploy", "--source", "user-stated", "--text",
                          "Deploy only from a tagged commit, after the full suite passed."], reference=False,
             fresh=True, why="no reference: the instructions table is new"),
    Workload("instructions-search", ["instructions", "review findings severity"], reference=False,
             why="no reference: the instructions table is new"),
    Workload("runbook", ["runbook", "deep-sleep"], reference=False, fixture=None,
             why="no reference: the runbooks were files in the checkout"),
    Workload("backfill", ["backfill", "--dir", "{ROOT}/notes"], fresh=True, setup=_notes),
]


def run_workload(w, base, binary, fixtures, ollama, dead, tursodb, runs, progress, launcher):
    fixture = fixtures.get(w.fixture) if w.fixture else None
    sides = ["binary"] + (["reference"] if w.reference and tursodb else [])
    machines = {s: Machine(base, f"{w.name}-{s}", s, binary, ollama, dead, tursodb) for s in sides}
    for m in machines.values():
        m.use(fixture)
        if w.name == "sleep-purge":
            # Each side purges behind a dump of its own making.
            _empty_backups(m)
            wall, user, system, rss, code, err = measure(m.argv(["backup"]), m.env(), m.root)
            if code != 0:
                sys.exit(f"{w.name}: the {m.side}'s backup failed: {err}")
            made = next((m.root / "backups").iterdir())
            m.saved_dump = m.root / "tmp" / made.name
            shutil.move(str(made), m.saved_dump)
        if w.name == "restore":
            _empty_backups(m)
            wall, user, system, rss, code, err = measure(m.argv(["backup"]), m.env(), m.root)
            if code != 0:
                sys.exit(f"{w.name}: the {m.side}'s backup failed: {err}")
    samples = {s: [] for s in sides}
    for i in range(runs):
        order = sides if i % 2 == 0 else list(reversed(sides))
        for s in order:
            m = machines[s]
            if w.fresh:
                m.use(fixture)
            if w.setup:
                w.setup(m)
            args = w.args if not (s == "reference" and w.name == "startup") else ["python-startup"]
            wall, user, system, rss, code, err = measure(m.argv(args), m.env(w.refused, w.env), m.root, w.stdin,
                                                         launcher)
            if code != w.expect:
                sys.exit(f"{w.name}: the {s} exited {code}, expected {w.expect}:\n{err[-2000:]}")
            if i > 0:  # the first run of each side is the warm-up
                samples[s].append({"wall": wall, "user": user, "sys": system, "rss": rss})
        progress(f"{w.name}: run {i + 1}/{runs}")
    shown = lambda m, args: " ".join(  # noqa: E731
        Path(a).name if i == 0 else a for i, a in enumerate(m.argv(args))
    ).replace(f"{REPO}{os.sep}", "").replace(str(m.root), "{ROOT}")
    reference_cmd = None
    if w.reference:
        probe = Machine(base, f"{w.name}-shown", "reference", binary, ollama, dead, tursodb)
        reference_cmd = shown(probe, ["python-startup"] if w.name == "startup" else w.args)
    return {
        "name": w.name,
        "binary": samples["binary"],
        "reference": samples.get("reference", []),
        "binary_cmd": shown(machines["binary"], w.args),
        "reference_cmd": reference_cmd,
        "no_reference": w.why or ("SKIPPED: tursodb not found" if w.reference else "none"),
    }


def find_tursodb():
    for candidate in (os.environ.get("TURSO_BIN"), str(Path.home() / ".turso/tursodb"), shutil.which("tursodb")):
        if candidate and Path(candidate).is_file():
            return Path(candidate)
    return None


def sizes_of(binary, tursodb):
    scripts = sorted([*(REPO / "scripts").glob("*.py"), *(REPO / "scripts").glob("*.sh"),
                      REPO / "investigations/eval-harness.py"])
    kib = lambda n: f"{n / 1024:,.0f} KiB"  # noqa: E731
    mib = lambda n: f"{n / 2**20:,.1f} MiB"  # noqa: E731
    import sysconfig
    interpreter = Path(sys.executable).resolve().stat().st_size
    shared = Path(sysconfig.get_config_var("LIBDIR") or "") / (sysconfig.get_config_var("INSTSONAME") or "-")
    if shared.is_file() and sysconfig.get_config_var("Py_ENABLE_SHARED"):
        interpreter += shared.resolve().stat().st_size
    stdlib = sum(f.stat().st_size for f in Path(sysconfig.get_paths()["stdlib"]).rglob("*")
                 if f.is_file() and not {"site-packages", "dist-packages"} & set(f.parts))
    out = {"binary (release, stripped)": mib(Path(binary).stat().st_size),
           "reference scripts": f"{kib(sum(p.stat().st_size for p in scripts))} in {len(scripts)} files",
           "python3 interpreter (executable and its shared library)": mib(interpreter),
           "python3 standard library on disk": mib(stdlib) + " (all of it, without site-packages; the scripts "
                                                         "import a small part; the interpreter's startup is the "
                                                         "`startup` workload)"}
    out["tursodb CLI the reference drives"] = mib(tursodb.stat().st_size) if tursodb else "not found"
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--binary", default=str(REPO / "target/release/supercharged-memory"))
    ap.add_argument("--runs", type=int, default=11, help="runs per side for sub-second workloads, warm-up included")
    ap.add_argument("--large-runs", type=int, default=6, help="runs per side over the large fixture")
    ap.add_argument("--embed-runs", type=int, default=4, help="runs of embed --missing")
    ap.add_argument("--rows", type=int, default=10000, help="generated semantic rows in the large fixture")
    ap.add_argument("--episodic-rows", type=int, default=2000, help="generated episodic rows in it")
    ap.add_argument("--unembedded", type=int, default=3000, help="rows without a vector for embed --missing")
    ap.add_argument("--only", help="comma-separated workload names")
    ap.add_argument("--out", help="also write the report to this file")
    ap.add_argument("--keep", action="store_true", help="keep the temporary machines")
    ap.add_argument("--measure", help="the measuring launcher (default: the release build of the "
                                      "bench-measure example)")
    ap.add_argument("--self-test", action="store_true")
    ap.add_argument("--vector", metavar="TEXT")
    a = ap.parse_args()
    if a.vector is not None:
        print(json.dumps(vector(a.vector)))
        return
    if a.self_test:
        self_test(Path(a.measure).resolve() if a.measure else None)
        return
    binary = Path(a.binary).resolve()
    if not binary.is_file():
        sys.exit(f"no binary at {binary}: run `cargo build --release` first (host architecture only)")
    launcher = None
    if hasattr(os, "wait4"):
        exe = "bench-measure.exe" if os.name == "nt" else "bench-measure"
        launcher = Path(a.measure).resolve() if a.measure else REPO / "target/release/examples" / exe
        if not launcher.is_file():
            sys.exit(f"no measuring launcher at {launcher}: run "
                     "`cargo build --release --example bench-measure` first (host architecture only)")
    chosen = WORKLOADS
    if a.only:
        names = set(a.only.split(","))
        unknown = names - {w.name for w in WORKLOADS}
        if unknown:
            sys.exit(f"unknown workload(s): {', '.join(sorted(unknown))}")
        chosen = [w for w in WORKLOADS if w.name in names]
    tursodb = find_tursodb()
    progress = lambda msg: print(msg, file=sys.stderr, flush=True)  # noqa: E731
    if not tursodb:
        progress("SKIPPED: tursodb not found, so the reference side of every workload is skipped")
    host = host_info(binary, tursodb)
    base = Path(tempfile.mkdtemp(prefix="sm-bench-"))
    server, ollama = start_ollama()
    try:
        dead = refusing_url()
        progress("building the fixtures ...")
        fixtures = {"embedded": build_fixture(base / "embedded.db", embed_all=True)}
        if any(w.fixture == "large" for w in chosen):
            fixtures["large"] = build_fixture(base / "large.db", embed_all=True,
                                              semantic=a.rows, episodic=a.episodic_rows)
        if any(w.fixture == "unembedded" for w in chosen):
            fixtures["unembedded"] = build_fixture(base / "unembedded.db", embed_all=True, unembedded=a.unembedded)
        fixture_rows = {name: counts(path) for name, path in fixtures.items()}
        results = []
        for w in chosen:
            runs = {"small": a.runs, "large": a.large_runs, "embed": a.embed_runs}[w.runs]
            results.append(run_workload(w, base, binary, fixtures, ollama, dead, tursodb, runs, progress,
                                        launcher))
        describe = lambda c: ", ".join(f"{t} {n:,}" for t, n in c.items())  # noqa: E731
        fx = {"embedded (every workload not named below)":
              f"tests/e2e/fixture.sql, every row with a vector: {describe(fixture_rows['embedded'])}"}
        if "large" in fixture_rows:
            fx["large (recall-large*)"] = (f"the same plus {a.rows:,} semantic and {a.episodic_rows:,} episodic "
                                           f"generated rows, all with a vector: {describe(fixture_rows['large'])}")
        if "unembedded" in fixture_rows:
            fx["unembedded (embed-missing)"] = (f"the embedded fixture plus {a.unembedded:,} semantic rows "
                                                f"without a vector: {describe(fixture_rows['unembedded'])}")
        method = [
            f"runs per side: {a.runs} for the e2e fixture, {a.large_runs} over the large one, "
            f"{a.embed_runs} for embed-missing; the first of each side is a warm-up and is discarded",
            "the sides alternate which goes first, run by run",
            "a workload that writes starts every run from a fresh byte copy of its fixture (copy not timed)",
            "wall: monotonic clock from spawn to exit; CPU and peak RSS: getrusage of the reaped child, "
            "collected by the native launcher bench/measure.rs"
            if launcher else
            "wall: monotonic clock from spawn to exit; peak working set; CPU figures unavailable on this OS",
            "the reference's CPU and RSS include the tursodb processes it starts and reaps",
            "spread is min – max; p95 interpolates linearly between ranks",
        ]
        report = render(results, host, sizes_of(binary, tursodb), fx, method)
        print(report, end="")
        if a.out:
            Path(a.out).write_text(report, encoding="utf-8")
    finally:
        server.shutdown()
        if not a.keep:
            shutil.rmtree(base, ignore_errors=True)


# --------------------------------------------------------------------------


def self_test(launcher):
    assert percentile([1, 2, 3, 4, 5], 50) == 3
    assert percentile([4, 1, 3, 2], 50) == 2.5
    assert abs(percentile(list(range(1, 21)), 95) - 19.05) < 1e-9
    assert percentile([7], 95) == 7
    s = summary([3, None, 1, 2])
    assert (s["n"], s["median"], s["min"], s["max"]) == (3, 2, 1, 3), s
    assert summary([None]) is None
    assert ratio(1, 4) == "0.25" and ratio(1, 0) == "n/a" and ratio(None, 1) == "n/a"
    assert strip_sql_comments("a -- c; d\nb 'x--y' -- z\n") == "a \nb 'x--y' \n"
    assert vector("") == [1] + [0] * (DIM - 1)
    assert vector("Word word WORD") == [3 * x for x in word(b"word")]
    assert vector("naïve") == [a + b for a, b in zip(word(b"na"), word(b"ve"))]
    with tempfile.TemporaryDirectory() as d:
        c = counts(build_fixture(Path(d) / "f.db"))
        assert c == {"semantic_memory": 22, "episodic_memory": 10, "instructions": 7}, c
        con = sqlite3.connect(Path(d) / "f.db")
        no_vector = con.execute("SELECT count(*) FROM semantic_memory WHERE embedding IS NULL").fetchone()[0]
        con.close()
        assert no_vector == 2, no_vector
        c = counts(build_fixture(Path(d) / "g.db", embed_all=True, semantic=30, episodic=5, unembedded=4))
        assert c == {"semantic_memory": 56, "episodic_memory": 15, "instructions": 7}, c
        con = sqlite3.connect(Path(d) / "g.db")
        no_vector = con.execute("SELECT count(*) FROM semantic_memory WHERE embedding IS NULL").fetchone()[0]
        con.close()
        assert no_vector == 4, no_vector
        assert list(generated_texts(3, 1)) == list(generated_texts(3, 1))
        os.makedirs(Path(d) / "tmp", exist_ok=True)
        wall, user, system, rss, code, _ = measure([sys.executable, "-c", "pass"], {"PATH": PATH}, d)
        assert code == 0 and wall > 0, (code, wall)
        if hasattr(os, "wait4"):
            assert rss and rss > 0 and user is not None, (rss, user)
        if launcher is not None:
            if not launcher.is_file():
                sys.exit(f"self-test: no measuring launcher at {launcher}")
            # Spawned from the launcher, a child's peak memory is its own, not
            # the interpreter's that started the measurement.
            direct = rss
            wall, user, system, rss, code, _ = measure([sys.executable, "-c", "pass"], {"PATH": PATH}, d,
                                                       launcher=launcher)
            assert code == 0 and wall > 0 and user is not None and rss and rss > 0, (code, wall, user, rss)
            assert rss < direct, (rss, direct)
            _, _, _, _, code, _ = measure([sys.executable, "-c", "raise SystemExit(3)"], {"PATH": PATH}, d,
                                          launcher=launcher)
            assert code == 3, code
    sample = [{"wall": 0.01, "user": 0.005, "sys": 0.002, "rss": 2048}] * 3
    report = render([{"name": "w", "binary": sample, "reference": sample, "binary_cmd": "b",
                      "reference_cmd": "r", "no_reference": ""}],
                    {"CPU": "x"}, {"binary": "1 MiB"}, {"f": "rows"}, ["m"])
    for needle in ("## Host", "## Summary", "### w", "| wall ms | 10.0 | 10.0 | 10.0 – 10.0 | 10.0 | 10.0 |",
                   "| peak RSS MiB | 2.0 |", "| 1.00 |"):
        assert needle in report, (needle, report)
    print("self-test ok")


if __name__ == "__main__":
    main()
