//! `sleep` end to end: the real binary, installed into a directory of its own,
//! with a scrubbed environment and a database of its own.
//!
//! What a unit test cannot show: the purge gate across separate processes --
//! where every other opener of the database bumps its `-tshm` -- and the topic
//! index read from a real pipe.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_supercharged-memory");
const NAME: &str = "supercharged-memory";
const SCHEMA: &str = include_str!("../schema.sql");

struct Machine {
    root: PathBuf,
    exe: PathBuf,
}

impl Machine {
    /// The binary in `<root>/app`, a database at `<root>/memory.db`, dumps
    /// going to `<root>/backups`. Semantic row 1 is current, 2 is superseded by
    /// 1 and tagged to a coworker, 3 and 4 are retired; two episodic rows. No
    /// row has an embedding: the state a machine without Ollama is in.
    fn new(test: &str) -> Machine {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("sleep-{test}"));
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
             INSERT INTO semantic_memory (category, memory_text) VALUES
                 ('reference', 'run supercharged-memory status first'),
                 ('reference', 'run python3 scripts/recall.py --status first'),
                 ('reference', 'a retired fact'),
                 ('reference', 'another retired fact');
             UPDATE semantic_memory SET superseded_by = 1 WHERE id = 2;
             UPDATE semantic_memory SET retired_at = '2026-02-02' WHERE id IN (3, 4);
             INSERT INTO episodic_memory (event_type, importance, memory_text) VALUES
                 ('note', 'routine', 'one'), ('note', 'routine', 'two');
             INSERT INTO coworkers (name, expertise, personality) VALUES ('jeff', 'x', 'y');
             INSERT INTO memory_coworkers (memory_table, memory_id, coworker_id) VALUES
                 ('semantic', 2, 1), ('episodic', 2, 1);
             INSERT INTO topic_keywords (topic, keywords) VALUES ('old', 'x');"
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

    fn query(&self, sql: &str) -> Vec<Vec<turso::Value>> {
        let p = self.db().to_string_lossy().to_string();
        pollster::block_on(async {
            let db = turso::Builder::new_local(&p)
                .experimental_multiprocess_wal(true)
                .build()
                .await
                .unwrap();
            let conn = db.connect().unwrap();
            let mut rows = conn.query(sql, ()).await.unwrap();
            let n = rows.column_names().len();
            let mut out = Vec::new();
            while let Some(row) = rows.next().await.unwrap() {
                out.push((0..n).map(|i| row.get_value(i).unwrap()).collect());
            }
            out
        })
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(&self.exe);
        cmd.env_clear()
            .env("HOME", self.root.join("home"))
            .env("OLLAMA_URL", "http://127.0.0.1:9")
            .env("SUPERCHARGED_MEMORY_TURSO_PATH", self.db())
            .env("BACKUP_DIR", self.root.join("backups"))
            .args(args);
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        // File times tick coarsely; keep each step on a tick of its own.
        std::thread::sleep(Duration::from_millis(30));
        self.command(args).output().unwrap()
    }

    fn piped(&self, args: &[&str], stdin: &str) -> Output {
        let mut child = self
            .command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn refused(o: &Output) -> String {
    assert_eq!(o.status.code(), Some(2), "{}{}", stdout(o), stderr(o));
    assert_eq!(stdout(o), "");
    let err = stderr(o);
    assert!(err.starts_with("refused: "), "{err}");
    err
}

fn ok(o: &Output) -> String {
    assert!(o.status.success(), "{}{}", stdout(o), stderr(o));
    stdout(o)
}

fn ids(m: &Machine) -> Vec<i64> {
    m.query("SELECT id FROM semantic_memory ORDER BY id;")
        .into_iter()
        .map(|r| match r[0] {
            turso::Value::Integer(n) => n,
            ref other => panic!("{other:?}"),
        })
        .collect()
}

/// D1 then D2 as separate processes, with a reader in between: the backup
/// makes the purge possible, the reader does not undo that, and the purge's
/// own write makes a second purge wait for a new backup.
#[test]
fn a_purge_pass_across_processes() {
    let m = Machine::new("purge-pass");
    let purge = ["sleep", "--purge", "2,3", "--confirm-purge"];

    let err = refused(&m.run(&purge));
    assert!(err.contains("no backup found"), "{err}");

    let dump = PathBuf::from(ok(&m.run(&["backup"])).trim_end());
    let status = ok(&m.run(&["status"]));
    assert!(status.starts_with("DEGRADED 6"), "{status}");
    ok(&m.run(&["sleep", "--staleness"]));
    ok(&m.run(&["recall", "--count"]));

    let out = ok(&m.run(&purge));
    assert_eq!(
        out,
        format!(
            "purged 2 semantic row(s): 2,3\ncleared 1 memory_coworkers row(s)\n\
             backup verified: {}\n",
            dump.display()
        )
    );
    assert_eq!(ids(&m), vec![1, 4]);
    assert_eq!(
        m.query("SELECT memory_table, memory_id FROM memory_coworkers;"),
        vec![vec![
            turso::Value::Text("episodic".into()),
            turso::Value::Integer(2)
        ]]
    );

    let err = refused(&m.run(&["sleep", "--purge", "4", "--confirm-purge"]));
    assert!(err.contains("ONE --purge call"), "{err}");
    assert_eq!(ids(&m), vec![1, 4]);

    ok(&m.run(&["backup"]));
    ok(&m.run(&["sleep", "--purge", "4", "--confirm-purge"]));
    assert_eq!(ids(&m), vec![1]);
    assert_eq!(
        m.query("SELECT count(*) FROM episodic_memory;"),
        vec![vec![turso::Value::Integer(2)]]
    );
}

#[test]
fn the_topic_index_is_replaced_from_a_real_pipe() {
    let m = Machine::new("topics");
    let out = m.piped(
        &["sleep", "--rebuild-topics"],
        r#"[{"topic": "Schlaf", "keywords": "Tiefschlaf, Grüße"}]"#,
    );
    assert_eq!(ok(&out), "rebuilt topic_keywords: 1 topic(s)\n");
    let want = vec![vec![
        turso::Value::Text("Schlaf".into()),
        turso::Value::Text("Tiefschlaf, Grüße".into()),
    ]];
    assert_eq!(m.query("SELECT topic, keywords FROM topic_keywords;"), want);

    let over = format!(r#"[{{"topic": "t", "keywords": "{}"}}]"#, "k".repeat(500));
    let err = refused(&m.piped(&["sleep", "--rebuild-topics"], &over));
    assert!(err.contains("over the 500-char cap"), "{err}");
    assert_eq!(m.query("SELECT topic, keywords FROM topic_keywords;"), want);
}

/// Without a single embedding the vector reports refuse and point at the fix,
/// while the reports that need none answer in JSON on stdout.
#[test]
fn a_corpus_without_embeddings_refuses_clustering_but_still_reports() {
    let m = Machine::new("degraded");
    for args in [
        vec!["sleep", "--cluster"],
        vec!["sleep", "--contradiction-candidates"],
        vec!["sleep", "--cluster", "--table", "episodic"],
    ] {
        let err = refused(&m.run(&args));
        assert!(err.contains("embed --missing"), "{args:?}: {err}");
    }

    let out = m.run(&["sleep", "--staleness"]);
    let v: serde_json::Value = serde_json::from_str(&ok(&out)).unwrap();
    assert_eq!(v["n_current"], 1);
    assert_eq!(
        v["n_without_embedding"],
        serde_json::json!({"semantic": 1, "episodic": 2})
    );
    assert_eq!(stderr(&out), "");

    ok(&m.run(&["sleep", "--retire", "1"]));
    m.exec("UPDATE semantic_memory SET superseded_by = NULL WHERE id = 2;");
    let v: serde_json::Value = serde_json::from_str(&ok(&m.run(&[
        "sleep",
        "--verify-candidates",
        "--limit",
        "1",
    ])))
    .unwrap();
    assert_eq!(v["n_candidates"], 1);
    assert_eq!(v["candidates"][0]["id"], 2);
    assert_eq!(
        v["candidates"][0]["artifacts"],
        serde_json::json!(["paths", "flags", "python"])
    );

    ok(&m.run(&["sleep", "--mark-processed", "1,2"]));
    assert_eq!(
        m.query("SELECT count(*) FROM episodic_memory WHERE processed_at IS NOT NULL;"),
        vec![vec![turso::Value::Integer(2)]]
    );
}
