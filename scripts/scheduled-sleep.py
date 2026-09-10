#!/usr/bin/env python3
"""Run a sleep pass unattended, on a schedule (see ../instructions/SLEEP.md and
../instructions/DEEP-SLEEP.md for what the passes actually do).

    scheduled-sleep.py                  # auto: whichever pass is due (what the timer runs)
    scheduled-sleep.py --mode daily     # normal sleep, once per calendar day
    scheduled-sleep.py --mode weekly    # deep-sleep PREPARATION, once per ISO week

WHY THIS EXISTS: consolidation only ever happened when someone remembered to ask
for it, so the topic index went stale for as long as nobody did. A SessionStart
hook is not the answer either - it fires only when a session starts, so a busy
week without opening the tool would never consolidate.

WHAT --mode weekly WILL NOT DO. Deep sleep gates three phases on the user: D2
purge (hard-deletes rows, the one operation in this system that destroys a
memory), D3 merges, D4 patterns. Those gates are the design. So the weekly run
does everything up to the writes - health check, backup, clustering, merge and
pattern proposals, regression report - and writes a decision queue the user
approves later, in a normal session. It never purges and never merges.

CATCH-UP, AND WHY THE SCHEDULE IS HOURLY. The point of a schedule here is "it
happened today", not "it happened at 12:00". Rather than depend on each
platform's missed-start feature (Task Scheduler StartWhenAvailable, systemd
Persistent=true, launchd's wake behaviour, and plain cron which has none), the
installer registers an HOURLY trigger everywhere and this script decides:

  - a marker file records the last COMPLETED run's period (date, or ISO week)
  - --not-before says the earliest hour that period may run

So a machine that was off at noon runs at the first hourly tick after it boots,
on every platform, with identical semantics. The common case - already ran this
period - costs one file read and exits before touching the network.

FAILURE POLICY: the marker is written only on success. A failed run therefore
retries on the next tick instead of being silently skipped for the period. A run
that cannot even start (Ollama down, DB missing) is a failure, not a no-op.

PERMISSIONS: unattended, so `claude` is invoked with an explicit --allowedTools
list plus --permission-prompts none. Anything outside the list is DENIED rather
than waiting for a human, so the job can fail but can never hang. If the log
shows a denial, widen ALLOWED_TOOLS below - do not reach for
--dangerously-skip-permissions, which hands an unattended job every tool on the
machine.

Manual:      scheduled-sleep.py --mode daily --force
Dry run:     scheduled-sleep.py --mode daily --dry-run
Install:     install-schedule.py            (see that script for the schedulers)
Logs:        <state dir>/scheduled-sleep.log
"""
import argparse
import datetime as dt
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent

# Claude Code keeps per-user env in ~/.claude/settings.json and injects it into
# SESSIONS ONLY. A cron job, a systemd unit and a Scheduled Task inherit none of
# it, so a user who set SUPERCHARGED_MEMORY_TURSO_PATH there would have this run
# fall back to the default path, find nothing, and get a MISSING report that
# reads like data loss. Adopt that block before memlib resolves anything.
def adopt_claude_settings_env():
    settings = Path.home() / ".claude" / "settings.json"
    if not settings.is_file():
        return
    try:
        env = (json.loads(settings.read_text(encoding="utf-8")) or {}).get("env") or {}
    except (json.JSONDecodeError, OSError):
        return
    for key, value in env.items():
        # A real environment variable always wins; this is a fallback, not an override.
        if key not in os.environ and isinstance(value, str):
            os.environ[key] = value


adopt_claude_settings_env()
sys.path.insert(0, str(Path(__file__).parent))
import memlib as M  # noqa: E402

STATE_DIR = Path(os.environ.get("SUPERCHARGED_MEMORY_STATE_DIR", Path(M.DB).parent / "schedule"))
LOG_FILE = STATE_DIR / "scheduled-sleep.log"
LOCK_FILE = STATE_DIR / "sleep.lock"
LOG_MAX_BYTES = 1_000_000
LOCK_STALE_HOURS = 3

ALLOWED_TOOLS = [
    "Bash(python:*)", "Bash(python3:*)", "Bash(bash:*)", "Bash(tursodb:*)",
    "Read", "Glob", "Grep", "Task",
    "mcp__turso__execute_query", "mcp__turso__list_tables", "mcp__turso__describe_table",
]
WEEKLY_EXTRA_TOOLS = ["Write"]  # the review file

MODES = {
    "daily": {
        "marker": "last-daily-run",
        "not_before": "12:00",
        "timeout_min": 60,
        "runbook": "instructions/SLEEP.md",
    },
    "weekly": {
        "marker": "last-weekly-run",
        "not_before": "07:00",
        "weekday": 0,  # Monday
        "timeout_min": 120,
        "runbook": "instructions/DEEP-SLEEP.md",
    },
}


def log(message):
    STATE_DIR.mkdir(parents=True, exist_ok=True)
    if LOG_FILE.exists() and LOG_FILE.stat().st_size > LOG_MAX_BYTES:
        tail = LOG_FILE.read_text(encoding="utf-8", errors="replace").splitlines()[-500:]
        LOG_FILE.write_text("\n".join(tail) + "\n", encoding="utf-8")
    line = f"{dt.datetime.now():%Y-%m-%d %H:%M:%S}  {message}"
    with LOG_FILE.open("a", encoding="utf-8") as handle:
        handle.write(line + "\n")
    print(line, flush=True)


def period_tag(mode, now):
    if mode == "weekly":
        year, week, _ = now.isocalendar()
        return f"{year}-W{week:02d}"
    return now.strftime("%Y-%m-%d")


def is_due(mode, force=False):
    """Marker + window check only - no network, no lock. Used by --mode auto."""
    now = dt.datetime.now()
    spec = MODES[mode]
    marker = STATE_DIR / spec["marker"]
    if not force and marker.is_file():
        if marker.read_text(encoding="utf-8").strip() == period_tag(mode, now):
            return False
    if force:
        return True
    hour, minute = (int(x) for x in spec["not_before"].split(":"))
    weekday = spec.get("weekday")
    on_target_day = weekday is None or now.weekday() == weekday
    return not (on_target_day and (now.hour, now.minute) < (hour, minute))


def find_claude():
    found = shutil.which("claude")
    if found:
        return found
    for candidate in (Path.home() / ".local/bin/claude",
                      Path.home() / ".local/bin/claude.exe",
                      Path.home() / ".claude/local/claude"):
        if candidate.is_file():
            return str(candidate)
    return None


class Lock:
    """Daily and weekly drive `claude` against the same database, and the weekly
    run calls the daily one. Whichever starts second backs off; its next hourly
    tick picks the work up. O_EXCL so two processes cannot both believe they won."""

    def __init__(self, path):
        self.path = path
        self.held = False
        self.inherited = False

    def __enter__(self):
        # The weekly run already holds the lock when it calls the daily one.
        if os.environ.get("SUPERCHARGED_MEMORY_SKIP_LOCK") == "1":
            self.held = True
            self.inherited = True
            return self
        STATE_DIR.mkdir(parents=True, exist_ok=True)
        if self.path.exists():
            age_h = (dt.datetime.now().timestamp() - self.path.stat().st_mtime) / 3600
            if age_h < LOCK_STALE_HOURS:
                return self
            log(f"note: clearing a stale lock ({age_h:.1f}h old)")
            self.path.unlink(missing_ok=True)
        try:
            handle = os.open(self.path, os.O_CREAT | os.O_EXCL | os.O_WRONLY)
        except FileExistsError:
            return self
        with os.fdopen(handle, "w") as fh:
            fh.write(str(os.getpid()))
        self.held = True
        return self

    def __exit__(self, *_):
        if self.held and not self.inherited:
            self.path.unlink(missing_ok=True)


DAILY_PROMPT = """Run the supercharged-memory sleep (consolidation pass) now.

This is the scheduled run, which the user set up deliberately - the runbook's
"never scheduled" wording does not apply to this invocation. Do not stop to ask
for confirmation.

Follow {runbook} step by step, including running the reading in subagents rather
than pulling memory_text into your own context.

This is a NORMAL sleep, not a deep sleep. Do not purge or delete any memory. Nobody
is watching this run, so do not ask anything: anything that would need a decision,
leave untouched and report it at the end.

Finish with a short plain-text summary: episodic rows processed, semantic rows
consolidated or retired, topics rebuilt, and anything you skipped and why.
"""

WEEKLY_PROMPT = """Prepare a deep sleep for the supercharged-memory corpus.

This is the scheduled weekly preparation run, which the user set up deliberately.
NOBODY IS WATCHING IT. That changes what you may do, not how carefully you do it.

Follow {runbook}, these phases only:
  - D0 health check
  - D1 backup (run it: it is the undo for any purge the user later approves)
  - D3 step 1 mechanical clustering, and step 2 the per-cluster proposal workers
  - D4 pattern mining, map and reduce, proposals only
  - D6.2 regression report if an eval set exists; skip and say so if eval_cases is empty
Run the reading in subagents exactly as the subagent contract requires.

DO NOT, under any circumstance:
  - purge anything (D2). Do not call sleep.py --purge. Produce the candidate LIST only.
  - apply a merge (D3 step 3) or write a pattern row (D4 step 5).
  - repoint or retire an eval case (D6.1). List the proposals.
  - ask a question. There is no one to answer it.
  - rebuild the topic index (D5): nothing was applied, so it has not changed.

Write your output to this file, overwriting it:
  {review_file}

Make it a decision queue that can be acted on in one pass, not a narrative:
  - Purge candidates: id, topic, created_at, why it is superseded or retired.
  - Merge proposals: cluster, ids, proposed topic, merged length, why_safe. Flag any
    member that already supersedes earlier rows - repeated merging loses the
    identifiers that make this corpus useful.
  - Pattern proposals: the claim, its evidence_ids, and whether it clears the >=3
    events and >=2 calendar months thresholds.
  - Eval-case proposals, split into repoint and retire.
  - Anything skipped, and why. A worker that died is a gap: say so.
End the file with the exact commands to apply each section, so approving is a
matter of deleting the lines the user does not want.

Then print a short plain-text summary to stdout: counts per section and the path.

Treat every candidate list you produce as a LEAD, not a decision: pairs that share
vocabulary while stating different things are the common false positive, and the
reviewer needs enough context in why_safe to catch that without re-reading the rows.
"""


def preflight(spec, now, args):
    """Whether this hourly tick is allowed to start the pass."""
    if not args.force:
        hour, minute = (int(x) for x in spec["not_before"].split(":"))
        weekday = spec.get("weekday")
        # Weekly: the hour only gates the target weekday. Later in the same ISO week
        # is a catch-up for a machine that was off on Monday, and must not wait for
        # 07:00 all over again.
        on_target_day = weekday is None or now.weekday() == weekday
        if on_target_day and (now.hour, now.minute) < (hour, minute):
            return f"skip: before {spec['not_before']}"
    if not M.db_exists():
        return f"abort: no database at {M.DB} - not creating one; run recall.py --candidates"
    return None


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--mode", choices=[*sorted(MODES), "auto"], default="auto",
                        help="auto (default) runs whichever pass is due; the timer uses it")
    parser.add_argument("--force", action="store_true",
                        help="ignore the period marker and the not-before window")
    parser.add_argument("--dry-run", action="store_true",
                        help="run the preflight and print what would happen")
    args = parser.parse_args()

    if args.mode == "auto":
        # One timer, one entry point. Weekly first: it runs the daily pass itself as
        # its D0 prerequisite, so doing daily first would just do it twice.
        for mode in ("weekly", "daily"):
            if not is_due(mode, args.force):
                continue
            argv = [sys.executable, str(Path(__file__).resolve()), "--mode", mode]
            if args.force:
                argv.append("--force")
            if args.dry_run:
                argv.append("--dry-run")
            return subprocess.run(argv).returncode
        return 0

    spec = MODES[args.mode]
    now = dt.datetime.now()
    tag = period_tag(args.mode, now)
    marker = STATE_DIR / spec["marker"]

    # Cheapest check first: on an hourly trigger this is the common path.
    if not args.force and marker.is_file():
        if marker.read_text(encoding="utf-8").strip() == tag:
            return 0

    reason = preflight(spec, now, args)
    if reason:
        if reason.startswith("abort"):
            log(f"[{args.mode}] {reason}")
            return 1
        return 0

    if args.dry_run:
        review_file = STATE_DIR / f"deep-sleep-review-{now:%Y-%m-%d}.md"
        log(f"[{args.mode}] dry run: would run in {REPO} (db {M.DB}, marker {tag})")
        log(f"[{args.mode}] dry run: claude={find_claude()} ollama_up={M.ollama_up()}")
        if args.mode == "weekly":
            log(f"[{args.mode}] dry run: review file would be {review_file}")
        return 0

    with Lock(LOCK_FILE) as lock:
        if not lock.held:
            log(f"[{args.mode}] skip: another sleep job holds the lock - next tick retries")
            return 0

        claude = find_claude()
        if not claude:
            log(f"[{args.mode}] abort: the `claude` CLI is not on PATH")
            return 1
        if not M.ollama_up():
            # Not fatal-looking on purpose: on a boot-time catch-up Ollama is often
            # still starting. No marker, so the next tick tries again.
            log(f"[{args.mode}] abort: Ollama not reachable at {M.OLLAMA} - next tick retries")
            return 1

        review_file = STATE_DIR / f"deep-sleep-review-{now:%Y-%m-%d}.md"
        template = WEEKLY_PROMPT if args.mode == "weekly" else DAILY_PROMPT
        prompt = template.format(runbook=(REPO / spec["runbook"]).as_posix(),
                                 review_file=review_file.as_posix())
        tools = ALLOWED_TOOLS + (WEEKLY_EXTRA_TOOLS if args.mode == "weekly" else [])

        # D0 wants the normal pass done before deep sleep, or compaction merges
        # against a corpus missing the week's lessons. The daily run self-throttles,
        # so this is a no-op when it already happened today.
        if args.mode == "weekly":
            log("[weekly] running the normal sleep first (prerequisite)")
            daily = subprocess.run([sys.executable, str(Path(__file__).resolve()),
                                    "--mode", "daily", "--force"],
                                   env={**os.environ, "SUPERCHARGED_MEMORY_SKIP_LOCK": "1"})
            if daily.returncode != 0:
                log(f"[weekly] abort: the normal sleep failed ({daily.returncode})")
                return daily.returncode

        log(f"[{args.mode}] starting ({M.DB})")
        try:
            done = subprocess.run(
                [claude, "-p", "--allowedTools", *tools, "--permission-prompts", "none"],
                input=prompt, text=True, capture_output=True, cwd=REPO,
                timeout=spec["timeout_min"] * 60)
        except subprocess.TimeoutExpired:
            log(f"[{args.mode}] FAILED: timed out after {spec['timeout_min']}min - next tick retries")
            return 1

        for line in (done.stdout or "").splitlines():
            log(f"[{args.mode}] claude: {line}")
        if done.returncode != 0:
            for line in (done.stderr or "").splitlines()[-10:]:
                log(f"[{args.mode}] claude stderr: {line}")
            log(f"[{args.mode}] FAILED: claude exited {done.returncode} - next tick retries")
            return done.returncode
        if args.mode == "weekly" and not review_file.is_file():
            log(f"[{args.mode}] FAILED: no review file at {review_file} - next tick retries")
            return 1

        marker.write_text(tag, encoding="utf-8")
        log(f"[{args.mode}] done (marker {tag})")
        if args.mode == "weekly":
            log(f"[weekly] review ready: {review_file}")
            log("[weekly] nothing was purged, merged or written - approve it in a session")
        return 0


if __name__ == "__main__":
    sys.exit(main())
