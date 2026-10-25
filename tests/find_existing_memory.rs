//! `find-existing-memory` end to end: the real binary over a tree holding every
//! trap the probe was built around, with no database and no Ollama.
//!
//! The golden file is the Python reference's output over this same tree, with
//! the tree's location replaced by `{ROOT}` and `remember.py` named `remember`.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_supercharged-memory");
const GOLDEN: &str = include_str!("fixtures/find-existing-memory.json");

/// A fresh directory under cargo's per-test scratch space. Mode-000 entries
/// left by an earlier run are opened up first, or they could not be removed.
fn scratch(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("find-existing-{test}"));
    unlock(&dir);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::canonicalize(&dir).unwrap()
}

fn unlock(p: &Path) {
    let Ok(meta) = fs::symlink_metadata(p) else {
        return;
    };
    if meta.file_type().is_symlink() {
        return;
    }
    let mode = if meta.is_dir() { 0o755 } else { 0o644 };
    let _ = fs::set_permissions(p, fs::Permissions::from_mode(mode));
    if meta.is_dir()
        && let Ok(entries) = fs::read_dir(p)
    {
        for e in entries.flatten() {
            unlock(&e.path());
        }
    }
}

fn write(p: &Path, text: &str) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, text).unwrap();
}

fn lock(p: &Path) {
    fs::set_permissions(p, fs::Permissions::from_mode(0o000)).unwrap();
}

/// The reference tree: a mode-000 directory and file, an ancestor symlink, a
/// directory loop, a looping and a broken file symlink, `notes.MD`/`other.Md`,
/// nested memory, protected directories at the root (and a same-named topic
/// directory deeper), a symlinked directory outside the tree, project memory,
/// a project memory directory symlinked back at the global one, indexes, an
/// empty file, files over the budget and over the cap, CRLF text, German text,
/// and a CLAUDE.md holding a legacy managed block.
fn reference_tree(root: &Path) {
    let outside = root.with_file_name("claude-outside");
    write(&outside.join("real.md"), "outside fact\n");

    let m = root.join("memory");
    for d in [
        "team/nested",
        "topic/agents",
        "agents/sub",
        ".git",
        "noaccess",
        "a",
    ] {
        fs::create_dir_all(m.join(d)).unwrap();
    }
    write(&m.join("fact.md"), "---\nname: x\n---\nA global fact.\n");
    write(&m.join("notes.MD"), "Upper case suffix.\n");
    write(&m.join("other.Md"), "Mixed case suffix.\n");
    write(&m.join("MEMORY.md"), "- [fact](fact.md)\n");
    write(&m.join("empty.md"), "   \n\t\n");
    write(
        &m.join("crlf.md"),
        "line one\r\nline two\r\nline three\rend\r\n",
    );
    write(&m.join("german.md"), "Größe ändern überprüfen — äöü ß\n");
    write(&m.join("readme.txt"), "not memory\n");
    write(&m.join("team/nested/deep.md"), "deep\n");
    write(&m.join("topic/agents/kept.md"), "topic agents are memory\n");
    write(&m.join("agents/one.md"), "subagent\n");
    write(&m.join("agents/sub/two.md"), "subagent\n");
    write(&m.join(".git/HEAD.md"), "git\n");
    write(&m.join("noaccess/inside.md"), "hidden\n");
    write(&m.join("budget.md"), &format!("{}\n", "x".repeat(1900)));
    write(&m.join("cap.md"), &format!("{}\n", "y".repeat(2100)));
    write(&m.join("locked.md"), "secret\n");
    symlink("..", m.join("loop")).unwrap();
    symlink("..", m.join("a/back")).unwrap();
    symlink("/nonexistent/target.md", m.join("broken.md")).unwrap();
    symlink("selfloop.md", m.join("selfloop.md")).unwrap();
    symlink(&outside, m.join("linkdir")).unwrap();
    lock(&m.join("noaccess"));
    lock(&m.join("locked.md"));

    let p = root.join("projects");
    for d in [
        "-home-u-proj/memory/team",
        "-home-u-other",
        "-home-u-third/memory",
    ] {
        fs::create_dir_all(p.join(d)).unwrap();
    }
    write(&p.join("-home-u-proj/memory/pfact.md"), "project fact\n");
    write(
        &p.join("-home-u-proj/memory/team/pdeep.md"),
        "nested project fact\n",
    );
    write(
        &p.join("-home-u-proj/memory/MEMORY.md"),
        "- [pfact](pfact.md)\n",
    );
    write(&p.join("-home-u-other/session.jsonl"), "{}\n");
    symlink("../../../memory", p.join("-home-u-third/memory/global")).unwrap();

    write(
        &root.join("CLAUDE.md"),
        "# My rules\n\nAlways use pnpm.\n\n\
         <!-- BEGIN agentic-memory (managed by install-claude-md.sh) -->\n\
         <!-- supercharged-memory: synced-at abc123 -->\n\
         # Long-term memory — Turso vector DB (supercharged-memory)\n\
         lots of managed text\n\
         <!-- END agentic-memory -->\n\nTrailing user note.\n",
    );
}

/// Run with nothing inherited: a database path that does not exist and an
/// Ollama address that refuses at once.
fn run(dir: &Path, config: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .env_clear()
        .env("HOME", dir.join("home"))
        .env("CLAUDE_CONFIG_DIR", config)
        .env("SUPERCHARGED_MEMORY_TURSO_PATH", dir.join("db/mem.db"))
        .env("OLLAMA_URL", "http://127.0.0.1:9")
        .arg("find-existing-memory")
        .args(args)
        .output()
        .unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// Mode 000 means nothing to a privileged process, and the golden file records
/// what an ordinary user sees.
fn mode_000_denies(root: &Path) -> bool {
    let denied = fs::read_dir(root.join("memory/noaccess")).is_err();
    if !denied {
        eprintln!("mode 000 does not deny access here (root?): golden comparison not exercised");
    }
    denied
}

#[test]
fn the_reference_tree_reports_what_the_python_probe_reports() {
    let dir = scratch("golden");
    let root = dir.join("claude");
    reference_tree(&root);
    if !mode_000_denies(&root) {
        return;
    }
    let o = run(&dir, &root, &[]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let want = GOLDEN.replace("{ROOT}", root.to_str().unwrap());
    let got = stdout(&o);
    if got != want {
        let first = got
            .lines()
            .zip(want.lines())
            .position(|(g, w)| g != w)
            .unwrap_or(0);
        panic!(
            "output differs from the reference from line {} on:\n got: {:?}\nwant: {:?}\n\nfull output:\n{got}",
            first + 1,
            got.lines().nth(first),
            want.lines().nth(first),
        );
    }
}

/// The probe runs before either dependency is proven to work, so it must need
/// neither -- and must not create the database it was pointed at.
#[test]
fn runs_with_no_database_and_no_ollama() {
    let dir = scratch("no-deps");
    let root = dir.join("claude");
    write(&root.join("memory/fact.md"), "a fact\n");
    let o = run(&dir, &root, &[]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stderr(&o), "");
    let report: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(report["n_memory_files"], 1);
    assert_eq!(report["nothing_to_do"], false);
    assert_eq!(report["config_dir"], root.to_str().unwrap());
    assert!(
        !dir.join("db").exists(),
        "no database directory was created"
    );
}

#[test]
fn quiet_prints_one_word_and_exits_zero() {
    let dir = scratch("quiet");
    let root = dir.join("claude");
    fs::create_dir_all(&root).unwrap();

    let o = run(&dir, &root, &["--quiet"]);
    assert_eq!((o.status.code(), stdout(&o)), (Some(0), "nothing\n".into()));

    // A CLAUDE.md alone is context, not memory to import.
    write(&root.join("CLAUDE.md"), "Always use pnpm.\n");
    let o = run(&dir, &root, &["--quiet"]);
    assert_eq!((o.status.code(), stdout(&o)), (Some(0), "nothing\n".into()));

    write(&root.join("memory/fact.md"), "a fact\n");
    let o = run(&dir, &root, &["--quiet"]);
    assert_eq!((o.status.code(), stdout(&o)), (Some(0), "found\n".into()));
}
