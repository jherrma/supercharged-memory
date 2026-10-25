//! The `.env` beside the binary, end to end: the real executable, run from a
//! directory of its own, with a scrubbed environment.
//!
//! These cover what a unit test cannot: that the file is found next to the
//! binary the operating system actually ran -- through a symlink, too -- and that
//! every command sees it before doing anything.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_supercharged-memory");
const NAME: &str = "supercharged-memory";

/// A fresh directory under cargo's per-test scratch space, which sits on the
/// same file system as the built binary so it can be hard-linked rather than
/// copied.
fn scratch(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("dotenv-{test}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Install the binary into `dir`, as a user would: its own directory, its own
/// `.env` beside it.
fn install(dir: &Path) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let exe = dir.join(NAME);
    if fs::hard_link(BIN, &exe).is_err() {
        fs::copy(BIN, &exe).unwrap();
    }
    exe
}

/// Run with nothing inherited but what the test sets: no stray
/// `SUPERCHARGED_MEMORY_TURSO_PATH` from the developer's shell, and an Ollama
/// address that refuses at once.
fn run(exe: &Path, home: &Path, vars: &[(&str, &str)], args: &[&str]) -> Output {
    let mut cmd = Command::new(exe);
    cmd.env_clear()
        .env("HOME", home)
        .env("OLLAMA_URL", "http://127.0.0.1:9")
        .args(args);
    for (k, v) in vars {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn config_line(o: &Output, key: &str) -> String {
    stdout(o)
        .lines()
        .find(|l| l.starts_with(&format!("{key} ")))
        .unwrap_or_else(|| panic!("no {key} line in:\n{}\n{}", stdout(o), stderr(o)))
        .to_string()
}

#[cfg(unix)]
#[test]
fn through_a_symlink_the_dotenv_beside_the_target_is_read() {
    let dir = scratch("symlink");
    let exe = install(&dir.join("real"));
    fs::write(
        dir.join("real/.env"),
        "SUPERCHARGED_MEMORY_TURSO_PATH=/from/the/real/dir.db\n",
    )
    .unwrap();
    let link_dir = dir.join("bin");
    fs::create_dir_all(&link_dir).unwrap();
    std::os::unix::fs::symlink(&exe, link_dir.join(NAME)).unwrap();
    fs::write(
        link_dir.join(".env"),
        "SUPERCHARGED_MEMORY_TURSO_PATH=/decoy/beside/the/link.db\n",
    )
    .unwrap();

    let out = run(&link_dir.join(NAME), &dir, &[], &["config"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let line = config_line(&out, "SUPERCHARGED_MEMORY_TURSO_PATH");
    assert!(line.contains("/from/the/real/dir.db"), "{line}");
    assert!(line.ends_with("[.env]"), "{line}");
}

#[test]
fn a_malformed_dotenv_makes_every_command_refuse() {
    let dir = scratch("malformed");
    let exe = install(&dir.join("app"));
    fs::write(
        dir.join("app/.env"),
        "# fine\nSUPERCHARGED_MEMORY_TURSO_PATH=/x.db\nBACKUP_DIR=/two words\n",
    )
    .unwrap();
    for args in [
        &["status"][..],
        &["config"],
        &["recall", "anything"],
        &["recall", "--baseline"],
        &["remember", "--table", "semantic", "--text", "x"],
        &["coworkers"],
        &["backup"],
    ] {
        let out = run(&exe, &dir, &[], args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", stderr(&out));
        let err = stderr(&out);
        assert!(err.starts_with("refused: "), "{args:?}: {err}");
        assert!(
            err.contains(".env") && err.contains("line 3"),
            "{args:?}: {err}"
        );
        assert!(stdout(&out).is_empty(), "{args:?} printed {}", stdout(&out));
    }
}

#[test]
fn config_labels_each_source() {
    let dir = scratch("sources");
    let exe = install(&dir.join("app"));

    // Built-in default: no .env, no variable.
    let out = run(&exe, &dir, &[], &["config"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let line = config_line(&out, "SUPERCHARGED_MEMORY_TURSO_PATH");
    let xdg = dir.join(".local/share/turso/supercharged-memory.db");
    assert!(line.contains(&xdg.display().to_string()), "{line}");
    assert!(line.ends_with("[default]"), "{line}");

    // The .env.
    fs::write(
        dir.join("app/.env"),
        "SUPERCHARGED_MEMORY_TURSO_PATH=\"/dot env/mem.db\"\n",
    )
    .unwrap();
    let out = run(&exe, &dir, &[], &["config"]);
    let line = config_line(&out, "SUPERCHARGED_MEMORY_TURSO_PATH");
    assert!(
        line.contains("/dot env/mem.db") && line.ends_with("[.env]"),
        "{line}"
    );

    // The real environment wins, and says what it overrides.
    let out = run(
        &exe,
        &dir,
        &[("SUPERCHARGED_MEMORY_TURSO_PATH", "/real/env.db")],
        &["config"],
    );
    let line = config_line(&out, "SUPERCHARGED_MEMORY_TURSO_PATH");
    assert!(line.contains("/real/env.db"), "{line}");
    assert!(line.contains("[env"), "{line}");
    assert!(
        line.contains("/dot env/mem.db"),
        "the overridden value is shown: {line}"
    );

    // An empty real variable counts as unset, so the .env applies.
    let out = run(
        &exe,
        &dir,
        &[("SUPERCHARGED_MEMORY_TURSO_PATH", "")],
        &["config"],
    );
    let line = config_line(&out, "SUPERCHARGED_MEMORY_TURSO_PATH");
    assert!(line.ends_with("[.env]"), "{line}");
}

#[test]
fn status_without_dotenv_or_database_names_both() {
    let dir = scratch("missing");
    let exe = install(&dir.join("app"));
    let out = run(&exe, &dir, &[], &["status"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.starts_with("MISSING\n"), "{text}");
    let xdg = dir.join(".local/share/turso/supercharged-memory.db");
    assert!(text.contains(&xdg.display().to_string()), "{text}");
    assert!(
        text.contains(&format!(
            "no .env next to {}",
            exe.canonicalize().unwrap().display()
        )),
        "{text}"
    );
    assert!(!xdg.exists(), "status must never create the database");
}

#[test]
fn an_unknown_key_warns_and_the_command_still_runs() {
    let dir = scratch("unknown-key");
    let exe = install(&dir.join("app"));
    fs::write(dir.join("app/.env"), "FROM_A_NEWER_BINARY=1\n").unwrap();
    let out = run(&exe, &dir, &[], &["config"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("FROM_A_NEWER_BINARY"),
        "{}",
        stderr(&out)
    );
    assert!(stderr(&out).starts_with("warning: "), "{}", stderr(&out));
}
