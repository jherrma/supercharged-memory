//! `coworkers` end to end: the real binary, installed into a directory of its
//! own, with a scrubbed environment and a database of its own. Ollama is
//! pointed at a port that refuses: coworkers never need it, and the feedback
//! `remember` tags to them is stored without an embedding.
//!
//! What a unit test cannot show: the exit codes and which stream carries what,
//! as a shell sees them, and that `remember --coworker`, `coworkers --load`
//! and `recall --coworker` agree about who sees what.

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
    /// a baseline rule and one global fact.
    fn new(test: &str) -> Machine {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("coworkers-{test}"));
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
                 ('baseline', 'start', 'run status first', '2026-01-01 00:00:00'),
                 ('reference', 'reviews', 'reviews follow the checklist', '2026-01-02 00:00:00');"
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

    /// One integer from the database.
    fn count(&self, sql: &str) -> i64 {
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

/// Add two coworkers, appraise one twice, tag feedback to each, load and
/// recall as each, retire and reactivate -- the writes in the reference's
/// own words.
#[test]
fn a_coworker_is_added_appraised_loaded_retired_and_reactivated_through_the_binary() {
    let m = Machine::new("lifecycle");
    let empty = ok(&m.run(&["coworkers", "--list"]));
    assert!(empty.starts_with("===== coworkers (0) =====\n"), "{empty}");

    for name in ["jeff", "ada"] {
        assert_eq!(
            ok(&m.run(&[
                "coworkers",
                "--add",
                "--name",
                name,
                "--expertise",
                "rust, sql",
                "--personality",
                "terse, sceptical",
            ])),
            format!("added coworker '{name}'\n")
        );
    }
    assert_eq!(
        ok(&m.run(&[
            "coworkers",
            "--appraise",
            "jeff",
            "--trust",
            "trusted",
            "--text",
            "good reviews",
        ])),
        "appraised 'jeff': trust=trusted (first appraisal)\n"
    );

    // Feedback is tagged by `remember`, in the same second as the appraisal
    // or later: either way it is in jeff's window.
    for (name, text) in [
        ("jeff", "be blunter in reviews"),
        ("ada", "ada likes tables"),
    ] {
        ok(&m.run(&[
            "remember",
            "--table",
            "semantic",
            "--category",
            "feedback",
            "--source",
            "user-stated",
            "--coworker",
            name,
            "--text",
            text,
        ]));
    }
    let loaded = ok(&m.run(&["coworkers", "--load", "jeff"]));
    assert!(
        loaded.starts_with("===== coworker 'jeff' =====\n"),
        "{loaded}"
    );
    assert!(loaded.contains("trust_level = trusted\n"), "{loaded}");
    assert!(
        loaded.contains("memory_text = be blunter in reviews\n"),
        "{loaded}"
    );
    assert!(!loaded.contains("ada likes tables"), "{loaded}");
    assert!(
        loaded.contains("Baseline rules stay a hard floor"),
        "{loaded}"
    );

    let recalled = ok(&m.run(&[
        "recall",
        "--coworker",
        "jeff",
        "--table",
        "semantic",
        "blunter tables checklist",
    ]));
    assert!(recalled.contains("be blunter in reviews"), "{recalled}");
    assert!(
        recalled.contains("reviews follow the checklist"),
        "{recalled}"
    );
    assert!(!recalled.contains("ada likes tables"), "{recalled}");

    assert_eq!(
        ok(&m.run(&[
            "coworkers",
            "--appraise",
            "jeff",
            "--trust",
            "autonomous",
            "--text",
            "great reviews",
        ])),
        "appraised 'jeff': trust=autonomous (superseded appraisal 1)\n"
    );
    assert_eq!(
        m.count("SELECT count(*) FROM appraisals WHERE superseded_by IS NULL;"),
        1
    );
    assert_eq!(
        ok(&m.run(&["coworkers", "--set-trust", "jeff", "supervised"])),
        "'jeff' trust_level -> supervised\n"
    );

    assert_eq!(
        ok(&m.run(&["coworkers", "--retire", "jeff"])),
        "'jeff' retired\n"
    );
    let err = refused(&m.run(&["coworkers", "--retire", "jeff"]));
    assert!(err.contains("already retired"), "{err}");
    let loaded = ok(&m.run(&["coworkers", "--load", "jeff"]));
    assert!(
        loaded.starts_with("RETIRED: coworker 'jeff' is retired (active = 0)"),
        "{loaded}"
    );
    let listed = ok(&m.run(&["coworkers", "--list"]));
    assert!(
        listed.starts_with("===== coworkers (2; active = 0 means retired) =====\n"),
        "{listed}"
    );
    assert!(listed.contains("     active = 0\n"), "{listed}");
    assert_eq!(
        ok(&m.run(&["coworkers", "--reactivate", "jeff"])),
        "'jeff' reactivated\n"
    );
    assert_eq!(
        m.count("SELECT count(*) FROM coworkers WHERE active = 1;"),
        2
    );
    assert_eq!(m.count("SELECT count(*) FROM appraisals;"), 2);
}

/// Every refusal: exit 2, `refused:` on stderr, nothing on stdout, nothing
/// written. A missing mode is a usage error.
#[test]
fn each_refusal_exits_2_and_writes_nothing() {
    let m = Machine::new("refusals");
    ok(&m.run(&[
        "coworkers",
        "--add",
        "--name",
        "jeff",
        "--expertise",
        "rust",
        "--personality",
        "terse",
    ]));
    let snapshot = |m: &Machine| {
        (
            m.count("SELECT count(*) FROM coworkers;"),
            m.count("SELECT count(*) FROM appraisals;"),
            m.count("SELECT count(*) FROM coworkers WHERE trust_level = 'supervised';"),
        )
    };
    let before = snapshot(&m);
    let long_name = "n".repeat(65);
    let long_text = "t".repeat(2001);
    for (args, want) in [
        (
            vec!["--appraise", "jeff", "--trust", "bogus", "--text", "x"],
            "refused: --trust must be one of ('supervised', 'trusted', 'autonomous').\n",
        ),
        (
            vec!["--set-trust", "jeff", "bogus"],
            "refused: trust level must be one of ('supervised', 'trusted', 'autonomous').\n",
        ),
        (
            vec![
                "--add",
                "--name",
                &long_name,
                "--expertise",
                "x",
                "--personality",
                "y",
            ],
            "refused: --name is 65 chars; hard cap 64. Nothing was stored.\n",
        ),
        (
            vec![
                "--appraise",
                "jeff",
                "--trust",
                "trusted",
                "--text",
                &long_text,
            ],
            "refused: --text is 2001 chars; hard cap 2000. Nothing was stored.\n",
        ),
        (
            vec![
                "--add",
                "--name",
                "jeff",
                "--expertise",
                "x",
                "--personality",
                "y",
            ],
            "refused: coworker 'jeff' already exists (id 1). Use --reactivate if retired, or \
             pick a different name.\n",
        ),
        (
            vec!["--appraise", "nobody", "--trust", "trusted", "--text", "x"],
            "refused: no coworker named 'nobody'.\n",
        ),
        (
            vec!["--load", "nobody"],
            "refused: no coworker named 'nobody'.\n",
        ),
        (
            vec!["--retire", "nobody"],
            "refused: no coworker named 'nobody'.\n",
        ),
    ] {
        let mut full = vec!["coworkers"];
        full.extend(&args);
        assert_eq!(refused(&m.run(&full)), want, "{args:?}");
    }
    assert_eq!(snapshot(&m), before);
    assert_eq!(m.run(&["coworkers"]).status.code(), Some(64));
}
