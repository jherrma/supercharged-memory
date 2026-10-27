//! The controlled environment: machines, the commands run in them, the
//! comparison of the two implementations, and the database state they leave.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};

use crate::differences::Difference;
use crate::{ollama, vectors};

const BIN: &str = env!("CARGO_BIN_EXE_supercharged-memory");
const SCHEMA: &str = include_str!("../../schema.sql");
const FIXTURE: &str = include_str!("fixture.sql");

/// The one line of the reference backup script that writes outside its
/// directories; the Python machines get a copy that writes under their own
/// `TMPDIR` instead.
const BACKUP_ERR_LINE: &str = "ERR=/tmp/supercharged-memory-backup.err";

/// A search path that holds the system tools the reference scripts call
/// (`bash`, `gzip`, `date`, ...) and nothing of the developer's own.
const PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// Comparisons that could not run, by name, for the closing report.
static SKIPPED: Mutex<Vec<String>> = Mutex::new(Vec::new());

pub fn skipped() -> Vec<String> {
    SKIPPED.lock().unwrap().clone()
}

/// The Python reference: an interpreter and the tursodb CLI its scripts drive.
#[derive(Clone, Debug)]
pub struct Python {
    pub python3: PathBuf,
    pub tursodb: PathBuf,
}

/// What a machine's database starts as.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Fixture {
    /// No database file at all.
    None,
    /// The schema and nothing else.
    Schema,
    /// `fixture.sql`; rows whose `embed_model` is NULL stay without a vector.
    Full,
    /// `fixture.sql` with a vector on every row.
    Embedded,
    /// `Full` without the `instructions` table, as a database from before it.
    PreInstructions,
}

impl Fixture {
    fn index(self) -> usize {
        self as usize
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Rust,
    Python,
}

pub struct Ctx {
    pub work: PathBuf,
    pub ollama_url: String,
    pub dead_url: String,
    pub python: Option<Python>,
    python_missing: String,
    reference_versions: String,
    fixtures: [OnceLock<PathBuf>; 5],
    fixture_states: [OnceLock<String>; 5],
    roots: Mutex<BTreeSet<PathBuf>>,
    exercised: Mutex<BTreeSet<&'static str>>,
}

/// An executable file on the real `PATH`.
fn on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// The first line a program prints for `--version`.
fn version(program: &Path) -> String {
    Command::new(program)
        .arg("--version")
        .env_clear()
        .env("PATH", PATH)
        .output()
        .map(|o| {
            let text = if o.stdout.is_empty() {
                o.stderr
            } else {
                o.stdout
            };
            String::from_utf8_lossy(&text)
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string()
        })
        .unwrap_or_default()
}

/// Where the reference can run: `python3` on the real `PATH` and the tursodb
/// CLI at `TURSO_BIN`, `~/.turso/tursodb` or on the `PATH`. Read from the real
/// environment once; no child ever sees that environment.
fn find_python() -> Result<Python, String> {
    let python3 = on_path("python3").ok_or("python3 not found on PATH")?;
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let tursodb = std::env::var_os("TURSO_BIN")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_file())
        .or_else(|| Some(home.join(".turso/tursodb")).filter(|p| p.is_file()))
        .or_else(|| on_path("tursodb"))
        .ok_or("the tursodb CLI the Python scripts drive was not found (TURSO_BIN, ~/.turso/tursodb, PATH)")?;
    Ok(Python { python3, tursodb })
}

impl Ctx {
    pub fn new() -> Ctx {
        let work = Path::new(env!("CARGO_TARGET_TMPDIR")).join("e2e");
        let _ = fs::remove_dir_all(&work);
        fs::create_dir_all(work.join("fixtures")).unwrap();
        let work = work.canonicalize().unwrap();
        let (python, python_missing, reference_versions) = match find_python() {
            Ok(py) => {
                let versions = format!(
                    "{} ({}), {} ({})",
                    version(&py.python3),
                    py.python3.display(),
                    version(&py.tursodb),
                    py.tursodb.display()
                );
                (Some(py), String::new(), versions)
            }
            Err(why) => (None, why.to_string(), String::new()),
        };
        Ctx {
            work,
            ollama_url: ollama::start(),
            dead_url: ollama::refusing_url(),
            python,
            python_missing,
            reference_versions,
            fixtures: Default::default(),
            fixture_states: Default::default(),
            roots: Mutex::new(BTreeSet::new()),
            exercised: Mutex::new(BTreeSet::new()),
        }
    }

    pub fn reference_line(&self) -> String {
        match &self.python {
            Some(_) => format!(
                "comparisons with the Python reference: on -- {}",
                self.reference_versions
            ),
            None => format!(
                "comparisons with the Python reference: SKIPPED -- {}; each one is named below",
                self.python_missing
            ),
        }
    }

    pub fn python_available(&self) -> bool {
        self.python.is_some()
    }

    pub fn exercised(&self) -> BTreeSet<&'static str> {
        self.exercised.lock().unwrap().clone()
    }

    /// Record that a test asserted this difference as the binary's behaviour.
    pub fn asserted(&self, d: &Difference) {
        self.exercised.lock().unwrap().insert(d.id);
    }

    /// Record a comparison that could not run.
    pub fn skip(&self, test: &str, what: &str) {
        SKIPPED
            .lock()
            .unwrap()
            .push(format!("{test}: {what} ({})", self.python_missing));
    }

    /// The fixture's database file, built on first use. Machines copy it.
    pub fn fixture(&self, fixture: Fixture) -> Option<&Path> {
        if fixture == Fixture::None {
            return None;
        }
        let path = self.fixtures[fixture.index()].get_or_init(|| {
            let path = self.work.join("fixtures").join(format!("{fixture:?}.db"));
            build_fixture(fixture, &path);
            path
        });
        Some(path)
    }

    /// The state of a fixture as built, for "nothing was written" checks.
    pub fn fixture_state(&self, fixture: Fixture) -> String {
        self.fixture_states[fixture.index()]
            .get_or_init(|| match self.fixture(fixture) {
                None => String::new(),
                Some(path) => {
                    let copy = self
                        .work
                        .join("fixtures")
                        .join(format!("{fixture:?}-state.db"));
                    fs::copy(path, &copy).unwrap();
                    state_of(&copy)
                }
            })
            .clone()
    }

    /// A machine of one side, its database a copy of `fixture`.
    pub fn machine(&self, test: &str, label: &str, side: Side, fixture: Fixture) -> Machine {
        let side_name = match side {
            Side::Rust => "rs",
            Side::Python => "py",
        };
        let root = self.work.join(format!("{test}-{label}-{side_name}"));
        assert!(
            self.roots.lock().unwrap().insert(root.clone()),
            "two machines named {}: give each its own label",
            root.display()
        );
        for dir in ["home", "data/turso", "config", "claude", "backups", "tmp"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        let m = Machine {
            root,
            side,
            ollama_url: self.ollama_url.clone(),
            dead_url: self.dead_url.clone(),
            python: self.python.clone(),
            env_config: true,
        };
        match side {
            Side::Rust => {
                fs::create_dir_all(m.root.join("app")).unwrap();
                if fs::hard_link(BIN, m.exe()).is_err() {
                    fs::copy(BIN, m.exe()).unwrap();
                }
                fs::write(
                    m.root.join("app/.env"),
                    format!(
                        "SUPERCHARGED_MEMORY_TURSO_PATH={}\nBACKUP_DIR={}\nOLLAMA_URL={}\n",
                        m.db().display(),
                        m.backups().display(),
                        self.ollama_url
                    ),
                )
                .unwrap();
            }
            Side::Python => {
                let script =
                    fs::read_to_string(repo().join("scripts/supercharged-memory-backup.sh"))
                        .unwrap();
                assert_eq!(
                    script.matches(BACKUP_ERR_LINE).count(),
                    1,
                    "the reference backup script no longer writes its error file where the suite expects"
                );
                m.write(
                    "tools/supercharged-memory-backup.sh",
                    &script.replace(
                        BACKUP_ERR_LINE,
                        "ERR=\"$TMPDIR/supercharged-memory-backup.err\"",
                    ),
                );
            }
        }
        if let Some(path) = self.fixture(fixture) {
            fs::copy(path, m.db()).unwrap();
        }
        m
    }

    /// A machine with no database and no `.env`, configured by nothing but
    /// its directories: what `init` starts from.
    pub fn bare_machine(&self, test: &str, label: &str) -> Machine {
        let mut m = self.machine(test, label, Side::Rust, Fixture::None);
        fs::remove_file(m.root.join("app/.env")).unwrap();
        m.env_config = false;
        m
    }

    /// A machine per side from the same fixture; the Python one only when the
    /// reference can run here.
    pub fn pair<'a>(&'a self, test: &'static str, label: &str, fixture: Fixture) -> Pair<'a> {
        Pair {
            ctx: self,
            test,
            rs: self.machine(test, label, Side::Rust, fixture),
            py: self
                .python
                .as_ref()
                .map(|_| self.machine(test, label, Side::Python, fixture)),
        }
    }
}

/// Build a fixture database with the engine itself, then checkpoint it so
/// the main file alone holds every row and can be copied on its own.
fn build_fixture(fixture: Fixture, path: &Path) {
    let sql = match fixture {
        Fixture::None => unreachable!(),
        Fixture::Schema => SCHEMA.to_string(),
        _ => format!("{SCHEMA}\n{FIXTURE}"),
    };
    let p = path.display().to_string();
    pollster::block_on(async {
        let db = turso::Builder::new_local(&p)
            .experimental_multiprocess_wal(true)
            .build()
            .await
            .unwrap();
        let conn = db.connect().unwrap();
        conn.execute_batch(&sql).await.unwrap();
        if fixture != Fixture::Schema {
            let only = if fixture == Fixture::Embedded {
                ""
            } else {
                " WHERE embed_model IS NOT NULL"
            };
            for table in ["semantic_memory", "episodic_memory", "instructions"] {
                let mut rows = conn
                    .query(&format!("SELECT id, memory_text FROM {table}{only}"), ())
                    .await
                    .unwrap();
                let mut todo = Vec::new();
                while let Some(row) = rows.next().await.unwrap() {
                    let id = match row.get_value(0).unwrap() {
                        turso::Value::Integer(i) => i,
                        other => panic!("{other:?}"),
                    };
                    let text = match row.get_value(1).unwrap() {
                        turso::Value::Text(s) => s,
                        other => panic!("{other:?}"),
                    };
                    todo.push((id, text));
                }
                drop(rows);
                for (id, text) in todo {
                    conn.execute(
                        &format!("UPDATE {table} SET embedding = ?1, embed_model = 'bge-m3' WHERE id = ?2"),
                        (turso::Value::Blob(vectors::blob(&text)), id),
                    )
                    .await
                    .unwrap();
                }
            }
        }
        if fixture == Fixture::PreInstructions {
            conn.execute_batch("DROP INDEX IF EXISTS idx_instr_retired; DROP TABLE instructions;")
                .await
                .unwrap();
        }
        let mut done = conn
            .query("PRAGMA wal_checkpoint(TRUNCATE)", ())
            .await
            .unwrap();
        while done.next().await.unwrap().is_some() {}
    });
}

/// One command, as the binary takes it; the reference's script and
/// arguments follow from the subcommand.
#[derive(Clone, Debug, Default)]
pub struct Cmd {
    pub args: Vec<String>,
    pub stdin: Option<String>,
    pub env: Vec<(String, String)>,
    pub refused_ollama: bool,
}

/// `cmd(&["recall", "query"])`. `{ROOT}` in an argument becomes the machine's
/// root directory.
pub fn cmd(args: &[&str]) -> Cmd {
    Cmd {
        args: args.iter().map(|s| s.to_string()).collect(),
        ..Cmd::default()
    }
}

impl Cmd {
    pub fn stdin(mut self, text: &str) -> Cmd {
        self.stdin = Some(text.to_string());
        self
    }

    pub fn env(mut self, key: &str, value: &str) -> Cmd {
        self.env.push((key.to_string(), value.to_string()));
        self
    }

    /// Point Ollama at a port that refuses the connection.
    pub fn without_ollama(mut self) -> Cmd {
        self.refused_ollama = true;
        self
    }
}

/// A finished command, its output with the machine's root written `{ROOT}`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Run {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    /// Exit 0; the stdout.
    pub fn ok(&self) -> &str {
        assert_eq!(self.code, Some(0), "expected success:\n{self}");
        &self.stdout
    }

    /// Exit 2, nothing on stdout, stderr starting `refused: `; the stderr.
    pub fn refused(&self) -> &str {
        assert_eq!(self.code, Some(2), "expected a refusal:\n{self}");
        assert_eq!(
            self.stdout, "",
            "a refusal prints nothing on stdout:\n{self}"
        );
        assert!(
            self.stderr.starts_with("refused: "),
            "expected `refused: `:\n{self}"
        );
        &self.stderr
    }

    /// Exit 1 with a line starting `error: ` on stderr; the stderr.
    pub fn failed(&self) -> &str {
        assert_eq!(self.code, Some(1), "expected a failure:\n{self}");
        assert!(
            self.stderr.lines().any(|l| l.starts_with("error: ")),
            "expected `error: `:\n{self}"
        );
        &self.stderr
    }

    /// Exit 64 with clap's `error: ` on stderr.
    pub fn usage(&self) -> &str {
        assert_eq!(self.code, Some(64), "expected a usage error:\n{self}");
        assert!(
            self.stderr.starts_with("error: "),
            "expected `error: `:\n{self}"
        );
        &self.stderr
    }
}

impl std::fmt::Display for Run {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "exit {:?}\n--- stdout\n{}--- stderr\n{}",
            self.code, self.stdout, self.stderr
        )
    }
}

pub struct Machine {
    pub root: PathBuf,
    pub side: Side,
    ollama_url: String,
    dead_url: String,
    python: Option<Python>,
    /// Whether the database path and the backup directory are passed in the
    /// environment, as the reference needs them. Without it the binary reads
    /// them from its `.env` alone.
    env_config: bool,
}

impl Machine {
    pub fn db(&self) -> PathBuf {
        self.root.join("data/turso/supercharged-memory.db")
    }

    pub fn backups(&self) -> PathBuf {
        self.root.join("backups")
    }

    pub fn exe(&self) -> PathBuf {
        self.root.join("app/supercharged-memory")
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        if rel.is_empty() {
            self.root.clone()
        } else {
            self.root.join(rel)
        }
    }

    /// The binary reads its database path and backup directory from its
    /// `.env` alone, as on a machine `init` set up.
    pub fn dotenv_only(mut self) -> Machine {
        self.env_config = false;
        self
    }

    /// Write a file under the root, creating its directories.
    pub fn write(&self, rel: &str, contents: &str) {
        self.write_bytes(rel, contents.as_bytes());
    }

    /// The same for bytes that need not be text.
    pub fn write_bytes(&self, rel: &str, contents: &[u8]) {
        let path = self.path(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    /// Append to a file under the root.
    pub fn append(&self, rel: &str, contents: &str) {
        let mut f = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(self.path(rel))
            .unwrap();
        f.write_all(contents.as_bytes()).unwrap();
    }

    /// A file under the root, as text.
    pub fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.path(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
    }

    /// The names in a directory under the root, sorted.
    pub fn list(&self, rel: &str) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.path(rel))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// A symlink under the root pointing at `target`.
    #[cfg(unix)]
    pub fn symlink(&self, rel: &str, target: &str) {
        let path = self.path(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(target, path).unwrap();
    }

    #[cfg(not(unix))]
    pub fn symlink(&self, _rel: &str, _target: &str) {
        panic!("symlinks need unix")
    }

    /// A copy of a fixture database at a path under the root.
    pub fn place(&self, ctx: &Ctx, fixture: Fixture, rel: &str) {
        let source = ctx.fixture(fixture).expect("a fixture with a database");
        let path = self.path(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::copy(source, path).unwrap();
    }

    /// The variables a child gets: these and nothing else.
    fn env(&self, c: &Cmd) -> Vec<(String, OsString)> {
        let at = |rel: &str| self.root.join(rel).into_os_string();
        let url = if c.refused_ollama {
            &self.dead_url
        } else {
            &self.ollama_url
        };
        let mut env: Vec<(String, OsString)> = vec![
            ("HOME".into(), at("home")),
            ("XDG_DATA_HOME".into(), at("data")),
            ("XDG_CONFIG_HOME".into(), at("config")),
            ("CLAUDE_CONFIG_DIR".into(), at("claude")),
            ("TMPDIR".into(), at("tmp")),
            ("PATH".into(), PATH.into()),
            ("LANG".into(), "C.UTF-8".into()),
            ("TZ".into(), "UTC".into()),
            // Always set, so no side ever falls back to a real Ollama's port.
            ("OLLAMA_URL".into(), url.into()),
        ];
        if self.env_config {
            env.push((
                "SUPERCHARGED_MEMORY_TURSO_PATH".into(),
                self.db().into_os_string(),
            ));
            env.push(("BACKUP_DIR".into(), self.backups().into_os_string()));
        }
        if self.side == Side::Python {
            let py = self
                .python
                .as_ref()
                .expect("a Python machine without Python");
            env.push(("TURSO_BIN".into(), py.tursodb.clone().into_os_string()));
            env.push(("PYTHONDONTWRITEBYTECODE".into(), "1".into()));
        }
        for (k, v) in &c.env {
            env.retain(|(key, _)| key != k);
            env.push((k.clone(), v.into()));
        }
        env
    }

    /// The reference's command line for one of the binary's.
    fn reference(&self, args: &[String]) -> Command {
        let py = self
            .python
            .as_ref()
            .expect("a Python machine without Python");
        let (sub, rest) = args.split_first().expect("a command needs a subcommand");
        let script = match sub.as_str() {
            "status" | "recall" => "scripts/recall.py",
            "remember" => "scripts/remember.py",
            "sleep" => "scripts/sleep.py",
            "coworkers" => "scripts/coworkers.py",
            "eval" => "investigations/eval-harness.py",
            "restore" => "scripts/restore.py",
            "backfill" => "scripts/backfill.py",
            "find-existing-memory" => "scripts/find-existing-memory.py",
            "backup" => {
                let mut command = Command::new("bash");
                command.arg(self.path("tools/supercharged-memory-backup.sh"));
                return command;
            }
            other => panic!("`{other}` has no Python reference"),
        };
        let mut command = Command::new(&py.python3);
        command.arg("-B").arg(repo().join(script));
        if sub == "status" {
            command.arg("--status");
        } else {
            command.args(rest);
        }
        command
    }

    /// Run one command on this machine's side.
    pub fn run(&self, c: &Cmd) -> Run {
        let root = self.root.display().to_string();
        let args: Vec<String> = c.args.iter().map(|a| a.replace("{ROOT}", &root)).collect();
        let mut command = match self.side {
            Side::Rust => {
                let mut command = Command::new(self.exe());
                command.args(&args);
                command
            }
            Side::Python => self.reference(&args),
        };
        command.env_clear().current_dir(&self.root);
        for (k, v) in self.env(c) {
            command.env(k, v);
        }
        command
            .stdin(if c.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {args:?}: {e}"));
        if let Some(text) = &c.stdin {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(text.as_bytes())
                .unwrap();
        }
        let out = child.wait_with_output().unwrap();
        let clean = |bytes: &[u8]| {
            String::from_utf8_lossy(bytes)
                .replace(&root, "{ROOT}")
                .replace(&repo().display().to_string(), "{REPO}")
        };
        let run = Run {
            code: out.status.code(),
            stdout: clean(&out.stdout),
            stderr: clean(&out.stderr),
        };
        if self.side == Side::Rust {
            check_prefixes(&args, &run);
        }
        run
    }

    /// Execute SQL on this machine's database from the test process.
    pub fn sql(&self, sql: &str) {
        let p = self.db().display().to_string();
        pollster::block_on(async {
            let db = turso::Builder::new_local(&p)
                .experimental_multiprocess_wal(true)
                .build()
                .await
                .unwrap();
            db.connect().unwrap().execute_batch(sql).await.unwrap();
        });
    }

    /// One value from this machine's database, as text (`NULL` for NULL).
    pub fn value(&self, sql: &str) -> String {
        let p = self.db().display().to_string();
        pollster::block_on(async {
            let db = turso::Builder::new_local(&p)
                .experimental_multiprocess_wal(true)
                .build()
                .await
                .unwrap();
            let conn = db.connect().unwrap();
            let mut rows = conn
                .query(sql, ())
                .await
                .unwrap_or_else(|e| panic!("{sql}: {e}"));
            let row = rows
                .next()
                .await
                .unwrap()
                .unwrap_or_else(|| panic!("no row: {sql}"));
            text(&row.get_value(0).unwrap(), false)
        })
    }

    /// Every table, every row, every column, the same query for both sides.
    /// Timestamps written during the run read `{NOW}`; a vector reads as its
    /// length and a hash of its bytes.
    pub fn state(&self) -> String {
        state_of(&self.db())
    }

    /// [`Machine::state`] of another database file under the root.
    pub fn state_of(&self, rel: &str) -> String {
        state_of(&self.path(rel))
    }
}

/// What the runbooks parse, held for every run of the binary: a refusal is
/// exit 2 with `refused: ` first on stderr, a failure exit 1 with an `error: `
/// line, a usage error exit 64 with `error: ` first, and anything else on
/// stderr opens with `warning: `, `warn: ` or a progress label (`backup: `,
/// `<table>: batch committed`).
fn check_prefixes(args: &[String], run: &Run) {
    let first = run.stderr.lines().next().unwrap_or("");
    let progress = |l: &str| {
        l.starts_with("backup: ")
            || ["semantic", "episodic", "instructions"]
                .iter()
                .any(|t| l.starts_with(&format!("{t}: batch committed")))
    };
    let ok = match run.code {
        Some(0) => {
            run.stderr.is_empty()
                || first.starts_with("warning: ")
                || first.starts_with("warn: ")
                || progress(first)
        }
        Some(1) => run.stderr.lines().any(|l| l.starts_with("error: ")),
        Some(2) => first.starts_with("refused: "),
        Some(64) => first.starts_with("error: "),
        _ => false,
    };
    assert!(
        ok,
        "{args:?}: exit code and stderr prefix do not match the contract:\n{run}"
    );
}

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h: u64, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// A stamp written during the run: the fixture's dates all lie in 2025.
fn is_run_timestamp(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 19
        && b.iter().enumerate().all(|(i, c)| match i {
            4 | 7 => *c == b'-',
            10 => *c == b' ',
            13 | 16 => *c == b':',
            _ => c.is_ascii_digit(),
        })
        && &s[..4] >= "2026"
}

fn text(v: &turso::Value, normalise: bool) -> String {
    match v {
        turso::Value::Null => "NULL".into(),
        turso::Value::Integer(i) => i.to_string(),
        turso::Value::Real(f) => format!("{f:?}"),
        turso::Value::Text(s) if normalise && is_run_timestamp(s) => "{NOW}".into(),
        turso::Value::Text(s) if normalise => format!("{s:?}"),
        turso::Value::Text(s) => s.clone(),
        turso::Value::Blob(b) => format!("blob[{}]:{:016x}", b.len(), fnv(b)),
    }
}

/// Every table of a database file and all its rows, in one canonical order.
pub fn state_of(db: &Path) -> String {
    let p = db.display().to_string();
    pollster::block_on(async {
        let db = turso::Builder::new_local(&p)
            .experimental_multiprocess_wal(true)
            .build()
            .await
            .unwrap();
        let conn = db.connect().unwrap();
        let mut names = Vec::new();
        let mut rows = conn
            .query(
                "SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
                 AND name NOT LIKE '__turso_internal%' ORDER BY name",
                (),
            )
            .await
            .unwrap();
        while let Some(row) = rows.next().await.unwrap() {
            names.push(text(&row.get_value(0).unwrap(), false));
        }
        drop(rows);
        let mut out = String::new();
        for name in names {
            let mut rows = conn
                .query(&format!("SELECT * FROM \"{name}\""), ())
                .await
                .unwrap();
            let columns = rows.column_names().join(", ");
            let mut lines = Vec::new();
            while let Some(row) = rows.next().await.unwrap() {
                let cells: Vec<String> = (0..row.column_count())
                    .map(|i| text(&row.get_value(i).unwrap(), true))
                    .collect();
                lines.push(cells.join(" | "));
            }
            lines.sort();
            out.push_str(&format!("== {name} ({} rows): {columns}\n", lines.len()));
            for line in lines {
                out.push_str(&format!("   {line}\n"));
            }
        }
        out
    })
}

pub struct Pair<'a> {
    ctx: &'a Ctx,
    pub test: &'static str,
    pub rs: Machine,
    pub py: Option<Machine>,
}

impl Pair<'_> {
    /// Both runs; the reference's only when it can run.
    pub fn run(&self, cmd: &Cmd) -> (Run, Option<Run>) {
        (self.rs.run(cmd), self.py.as_ref().map(|m| m.run(cmd)))
    }

    /// Run on both sides and require identical exit codes and output.
    pub fn same(&self, what: &str, cmd: &Cmd) -> Run {
        self.differ(what, cmd, &[])
    }

    /// Run on both sides; after `diffs` undo what they explain, the outputs
    /// must be identical, and each of them must have changed something.
    pub fn differ(&self, what: &str, cmd: &Cmd, diffs: &[&Difference]) -> Run {
        let (rs, py) = self.run(cmd);
        match py {
            Some(py) => compare(self.ctx, self.test, what, &rs, &py, diffs),
            None => self.ctx.skip(self.test, what),
        }
        rs
    }

    /// The two databases must hold the same rows.
    pub fn same_state(&self, what: &str) {
        match &self.py {
            Some(py) => {
                let (a, b) = (self.rs.state(), py.state());
                assert!(
                    a == b,
                    "{}: {what}: the database states differ:\n{}",
                    self.test,
                    diff(&b, &a)
                );
            }
            None => self
                .ctx
                .skip(self.test, &format!("{what} (database state)")),
        }
    }

    /// The same fixture copy under both roots.
    pub fn place(&self, fixture: Fixture, rel: &str) {
        self.rs.place(self.ctx, fixture, rel);
        if let Some(py) = &self.py {
            py.place(self.ctx, fixture, rel);
        }
    }

    /// The same symlink under both roots.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn symlink(&self, rel: &str, target: &str) {
        self.rs.symlink(rel, target);
        if let Some(py) = &self.py {
            py.symlink(rel, target);
        }
    }

    /// Write the same file under both roots.
    pub fn write(&self, rel: &str, contents: &str) {
        self.write_bytes(rel, contents.as_bytes());
    }

    /// The same for bytes that need not be text.
    pub fn write_bytes(&self, rel: &str, contents: &[u8]) {
        self.rs.write_bytes(rel, contents);
        if let Some(py) = &self.py {
            py.write_bytes(rel, contents);
        }
    }
}

/// Compare two runs once `diffs` have undone what they explain.
pub fn compare(ctx: &Ctx, test: &str, what: &str, rs: &Run, py: &Run, diffs: &[&Difference]) {
    let (mut a, mut b) = (rs.clone(), py.clone());
    for d in diffs {
        let undo = d
            .undo
            .unwrap_or_else(|| panic!("`{}` has no undo: assert it in the test instead", d.id));
        let (was_a, was_b) = (a.clone(), b.clone());
        undo(&mut a, Side::Rust);
        undo(&mut b, Side::Python);
        assert!(
            a != was_a || b != was_b,
            "{test}: {what}: the difference `{}` changed nothing here, so it does not apply:\n\
             binary:\n{rs}\nreference:\n{py}",
            d.id
        );
        ctx.asserted(d);
    }
    if a != b {
        panic!(
            "{test}: {what}: the binary and the reference differ in a way the difference table does not explain\n\
             (reference first; after undoing {:?}):\nexit: {:?} vs {:?}\nstdout:\n{}stderr:\n{}\n\
             binary as run:\n{rs}\nreference as run:\n{py}",
            diffs.iter().map(|d| d.id).collect::<Vec<_>>(),
            b.code,
            a.code,
            diff(&b.stdout, &a.stdout),
            diff(&b.stderr, &a.stderr),
        );
    }
}

/// A line diff, reference first, for a failure message.
pub fn diff(reference: &str, binary: &str) -> String {
    let mut out = String::new();
    for (i, (a, b)) in reference.lines().zip(binary.lines()).enumerate() {
        if a != b {
            out.push_str(&format!(
                "line {}:\n  reference: {a}\n  binary   : {b}\n",
                i + 1
            ));
        }
    }
    let (na, nb) = (reference.lines().count(), binary.lines().count());
    if na != nb {
        out.push_str(&format!("reference has {na} lines, binary {nb}\n"));
        let longer = if na > nb { reference } else { binary };
        for line in longer.lines().skip(na.min(nb)) {
            out.push_str(&format!("  extra: {line}\n"));
        }
    }
    out
}

/// The repository root, for the reference scripts and the runbook sources.
pub fn repo() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}
