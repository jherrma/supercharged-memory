#!/usr/bin/env python3
"""Register (or remove) the hourly trigger that drives scheduled-sleep.py.

    install-schedule.py                 # install for this platform
    install-schedule.py --dry-run       # print the unit/plist/XML and the command, change nothing
    install-schedule.py --uninstall     # remove it
    install-schedule.py --status        # is it registered, and when did it last run

WHY HOURLY, FOR A DAILY AND A WEEKLY PASS: the schedule owns "how often we check",
scheduled-sleep.py owns "is a pass due" - a marker file per period plus an
earliest-hour window. Keeping the decision in one place is what makes the four
schedulers below behave identically, including the catch-up a laptop needs. Put
the timing in the scheduler instead and you inherit its missed-start semantics:
Task Scheduler has StartWhenAvailable, systemd has Persistent=true, launchd runs
a missed calendar job on wake, and plain cron has nothing at all.

Each platform gets a FILE (XML / plist / systemd units) rather than a long
command line. Quoting a python invocation through `schtasks /TR` or a crontab
entry is where these installers usually break, and a file is also something the
user can read, diff and hand-edit.

  Windows   Task Scheduler task `SuperchargedMemorySleep`, registered from XML
  macOS     launchd agent ~/Library/LaunchAgents/com.supercharged-memory.sleep.plist
  Linux     systemd --user timer supercharged-memory-sleep.timer, else a crontab line

Idempotent: re-running replaces what is there.
"""
import argparse
import os
import platform
import shlex
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from xml.sax.saxutils import escape

REPO = Path(__file__).resolve().parent.parent
RUNNER = REPO / "scripts" / "scheduled-sleep.py"
LABEL = "com.supercharged-memory.sleep"
WIN_TASK = "SuperchargedMemorySleep"
UNIT = "supercharged-memory-sleep"


def python_exe():
    # sys.executable is the interpreter that has memlib's dependencies; a bare
    # "python3" in a unit file can easily be a different one.
    return sys.executable or shutil.which("python3") or "python3"


# Both the Task Scheduler task and the launchd agent are XML documents, and the
# three values we interpolate are user paths. `&` is legal in a Windows path
# (`C:\Users\R&D\...`) and in a macOS home directory, and it makes the document
# malformed - schtasks then reports a parse error against the temp file rather
# than against the path that caused it.
def xml_text(value):
    return escape(str(value))


# ----------------------------------------------------------------- Windows ---
def windows_xml():
    return f"""<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>supercharged-memory: run a sleep pass when one is due (hourly check).</Description>
  </RegistrationInfo>
  <Triggers>
    <CalendarTrigger>
      <StartBoundary>2000-01-01T00:00:00</StartBoundary>
      <Repetition><Interval>PT1H</Interval><StopAtDurationEnd>false</StopAtDurationEnd></Repetition>
      <ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay>
      <Enabled>true</Enabled>
    </CalendarTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <StartWhenAvailable>true</StartWhenAvailable>
    <!-- Must exceed the runner's own budget, which is 60min for the daily
         prerequisite plus 120min for the weekly pass. At exactly PT3H the task
         is killed at the boundary from outside, so the runner's TimeoutExpired
         handler never runs and nothing reaches the log. -->
    <ExecutionTimeLimit>PT4H</ExecutionTimeLimit>
    <Enabled>true</Enabled>
    <Hidden>true</Hidden>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{xml_text(python_exe())}</Command>
      <Arguments>"{xml_text(RUNNER)}"</Arguments>
      <WorkingDirectory>{xml_text(REPO)}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"""


def windows_install(dry_run):
    xml = windows_xml()
    if dry_run:
        print(xml)
        print(f"$ schtasks /Create /TN {WIN_TASK} /XML <the file above> /F")
        return 0
    # /XML wants UTF-16, which is also what the declaration above says.
    with tempfile.NamedTemporaryFile("w", suffix=".xml", delete=False,
                                     encoding="utf-16") as handle:
        handle.write(xml)
        path = handle.name
    try:
        done = subprocess.run(["schtasks", "/Create", "/TN", WIN_TASK, "/XML", path, "/F"],
                              capture_output=True, text=True)
        print((done.stdout or done.stderr).strip())
        return done.returncode
    finally:
        os.unlink(path)


def windows_uninstall(dry_run):
    if dry_run:
        print(f"$ schtasks /Delete /TN {WIN_TASK} /F")
        return 0
    done = subprocess.run(["schtasks", "/Delete", "/TN", WIN_TASK, "/F"],
                          capture_output=True, text=True)
    print((done.stdout or done.stderr).strip())
    return done.returncode


def windows_status():
    done = subprocess.run(["schtasks", "/Query", "/TN", WIN_TASK, "/V", "/FO", "LIST"],
                          capture_output=True, text=True)
    if done.returncode != 0:
        print(f"not registered ({WIN_TASK})")
        return 1
    for line in done.stdout.splitlines():
        if any(k in line for k in ("TaskName:", "Status:", "Last Run Time:",
                                   "Last Result:", "Next Run Time:")):
            print(line.strip())
    return 0


# ------------------------------------------------------------------- macOS ---
def macos_plist_path():
    return Path.home() / "Library" / "LaunchAgents" / f"{LABEL}.plist"


def macos_plist():
    return f"""<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{xml_text(python_exe())}</string>
    <string>{xml_text(RUNNER)}</string>
  </array>
  <key>WorkingDirectory</key><string>{xml_text(REPO)}</string>
  <key>StartInterval</key><integer>3600</integer>
  <key>RunAtLoad</key><true/>
  <key>ProcessType</key><string>Background</string>
</dict>
</plist>
"""


def macos_install(dry_run):
    path = macos_plist_path()
    if dry_run:
        print(macos_plist())
        print(f"$ launchctl unload {path} 2>/dev/null; launchctl load {path}")
        return 0
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(macos_plist(), encoding="utf-8")
    subprocess.run(["launchctl", "unload", str(path)], capture_output=True)
    done = subprocess.run(["launchctl", "load", str(path)], capture_output=True, text=True)
    print((done.stdout or done.stderr).strip() or f"loaded {path}")
    return done.returncode


def macos_uninstall(dry_run):
    path = macos_plist_path()
    if dry_run:
        print(f"$ launchctl unload {path} && rm {path}")
        return 0
    subprocess.run(["launchctl", "unload", str(path)], capture_output=True)
    path.unlink(missing_ok=True)
    print(f"removed {path}")
    return 0


def macos_status():
    path = macos_plist_path()
    print(f"plist: {path} ({'present' if path.exists() else 'MISSING'})")
    done = subprocess.run(["launchctl", "list", LABEL], capture_output=True, text=True)
    print(done.stdout.strip() if done.returncode == 0 else "not loaded")
    return 0 if done.returncode == 0 else 1


# ------------------------------------------------------------------- Linux ---
def systemd_dir():
    return Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config")) / "systemd" / "user"


def systemd_units():
    service = f"""[Unit]
Description=supercharged-memory: run a sleep pass when one is due

[Service]
Type=oneshot
WorkingDirectory={REPO}
# Quoted: systemd splits ExecStart on whitespace, so a repo or interpreter path
# containing a space would arrive as two arguments. WorkingDirectory takes the
# rest of the line and needs no quoting.
ExecStart="{python_exe()}" "{RUNNER}"
"""
    timer = f"""[Unit]
Description=supercharged-memory: hourly check for a due sleep pass

[Timer]
OnCalendar=hourly
# Belt and braces: the runner's marker file already provides catch-up, but this
# also fires the tick itself after downtime instead of waiting for the next hour.
Persistent=true
AccuracySec=1m

[Install]
WantedBy=timers.target
"""
    return service, timer


def have_systemd():
    return shutil.which("systemctl") is not None and Path("/run/systemd/system").exists()


def linux_install(dry_run):
    service, timer = systemd_units()
    if not have_systemd():
        return cron_install(dry_run)
    directory = systemd_dir()
    if dry_run:
        print(f"# {directory / (UNIT + '.service')}\n{service}")
        print(f"# {directory / (UNIT + '.timer')}\n{timer}")
        print(f"$ systemctl --user daemon-reload && systemctl --user enable --now {UNIT}.timer")
        return 0
    directory.mkdir(parents=True, exist_ok=True)
    (directory / f"{UNIT}.service").write_text(service, encoding="utf-8")
    (directory / f"{UNIT}.timer").write_text(timer, encoding="utf-8")
    subprocess.run(["systemctl", "--user", "daemon-reload"], check=False)
    done = subprocess.run(["systemctl", "--user", "enable", "--now", f"{UNIT}.timer"],
                          capture_output=True, text=True)
    print((done.stdout or done.stderr).strip() or f"enabled {UNIT}.timer")
    if done.returncode == 0:
        # Without lingering the timer stops at logout, which is exactly the machine
        # state a catch-up is for. Say so rather than let it look like it works.
        print(f"note: `loginctl enable-linger {os.environ.get('USER', '')}` keeps the "
              "timer running when you are not logged in")
    return done.returncode


CRON_MARK = "# supercharged-memory sleep"


def _crontab_lines(strict=True):
    # `crontab -l` exits non-zero for "no crontab for <user>" AND for a real
    # failure (cron not installed, spool directory unreadable, an SELinux
    # denial). Treating both as "empty" means the install path would then write
    # a crontab containing only our entry, silently replacing what was there.
    # strict=False is for the read-only status path, which must report rather
    # than exit - it is not about to rewrite anything.
    done = subprocess.run(["crontab", "-l"], capture_output=True, text=True)
    if done.returncode == 0:
        return done.stdout.splitlines()
    if "no crontab" in (done.stderr or "").lower():
        return []
    detail = (done.stderr or "").strip() or f"crontab -l exited {done.returncode}"
    if not strict:
        print(f"cannot read the crontab: {detail}")
        return []
    sys.exit(f"cannot read the crontab, refusing to replace it: {detail}")


def _write_crontab(lines, dry_run):
    body = "\n".join(lines).strip() + "\n"
    if dry_run:
        print(body)
        print("$ crontab - <the above>")
        return 0
    done = subprocess.run(["crontab", "-"], input=body, text=True, capture_output=True)
    print((done.stdout or done.stderr).strip() or "crontab updated")
    return done.returncode


def cron_install(dry_run):
    # Quoted, because any of the three paths may contain a space. `%` is not
    # quotable here: cron turns an unescaped one into a newline and feeds the
    # remainder to the job on stdin, so escape it rather than write an entry
    # that silently cannot work.
    command = (f"cd {shlex.quote(str(REPO))} && "
               f"{shlex.quote(python_exe())} {shlex.quote(str(RUNNER))} >/dev/null 2>&1")
    entry = f"0 * * * * {command.replace('%', chr(92) + '%')}  {CRON_MARK}"
    lines = [ln for ln in _crontab_lines() if CRON_MARK not in ln]
    lines.append(entry)
    print("no systemd --user available, falling back to cron")
    return _write_crontab(lines, dry_run)


def linux_uninstall(dry_run):
    if have_systemd():
        if dry_run:
            print(f"$ systemctl --user disable --now {UNIT}.timer && rm {systemd_dir()}/{UNIT}.*")
            return 0
        subprocess.run(["systemctl", "--user", "disable", "--now", f"{UNIT}.timer"],
                       capture_output=True)
        for suffix in (".service", ".timer"):
            (systemd_dir() / f"{UNIT}{suffix}").unlink(missing_ok=True)
        subprocess.run(["systemctl", "--user", "daemon-reload"], check=False)
        print(f"removed {UNIT}.timer")
        return 0
    return _write_crontab([ln for ln in _crontab_lines() if CRON_MARK not in ln], dry_run)


def linux_status():
    if have_systemd():
        done = subprocess.run(["systemctl", "--user", "list-timers", f"{UNIT}.timer",
                               "--no-pager"], capture_output=True, text=True)
        # list-timers exits 0 and prints an empty table for a timer that does not
        # exist, so its exit code says nothing. The other two platforms return 1
        # when nothing is registered; match them or a scripted check is wrong here.
        registered = f"{UNIT}.timer" in done.stdout
        print(done.stdout.strip() if registered else "not registered")
        return 0 if registered else 1
    hits = [ln for ln in _crontab_lines(strict=False) if CRON_MARK in ln]
    print("\n".join(hits) if hits else "not registered (no cron entry)")
    return 0 if hits else 1


# --------------------------------------------------------------- the passes ---
# Whether the TRIGGER is registered is a per-platform question; when a PASS last
# ran is not, and it is the one the user actually asks. The trigger fires hourly,
# so its own "Last Run Time" is always minutes ago and its "Last Result: 0" is
# what a not-due tick returns - reading either as health is the mistake this
# section exists to prevent.
_RUNNER_MODULE = None


def _runner():
    """Load the runner itself rather than re-deriving its state directory and its
    due rule here - two answers to either question is how they drift. Importing it
    also adopts ~/.claude/settings.json env, which is usually where the DB path is
    set, so this resolves the same paths a scheduled run would."""
    global _RUNNER_MODULE
    if _RUNNER_MODULE is None:
        import importlib.util
        spec = importlib.util.spec_from_file_location("scheduled_sleep", RUNNER)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        _RUNNER_MODULE = module
    return _RUNNER_MODULE


def due_now():
    """Which passes would run on the next tick. A fresh install has no markers, so
    on any day but Monday this is the WEEKLY pass (its catch-up branch) - which
    runs the normal sleep first. Up to three hours of `claude`, starting within
    the hour, on a machine the user has just agreed to put a background job on.
    Say it at install time rather than let them discover it."""
    try:
        runner = _runner()
        return [mode for mode in ("weekly", "daily") if runner.is_due(mode)]
    except (Exception, SystemExit):  # noqa: BLE001 - advisory only, never fatal
        return None


def pass_status():
    try:
        state = _runner().STATE_DIR
    except (Exception, SystemExit) as exc:  # noqa: BLE001 - status must not abort
        print(f"pass state: unavailable ({type(exc).__name__}: {exc})")
        return
    print(f"state dir: {state}")
    # The trigger's own exit code is printed above and needs a legend: a tick with
    # nothing due and a completed pass both exit 0, a lock back-off exits 75, and
    # only anything else is a real failure.
    print("trigger exit codes: 0 nothing due or pass completed, "
          "75 another pass held the lock, other = failure")
    for marker, label in (("last-daily-run", "last daily sleep"),
                          ("last-weekly-run", "last weekly preparation")):
        path = state / marker
        value = path.read_text(encoding="utf-8").strip() if path.is_file() else "never"
        print(f"{label}: {value}")
    # The weekly pass exists to leave a decision queue, so name it here. The log
    # tail below carries a "review ready" line only until it scrolls off, and a
    # queue nobody is pointed at is a queue nobody acts on.
    reviews = sorted(state.glob("deep-sleep-review-*.md"))
    if reviews:
        print(f"review queue awaiting you: {reviews[-1]}")
        if len(reviews) > 1:
            print(f"  ({len(reviews) - 1} older review file(s) alongside it)")
    log_file = state / "scheduled-sleep.log"
    if not log_file.is_file():
        print("log: none yet")
        return
    tail = log_file.read_text(encoding="utf-8", errors="replace").splitlines()[-5:]
    print("log (last 5 lines):")
    for line in tail:
        print(f"  {line}")


PLATFORMS = {
    "Windows": (windows_install, windows_uninstall, windows_status),
    "Darwin": (macos_install, macos_uninstall, macos_status),
    "Linux": (linux_install, linux_uninstall, linux_status),
}


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    group = parser.add_mutually_exclusive_group()
    group.add_argument("--uninstall", action="store_true")
    group.add_argument("--status", action="store_true")
    parser.add_argument("--dry-run", action="store_true",
                        help="print the unit/plist/XML and the command, change nothing")
    args = parser.parse_args()

    system = platform.system()
    if system not in PLATFORMS:
        sys.exit(f"unsupported platform: {system}")
    if not RUNNER.is_file():
        sys.exit(f"runner not found: {RUNNER}")
    install, uninstall, status = PLATFORMS[system]

    if args.status:
        code = status()
        print()
        pass_status()
        return code
    if args.uninstall:
        return uninstall(args.dry_run)
    code = install(args.dry_run)
    if code == 0 and not args.dry_run:
        print("\nInstalled. It checks hourly and runs a pass when one is due:")
        print("  daily  normal sleep, not before 12:00")
        print("  weekly deep-sleep preparation, Mondays not before 07:00 (catches up later in the week)")
        due = due_now()
        if due and "weekly" in due:
            # The weekly pass runs the daily one as its D0 prerequisite and stamps
            # its marker, so listing both would overstate what happens.
            print("\nDue right now: weekly, which runs the normal sleep first. "
                  "Expect it to start within the hour.")
        elif due:
            print(f"\nDue right now: {' and '.join(due)}. Expect it to start within the hour.")
        elif due is not None:
            print("\nNothing is due right now; the next pass runs at its usual time.")
        print(f"Verify with: {python_exe()} {RUNNER} --dry-run")
    return code


if __name__ == "__main__":
    sys.exit(main())
