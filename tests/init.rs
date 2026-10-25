//! `init` end to end: the real binary, installed into a directory of its own,
//! with a scrubbed environment -- no PATH (so no tursodb), a home of its own (so
//! no `~/.claude`), and an Ollama address that refuses at once.
//!
//! The embedded CLAUDE.md template still names the Python scripts, which a
//! binary cannot fill in, so on a machine without the instructions `init`
//! refuses outright; on a machine that has them (the legacy installer's block)
//! it runs to the end. The rendering itself is unit-tested against a template
//! the binary can fill.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_supercharged-memory");
const NAME: &str = "supercharged-memory";

const LEGACY_MD: &str = "# Mine\n\nAlways use pnpm.\n\n\
    <!-- BEGIN agentic-memory (managed by install-claude-md.sh) -->\n\
    <!-- supercharged-memory: synced-at 0123abcd -->\n\
    # Long-term memory — Turso vector DB (supercharged-memory)\n\
    \n\
    them as `python3 \"/repo/scripts/<cmd>\"`.\n\
    <!-- END agentic-memory -->\n";

struct Machine {
    root: PathBuf,
    exe: PathBuf,
}

impl Machine {
    /// A home and the binary in `<root>/app`, on the same file system as the
    /// built binary so it can be hard-linked rather than copied.
    fn new(test: &str) -> Machine {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("init-{test}"));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("app")).unwrap();
        fs::create_dir_all(root.join("home")).unwrap();
        let root = root.canonicalize().unwrap();
        let exe = root.join("app").join(NAME);
        if fs::hard_link(BIN, &exe).is_err() {
            fs::copy(BIN, &exe).unwrap();
        }
        Machine { root, exe }
    }

    fn with_legacy_block(test: &str) -> Machine {
        let m = Machine::new(test);
        fs::create_dir_all(m.root.join("home/.claude")).unwrap();
        fs::write(m.claude_md(), LEGACY_MD).unwrap();
        m
    }

    fn run_as(&self, exe: &Path, args: &[&str]) -> Output {
        Command::new(exe)
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env("OLLAMA_URL", "http://127.0.0.1:9")
            .args(args)
            .output()
            .unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_as(&self.exe, args)
    }

    fn db(&self) -> PathBuf {
        self.root
            .join("home/.local/share/turso/supercharged-memory.db")
    }

    fn dotenv(&self) -> PathBuf {
        self.root.join("app/.env")
    }

    fn claude_md(&self) -> PathBuf {
        self.root.join("home/.claude/CLAUDE.md")
    }

    /// Every directory and file under the machine, with each file's bytes. The
    /// binary itself is left out: it is hard-linked, and large.
    fn snapshot(&self) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        fn walk(p: &Path, skip: &Path, out: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
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
                out.insert(p.to_path_buf(), Some(fs::read(p).unwrap()));
            }
        }
        let mut out = BTreeMap::new();
        walk(&self.root, &self.exe, &mut out);
        out
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn a_fresh_machine_is_refused_while_the_template_names_the_python_scripts() {
    let m = Machine::new("fresh");
    let before = m.snapshot();
    for args in [&["init"][..], &["init", "-y"]] {
        let out = m.run(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", stderr(&out));
        let err = stderr(&out);
        assert!(err.starts_with("refused: "), "{args:?}: {err}");
        assert!(err.contains("{{BASE_PATH}}"), "{args:?}: {err}");
        assert_eq!(m.snapshot(), before, "{args:?} wrote something");
    }
}

#[test]
fn a_legacy_install_is_adopted_end_to_end() {
    let m = Machine::with_legacy_block("legacy");

    // The dry run: a plan, the loss of both missing dependencies in its last
    // line, and nothing written.
    let before = m.snapshot();
    let out = m.run(&["init"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    let last = text.lines().last().unwrap_or("");
    assert!(
        last.starts_with("Ask the user to confirm, and to acknowledge continuing without: "),
        "{text}"
    );
    assert!(
        last.contains("Ollama") && last.contains("tursodb"),
        "{last}"
    );
    assert_eq!(m.snapshot(), before, "the dry run wrote something");

    // -y: the database, the .env, and CLAUDE.md left exactly as it was.
    let out = m.run(&["init", "-y"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("init leaves it untouched"),
        "the cutover note:\n{text}"
    );
    assert!(m.db().exists());
    assert_eq!(fs::read_to_string(m.claude_md()).unwrap(), LEGACY_MD);
    let env = fs::read_to_string(m.dotenv()).unwrap();
    for line in [
        format!("SUPERCHARGED_MEMORY_TURSO_PATH={}", m.db().display()),
        format!(
            "BACKUP_DIR={}",
            m.root
                .join("home/.local/share/supercharged-memory/backups")
                .display()
        ),
        format!("SUPERCHARGED_MEMORY_VERSION={}", env!("CARGO_PKG_VERSION")),
        "EMBEDDINGS=off".to_string(),
    ] {
        assert!(env.lines().any(|l| l == line), "{line:?} not in:\n{env}");
    }

    // The binary now runs on what init wrote.
    let status = m.run(&["status"]);
    let s = stdout(&status);
    assert!(s.starts_with("DEGRADED 0\n"), "{s}");
    assert!(s.contains("EMBEDDINGS=off"), "{s}");

    // A second -y: nothing to do, nothing touched.
    let md = fs::read(m.claude_md()).unwrap();
    let dotenv = fs::read(m.dotenv()).unwrap();
    let db = fs::read(m.db()).unwrap();
    let out = m.run(&["init", "-y"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).lines().any(|l| l.starts_with("Nothing to do")),
        "{}",
        stdout(&out)
    );
    assert_eq!(fs::read(m.claude_md()).unwrap(), md);
    assert_eq!(fs::read(m.dotenv()).unwrap(), dotenv);
    assert_eq!(fs::read(m.db()).unwrap(), db);
}

#[cfg(unix)]
#[test]
fn through_a_symlink_init_writes_the_dotenv_beside_the_target() {
    let m = Machine::with_legacy_block("symlink");
    let bin = m.root.join("home/.local/bin");
    fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(&m.exe, bin.join(NAME)).unwrap();

    let out = m.run_as(&bin.join(NAME), &["init", "-y"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(m.dotenv().exists());
    assert!(!bin.join(".env").exists());
    assert!(stdout(&out).contains(&m.exe.display().to_string()));
}
