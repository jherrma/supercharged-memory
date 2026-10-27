//! `eval` end to end: the real binary, installed into a directory of its own,
//! with a scrubbed environment and a database of its own. Ollama is pointed at
//! a port that refuses, so what is shown here is what works without it:
//! validation and the case writes -- and that the scoring modes refuse instead
//! of guessing.
//!
//! What a unit test cannot show: the exit codes and which stream carries what,
//! as a shell sees them.

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
    /// The binary in `<root>/app` and a database at `<root>/memory.db` holding
    /// three semantic facts, created on known dates, none with a vector.
    fn new(test: &str) -> Machine {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("eval-{test}"));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("app")).unwrap();
        fs::create_dir_all(root.join("home")).unwrap();
        let root = root.canonicalize().unwrap();
        let exe = root.join("app").join(NAME);
        if fs::hard_link(BIN, &exe).is_err() {
            fs::copy(BIN, &exe).unwrap();
        }
        let m = Machine { root, exe };
        m.exec(&format!(
            "{SCHEMA}
             INSERT INTO semantic_memory (category, topic, memory_text, created_at) VALUES
                 ('reference', 'a', 'first fact', '2026-01-01 00:00:00'),
                 ('reference', 'b', 'second fact', '2026-01-02 00:00:00'),
                 ('reference', 'c', 'third fact', '2026-01-03 00:00:00');"
        ));
        m
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

    /// One text value from the database.
    fn text(&self, sql: &str) -> String {
        let p = self.db().to_string_lossy().to_string();
        pollster::block_on(async {
            let db = turso::Builder::new_local(&p)
                .experimental_multiprocess_wal(true)
                .build()
                .await
                .unwrap();
            let conn = db.connect().unwrap();
            let mut rows = conn.query(sql, ()).await.unwrap();
            match rows.next().await.unwrap().unwrap().get_value(0).unwrap() {
                turso::Value::Text(s) => s,
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

fn ok(o: &Output) -> String {
    assert!(o.status.success(), "{}{}", stdout(o), stderr(o));
    stdout(o)
}

fn refused(o: &Output) -> String {
    assert_eq!(o.status.code(), Some(2), "{}{}", stdout(o), stderr(o));
    assert_eq!(stdout(o), "");
    let err = stderr(o);
    assert!(err.starts_with("refused: "), "{err}");
    err
}

fn add_case(m: &Machine, id: &str, class: &str, expect: &str) -> Output {
    m.run(&[
        "eval",
        "--add-case",
        "--id",
        id,
        "--class",
        class,
        "--table",
        "semantic",
        "--query",
        "a query a person would type",
        "--expect",
        expect,
    ])
}

/// Author, supersede, validate, repoint, validate again, retire: the D6
/// loop with no raw SQL, and no Ollama.
#[test]
fn the_eval_set_is_authored_validated_repointed_and_retired_without_ollama() {
    let m = Machine::new("lifecycle");
    let empty = ok(&m.run(&["eval", "--validate"]));
    assert!(empty.contains("nothing to validate"), "{empty}");
    assert_eq!(ok(&m.run(&["eval", "--last-run"])), "none\n");

    ok(&add_case(&m, "s01", "semantic", "1,2"));
    ok(&add_case(&m, "s02", "semantic", "3"));
    assert_eq!(
        m.text("SELECT expect_stamps FROM eval_cases WHERE id = 's01';"),
        "2026-01-01 00:00:00,2026-01-02 00:00:00",
        "stamps come from the targets"
    );
    let clean = ok(&m.run(&["eval", "--validate"]));
    assert!(
        clean.contains("all cases point at live, current rows"),
        "{clean}"
    );

    // Row 3 is merged into a new row 4.
    m.exec(
        "INSERT INTO semantic_memory (category, topic, memory_text, created_at)
             VALUES ('reference', 'd', 'the merged fact', '2026-02-01 00:00:00');
         UPDATE semantic_memory SET superseded_by = 4 WHERE id = 3;",
    );
    let broken = m.run(&["eval", "--validate"]);
    assert_eq!(
        broken.status.code(),
        Some(1),
        "{}{}",
        stdout(&broken),
        stderr(&broken)
    );
    let text = stdout(&broken);
    assert!(
        text.contains("s02    [semantic] NO VALID TARGET LEFT"),
        "{text}"
    );
    assert!(text.contains("3 -> 4 (superseded; repoint)"), "{text}");
    assert!(
        stderr(&broken).starts_with("error: "),
        "{}",
        stderr(&broken)
    );

    ok(&m.run(&["eval", "--repoint", "s02", "4"]));
    assert_eq!(
        m.text("SELECT expect_stamps FROM eval_cases WHERE id = 's02';"),
        "2026-02-01 00:00:00"
    );
    ok(&m.run(&["eval", "--validate"]));

    let out = ok(&m.run(&["eval", "--retire-case", "s01"]));
    assert!(out.contains("nothing was deleted"), "{out}");
    assert_eq!(
        m.text("SELECT count(*) || '/' || count(retired_at) FROM eval_cases;"),
        "2/1"
    );
}

#[test]
fn what_cannot_be_done_is_refused_with_exit_two_and_nothing_on_stdout() {
    let m = Machine::new("refusals");
    ok(&add_case(&m, "s01", "semantic", "1"));
    refused(&m.run(&["eval", "--retire-case", "nope"]));
    refused(&add_case(&m, "s01", "semantic", "1"));
    refused(&add_case(&m, "s02", "another-class", "1"));
    refused(&add_case(&m, "s02", "semantic", "999"));
    refused(&m.run(&["eval", "--repoint", "s01", "999"]));
    let err = refused(&m.run(&["eval", "--import", "/nonexistent-eval-dir"]));
    assert!(err.contains("eval.jsonl"), "{err}");
    assert_eq!(m.text("SELECT count(*) || '' FROM eval_cases;"), "1");
}

/// Scoring needs embeddings. With Ollama refusing connections the run says so
/// instead of measuring nothing, and records nothing.
#[test]
fn the_scoring_modes_refuse_without_ollama_and_record_nothing() {
    let m = Machine::new("no-ollama");
    ok(&add_case(&m, "s01", "semantic", "1"));
    for mode in [
        vec!["--report"],
        vec!["--sweep", "0.1,0.2"],
        vec!["--variants"],
    ] {
        let mut args = vec!["eval"];
        args.extend(&mode);
        let err = refused(&m.run(&args));
        assert!(
            err.contains("Ollama unreachable at http://127.0.0.1:9"),
            "{mode:?}: {err}"
        );
    }
    assert_eq!(m.text("SELECT count(*) || '' FROM eval_runs;"), "0");
}

#[test]
fn usage_errors_exit_64() {
    let m = Machine::new("usage");
    for args in [
        vec!["eval"],
        vec!["eval", "--validate", "--report"],
        vec!["eval", "--sweep", "a,b"],
        vec!["eval", "--add-case", "--id", "x"],
    ] {
        let o = m.run(&args);
        assert_eq!(o.status.code(), Some(64), "{args:?}: {}", stderr(&o));
    }
}

#[test]
fn a_missing_database_is_refused_and_never_created() {
    let m = Machine::new("missing-db");
    fs::remove_file(m.db()).unwrap();
    let _ = fs::remove_file(m.root.join("memory.db-wal"));
    let err = refused(&m.run(&["eval", "--validate"]));
    assert!(err.contains("memory DB missing"), "{err}");
    assert!(!m.db().exists());
}
