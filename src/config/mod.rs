//! Environment resolution: the one place every other module reads its settings.
//!
//! Names and defaults are inherited unchanged from `scripts/memlib.py`, because
//! an existing install already has them in `~/.claude/settings.json`.
//!
//! Resolution is a pure function of an environment lookup, not of the process
//! environment, so the matrix that caught the shell/Python drift
//! (`TURSO_VFS=' none '` killed the daily backup while every script kept working)
//! is a unit test rather than a comment claiming two implementations agree.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const DB_ENV: &str = "SUPERCHARGED_MEMORY_TURSO_PATH";
/// bge-m3's output width. Asserted on every embedding; see `crate::embed`.
pub const DIM: usize = 1024;
/// `memory_text`'s CHECK cap, keywords included. Enforced by story 005.
#[allow(dead_code)]
pub const MAX_TEXT: usize = 2000;
pub const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";
pub const DEFAULT_EMBED_MODEL: &str = "bge-m3";
pub const DEFAULT_RECALL_ALPHA: f64 = 0.15;

/// Where an environment variable came from, for `--status` to explain a wrong path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Env,
    Default,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub db_path: PathBuf,
    pub db_source: Source,
    pub backup_dir: PathBuf,
    pub ollama_url: String,
    /// Read by story 005 (`remember`).
    #[allow(dead_code)]
    pub embed_model: String,
    /// Read by story 004 (`recall`) -- the keyword weight in the hybrid score.
    #[allow(dead_code)]
    pub recall_alpha: f64,
    /// The IO backend to open with, or `None` for the platform default.
    pub vfs: Option<String>,
    /// Non-recursive places a live database plausibly lives, searched only when
    /// the configured path turns up empty — to tell "you pointed me somewhere
    /// wrong" apart from "your memory is genuinely gone".
    pub search_dirs: Vec<PathBuf>,
}

/// Everything the resolver needs from the outside world, so a test can supply it.
pub struct Env {
    pub vars: HashMap<String, String>,
    pub home: PathBuf,
    pub windows: bool,
}

impl Env {
    /// The real process environment.
    pub fn system() -> Self {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        Env {
            vars: std::env::vars().collect(),
            home,
            windows: cfg!(windows),
        }
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.vars.get(key).map(String::as_str)
    }
}

#[derive(Debug)]
pub enum ConfigError {
    /// A value was present but unusable. Refusing beats silently substituting a
    /// default: a mistyped RECALL_ALPHA would otherwise change every ranking
    /// without a word.
    Invalid {
        var: String,
        value: String,
        why: String,
    },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ConfigError::Invalid { var, value, why } = self;
        write!(f, "{var}={value:?} is not usable: {why}")
    }
}

impl Config {
    pub fn from_env(env: &Env) -> Result<Config, ConfigError> {
        let data_home = env
            .get("XDG_DATA_HOME")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| env.home.join(".local/share"));

        let (db_path, db_source) = match env.get(DB_ENV) {
            Some(p) if !p.is_empty() => (PathBuf::from(p), Source::Env),
            _ => (
                data_home.join("turso").join("supercharged-memory.db"),
                Source::Default,
            ),
        };

        // NOTE (story 003): memlib.py defaulted BACKUP_DIR to the repo's own
        // backups/ directory, derived from __file__. A binary has no repo, so the
        // default moves under XDG. An existing install therefore has to set
        // BACKUP_DIR or move its dumps -- story 016 owes this a migration note.
        // The candidate scan below still looks in whatever BACKUP_DIR resolves to,
        // so a machine that sets it keeps the MISSING failsafe intact.
        let backup_dir = match env.get("BACKUP_DIR") {
            Some(p) if !p.is_empty() => PathBuf::from(p),
            _ => data_home.join("supercharged-memory").join("backups"),
        };

        let recall_alpha = match env.get("RECALL_ALPHA") {
            Some(raw) if !raw.trim().is_empty() => {
                raw.trim()
                    .parse::<f64>()
                    .map_err(|e| ConfigError::Invalid {
                        var: "RECALL_ALPHA".into(),
                        value: raw.into(),
                        why: e.to_string(),
                    })?
            }
            _ => DEFAULT_RECALL_ALPHA,
        };

        let search_dirs = search_dirs(&db_path, &data_home, env);

        Ok(Config {
            db_path,
            db_source,
            backup_dir,
            ollama_url: env
                .get("OLLAMA_URL")
                .filter(|s| !s.is_empty())
                .unwrap_or(DEFAULT_OLLAMA_URL)
                .trim_end_matches('/')
                .to_string(),
            embed_model: env
                .get("EMBED_MODEL")
                .filter(|s| !s.is_empty())
                .unwrap_or(DEFAULT_EMBED_MODEL)
                .to_string(),
            recall_alpha,
            vfs: resolve_vfs(env.get("TURSO_VFS"), env.windows),
            search_dirs,
        })
    }

    pub fn db_exists(&self) -> bool {
        self.db_path.exists()
    }

    /// How the database path was arrived at, phrased for `--status`.
    pub fn db_source_label(&self) -> String {
        match self.db_source {
            Source::Env => format!("{DB_ENV} env var"),
            Source::Default => format!("built-in default (no {DB_ENV} set)"),
        }
    }
}

/// Windows' default IO backend refuses multiprocess WAL outright, which makes
/// every open fail; tursodb's own help names the IOCP backend as the fix.
/// `TURSO_VFS` overrides in both directions: name another backend, or set it to
/// `none` (or empty) to drop the option entirely -- what a future Turso that
/// supports multiprocess WAL natively on Windows will need.
///
/// Trim, then lowercase. The shell backup script lowercased without trimming, and
/// `TURSO_VFS=' none '` reached the command intact: the daily backup died with
/// `no such VFS:  none` while every Python script kept working.
pub fn resolve_vfs(raw: Option<&str>, windows: bool) -> Option<String> {
    match raw {
        None => windows.then(|| "experimental_win_iocp".to_string()),
        Some(v) => {
            let v = v.trim();
            if v.is_empty() || v.eq_ignore_ascii_case("none") {
                None
            } else {
                Some(v.to_string())
            }
        }
    }
}

fn search_dirs(db_path: &Path, data_home: &Path, env: &Env) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(parent) = db_path.parent() {
        dirs.push(parent.to_path_buf());
    }
    dirs.push(data_home.join("turso"));
    dirs.push(env.home.join(".local/share/turso"));
    dirs.push(env.home.join("turso"));
    dirs.push(env.home.join(".turso"));
    // SETUP.md's recommended Windows locations.
    for var in ["LOCALAPPDATA", "APPDATA"] {
        if let Some(v) = env.get(var).filter(|s| !s.is_empty()) {
            dirs.push(PathBuf::from(v).join("turso"));
        }
    }
    dirs.dedup();
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Env {
        Env {
            vars: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            home: PathBuf::from("/home/tester"),
            windows: false,
        }
    }

    #[test]
    fn defaults_match_the_python_implementation() {
        let c = Config::from_env(&env(&[])).unwrap();
        assert_eq!(
            c.db_path,
            PathBuf::from("/home/tester/.local/share/turso/supercharged-memory.db")
        );
        assert_eq!(c.db_source, Source::Default);
        assert_eq!(c.ollama_url, DEFAULT_OLLAMA_URL);
        assert_eq!(c.embed_model, DEFAULT_EMBED_MODEL);
        assert_eq!(c.recall_alpha, DEFAULT_RECALL_ALPHA);
        assert_eq!(c.vfs, None);
    }

    #[test]
    fn xdg_data_home_moves_the_default_database() {
        let c = Config::from_env(&env(&[("XDG_DATA_HOME", "/data")])).unwrap();
        assert_eq!(
            c.db_path,
            PathBuf::from("/data/turso/supercharged-memory.db")
        );
        assert!(c.search_dirs.contains(&PathBuf::from("/data/turso")));
    }

    #[test]
    fn the_env_var_wins_and_is_reported_as_the_source() {
        let c = Config::from_env(&env(&[(DB_ENV, "/elsewhere/mem.db")])).unwrap();
        assert_eq!(c.db_path, PathBuf::from("/elsewhere/mem.db"));
        assert_eq!(c.db_source, Source::Env);
        assert!(c.db_source_label().contains(DB_ENV));
        // The configured path's own directory is searched first when it turns up
        // empty -- a sibling database is the likeliest thing a wrong path missed.
        assert_eq!(c.search_dirs[0], PathBuf::from("/elsewhere"));
    }

    #[test]
    fn an_empty_variable_is_treated_as_unset() {
        let c = Config::from_env(&env(&[(DB_ENV, ""), ("OLLAMA_URL", "")])).unwrap();
        assert_eq!(c.db_source, Source::Default);
        assert_eq!(c.ollama_url, DEFAULT_OLLAMA_URL);
    }

    #[test]
    fn a_trailing_slash_on_the_ollama_url_does_not_double_up() {
        let c = Config::from_env(&env(&[("OLLAMA_URL", "http://box:11434/")])).unwrap();
        assert_eq!(c.ollama_url, "http://box:11434");
    }

    #[test]
    fn an_unparseable_recall_alpha_is_refused_not_defaulted() {
        let err = Config::from_env(&env(&[("RECALL_ALPHA", "0,15")])).unwrap_err();
        assert!(err.to_string().contains("RECALL_ALPHA"));
    }

    /// The exact matrix that caught the shell/Python drift. Each row is a value a
    /// real machine has had in `TURSO_VFS`.
    #[test]
    fn turso_vfs_resolution_matrix() {
        let cases: &[(Option<&str>, bool, Option<&str>)] = &[
            (None, false, None),
            (None, true, Some("experimental_win_iocp")),
            (Some(""), true, None),
            (Some("   "), true, None),
            (Some("none"), true, None),
            (Some("NONE"), true, None),
            (Some(" none "), true, None), // the one that killed the daily backup
            (Some(" None\t"), false, None),
            (
                Some("experimental_win_iocp"),
                false,
                Some("experimental_win_iocp"),
            ),
            (Some("  memory  "), false, Some("memory")),
        ];
        for (raw, windows, want) in cases {
            let got = resolve_vfs(*raw, *windows);
            assert_eq!(
                got.as_deref(),
                *want,
                "TURSO_VFS={raw:?} on windows={windows}"
            );
        }
    }
}
