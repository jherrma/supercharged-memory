//! The suite's own runner: every test in parallel, each in a machine of its
//! own, with the real home checked before the first and after the last.
//!
//! It accepts the libtest arguments cargo passes through: positional filters,
//! `--skip <filter>`, `--exact`, `--list`, `--ignored` (there are no ignored
//! tests, so it runs none) and `--test-threads <n>`; the output-format flags
//! are accepted and ignored. `--differences` prints the difference table as
//! Markdown instead of running anything.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Instant, SystemTime};

use crate::differences;
use crate::harness::Ctx;

/// One end-to-end test.
#[derive(Clone, Copy)]
pub struct Test {
    pub name: &'static str,
    pub run: fn(&Ctx),
}

/// `tests![a, b]` lists the functions `a` and `b` of the calling module as
/// tests named `<module>::<function>`.
#[macro_export]
macro_rules! tests {
    ($($f:ident),* $(,)?) => {
        &[$($crate::runner::Test { name: concat!(module_path!(), "::", stringify!($f)), run: $f }),*]
    };
}

thread_local! {
    static PANIC: RefCell<Option<String>> = const { RefCell::new(None) };
}

struct Args {
    filters: Vec<String>,
    skips: Vec<String>,
    exact: bool,
    list: bool,
    differences: bool,
    ignored: bool,
    threads: Option<usize>,
}

fn parse_args() -> Args {
    let mut a = Args {
        filters: Vec::new(),
        skips: Vec::new(),
        exact: false,
        list: false,
        differences: false,
        ignored: false,
        threads: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--list" => a.list = true,
            "--differences" => a.differences = true,
            "--exact" => a.exact = true,
            "--ignored" => a.ignored = true,
            "--skip" => a.skips.extend(it.next()),
            "--test-threads" => a.threads = it.next().and_then(|n| n.parse().ok()),
            "--color" | "--format" | "-Z" | "--logfile" | "--report-time" => {
                it.next();
            }
            s if s.starts_with("--test-threads=") => a.threads = s[15..].parse().ok(),
            s if s.starts_with('-') => {}
            s => a.filters.push(s.to_string()),
        }
    }
    a
}

fn selected(args: &Args, name: &str) -> bool {
    let matches = |f: &String| {
        if args.exact {
            name == f
        } else {
            name.contains(f.as_str())
        }
    };
    (args.filters.is_empty() || args.filters.iter().any(matches))
        && !args.skips.iter().any(|s| name.contains(s.as_str()))
}

enum Outcome {
    Passed,
    Failed(String),
}

pub fn main(tests: Vec<Test>) -> ExitCode {
    let args = parse_args();
    let tests: Vec<Test> = tests
        .into_iter()
        .map(|t| Test {
            name: t.name.split_once("::").map_or(t.name, |(_, rest)| rest),
            run: t.run,
        })
        .filter(|t| selected(&args, t.name))
        .collect();
    if args.list {
        for t in &tests {
            println!("{}: test", t.name);
        }
        return ExitCode::SUCCESS;
    }
    if args.differences {
        print!("{}", differences::markdown());
        return ExitCode::SUCCESS;
    }
    if args.ignored {
        println!("running 0 tests (no end-to-end test is ignored)");
        return ExitCode::SUCCESS;
    }

    let before = snapshot();
    let ctx = Ctx::new();
    println!("running {} tests", tests.len());
    println!("{}", ctx.reference_line());

    panic::set_hook(Box::new(|info| {
        let text = info.to_string();
        PANIC.with(|p| *p.borrow_mut() = Some(text));
    }));
    let workers = args
        .threads
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()))
        .clamp(1, tests.len().max(1));
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<(&'static str, Outcome)>> = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    let Some(t) = tests.get(i) else { break };
                    let started = Instant::now();
                    let res = panic::catch_unwind(AssertUnwindSafe(|| (t.run)(&ctx)));
                    let outcome = match res {
                        Ok(()) => Outcome::Passed,
                        Err(_) => Outcome::Failed(
                            PANIC
                                .with(|p| p.borrow_mut().take())
                                .unwrap_or_else(|| "panicked".into()),
                        ),
                    };
                    let secs = started.elapsed().as_secs_f64();
                    let word = match outcome {
                        Outcome::Passed => "ok",
                        Outcome::Failed(_) => "FAILED",
                    };
                    println!("test {} ... {word} ({secs:.1}s)", t.name);
                    results.lock().unwrap().push((t.name, outcome));
                }
            });
        }
    });
    let _ = panic::take_hook();

    let mut results = results.into_inner().unwrap();
    results.sort_by_key(|(name, _)| *name);
    let mut failed: Vec<(String, String)> = results
        .iter()
        .filter_map(|(name, o)| match o {
            Outcome::Failed(why) => Some((name.to_string(), why.clone())),
            Outcome::Passed => None,
        })
        .collect();
    // The two whole-run checks below count as tests too.
    let mut ran = results.len();

    // A table entry no comparison exercised is a difference nobody checks any
    // more: it either went away or its test did. Only a whole run can tell.
    let whole_run = args.filters.is_empty() && args.skips.is_empty();
    if whole_run && ctx.python_available() {
        let unused = differences::unexercised(&ctx.exercised());
        let outcome = if unused.is_empty() { "ok" } else { "FAILED" };
        println!("test difference_table_is_exercised ... {outcome}");
        ran += 1;
        if !unused.is_empty() {
            failed.push((
                "difference_table_is_exercised".into(),
                format!(
                    "these entries of the difference table were never asserted: {}",
                    unused.join(", ")
                ),
            ));
        }
    }

    drop(ctx);
    let after = snapshot();
    let changed = compare_snapshots(&before, &after);
    ran += 1;
    println!(
        "test real_home_untouched ... {}",
        if changed.is_empty() { "ok" } else { "FAILED" }
    );
    if !changed.is_empty() {
        failed.push((
            "real_home_untouched".into(),
            format!(
                "the run changed paths outside its machines (if another session wrote its memory \
                 meanwhile, run the suite again):\n  {}",
                changed.join("\n  ")
            ),
        ));
    }

    let skipped = crate::harness::skipped();
    if !skipped.is_empty() {
        println!("\nskipped comparisons ({}):", skipped.len());
        for s in &skipped {
            println!("  SKIPPED {s}");
        }
    }
    for (name, why) in &failed {
        println!("\n---- {name} ----\n{why}");
    }
    let passed = ran - failed.len();
    println!(
        "\ntest result: {}. {passed} passed; {} failed; {} comparisons skipped",
        if failed.is_empty() { "ok" } else { "FAILED" },
        failed.len(),
        skipped.len()
    );
    if failed.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(101)
    }
}

/// Size and modification time of every path the suite must never write,
/// taken from the real environment: the live database and its WAL, the
/// Claude config files, the repository's `backups/` and the binary's default
/// backup directory, a `.env` beside the built binary, and the fixed error
/// file of the reference backup script. A missing path is recorded as missing.
///
/// `-tshm` is left out on purpose: any reader of the live database bumps its
/// modification time, so it would report every other session's `status`.
type Snapshot = BTreeMap<String, Option<(u64, SystemTime)>>;

fn snapshot() -> Snapshot {
    let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
    let home = env("HOME").map(PathBuf::from).unwrap_or_default();
    let data = env("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"));
    let live = env("SUPERCHARGED_MEMORY_TURSO_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| data.join("turso/supercharged-memory.db"));
    let claude = env("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".claude"));
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    let bin_dir = Path::new(env!("CARGO_BIN_EXE_supercharged-memory"))
        .parent()
        .unwrap()
        .to_path_buf();
    let mut files = vec![
        live.clone(),
        PathBuf::from(format!("{}-wal", live.display())),
        claude.join("CLAUDE.md"),
        claude.join("settings.json"),
        bin_dir.join(".env"),
        repo.join(".env"),
        PathBuf::from("/tmp/supercharged-memory-backup.err"),
    ];
    for dir in [
        repo.join("backups"),
        data.join("supercharged-memory/backups"),
        repo.join("scripts/__pycache__"),
    ] {
        files.push(dir.clone());
        if let Ok(entries) = std::fs::read_dir(&dir) {
            files.extend(entries.flatten().map(|e| e.path()));
        }
    }
    files
        .into_iter()
        .map(|p| {
            let meta = std::fs::symlink_metadata(&p).ok();
            let stamp = meta.and_then(|m| Some((m.len(), m.modified().ok()?)));
            (p.display().to_string(), stamp)
        })
        .collect()
}

fn compare_snapshots(before: &Snapshot, after: &Snapshot) -> Vec<String> {
    let mut out = Vec::new();
    for (path, was) in before {
        match after.get(path) {
            Some(now) if now == was => {}
            Some(now) => out.push(format!("{path}: {was:?} -> {now:?}")),
            None => out.push(format!("{path}: vanished from its directory")),
        }
    }
    for path in after.keys().filter(|p| !before.contains_key(*p)) {
        out.push(format!("{path}: appeared"));
    }
    out
}
