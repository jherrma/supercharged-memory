//! `list` and `show` end to end: the real binary, a scrubbed environment, a
//! database of its own and an Ollama that refuses connections.
//!
//! What a unit test cannot show: the exit codes and which stream carries what,
//! that nothing ever creates a database, and that the reads leave the database
//! file exactly as it was.

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
    fn new(test: &str) -> Machine {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("rows-{test}"));
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
             INSERT INTO semantic_memory (category, topic, source, memory_text, created_at) VALUES
                 ('reference', 'deploy', 'user-stated', 'the deploy needs a tag

Keywords: tag, release', '2026-01-02 00:00:00'),
                 ('reference', 'deploy', 'migration', 'an older deploy fact', '2026-01-01 00:00:00');
             UPDATE semantic_memory SET superseded_by = 1 WHERE id = 2;
             INSERT INTO episodic_memory (event_type, importance, topic, memory_text, created_at) \
                 VALUES ('note', 'routine', 'deployed', 'deployed once', '2026-01-03 00:00:00');
             INSERT INTO topic_keywords (topic, keywords, updated_at) \
                 VALUES ('deploy', 'tag, release', '2026-01-01 00:00:00'),
                        ('windows', 'iocp, PowerShell', '2026-01-02 00:00:00');"
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

#[test]
fn list_and_show_read_through_the_binary_with_ollama_stopped() {
    let m = Machine::new("reads");
    let out = ok(&m.run(&["list", "--table", "semantic", "--current", "--keywords"]));
    assert!(out.contains("            id = 1\n"), "{out}");
    assert!(out.contains("      keywords = tag, release\n"), "{out}");
    assert!(!out.contains("deploy needs a tag"), "{out}");
    assert!(!out.contains("older deploy"), "{out}");

    let out = ok(&m.run(&["list", "--table", "episodic", "--unprocessed"]));
    assert!(out.contains("    event_type = note\n"), "{out}");

    let out = ok(&m.run(&["show", "--table", "semantic", "2,1"]));
    let older = out.find("an older deploy fact").unwrap();
    let newer = out.find("the deploy needs a tag").unwrap();
    assert!(older < newer, "{out}");
    assert!(!out.contains("embedding"), "{out}");
}

#[test]
fn an_empty_listing_is_silent_success() {
    let m = Machine::new("empty");
    let o = m.run(&["list", "--table", "semantic", "--topic", "nothing like it"]);
    assert_eq!(ok(&o), "");
    assert_eq!(stderr(&o), "");
}

#[test]
fn show_with_a_missing_id_prints_the_rest_and_fails_loudly() {
    let m = Machine::new("missing");
    let o = m.run(&["show", "--table", "semantic", "1,41,42"]);
    assert_eq!(o.status.code(), Some(1), "{}{}", stdout(&o), stderr(&o));
    assert!(
        stdout(&o).contains("the deploy needs a tag"),
        "{}",
        stdout(&o)
    );
    let err = stderr(&o);
    assert!(err.starts_with("error: "), "{err}");
    assert!(err.contains("41, 42"), "{err}");
}

#[test]
fn refusals_exit_two_with_nothing_on_stdout() {
    let m = Machine::new("refusals");
    for args in [
        vec!["list", "--table", "semantic", "--unprocessed"],
        vec!["list", "--table", "episodic", "--category", "pattern"],
        vec!["list", "--table", "semantic", "--since", "March 2024"],
        vec!["list", "--table", "semantic", "--ids", "1,x"],
        vec!["show", "--table", "semantic", "x"],
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
    let o = m.run(&["list", "--table", "semantic", "--current", "--not-current"]);
    assert_eq!(o.status.code(), Some(64), "{}", stderr(&o));
}

#[test]
fn neither_command_creates_a_database_or_touches_one() {
    let m = Machine::new("pure");
    let snapshot = |m: &Machine| {
        let mut names: Vec<(String, Vec<u8>)> = fs::read_dir(&m.root)
            .unwrap()
            .map(|e| e.unwrap())
            .filter(|e| e.file_name().to_string_lossy().starts_with("memory.db"))
            .map(|e| {
                (
                    e.file_name().to_string_lossy().into_owned(),
                    fs::read(e.path()).unwrap(),
                )
            })
            .collect();
        names.sort();
        names
    };
    let before = snapshot(&m);
    ok(&m.run(&["list", "--table", "semantic", "--preview", "10"]));
    ok(&m.run(&["show", "--table", "semantic", "1"]));
    ok(&m.run(&["recall", "--topics", "deploy"]));
    let after = snapshot(&m);
    // The sidecar that readers bump may differ; the data files may not.
    let data = |s: &[(String, Vec<u8>)]| {
        s.iter()
            .filter(|(n, _)| !n.ends_with("-tshm"))
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(data(&before), data(&after));

    let gone = Machine::new("no-db");
    for suffix in ["", "-wal", "-tshm"] {
        let _ = fs::remove_file(format!("{}{suffix}", gone.db().display()));
    }
    for args in [
        vec!["list", "--table", "semantic"],
        vec!["show", "--table", "semantic", "1"],
    ] {
        let o = gone.run(&args);
        assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
        assert!(
            stderr(&o).contains("refusing to silently create"),
            "{}",
            stderr(&o)
        );
    }
    assert!(!gone.db().exists());
}

#[test]
fn recall_topics_with_a_word_narrows_and_without_one_is_unchanged() {
    let m = Machine::new("topics");
    let whole = ok(&m.run(&["recall", "--topics"]));
    assert!(whole.contains("2 topic(s)"), "{whole}");
    assert!(
        whole.contains("windows") && whole.contains("deploy"),
        "{whole}"
    );

    let narrowed = ok(&m.run(&["recall", "--topics", "POWERSHELL"]));
    assert!(narrowed.contains("1 topic(s)"), "{narrowed}");
    assert!(narrowed.contains("windows"), "{narrowed}");
    assert!(!narrowed.contains("topic = deploy"), "{narrowed}");
}
