//! `restore` end to end: the real binary, installed into a directory of its
//! own, with a scrubbed environment, a database and a BACKUP_DIR of its own.
//!
//! What a unit test cannot show: the exit codes and which stream carries what,
//! as a shell sees them.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_supercharged-memory");
const NAME: &str = "supercharged-memory";
const SCHEMA: &str = include_str!("../schema.sql");
const TURSODB_DUMP: &str = include_str!("fixtures/tursodb-dump.sql");
const TEXT: &str = "a memory\nover 'two' lines; with \"quotes\" and ümlauts";

struct Machine {
    root: PathBuf,
    exe: PathBuf,
}

impl Machine {
    /// The binary in `<root>/app`, a database at `<root>/memory.db` holding one
    /// row in each memory table, and dumps in `<root>/backups`.
    fn new(test: &str) -> Machine {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("restore-{test}"));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("app")).unwrap();
        fs::create_dir_all(root.join("home")).unwrap();
        let root = root.canonicalize().unwrap();
        let exe = root.join("app").join(NAME);
        if fs::hard_link(BIN, &exe).is_err() {
            fs::copy(BIN, &exe).unwrap();
        }
        let m = Machine { root, exe };
        let p = m.db().to_string_lossy().to_string();
        let sql = format!(
            "{SCHEMA}
             INSERT INTO semantic_memory (category, memory_text) VALUES ('reference', '{}');
             INSERT INTO episodic_memory (event_type, importance, memory_text) \
                 VALUES ('note', 'routine', 'an event');",
            TEXT.replace('\'', "''")
        );
        pollster::block_on(async {
            let db = turso::Builder::new_local(&p)
                .experimental_multiprocess_wal(true)
                .build()
                .await
                .unwrap();
            db.connect().unwrap().execute_batch(&sql).await.unwrap();
        });
        m
    }

    fn db(&self) -> PathBuf {
        self.root.join("memory.db")
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

    fn texts(&self, db: &Path, sql: &str) -> Vec<String> {
        let p = db.to_string_lossy().to_string();
        pollster::block_on(async {
            let db = turso::Builder::new_local(&p)
                .experimental_multiprocess_wal(true)
                .build()
                .await
                .unwrap();
            let conn = db.connect().unwrap();
            let mut rows = conn.query(sql, ()).await.unwrap();
            let mut out = Vec::new();
            while let Some(row) = rows.next().await.unwrap() {
                out.push(match row.get_value(0).unwrap() {
                    turso::Value::Text(t) => t,
                    other => format!("{other:?}"),
                });
            }
            out
        })
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn path(p: &Path) -> &str {
    p.to_str().unwrap()
}

/// `backup`, then `restore --out` with no dump named: the newest backup comes
/// back, counted, and a second run onto the same file is refused.
#[test]
fn a_backup_restores_with_the_binary_alone_and_is_never_restored_over() {
    let m = Machine::new("round-trip");
    let out = m.run(&["backup"]);
    assert!(out.status.success(), "{}", stderr(&out));

    let restored = m.root.join("restored.db");
    let out = m.run(&["restore", "--out", path(&restored)]);
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    let printed = stdout(&out);
    assert!(printed.contains("newest in BACKUP_DIR"), "{printed}");
    assert!(printed.contains("RESTORE OK"), "{printed}");
    assert_eq!(
        m.texts(&restored, "SELECT memory_text FROM semantic_memory"),
        vec![TEXT.to_string()]
    );

    let out = m.run(&["restore", "--out", path(&restored)]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).starts_with("refused: "), "{}", stderr(&out));
    assert_eq!(stdout(&out), "");
}

/// A dump cut short exits 1: the per-table mismatch on stdout, the verdict on
/// stderr, and no file at the target.
#[test]
fn a_truncated_dump_exits_non_zero_with_the_mismatch_and_no_file() {
    let m = Machine::new("truncated");
    let cut = m.root.join("cut.sql");
    let at = TURSODB_DUMP
        .find("INSERT INTO \"episodic_memory\"")
        .unwrap()
        + 20;
    fs::write(&cut, &TURSODB_DUMP[..at]).unwrap();
    let restored = m.root.join("restored.db");
    let out = m.run(&["restore", "--dump", path(&cut), "--out", path(&restored)]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stdout(&out).contains("<<< MISMATCH"), "{}", stdout(&out));
    assert!(
        stderr(&out).starts_with("error: RESTORE INCOMPLETE"),
        "{}",
        stderr(&out)
    );
    assert!(!restored.exists());
}

/// A real `tursodb .dump`, the legacy format every existing backup has, goes
/// through the binary too.
#[test]
fn a_tursodb_dump_restores_through_the_binary() {
    let m = Machine::new("tursodb");
    let dump = m.root.join("legacy.sql");
    fs::write(&dump, TURSODB_DUMP).unwrap();
    let restored = m.root.join("restored.db");
    let out = m.run(&["restore", "--dump", path(&dump), "--out", path(&restored)]);
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    assert!(stdout(&out).contains("skipped 2"), "{}", stdout(&out));
    assert_eq!(
        m.texts(
            &restored,
            "SELECT memory_text FROM instructions ORDER BY id"
        ),
        vec![
            "always\nrun the tests; then 'lint'".to_string(),
            "an embedded instruction".to_string()
        ]
    );
}
