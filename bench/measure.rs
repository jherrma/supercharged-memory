//! Run one command and report what it cost, for the benchmark driver.
//!
//!     bench-measure <report.json> <program> [args...]
//!
//! The command inherits stdin, stdout and stderr. Once it has exited, one JSON
//! object goes to `<report.json>`: `code` (its exit status, null when a signal
//! ended it), `wall` (seconds, spawn to exit, monotonic clock), `user` and
//! `sys` (CPU seconds) and `rss_kib` (peak resident memory), the last three
//! from `getrusage(RUSAGE_CHILDREN)`, so they include every process the
//! command started and reaped: CPU summed, memory the largest.
//!
//! Why a native process at all: on Linux, exec keeps the memory high-water
//! mark of the process that called it, so a command spawned by a Python
//! interpreter reports at least the interpreter's own peak as its `ru_maxrss`
//! -- `/bin/true` measured 16 MiB that way. Spawned from this small program,
//! the floor is this program's own footprint, about a megabyte.

use std::ffi::OsString;
use std::process::{Command, ExitCode};
use std::time::Instant;

#[cfg(unix)]
mod usage {
    use std::os::raw::{c_int, c_long};

    #[repr(C)]
    #[derive(Default)]
    struct Timeval {
        sec: c_long,
        #[cfg(target_os = "macos")]
        usec: i32,
        #[cfg(target_os = "macos")]
        _pad: i32,
        #[cfg(not(target_os = "macos"))]
        usec: c_long,
    }

    #[repr(C)]
    #[derive(Default)]
    struct Rusage {
        utime: Timeval,
        stime: Timeval,
        maxrss: c_long,
        rest: [c_long; 13],
    }

    const RUSAGE_CHILDREN: c_int = -1;

    unsafe extern "C" {
        fn getrusage(who: c_int, usage: *mut Rusage) -> c_int;
    }

    /// User and system CPU seconds and the peak RSS in KiB of every child
    /// reaped so far.
    pub fn children() -> Option<(f64, f64, f64)> {
        let mut ru = Rusage::default();
        // SAFETY: `ru` is a valid, writable `struct rusage` with the platform's
        // layout, and getrusage only writes into it.
        if unsafe { getrusage(RUSAGE_CHILDREN, &mut ru) } != 0 {
            return None;
        }
        let secs = |t: &Timeval| t.sec as f64 + t.usec as f64 / 1e6;
        // macOS reports bytes, Linux and the BSDs KiB.
        let kib = if cfg!(target_os = "macos") {
            ru.maxrss as f64 / 1024.0
        } else {
            ru.maxrss as f64
        };
        Some((secs(&ru.utime), secs(&ru.stime), kib))
    }
}

#[cfg(not(unix))]
mod usage {
    pub fn children() -> Option<(f64, f64, f64)> {
        None
    }
}

fn number(x: Option<f64>) -> String {
    x.map_or("null".to_string(), |v| format!("{v}"))
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let [report, program, rest @ ..] = args.as_slice() else {
        eprintln!("usage: bench-measure <report.json> <program> [args...]");
        return ExitCode::from(64);
    };
    let start = Instant::now();
    let status = match Command::new(program).args(rest).status() {
        Ok(status) => status,
        Err(e) => {
            eprintln!(
                "bench-measure: cannot run {}: {e}",
                program.to_string_lossy()
            );
            return ExitCode::from(127);
        }
    };
    let wall = start.elapsed().as_secs_f64();
    let usage = usage::children();
    let json = format!(
        "{{\"code\": {}, \"wall\": {wall}, \"user\": {}, \"sys\": {}, \"rss_kib\": {}}}\n",
        status.code().map_or("null".to_string(), |c| c.to_string()),
        number(usage.map(|u| u.0)),
        number(usage.map(|u| u.1)),
        number(usage.map(|u| u.2)),
    );
    if let Err(e) = std::fs::write(report, json) {
        eprintln!(
            "bench-measure: cannot write {}: {e}",
            report.to_string_lossy()
        );
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
