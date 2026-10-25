//! `update` end to end: the real binary, installed into a directory of its
//! own with its `.env`, a database and a CLAUDE.md, run with a scrubbed
//! environment.
//!
//! The embedded CLAUDE.md template still names the Python scripts, which a
//! binary cannot fill in, so here the CLAUDE.md report says it could not
//! compare, and `--print-block` refuses. The comparison itself is unit-tested
//! against a template the binary can fill.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::SystemTime;

const BIN: &str = env!("CARGO_BIN_EXE_supercharged-memory");
const NAME: &str = "supercharged-memory";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const SCHEMA: &str = include_str!("../schema.sql");

const BLOCK_MD: &str = "# Mine\n\n\
    <!-- BEGIN agentic-memory (managed by supercharged-memory) -->\n\
    <!-- supercharged-memory: rendered-by 0.0.1 -->\n\
    # Long-term memory — Turso vector DB (supercharged-memory)\n\
    **Episodic policy on this machine — `manual`: store episodic memories ONLY when asked.**\n\
    <!-- END agentic-memory -->\n";

const LEGACY_MD: &str = "# Mine\n\n\
    <!-- BEGIN agentic-memory (managed by install-claude-md.sh) -->\n\
    <!-- supercharged-memory: synced-at 0123abcd -->\n\
    # Long-term memory — Turso vector DB (supercharged-memory)\n\
    them as `python3 \"/repo/scripts/<cmd>\"`.\n\
    <!-- END agentic-memory -->\n";

struct Machine {
    root: PathBuf,
    exe: PathBuf,
}

impl Machine {
    /// The binary in `<root>/app`, hard-linked from the build so that its
    /// `.env` is the one beside the link; a database from the schema with one
    /// memory; a home.
    fn new(test: &str) -> Machine {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("update-{test}"));
        let _ = fs::remove_dir_all(&root);
        for dir in ["app", "home/.claude", "data"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        let root = root.canonicalize().unwrap();
        let exe = root.join("app").join(NAME);
        if fs::hard_link(BIN, &exe).is_err() {
            fs::copy(BIN, &exe).unwrap();
        }
        let m = Machine { root, exe };
        let p = m.db().to_string_lossy().to_string();
        // As it stood before the instructions table: dropped explicitly, so
        // this keeps testing the migration once the schema carries the table.
        let sql = format!(
            "{SCHEMA}\nDROP INDEX IF EXISTS idx_instr_retired; DROP TABLE IF EXISTS instructions;\n\
             INSERT INTO semantic_memory (category, memory_text) VALUES ('reference', 'a fact');"
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

    /// Set up by init at `version`: the `.env`, and CLAUDE.md with a block.
    fn installed(test: &str, version: &str) -> Machine {
        let m = Machine::new(test);
        fs::write(
            m.dotenv(),
            format!(
                "SUPERCHARGED_MEMORY_TURSO_PATH={}\nBACKUP_DIR={}\nSUPERCHARGED_MEMORY_VERSION={version}\nEMBEDDINGS=off\n",
                m.db().display(),
                m.backups().display()
            ),
        )
        .unwrap();
        fs::write(m.claude_md(), BLOCK_MD).unwrap();
        m
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(&self.exe)
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env("OLLAMA_URL", "http://127.0.0.1:9")
            .args(args)
            .output()
            .unwrap()
    }

    fn db(&self) -> PathBuf {
        self.root.join("data/memory.db")
    }

    fn backups(&self) -> PathBuf {
        self.root.join("backups")
    }

    fn dotenv(&self) -> PathBuf {
        self.root.join("app/.env")
    }

    fn claude_md(&self) -> PathBuf {
        self.root.join("home/.claude/CLAUDE.md")
    }

    /// Every file under the machine but the binary: bytes and mtime.
    fn snapshot(&self) -> Snapshot {
        fn walk(p: &Path, skip: &Path, out: &mut Snapshot) {
            if p == skip {
                return;
            }
            let meta = fs::symlink_metadata(p).unwrap();
            if meta.is_dir() {
                out.insert(p.to_path_buf(), None);
                for e in fs::read_dir(p).unwrap() {
                    walk(&e.unwrap().path(), skip, out);
                }
            } else {
                // Every process that opens a database whose WAL holds frames
                // touches the multiprocess-WAL coordination file's mtime --
                // `status` too -- and leaves its bytes alone. Its bytes count;
                // its mtime is not the database's.
                let mtime = if p.to_string_lossy().ends_with("-tshm") {
                    SystemTime::UNIX_EPOCH
                } else {
                    meta.modified().unwrap()
                };
                out.insert(p.to_path_buf(), Some((fs::read(p).unwrap(), mtime)));
            }
        }
        let mut out = BTreeMap::new();
        walk(&self.root, &self.exe, &mut out);
        out
    }

    fn has_table(&self, name: &str) -> bool {
        let p = self.db().to_string_lossy().to_string();
        pollster::block_on(async {
            let db = turso::Builder::new_local(&p)
                .experimental_multiprocess_wal(true)
                .build()
                .await
                .unwrap();
            let conn = db.connect().unwrap();
            let mut rows = conn
                .query(
                    "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = ?",
                    [name],
                )
                .await
                .unwrap();
            let row = rows.next().await.unwrap().unwrap();
            row.get_value(0).unwrap() != turso::Value::Integer(0)
        })
    }
}

type Snapshot = BTreeMap<PathBuf, Option<(Vec<u8>, SystemTime)>>;

/// The paths whose presence, bytes or mtime differ between two snapshots.
fn changed(before: &Snapshot, after: &Snapshot) -> Vec<String> {
    let mut out = Vec::new();
    for (path, was) in before {
        match after.get(path) {
            None => out.push(format!("removed {}", path.display())),
            Some(now) if now == was => {}
            Some(now) => out.push(format!(
                "changed {} ({})",
                path.display(),
                match (was, now) {
                    (Some((a, _)), Some((b, _))) if a == b => "mtime only",
                    _ => "bytes",
                }
            )),
        }
    }
    for path in after.keys().filter(|p| !before.contains_key(*p)) {
        out.push(format!("added {}", path.display()));
    }
    out
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn last_line(s: &str) -> &str {
    s.lines().last().unwrap_or("")
}

#[test]
fn a_dry_run_then_yes_then_up_to_date() {
    let m = Machine::installed("cycle", "0.0.1");

    // The dry run: the note, the pending migration, and nothing written.
    let before = m.snapshot();
    let out = m.run(&["update"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert_eq!(last_line(&text), "Ask the user to confirm", "{text}");
    assert!(text.contains("The `instructions` table"), "{text}");
    assert!(text.contains("table `instructions` exists"), "{text}");
    // The interim state: the template names the Python scripts.
    assert!(text.contains("not compared"), "{text}");
    assert!(text.contains("{{BASE_PATH}}"), "{text}");
    assert_eq!(
        changed(&before, &m.snapshot()),
        Vec::<String>::new(),
        "the dry run wrote something"
    );

    // -y: a backup first, the migration, the version.
    let out = m.run(&["update", "-y"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    let dumps: Vec<PathBuf> = fs::read_dir(m.backups())
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(dumps.len(), 1, "{dumps:?}");
    assert!(text.contains(&dumps[0].display().to_string()), "{text}");
    assert!(m.has_table("instructions"));
    let env = fs::read_to_string(m.dotenv()).unwrap();
    assert!(
        env.lines()
            .any(|l| l == format!("SUPERCHARGED_MEMORY_VERSION={VERSION}")),
        "{env}"
    );
    assert_eq!(fs::read_to_string(m.claude_md()).unwrap(), BLOCK_MD);

    // Again: up to date, and nothing written.
    let before = m.snapshot();
    let out = m.run(&["update"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        last_line(&stdout(&out)).starts_with("Up to date"),
        "{}",
        stdout(&out)
    );
    assert_eq!(changed(&before, &m.snapshot()), Vec::<String>::new());
}

#[test]
fn refusals_exit_2_and_write_nothing() {
    // No .env: not set up by init.
    let m = Machine::new("no-dotenv");
    fs::write(m.claude_md(), BLOCK_MD).unwrap();
    let before = m.snapshot();
    for args in [&["update"][..], &["update", "-y"]] {
        let out = m.run(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", stderr(&out));
        let err = stderr(&out);
        assert!(err.starts_with("refused: "), "{err}");
        assert!(err.contains("init"), "{err}");
        assert_eq!(stdout(&out), "", "{args:?}");
    }
    assert_eq!(changed(&before, &m.snapshot()), Vec::<String>::new());

    // A downgrade: the .env was written by a newer binary.
    let m = Machine::installed("downgrade", "999.0.0");
    let before = m.snapshot();
    let out = m.run(&["update", "-y"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("999.0.0"), "{}", stderr(&out));
    assert_eq!(changed(&before, &m.snapshot()), Vec::<String>::new());
}

#[test]
fn a_legacy_machine_gets_the_cutover_note() {
    let m = Machine::new("legacy");
    fs::write(m.claude_md(), LEGACY_MD).unwrap();
    let before = m.snapshot();
    let out = m.run(&["update"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("init leaves it untouched"),
        "{}",
        stdout(&out)
    );
    let out = m.run(&["update", "-y"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert_eq!(changed(&before, &m.snapshot()), Vec::<String>::new());
}

#[test]
fn print_block_is_refused_while_the_template_names_the_python_scripts() {
    let m = Machine::installed("print-block", VERSION);
    let out = m.run(&["update", "--print-block"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("{{BASE_PATH}}"), "{}", stderr(&out));
    assert_eq!(stdout(&out), "");
}
