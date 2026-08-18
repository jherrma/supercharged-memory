#!/usr/bin/env python3
"""Restore a backup dump into a FRESH database, and prove it worked.

Piping a dump into tursodb DOES NOT WORK and fails *partially*, which is worse than
failing outright: the CLI splits its input on LINE boundaries, but `memory_text`
contains newlines, so most of a dump's lines are continuations inside string
literals. The first multi-line INSERT ends its statement mid-value, everything after
is parsed as garbage until something happens to parse again, and you are left with an
arbitrary fraction of the rows plus a misleading "table ... does not exist" error.
`.read` has the same defect.

So: split on ';' that is OUTSIDE any quoted string, hand each statement to tursodb as
an argv argument, then COUNT what landed against what the dump contained. A restore
that is not counted is not a restore.

    restore.py --out /tmp/check.db                 # newest backup, verify it restores
    restore.py --dump Backups/2026-08-18-*.sql.gz --out ~/.local/share/turso/new.db

Refuses to write to a path that already exists — move the old file aside yourself.
That is deliberate: restoring over a live DB loses everything since the backup.
"""
import argparse, gzip, re, subprocess, sys
sys.dont_write_bytecode = True
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent))
import memlib as M  # noqa: E402

# Meaningless when every statement runs in its own process, and COMMIT without an
# open transaction is a hard error that would otherwise look like a real failure.
SKIP = re.compile(r"^\s*(BEGIN|COMMIT|END|ROLLBACK|PRAGMA)\b", re.I)


def split_statements(sql):
    """Statement boundaries are ';' outside quotes. Doubled '' and "" are escapes."""
    out, start, i, n = [], 0, 0, len(sql)
    in_single = in_double = False
    while i < n:
        c = sql[i]
        if in_single:
            if c == "'":
                if i + 1 < n and sql[i + 1] == "'":
                    i += 2
                    continue
                in_single = False
        elif in_double:
            if c == '"':
                if i + 1 < n and sql[i + 1] == '"':
                    i += 2
                    continue
                in_double = False
        elif c == "'":
            in_single = True
        elif c == '"':
            in_double = True
        elif c == ";":
            s = sql[start:i + 1].strip()
            if s:
                out.append(s)
            start = i + 1
        i += 1
    tail = sql[start:].strip()
    if tail:
        out.append(tail)
    if in_single or in_double:
        sys.exit("dump ends inside an unterminated string literal — truncated file?")
    return out


def read_dump(path):
    data = Path(path).read_bytes()
    if data[:2] == b"\x1f\x8b":
        data = gzip.decompress(data)
    return data.decode("utf-8")


def newest_backup():
    d = Path(M.BACKUP_DIR)
    files = sorted(d.glob("*supercharged-memory*.sql*"), key=lambda p: p.stat().st_mtime)
    if not files:
        sys.exit(f"no backup found in {d}")
    return files[-1]


def expected_counts(stmts):
    """Rows the dump claims to carry, per table — the number the restore must match."""
    counts = {}
    for s in stmts:
        m = re.match(r'INSERT\s+INTO\s+"?(\w+)"?', s, re.I)
        if m:
            counts[m.group(1)] = counts.get(m.group(1), 0) + 1
    return counts


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dump", help="path to a .sql or .sql.gz dump (default: newest backup)")
    ap.add_argument("--out", required=True, help="target DB path; must NOT already exist")
    ap.add_argument("--force", action="store_true",
                    help="allow writing to an existing path (this can destroy data)")
    a = ap.parse_args()

    out = Path(a.out)
    if out.exists() and not a.force:
        sys.exit(f"refusing to write to existing file: {out}\n"
                 f"  Restoring over a live DB loses everything since the backup.\n"
                 f"  Move it aside first, or pass --force if you truly mean it.")
    if a.force:
        for suffix in ("", "-wal", "-shm", "-tshm"):
            p = Path(str(out) + suffix)
            if p.exists():
                p.unlink()

    dump = Path(a.dump) if a.dump else newest_backup()
    stmts = [s for s in split_statements(read_dump(dump)) if not SKIP.match(s)]
    want = expected_counts(stmts)
    print(f"dump   : {dump}")
    print(f"target : {out}")
    print(f"{len(stmts)} statements, {sum(want.values())} rows across {len(want)} table(s)")

    failures = []
    for k, s in enumerate(stmts, 1):
        p = subprocess.run([M.TURSO, str(out), M.FLAG, "-q", s],
                           capture_output=True, text=True, encoding="utf-8", timeout=300)
        if p.returncode != 0 or "error" in p.stderr.lower():
            failures.append((k, p.stderr.strip()[:160], s[:100]))
        if k % 200 == 0:
            print(f"  {k}/{len(stmts)}", flush=True)

    print("\ntable                 in dump   restored")
    bad = False
    for t in sorted(want):
        # Not M.exec_sql(): that targets memlib.DB, i.e. the LIVE database. The
        # whole point is to count what landed in the restored file.
        r = subprocess.run([M.TURSO, str(out), M.FLAG, "-q", "-m", "list",
                            f"SELECT count(*) FROM {t};"],
                           capture_output=True, text=True, encoding="utf-8", timeout=60)
        got = r.stdout.strip().splitlines()[0] if r.stdout.strip() else "MISSING"
        ok = got == str(want[t])
        bad |= not ok
        print(f"{t:<20} {want[t]:>8}   {got:>8}  {'' if ok else '  <<< MISMATCH'}")

    for k, err, s in failures[:5]:
        print(f"\nFAILED statement {k}: {err}\n  {s}")
    if len(failures) > 5:
        print(f"... and {len(failures) - 5} more")

    if bad or failures:
        sys.exit(f"\nRESTORE INCOMPLETE — {len(failures)} failed statement(s). "
                 f"Do not use this database.")
    print(f"\nRESTORE OK — every row in the dump is present in {out}")


if __name__ == "__main__":
    main()
