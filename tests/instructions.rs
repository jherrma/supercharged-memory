//! `instruct` and `instructions` end to end: the real binary, installed into a
//! directory of its own, with a scrubbed environment and a database of its own.
//! Ollama is pointed at a port that refuses, the state a machine without it is
//! in.
//!
//! What a unit test cannot show: the exit codes and which stream carries what,
//! as a shell sees them, and that the session-start commands print the same
//! bytes once instructions exist.

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
    /// a baseline rule, a fact, an event and a topic.
    fn new(test: &str) -> Machine {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("instructions-{test}"));
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
                 ('reference', 'deploy', 'the deploy needs a tag', '2026-01-02 00:00:00');
             INSERT INTO episodic_memory (event_type, importance, memory_text, created_at) \
                 VALUES ('note', 'routine', 'deployed once', '2026-01-03 00:00:00');
             INSERT INTO topic_keywords (topic, keywords, updated_at) \
                 VALUES ('deploy', 'tag, release', '2026-01-01 00:00:00');"
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

/// Store without Ollama, look it up three ways, revise it, retire it.
#[test]
fn an_instruction_is_stored_found_revised_and_retired_through_the_binary() {
    let m = Machine::new("lifecycle");
    let out = m.run(&[
        "instruct",
        "--topic",
        "release-notes",
        "--text",
        "Bullet points,\nnewest first; quote 'every' issue.",
        "--keywords",
        "changelog",
    ]);
    assert_eq!(
        ok(&out),
        "stored instruction 1 (release-notes) WITHOUT embedding — Ollama unreachable at \
         http://127.0.0.1:9; run embed --missing once Ollama is available\n"
    );
    assert!(
        stderr(&out).starts_with("warning: EMBEDDINGS=on"),
        "{}",
        stderr(&out)
    );

    let got = ok(&m.run(&["instructions", "--get", "release-notes"]));
    assert!(
        got.starts_with("===== instruction 'release-notes' (1 current, newest first) =====\n"),
        "{got}"
    );
    assert!(
        got.contains("memory_text = Bullet points,\nnewest first; quote 'every' issue.\n\nKeywords: changelog\n"),
        "{got}"
    );

    ok(&m.run(&[
        "instruct",
        "--topic",
        "deploy",
        "--text",
        "Tag the release before deploying.",
    ]));
    let listed = ok(&m.run(&["instructions", "--list"]));
    assert!(listed.contains("topic = release-notes"), "{listed}");
    assert!(listed.contains("topic = deploy"), "{listed}");
    assert!(!listed.contains("Bullet points"), "{listed}");

    let found = ok(&m.run(&["instructions", "changelog"]));
    assert!(
        found.starts_with("===== instructions (Ollama down — keyword-only, top 5) =====\n"),
        "{found}"
    );
    assert!(found.contains("id = 1\n"), "{found}");
    assert!(!found.contains("id = 2\n"), "{found}");

    let err = refused(&m.run(&["instruct", "--topic", "release-notes", "--text", "  "]));
    assert!(err.contains("--text is empty"), "{err}");

    let revised = ok(&m.run(&[
        "instruct",
        "--topic",
        "release-notes",
        "--text",
        "Bullet points, oldest first.",
        "--replaces",
        "1",
    ]));
    assert!(
        revised.starts_with(
            "stored instruction 3 (release-notes), replacing instruction 1 (now retired)"
        ),
        "{revised}"
    );
    let err = refused(&m.run(&["instruct", "--retire", "1"]));
    assert!(err.contains("already retired"), "{err}");

    assert_eq!(
        ok(&m.run(&["instruct", "--retire", "3"])),
        "retired instruction 3 (release-notes)\n"
    );
    let none = ok(&m.run(&["instructions", "--get", "release-notes"]));
    assert!(none.contains("(none current)"), "{none}");
}

/// Instructions are not loaded at session start: what a session begins with
/// prints the same bytes before and after instructions exist -- a current one
/// with its embedding, and one stored through the binary, then retired. (A
/// current one WITHOUT an embedding is counted on status's `unembedded:` line,
/// on purpose: `embed --missing` fills it.)
#[test]
fn session_start_prints_the_same_with_instructions_in_the_database() {
    let m = Machine::new("session-start");
    let start = [
        vec!["status"],
        vec!["recall", "--baseline"],
        vec!["recall", "--topics"],
        vec!["sleep", "--staleness"],
        vec!["sleep", "--verify-candidates"],
    ];
    let before: Vec<String> = start.iter().map(|args| ok(&m.run(args))).collect();
    for (args, said) in [
        (
            vec![
                "instruct",
                "--topic",
                "start",
                "--text",
                "run scripts/recall.py --status and the baseline first",
            ],
            "stored instruction 1 (start)",
        ),
        (
            vec!["instruct", "--topic", "deploy", "--text", "tag the release"],
            "stored instruction 2 (deploy)",
        ),
        (vec!["instruct", "--retire", "1"], "retired instruction 1"),
        (vec!["instruct", "--retire", "2"], "retired instruction 2"),
    ] {
        let out = ok(&m.run(&args));
        assert!(out.starts_with(said), "{args:?}: {out}");
    }
    m.exec(&format!(
        "INSERT INTO instructions (topic, memory_text, embed_model, embedding) \
         VALUES ('start', 'status, then the baseline', 'bge-m3', vector32('[{}]'));",
        vec!["0.5"; 1024].join(",")
    ));
    let after: Vec<String> = start.iter().map(|args| ok(&m.run(args))).collect();
    assert_eq!(after, before);
}

/// A database from before the table: both commands say what to run, and
/// neither creates anything.
#[test]
fn a_database_from_before_the_instructions_table_is_pointed_at_update() {
    let m = Machine::new("pre-instructions");
    m.exec("DROP INDEX IF EXISTS idx_instr_retired; DROP TABLE IF EXISTS instructions;");
    for args in [
        vec!["instructions", "--list"],
        vec!["instructions", "deploy"],
        vec!["instruct", "--topic", "deploy", "--text", "tag first"],
    ] {
        let err = refused(&m.run(&args));
        assert!(err.contains("run update"), "{args:?}: {err}");
    }
    let status = ok(&m.run(&["status"]));
    assert!(
        status.ends_with("schema: 1 migration(s) pending (instructions) — run update\n"),
        "{status}"
    );
}
