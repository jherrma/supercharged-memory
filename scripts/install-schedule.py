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
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
RUNNER = REPO / "scripts" / "scheduled-sleep.py"
LABEL = "com.supercharged-memory.sleep"
WIN_TASK = "SuperchargedMemorySleep"
UNIT = "supercharged-memory-sleep"


def python_exe():
    # sys.executable is the interpreter that has memlib's dependencies; a bare
    # "python3" in a unit file can easily be a different one.
    return sys.executable or shutil.which("python3") or "python3"


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
    <ExecutionTimeLimit>PT3H</ExecutionTimeLimit>
    <Enabled>true</Enabled>
    <Hidden>true</Hidden>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{python_exe()}</Command>
      <Arguments>"{RUNNER}"</Arguments>
      <WorkingDirectory>{REPO}</WorkingDirectory>
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
    <string>{python_exe()}</string>
    <string>{RUNNER}</string>
  </array>
  <key>WorkingDirectory</key><string>{REPO}</string>
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
ExecStart={python_exe()} {RUNNER}
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


def _crontab_lines():
    done = subprocess.run(["crontab", "-l"], capture_output=True, text=True)
    return [] if done.returncode != 0 else done.stdout.splitlines()


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
    entry = f"0 * * * * cd {REPO} && {python_exe()} {RUNNER} >/dev/null 2>&1  {CRON_MARK}"
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
        print(done.stdout.strip() or "not registered")
        return done.returncode
    hits = [ln for ln in _crontab_lines() if CRON_MARK in ln]
    print("\n".join(hits) if hits else "not registered (no cron entry)")
    return 0 if hits else 1


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
        return status()
    if args.uninstall:
        return uninstall(args.dry_run)
    code = install(args.dry_run)
    if code == 0 and not args.dry_run:
        print(f"\nInstalled. It checks hourly and runs a pass when one is due:")
        print(f"  daily  normal sleep, not before 12:00")
        print(f"  weekly deep-sleep preparation, Mondays not before 07:00 (catches up later in the week)")
        print(f"Verify with: {python_exe()} {RUNNER} --dry-run")
    return code


if __name__ == "__main__":
    sys.exit(main())
