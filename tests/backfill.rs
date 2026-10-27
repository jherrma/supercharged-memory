//! `backfill` end to end: the real binary, a scrubbed environment, a database of
//! its own and an Ollama that refuses connections.
//!
//! What a unit test cannot show: which stream carries what, the exit codes, that
//! a database is never created for a missing one, and that a second run over the
//! same directory leaves the corpus as it was.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_supercharged-memory");
const NAME: &str = "supercharged-memory";
const SCHEMA: &str = include_str!("../schema.sql");

struct Machine {
    root: PathBuf,
    exe: PathBuf,
}

impl Machine {
    fn new(test: &str, with_db: bool) -> Machine {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("backfill-{test}"));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("app")).unwrap();
        fs::create_dir_all(root.join("home")).unwrap();
        fs::create_dir_all(root.join("notes/sub")).unwrap();
        let root = root.canonicalize().unwrap();
        let exe = root.join("app").join(NAME);
        if fs::hard_link(BIN, &exe).is_err() {
            fs::copy(BIN, &exe).unwrap();
        }
        let m = Machine { root, exe };
        if with_db {
            m.exec(SCHEMA);
        }
        fs::write(m.notes().join("deploy-notes.md"), "deploys need a tag\n").unwrap();
        fs::write(
            m.notes().join("sub/Wal Flag.MD"),
            "turso wants the wal flag",
        )
        .unwrap();
        fs::write(m.notes().join("big.md"), "x".repeat(2001)).unwrap();
        fs::write(m.notes().join("skipped.txt"), "not markdown").unwrap();
        m
    }

    fn notes(&self) -> PathBuf {
        self.root.join("notes")
    }

    fn db(&self) -> PathBuf {
        self.root.join("memory.db")
    }

    fn exec(&self, sql: &str) {
        let p = self.db().to_string_lossy().to_string();
        pollster::block_on(async {
            let db = turso::Builder::new_local(&p)
                .experimental_multiprocess_wal(true)
                .build()
                .await
                .unwrap();
            db.connect().unwrap().execute_batch(sql).await.unwrap();
        });
    }

    fn count(&self) -> i64 {
        let p = self.db().to_string_lossy().to_string();
        pollster::block_on(async {
            let db = turso::Builder::new_local(&p)
                .experimental_multiprocess_wal(true)
                .build()
                .await
                .unwrap();
            let mut rows = db
                .connect()
                .unwrap()
                .query("SELECT count(*) FROM semantic_memory", ())
                .await
                .unwrap();
            let row = rows.next().await.unwrap().unwrap();
            match row.get_value(0).unwrap() {
                turso::Value::Integer(n) => n,
                other => panic!("{other:?}"),
            }
        })
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(&self.exe)
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env("OLLAMA_URL", "http://127.0.0.1:9")
            .env("SUPERCHARGED_MEMORY_TURSO_PATH", self.db())
            .env("BACKUP_DIR", self.root.join("backups"))
            .args(args)
            .output()
            .unwrap()
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn a_directory_imports_without_ollama_and_a_rerun_adds_nothing() {
    let m = Machine::new("import", true);
    let dir = m.notes().to_string_lossy().to_string();
    let o = m.run(&["backfill", "--dir", &dir]);
    assert_eq!(o.status.code(), Some(0), "{}{}", stdout(&o), stderr(&o));
    let out = stdout(&o);
    assert!(
        out.contains("stored deploy-notes.md WITHOUT embedding — Ollama unreachable"),
        "{out}"
    );
    assert!(
        out.contains("stored sub/Wal Flag.MD WITHOUT embedding"),
        "{out}"
    );
    assert!(
        out.contains("SKIP big.md: 2001 chars > 2000 cap — split it manually"),
        "{out}"
    );
    assert!(out.contains("backfilled 2 file(s); skipped 1\n"), "{out}");
    assert!(
        out.contains("2 stored WITHOUT embedding — Ollama unreachable"),
        "{out}"
    );
    assert!(out.contains("embed --missing"), "{out}");
    assert!(
        stderr(&o).starts_with("warning: EMBEDDINGS=on"),
        "{}",
        stderr(&o)
    );
    assert_eq!(m.count(), 2);

    let o = m.run(&["backfill", "--dir", &dir]);
    assert_eq!(o.status.code(), Some(0), "{}{}", stdout(&o), stderr(&o));
    let out = stdout(&o);
    assert!(
        out.contains("SKIP deploy-notes.md: identical memory"),
        "{out}"
    );
    assert!(out.contains("backfilled 0 file(s); skipped 3\n"), "{out}");
    assert_eq!(m.count(), 2);
}

#[test]
fn refusals_exit_two_with_nothing_on_stdout() {
    let m = Machine::new("refusals", true);
    let dir = m.notes().to_string_lossy().to_string();
    let missing = m.root.join("nowhere").to_string_lossy().to_string();
    for args in [
        vec!["backfill", "--dir", &missing],
        vec!["backfill", "--dir", &dir, "--glob", "*.rst"],
        vec!["backfill", "--dir", &dir, "--category", "baseline"],
        vec!["backfill", "--dir", &dir, "--category", "gotcha"],
    ] {
        let o = m.run(&args);
        assert_eq!(o.status.code(), Some(2), "{args:?}: {}", stderr(&o));
        assert_eq!(stdout(&o), "", "{args:?}");
        assert!(
            stderr(&o).starts_with("refused: "),
            "{args:?}: {}",
            stderr(&o)
        );
    }
    assert_eq!(m.count(), 0);
}

#[test]
fn a_missing_database_is_refused_and_none_is_created() {
    let m = Machine::new("no-db", false);
    let dir = m.notes().to_string_lossy().to_string();
    let o = m.run(&["backfill", "--dir", &dir]);
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    assert!(stderr(&o).contains("memory DB missing"), "{}", stderr(&o));
    assert!(!m.db().exists());
}

#[test]
fn a_wrong_invocation_is_a_usage_error() {
    let m = Machine::new("usage", true);
    let dir = m.notes().to_string_lossy().to_string();
    for args in [
        vec!["backfill"],
        vec!["backfill", "--dir", &dir, "--table", "instructions"],
        vec!["backfill", "--dir", &dir, "--force"],
    ] {
        let o = m.run(&args);
        assert_eq!(o.status.code(), Some(64), "{args:?}: {}", stderr(&o));
    }
    assert_eq!(m.count(), 0);
}
