//! `runbook` end to end: the real binary, with no database, no .env and no Ollama.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_supercharged-memory");

const FILES: [(&str, &str); 6] = [
    ("setup", "SETUP.md"),
    ("update", "UPDATE.md"),
    ("sleep", "SLEEP.md"),
    ("deep-sleep", "DEEP-SLEEP.md"),
    ("migrate", "MIGRATE-EXISTING-MEMORY.md"),
    ("new-coworker", "NEW-COWORKER.md"),
];

fn scratch(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("runbook-{test}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Nothing inherited: a database path that does not exist and an Ollama
/// address that refuses at once.
fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .env_clear()
        .env("HOME", dir.join("home"))
        .env("SUPERCHARGED_MEMORY_TURSO_PATH", dir.join("db/mem.db"))
        .env("OLLAMA_URL", "http://127.0.0.1:9")
        .arg("runbook")
        .args(args)
        .output()
        .unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn each_name_prints_its_source_file_and_nothing_else() {
    let dir = scratch("print");
    for (name, file) in FILES {
        let out = run(&dir, &[name]);
        assert_eq!(out.status.code(), Some(0), "{name}: {}", text(&out.stderr));
        let source = fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("instructions")
                .join(file),
        )
        .unwrap();
        // The only change is the binary's own path, written as a command line
        // writes it, where the source says {{EXE}}.
        let exe = fs::canonicalize(BIN).unwrap().display().to_string();
        let exe = if exe.contains(char::is_whitespace) {
            format!("\"{exe}\"")
        } else {
            exe
        };
        assert!(source.contains("{{EXE}}"), "{file} names no command");
        let want = source.replace("{{EXE}}", &exe);
        assert_eq!(text(&out.stdout), want, "{name} differs from {file}");
        assert!(out.stderr.is_empty(), "{name}: {}", text(&out.stderr));
    }
    assert!(
        !dir.join("db").exists(),
        "a runbook must not create a database"
    );
}

#[test]
fn an_unknown_name_is_refused_and_prints_nothing() {
    let dir = scratch("unknown");
    let out = run(&dir, &["slepe"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    let err = text(&out.stderr);
    assert!(err.starts_with("refused: "), "{err}");
    for (name, _) in FILES {
        assert!(err.contains(name), "{err}");
    }
}

#[test]
fn without_a_name_the_runbooks_are_listed_one_per_line() {
    let dir = scratch("list");
    let out = run(&dir, &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let listing = text(&out.stdout);
    let first: Vec<&str> = listing
        .lines()
        .map(|l| l.split_whitespace().next().unwrap())
        .collect();
    let want: Vec<&str> = FILES.iter().map(|(n, _)| *n).collect();
    assert_eq!(first, want, "{listing}");
}

#[test]
fn two_names_are_a_usage_error() {
    let dir = scratch("two");
    let out = run(&dir, &["sleep", "setup"]);
    assert_eq!(out.status.code(), Some(64));
    assert!(out.stdout.is_empty());
}
