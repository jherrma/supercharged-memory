//! `backup` end to end: the real binary, installed into a directory of its own,
//! with a scrubbed environment and a database of its own.
//!
//! What a unit test cannot show: what a shell actually captures from stdout,
//! and that a run whose stdout goes nowhere still writes its dump.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_supercharged-memory");
const NAME: &str = "supercharged-memory";
const SCHEMA: &str = include_str!("../schema.sql");
const TEXT: &str = "a memory\nover 'two' lines; with \"quotes\" and ümlauts";

struct Machine {
    root: PathBuf,
    exe: PathBuf,
}

impl Machine {
    /// The binary in `<root>/app`, a database at `<root>/memory.db` holding one
    /// row in each memory table, and dumps going to `<root>/backups`.
    fn new(test: &str) -> Machine {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("backup-{test}"));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("app")).unwrap();
        fs::create_dir_all(root.join("home")).unwrap();
        let root = root.canonicalize().unwrap();
        let exe = root.join("app").join(NAME);
        if fs::hard_link(BIN, &exe).is_err() {
            fs::copy(BIN, &exe).unwrap();
        }
        let m = Machine { root, exe };
        m.exec(
            &m.db(),
            &format!(
                "{SCHEMA}
                 INSERT INTO semantic_memory (category, memory_text) VALUES ('reference', '{}');
                 INSERT INTO episodic_memory (event_type, importance, memory_text) \
                     VALUES ('note', 'routine', 'an event');",
                TEXT.replace('\'', "''")
            ),
        );
        m
    }

    fn db(&self) -> PathBuf {
        self.root.join("memory.db")
    }

    fn backups(&self) -> PathBuf {
        self.root.join("backups")
    }

    fn exec(&self, db: &Path, sql: &str) {
        let p = db.to_string_lossy().to_string();
        pollster::block_on(async {
            let db = turso::Builder::new_local(&p)
                .experimental_multiprocess_wal(true)
                .build()
                .await
                .unwrap();
            db.connect().unwrap().execute_batch(sql).await.unwrap();
        });
    }

    fn query(&self, db: &Path, sql: &str) -> Vec<turso::Value> {
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
                out.push(row.get_value(0).unwrap());
            }
            out
        })
    }

    fn backup(&self) -> Command {
        let mut cmd = Command::new(&self.exe);
        cmd.env_clear()
            .env("HOME", self.root.join("home"))
            .env("OLLAMA_URL", "http://127.0.0.1:9")
            .env("SUPERCHARGED_MEMORY_TURSO_PATH", self.db())
            .env("BACKUP_DIR", self.backups())
            .arg("backup");
        cmd
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn daily_dumps(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("-supercharged-memory.sql.gz")
        })
        .collect()
}

/// `dump=$(supercharged-memory backup)`: the capture is the path, it exists,
/// and it restores.
#[test]
fn the_captured_stdout_is_the_dump_path_and_it_restores() {
    let m = Machine::new("capture");
    let out = m.backup().output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));

    let captured = stdout(&out);
    assert!(captured.ends_with('\n'), "{captured:?}");
    assert_eq!(captured.lines().count(), 1, "{captured:?}");
    let path = PathBuf::from(captured.trim_end_matches('\n'));
    assert!(path.is_absolute(), "{captured:?}");
    assert_eq!(path.parent(), Some(m.backups().as_path()));
    assert_eq!(daily_dumps(&m.backups()), vec![path.clone()]);
    assert!(stderr(&out).contains("backup:"), "{}", stderr(&out));

    let mut sql = String::new();
    flate2::read::GzDecoder::new(fs::File::open(&path).unwrap())
        .read_to_string(&mut sql)
        .unwrap();
    let restored = m.root.join("restored.db");
    m.exec(&restored, &sql);
    assert_eq!(
        m.query(&restored, "SELECT memory_text FROM semantic_memory"),
        vec![turso::Value::Text(TEXT.into())]
    );
    assert_eq!(
        m.query(&restored, "SELECT count(*) FROM episodic_memory"),
        vec![turso::Value::Integer(1)]
    );
}

/// `supercharged-memory backup > /dev/null`: the dump is still written and the
/// diagnostics still reach the terminal.
#[test]
fn stdout_sent_nowhere_still_writes_the_dump_and_the_diagnostics() {
    let m = Machine::new("devnull");
    let out = m
        .backup()
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(
        err.contains("backup:") && err.contains("semantic_memory"),
        "{err}"
    );
    assert_eq!(daily_dumps(&m.backups()).len(), 1);
}

/// A failure leaves stdout empty and the exit status non-zero, so a capture
/// is an empty string, never a diagnostic taken for a file name.
#[test]
fn a_forced_failure_prints_nothing_on_stdout() {
    let m = Machine::new("failure");

    // Refused: no database at the configured path.
    let out = m
        .backup()
        .env("SUPERCHARGED_MEMORY_TURSO_PATH", m.root.join("gone.db"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert_eq!(stdout(&out), "");
    assert!(stderr(&out).starts_with("refused: "), "{}", stderr(&out));
    assert!(!m.root.join("gone.db").exists());

    // Failed: BACKUP_DIR is a file, so nothing can be written there.
    fs::write(m.backups(), b"in the way").unwrap();
    let out = m.backup().output().unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert_eq!(stdout(&out), "");
    assert!(stderr(&out).starts_with("error: "), "{}", stderr(&out));
    assert_eq!(fs::read(m.backups()).unwrap(), b"in the way");
}
