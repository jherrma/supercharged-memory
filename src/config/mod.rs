//! Configuration resolution: the one place every other module reads its settings.
//!
//! Each key is resolved through three layers, highest first: the real
//! environment, the `.env` beside the binary (see `dotenv`), and a built-in
//! default. Names and defaults are inherited from `scripts/memlib.py`.
//!
//! Resolution is a pure function of an environment lookup and an already-parsed
//! `.env`, not of the process, so the matrix that caught the shell/Python drift
//! (`TURSO_VFS=' none '` killed the daily backup while every script kept working)
//! is a unit test rather than a comment claiming two implementations agree.

pub mod dotenv;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::Error;
pub use dotenv::DotEnv;

pub const DB_ENV: &str = "SUPERCHARGED_MEMORY_TURSO_PATH";
/// The version that last wrote the `.env`, recorded by first-run setup and
/// bumped by `update`.
pub const VERSION_KEY: &str = "SUPERCHARGED_MEMORY_VERSION";
/// bge-m3's output width. Asserted on every embedding; see `crate::embed`.
pub const DIM: usize = 1024;
/// `memory_text`'s CHECK cap, keywords included. Enforced by `remember`.
#[allow(dead_code)]
pub const MAX_TEXT: usize = 2000;
pub const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";
pub const DEFAULT_EMBED_MODEL: &str = "bge-m3";
pub const DEFAULT_RECALL_ALPHA: f64 = 0.15;

/// Every key the binary reads, in the order `config` prints them. A key in the
/// `.env` that is not here draws a warning, never a refusal: a `.env` written by
/// a newer binary must not brick an older one. That check is advisory on purpose.
pub const KEYS: [&str; 10] = [
    DB_ENV,
    "BACKUP_DIR",
    VERSION_KEY,
    "EMBEDDINGS",
    "OLLAMA_URL",
    "EMBED_MODEL",
    "RECALL_ALPHA",
    "TURSO_VFS",
    "TURSO_BIN",
    "SUPERCHARGED_MEMORY_EVAL_DIR",
];

/// Where a value came from, so a wrong path explains itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The real process environment.
    Env,
    /// The `.env` beside the binary.
    DotEnv,
    /// Built in.
    Default,
}

impl Source {
    pub fn as_str(&self) -> &'static str {
        match self {
            Source::Env => "env",
            Source::DotEnv => ".env",
            Source::Default => "default",
        }
    }
}

/// One resolved key, for `config` to print.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting {
    pub key: &'static str,
    /// The value as the binary uses it; `None` when unset and without a default.
    pub value: Option<String>,
    pub source: Source,
    /// A differing `.env` value that the real environment overrides. The legacy
    /// `~/.claude/settings.json` `env` block does exactly this to every Claude
    /// Code tool call, so it has to be visible.
    pub shadowed: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub db_path: PathBuf,
    pub db_source: Source,
    pub backup_dir: PathBuf,
    pub ollama_url: String,
    /// Read by `remember`.
    #[allow(dead_code)]
    pub embed_model: String,
    /// Read by `recall` -- the keyword weight in the hybrid score.
    #[allow(dead_code)]
    pub recall_alpha: f64,
    /// Whether embeddings are in use at all (`EMBEDDINGS=on|off`). With `off` the
    /// binary never contacts Ollama: recall is keyword-only and writes store
    /// their rows without a vector.
    pub embeddings: bool,
    /// The version recorded in the `.env`, if any. Read by `update`.
    #[allow(dead_code)]
    pub version: Option<String>,
    /// Where to look for the `tursodb` CLI. Only detected, never required.
    pub turso_bin: PathBuf,
    /// The eval harness's derived query-embedding cache.
    #[allow(dead_code)]
    pub eval_dir: PathBuf,
    /// The IO backend to open with, or `None` for the platform default.
    pub vfs: Option<String>,
    /// Non-recursive places a live database plausibly lives, searched only when
    /// the configured path turns up empty — to tell "you pointed me somewhere
    /// wrong" apart from "your memory is genuinely gone".
    pub search_dirs: Vec<PathBuf>,
    /// Claude Code's config dir: `CLAUDE_CONFIG_DIR` from the real environment,
    /// else `~/.claude`. Where its CLAUDE.md and file-based memory live.
    pub claude_dir: PathBuf,
    /// Claude Code's global config file, where user-scope MCP servers are
    /// registered: `~/.claude.json`, or inside `CLAUDE_CONFIG_DIR` when that
    /// is set.
    pub claude_json: PathBuf,
    /// The user's home directory.
    pub home: PathBuf,
    /// The binary and its `.env`, for the reports that explain a path.
    pub exe: PathBuf,
    pub dotenv_path: PathBuf,
    pub dotenv_present: bool,
    /// Every key in `KEYS` order, with its value and source.
    pub settings: Vec<Setting>,
    /// Advisory findings (unknown `.env` keys), printed to stderr once per run.
    pub warnings: Vec<String>,
}

/// Everything the resolver needs from the outside world, so a test can supply it.
pub struct Env {
    pub vars: HashMap<String, String>,
    pub home: PathBuf,
    pub windows: bool,
    /// The `.env` beside the binary, already parsed.
    pub dotenv: DotEnv,
}

impl Env {
    /// The real process environment, and the `.env` beside the running binary.
    pub fn system() -> Result<Self, ConfigError> {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let exe =
            std::env::current_exe().map_err(|e| ConfigError::Unlocated { why: e.to_string() })?;
        Ok(Env {
            vars: std::env::vars().collect(),
            home,
            windows: cfg!(windows),
            dotenv: DotEnv::beside(&exe)?,
        })
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
        /// Where it came from, phrased for the message.
        from: String,
        why: String,
    },
    /// The `.env` is not in the accepted format.
    Malformed {
        path: PathBuf,
        line: usize,
        why: String,
    },
    /// The `.env` exists but cannot be read.
    Unreadable { path: PathBuf, why: String },
    /// The running binary cannot be located, so neither can its `.env`.
    Unlocated { why: String },
    /// The writer was asked for something the format cannot hold.
    Unwritable { path: PathBuf, why: String },
    /// The file system refused a write.
    Io { path: PathBuf, why: String },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Invalid {
                var,
                value,
                from,
                why,
            } => write!(f, "{var}={value:?} (from {from}) is not usable: {why}"),
            ConfigError::Malformed { path, line, why } => {
                write!(f, "malformed {}, line {line}: {why}", path.display())
            }
            ConfigError::Unreadable { path, why } => {
                write!(f, "cannot read {}: {why}", path.display())
            }
            ConfigError::Unlocated { why } => write!(
                f,
                "cannot locate the running binary, so its {} cannot be read: {why}",
                dotenv::FILE_NAME
            ),
            ConfigError::Unwritable { path, why } => {
                write!(f, "cannot write {}: {why}", path.display())
            }
            ConfigError::Io { path, why } => {
                write!(f, "writing {} failed: {why}", path.display())
            }
        }
    }
}

/// Every configuration problem is a guard rail: the command declines to run on a
/// config it cannot trust. Only a failed write is an error.
impl From<ConfigError> for Error {
    fn from(e: ConfigError) -> Self {
        match e {
            ConfigError::Io { .. } => Error::failed(e.to_string()),
            _ => Error::refused(e.to_string()),
        }
    }
}

/// One key's precedence: real environment > `.env` > built-in default. `None`
/// means neither layer supplies a value and the caller's default applies.
///
/// An empty value counts as unset, so an exported-but-empty variable does not
/// mask the `.env` -- except where `empty_is_a_value`, which `TURSO_VFS` needs:
/// there an empty value has always meant "no IO backend option".
pub fn pick<'a>(
    env: Option<&'a str>,
    dotenv: Option<&'a str>,
    empty_is_a_value: bool,
) -> Option<(&'a str, Source)> {
    let usable = |v: &&str| empty_is_a_value || !v.is_empty();
    env.filter(usable)
        .map(|v| (v, Source::Env))
        .or_else(|| dotenv.filter(usable).map(|v| (v, Source::DotEnv)))
}

/// Resolves keys one at a time and records each resolution for `config`.
struct Resolver<'a> {
    env: &'a Env,
    settings: Vec<Setting>,
}

impl<'a> Resolver<'a> {
    fn pick(&self, key: &str, empty_is_a_value: bool) -> Option<(&'a str, Source)> {
        pick(
            self.env.get(key),
            self.env.dotenv.get(key),
            empty_is_a_value,
        )
    }

    fn record(&mut self, key: &'static str, value: Option<String>, source: Source) {
        let shadowed = match source {
            Source::Env => self
                .env
                .dotenv
                .get(key)
                .filter(|d| !d.is_empty() && Some(*d) != self.env.get(key))
                .map(str::to_string),
            Source::DotEnv | Source::Default => None,
        };
        self.settings.push(Setting {
            key,
            value,
            source,
            shadowed,
        });
    }

    fn invalid(
        &self,
        key: &str,
        value: &str,
        source: Source,
        why: impl Into<String>,
    ) -> ConfigError {
        ConfigError::Invalid {
            var: key.into(),
            value: value.into(),
            from: match source {
                Source::DotEnv => self.env.dotenv.path.display().to_string(),
                Source::Env | Source::Default => source.as_str().into(),
            },
            why: why.into(),
        }
    }

    /// A path. One from the `.env` must be absolute: the file exists so that
    /// every process finds the same place whatever its working directory, and
    /// nothing expands a `~`.
    fn path(
        &mut self,
        key: &'static str,
        default: impl FnOnce() -> PathBuf,
    ) -> Result<(PathBuf, Source), ConfigError> {
        let (path, source) = match self.pick(key, false) {
            Some((v, Source::DotEnv)) if !Path::new(v).is_absolute() => {
                return Err(self.invalid(
                    key,
                    v,
                    Source::DotEnv,
                    "a path in the .env must be absolute; it is read from every working directory",
                ));
            }
            Some((v, source)) => (PathBuf::from(v), source),
            None => (default(), Source::Default),
        };
        self.record(key, Some(path.display().to_string()), source);
        Ok((path, source))
    }

    /// Free text, with an optional default.
    fn text(&mut self, key: &'static str, default: Option<&str>) -> Option<String> {
        let (value, source) = match self.pick(key, false) {
            Some((v, source)) => (Some(v.to_string()), source),
            None => (default.map(str::to_string), Source::Default),
        };
        self.record(key, value.clone(), source);
        value
    }
}

impl Config {
    pub fn from_env(env: &Env) -> Result<Config, ConfigError> {
        // XDG_DATA_HOME is the system's own variable, not a `.env` key.
        let data_home = env
            .get("XDG_DATA_HOME")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| env.home.join(".local/share"));
        let mut r = Resolver {
            env,
            settings: Vec::new(),
        };

        let (db_path, db_source) = r.path(DB_ENV, || {
            data_home.join("turso").join("supercharged-memory.db")
        })?;

        // NOTE: memlib.py defaulted BACKUP_DIR to the repo's own backups/
        // directory, derived from __file__. A binary has no repo, so the default
        // moves under XDG, and an install that keeps its dumps elsewhere records
        // BACKUP_DIR in the .env. The candidate scan looks in whatever this
        // resolves to, so the MISSING failsafe follows it.
        let (backup_dir, _) = r.path("BACKUP_DIR", || {
            data_home.join("supercharged-memory").join("backups")
        })?;

        let version = r.text(VERSION_KEY, None);

        let embeddings = match r.pick("EMBEDDINGS", false) {
            Some((raw, source)) => match raw.trim().to_ascii_lowercase().as_str() {
                "on" => (true, source),
                "off" => (false, source),
                _ => return Err(r.invalid("EMBEDDINGS", raw, source, "expected on or off")),
            },
            None => (true, Source::Default),
        };
        r.record(
            "EMBEDDINGS",
            Some(if embeddings.0 { "on" } else { "off" }.into()),
            embeddings.1,
        );

        let ollama_url = match r.pick("OLLAMA_URL", false) {
            Some((v, source)) => (v.trim_end_matches('/').to_string(), source),
            None => (DEFAULT_OLLAMA_URL.to_string(), Source::Default),
        };
        r.record("OLLAMA_URL", Some(ollama_url.0.clone()), ollama_url.1);

        let embed_model = r
            .text("EMBED_MODEL", Some(DEFAULT_EMBED_MODEL))
            .unwrap_or_else(|| DEFAULT_EMBED_MODEL.into());

        // Whitespace-only counts as unset here, as it always has.
        let recall_alpha = match r.pick("RECALL_ALPHA", false) {
            Some((raw, source)) if !raw.trim().is_empty() => {
                let alpha = raw
                    .trim()
                    .parse::<f64>()
                    .map_err(|e| r.invalid("RECALL_ALPHA", raw, source, e.to_string()))?;
                (alpha, source)
            }
            _ => (DEFAULT_RECALL_ALPHA, Source::Default),
        };
        r.record(
            "RECALL_ALPHA",
            Some(recall_alpha.0.to_string()),
            recall_alpha.1,
        );

        let (raw_vfs, vfs_source) = match r.pick("TURSO_VFS", true) {
            Some((v, source)) => (Some(v), source),
            None => (None, Source::Default),
        };
        let vfs = resolve_vfs(raw_vfs, env.windows);
        r.record(
            "TURSO_VFS",
            Some(vfs.clone().unwrap_or_else(|| "none".into())),
            vfs_source,
        );

        let (turso_bin, _) = r.path("TURSO_BIN", || env.home.join(".turso").join("tursodb"))?;
        let (eval_dir, _) = r.path("SUPERCHARGED_MEMORY_EVAL_DIR", || {
            db_path.parent().unwrap_or(Path::new(".")).join("eval")
        })?;

        // Claude Code's own variable, not a `.env` key: the binary must look where
        // Claude Code itself looks, and only the real environment tells it that.
        // Re-collected from its components so it prints the way the reference's
        // `pathlib.Path` printed it -- no trailing or doubled separator.
        let config_dir = env.get("CLAUDE_CONFIG_DIR").filter(|s| !s.is_empty());
        let claude_dir: PathBuf = config_dir
            .map(PathBuf::from)
            .unwrap_or_else(|| env.home.join(".claude"))
            .components()
            .collect();
        let claude_json = match config_dir {
            Some(_) => claude_dir.join(".claude.json"),
            None => env.home.join(".claude.json"),
        };

        let mut settings = r.settings;
        settings.sort_by_key(|s| KEYS.iter().position(|k| *k == s.key));

        let warnings = env
            .dotenv
            .entries()
            .iter()
            .filter(|e| !KEYS.contains(&e.key.as_str()))
            .map(|e| {
                format!(
                    "unknown key {} in {} (line {}) is ignored: a newer binary wrote it, or it is a typo",
                    e.key,
                    env.dotenv.path.display(),
                    e.line
                )
            })
            .collect();

        Ok(Config {
            search_dirs: search_dirs(&db_path, &data_home, env),
            db_path,
            db_source,
            backup_dir,
            ollama_url: ollama_url.0,
            embed_model,
            recall_alpha: recall_alpha.0,
            embeddings: embeddings.0,
            version,
            turso_bin,
            eval_dir,
            vfs,
            claude_dir,
            claude_json,
            home: env.home.clone(),
            exe: env.dotenv.exe.clone(),
            dotenv_path: env.dotenv.path.clone(),
            dotenv_present: env.dotenv.present(),
            settings,
            warnings,
        })
    }

    pub fn db_exists(&self) -> bool {
        self.db_path.exists()
    }

    /// How the database path was arrived at, phrased for `status`. With no
    /// `.env` it says where one was looked for: a binary moved away from its
    /// `.env` must read as "wrong place", not as lost data.
    pub fn db_source_label(&self) -> String {
        match self.db_source {
            Source::Env => format!("{DB_ENV} env var"),
            Source::DotEnv => format!("{DB_ENV} in {}", self.dotenv_path.display()),
            Source::Default if self.dotenv_present => format!(
                "built-in default (no {DB_ENV} in the environment or in {})",
                self.dotenv_path.display()
            ),
            Source::Default => format!(
                "built-in default (no {DB_ENV} set; no {} next to {})",
                dotenv::FILE_NAME,
                self.exe.display()
            ),
        }
    }

    /// One key's resolution. Every key in `KEYS` is present.
    pub fn setting(&self, key: &str) -> Option<&Setting> {
        self.settings.iter().find(|s| s.key == key)
    }
}

/// What `config` prints: the binary, its `.env`, then every key with its value
/// and source.
pub fn report(cfg: &Config) -> Vec<String> {
    let mut lines = vec![
        format!("binary: {}", cfg.exe.display()),
        format!(
            ".env  : {} ({})",
            cfg.dotenv_path.display(),
            if cfg.dotenv_present {
                "found"
            } else {
                "not found"
            }
        ),
    ];
    let width = KEYS.iter().map(|k| k.len()).max().unwrap_or(0);
    for s in &cfg.settings {
        let value = s.value.as_deref().unwrap_or("(unset)");
        let source = match &s.shadowed {
            Some(d) => format!("{}; overrides .env value \"{d}\"", s.source.as_str()),
            None => s.source.as_str().to_string(),
        };
        lines.push(format!("{:<width$} = {value}  [{source}]", s.key));
    }
    lines.push("precedence: real environment > .env > built-in default".into());
    lines
}

/// `config`: print every resolved key, its value and its source. Read-only.
pub fn run(cfg: &Config) -> crate::error::Result<()> {
    for line in report(cfg) {
        println!("{line}");
    }
    Ok(())
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
pub(crate) mod tests {
    use super::*;
    use crate::error::EXIT_REFUSED;

    pub(crate) const EXE: &str = "/opt/sm/supercharged-memory";

    /// A `.env` beside `EXE` holding `text`.
    pub(crate) fn dotenv(text: &str) -> DotEnv {
        DotEnv {
            entries: Some(dotenv::parse(text).unwrap()),
            ..DotEnv::absent(PathBuf::from(EXE))
        }
    }

    fn env_with(pairs: &[(&str, &str)], dot: DotEnv) -> Env {
        Env {
            vars: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            home: PathBuf::from("/home/tester"),
            windows: false,
            dotenv: dot,
        }
    }

    fn env(pairs: &[(&str, &str)]) -> Env {
        env_with(pairs, DotEnv::absent(PathBuf::from(EXE)))
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
        assert!(c.embeddings);
        assert_eq!(c.version, None);
        assert_eq!(c.turso_bin, PathBuf::from("/home/tester/.turso/tursodb"));
        assert_eq!(
            c.eval_dir,
            PathBuf::from("/home/tester/.local/share/turso/eval")
        );
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
        let err: Error = Config::from_env(&env(&[("RECALL_ALPHA", "0,15")]))
            .unwrap_err()
            .into();
        assert!(err.to_string().contains("RECALL_ALPHA"));
        assert!(err.to_string().starts_with("refused: "), "{err}");
        assert_eq!(err.exit_code(), EXIT_REFUSED);
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

    // ---- precedence: real env > .env > default ----------------------------

    /// (real env, .env, empty_is_a_value, the winner)
    type Precedence = (
        Option<&'static str>,
        Option<&'static str>,
        bool,
        Option<(&'static str, Source)>,
    );

    #[test]
    fn precedence_matrix() {
        let cases: &[Precedence] = &[
            (Some("e"), Some("d"), false, Some(("e", Source::Env))),
            (Some("e"), None, false, Some(("e", Source::Env))),
            (Some(""), Some("d"), false, Some(("d", Source::DotEnv))),
            (Some(""), None, false, None),
            (None, Some("d"), false, Some(("d", Source::DotEnv))),
            (None, None, false, None),
            // An empty .env value is unset too.
            (None, Some(""), false, None),
            (Some(""), Some(""), false, None),
            // TURSO_VFS: an empty value is a value ("no IO backend option").
            (Some(""), Some("d"), true, Some(("", Source::Env))),
            (None, Some(""), true, Some(("", Source::DotEnv))),
            (Some("e"), Some("d"), true, Some(("e", Source::Env))),
            (None, None, true, None),
        ];
        for (e, d, empty_is_a_value, want) in cases {
            assert_eq!(
                pick(*e, *d, *empty_is_a_value),
                *want,
                "env={e:?} .env={d:?} empty_is_a_value={empty_is_a_value}"
            );
        }
    }

    /// A distinct, valid value per key for each layer.
    fn sample(key: &str) -> (&'static str, &'static str) {
        match key {
            DB_ENV => ("/env/mem.db", "/dot/mem.db"),
            "BACKUP_DIR" => ("/env/backups", "/dot/backups"),
            VERSION_KEY => ("0.2.0", "0.1.0"),
            "EMBEDDINGS" => ("off", "on"),
            "OLLAMA_URL" => ("http://env:1", "http://dot:1"),
            "EMBED_MODEL" => ("env-model", "dot-model"),
            "RECALL_ALPHA" => ("0.25", "0.5"),
            "TURSO_VFS" => ("envvfs", "dotvfs"),
            "TURSO_BIN" => ("/env/tursodb", "/dot/tursodb"),
            "SUPERCHARGED_MEMORY_EVAL_DIR" => ("/env/eval", "/dot/eval"),
            other => panic!("no sample for {other}"),
        }
    }

    /// The same matrix through the real resolver, for every key: real env set /
    /// empty / unset x `.env` set / absent.
    #[test]
    fn every_key_resolves_through_the_three_layers() {
        #[derive(Clone, Copy, Debug)]
        enum Real {
            Set,
            Empty,
            Unset,
        }
        for key in KEYS {
            let (ev, dv) = sample(key);
            for real in [Real::Set, Real::Empty, Real::Unset] {
                for dot_set in [true, false] {
                    let vars: Vec<(&str, &str)> = match real {
                        Real::Set => vec![(key, ev)],
                        Real::Empty => vec![(key, "")],
                        Real::Unset => vec![],
                    };
                    let dot = if dot_set {
                        dotenv(&format!("{key}={dv}\n"))
                    } else {
                        DotEnv::absent(PathBuf::from(EXE))
                    };
                    let c = Config::from_env(&env_with(&vars, dot)).unwrap();
                    let s = c
                        .setting(key)
                        .unwrap_or_else(|| panic!("{key} missing from settings"));
                    let (want_source, want_value) = match (real, dot_set) {
                        (Real::Set, _) => (Source::Env, Some(ev)),
                        (Real::Empty, _) if key == "TURSO_VFS" => (Source::Env, None),
                        (_, true) => (Source::DotEnv, Some(dv)),
                        (_, false) => (Source::Default, None),
                    };
                    let case = format!("{key}: real={real:?} .env set={dot_set}");
                    assert_eq!(s.source, want_source, "{case}");
                    if let Some(v) = want_value {
                        assert_eq!(s.value.as_deref(), Some(v), "{case}");
                    }
                }
            }
        }
    }

    #[test]
    fn every_documented_key_is_known() {
        for key in [
            "SUPERCHARGED_MEMORY_TURSO_PATH",
            "BACKUP_DIR",
            "SUPERCHARGED_MEMORY_VERSION",
            "EMBEDDINGS",
            "OLLAMA_URL",
            "EMBED_MODEL",
            "RECALL_ALPHA",
            "TURSO_VFS",
            "TURSO_BIN",
            "SUPERCHARGED_MEMORY_EVAL_DIR",
        ] {
            assert!(KEYS.contains(&key), "{key}");
        }
        let c = Config::from_env(&env(&[])).unwrap();
        assert_eq!(
            c.settings.iter().map(|s| s.key).collect::<Vec<_>>(),
            KEYS.to_vec()
        );
    }

    #[test]
    fn dotenv_values_reach_the_typed_fields() {
        let c = Config::from_env(&env_with(
            &[],
            dotenv(
                "SUPERCHARGED_MEMORY_TURSO_PATH=\"/my data/mem.db\"\n\
                 BACKUP_DIR=/dot/backups\n\
                 SUPERCHARGED_MEMORY_VERSION=0.3.0\n\
                 EMBEDDINGS=off\n\
                 OLLAMA_URL=http://box:11434/\n\
                 EMBED_MODEL=other\n\
                 RECALL_ALPHA=0.2\n\
                 TURSO_VFS=memory\n\
                 TURSO_BIN=/opt/tursodb\n\
                 SUPERCHARGED_MEMORY_EVAL_DIR=/dot/eval\n",
            ),
        ))
        .unwrap();
        assert_eq!(c.db_path, PathBuf::from("/my data/mem.db"));
        assert_eq!(c.db_source, Source::DotEnv);
        assert_eq!(c.backup_dir, PathBuf::from("/dot/backups"));
        assert_eq!(c.version.as_deref(), Some("0.3.0"));
        assert!(!c.embeddings);
        assert_eq!(c.ollama_url, "http://box:11434");
        assert_eq!(c.embed_model, "other");
        assert_eq!(c.recall_alpha, 0.2);
        assert_eq!(c.vfs.as_deref(), Some("memory"));
        assert_eq!(c.turso_bin, PathBuf::from("/opt/tursodb"));
        assert_eq!(c.eval_dir, PathBuf::from("/dot/eval"));
        // A sibling of the configured database is still searched first.
        assert_eq!(c.search_dirs[0], PathBuf::from("/my data"));
    }

    #[test]
    fn the_claude_config_dir_defaults_under_home_and_follows_its_env_var() {
        let c = Config::from_env(&env(&[])).unwrap();
        assert_eq!(c.claude_dir, PathBuf::from("/home/tester/.claude"));
        assert_eq!(c.home, PathBuf::from("/home/tester"));
        // Claude Code's global config sits in the home directory by default,
        // and inside the config dir when one is set.
        assert_eq!(c.claude_json, PathBuf::from("/home/tester/.claude.json"));
        let c = Config::from_env(&env(&[("CLAUDE_CONFIG_DIR", "/cfg/claude")])).unwrap();
        assert_eq!(c.claude_dir, PathBuf::from("/cfg/claude"));
        assert_eq!(c.claude_json, PathBuf::from("/cfg/claude/.claude.json"));
        // Empty is unset, as everywhere else.
        let c = Config::from_env(&env(&[("CLAUDE_CONFIG_DIR", "")])).unwrap();
        assert_eq!(c.claude_dir, PathBuf::from("/home/tester/.claude"));
        // Spelled the way the reference spells a path: no trailing or doubled slash.
        let c = Config::from_env(&env(&[("CLAUDE_CONFIG_DIR", "/cfg//claude/")])).unwrap();
        assert_eq!(c.claude_dir.to_str(), Some("/cfg/claude"));
    }

    /// Claude Code reads its config dir from its own environment, so the binary
    /// must look in the same place: a `.env` value would point the probe at a
    /// directory Claude Code never loads.
    #[test]
    fn the_claude_config_dir_is_not_read_from_the_dotenv() {
        let c =
            Config::from_env(&env_with(&[], dotenv("CLAUDE_CONFIG_DIR=/dot/claude\n"))).unwrap();
        assert_eq!(c.claude_dir, PathBuf::from("/home/tester/.claude"));
        assert_eq!(c.warnings.len(), 1, "{:?}", c.warnings);
        assert!(
            c.warnings[0].contains("CLAUDE_CONFIG_DIR"),
            "{:?}",
            c.warnings
        );
    }

    #[test]
    fn the_eval_dir_defaults_beside_the_resolved_database() {
        let c = Config::from_env(&env_with(
            &[],
            dotenv("SUPERCHARGED_MEMORY_TURSO_PATH=/dot/mem.db\n"),
        ))
        .unwrap();
        assert_eq!(c.eval_dir, PathBuf::from("/dot/eval"));
    }

    #[test]
    fn the_real_env_overriding_a_differing_dotenv_value_is_visible() {
        let c = Config::from_env(&env_with(
            &[(DB_ENV, "/legacy/mem.db")],
            dotenv("SUPERCHARGED_MEMORY_TURSO_PATH=/new/mem.db\nBACKUP_DIR=/b\n"),
        ))
        .unwrap();
        let s = c.setting(DB_ENV).unwrap();
        assert_eq!(s.source, Source::Env);
        assert_eq!(s.shadowed.as_deref(), Some("/new/mem.db"));
        // Same value in both: nothing is being overridden.
        let c = Config::from_env(&env_with(
            &[("BACKUP_DIR", "/b")],
            dotenv("BACKUP_DIR=/b\n"),
        ))
        .unwrap();
        assert_eq!(c.setting("BACKUP_DIR").unwrap().shadowed, None);
    }

    #[test]
    fn an_unknown_dotenv_key_is_a_warning_not_a_refusal() {
        let c = Config::from_env(&env_with(&[], dotenv("A_NEWER_KEY=1\nBACKUP_DIR=/b\n"))).unwrap();
        assert_eq!(c.warnings.len(), 1, "{:?}", c.warnings);
        assert!(c.warnings[0].contains("A_NEWER_KEY"), "{:?}", c.warnings);
        assert!(c.warnings[0].contains("/opt/sm/.env"), "{:?}", c.warnings);
        assert!(c.warnings[0].contains("line 1"), "{:?}", c.warnings);
        assert_eq!(c.backup_dir, PathBuf::from("/b"));
    }

    #[test]
    fn an_unusable_dotenv_value_is_refused_and_names_the_file() {
        for text in ["RECALL_ALPHA=0,15\n", "EMBEDDINGS=yes\n"] {
            let err: Error = Config::from_env(&env_with(&[], dotenv(text)))
                .unwrap_err()
                .into();
            let msg = err.to_string();
            assert_eq!(err.exit_code(), EXIT_REFUSED, "{text}");
            assert!(msg.starts_with("refused: "), "{msg}");
            assert!(msg.contains("/opt/sm/.env"), "{msg}");
        }
    }

    #[test]
    fn embeddings_accepts_on_and_off_only() {
        for (raw, want) in [("on", true), ("off", false), (" OFF ", false), ("On", true)] {
            let c = Config::from_env(&env(&[("EMBEDDINGS", raw)])).unwrap();
            assert_eq!(c.embeddings, want, "{raw:?}");
        }
        let err: Error = Config::from_env(&env(&[("EMBEDDINGS", "1")]))
            .unwrap_err()
            .into();
        assert_eq!(err.exit_code(), EXIT_REFUSED);
        assert!(err.to_string().starts_with("refused: "), "{err}");
    }

    /// The `.env` exists so that every process finds the same database whatever
    /// its working directory. A relative path would quietly undo that.
    #[test]
    fn a_relative_path_in_the_dotenv_is_refused() {
        for text in [
            "SUPERCHARGED_MEMORY_TURSO_PATH=mem.db\n",
            "BACKUP_DIR=~/backups\n",
            "TURSO_BIN=./tursodb\n",
            "SUPERCHARGED_MEMORY_EVAL_DIR=eval\n",
        ] {
            let err: Error = Config::from_env(&env_with(&[], dotenv(text)))
                .unwrap_err()
                .into();
            assert_eq!(err.exit_code(), EXIT_REFUSED, "{text}");
            assert!(err.to_string().starts_with("refused: "), "{err}");
            assert!(err.to_string().contains("absolute"), "{err}");
        }
    }

    // ---- labels and the `config` report ------------------------------------

    #[test]
    fn the_db_source_label_names_each_layer() {
        let by_env = Config::from_env(&env(&[(DB_ENV, "/e/mem.db")])).unwrap();
        assert!(by_env.db_source_label().contains("env var"));

        let by_dot = Config::from_env(&env_with(
            &[],
            dotenv("SUPERCHARGED_MEMORY_TURSO_PATH=/d/mem.db\n"),
        ))
        .unwrap();
        assert!(
            by_dot.db_source_label().contains("/opt/sm/.env"),
            "{}",
            by_dot.db_source_label()
        );

        let by_default = Config::from_env(&env(&[])).unwrap();
        let label = by_default.db_source_label();
        assert!(label.contains("built-in default"), "{label}");
        assert!(label.contains(&format!("no .env next to {EXE}")), "{label}");

        // A .env that exists but does not set the path says so instead.
        let by_default = Config::from_env(&env_with(&[], dotenv("BACKUP_DIR=/b\n"))).unwrap();
        let label = by_default.db_source_label();
        assert!(label.contains("built-in default"), "{label}");
        assert!(label.contains("/opt/sm/.env"), "{label}");
        assert!(!label.contains("no .env next to"), "{label}");
    }

    fn report_line(cfg: &Config, key: &str) -> String {
        report(cfg)
            .into_iter()
            .find(|l| l.starts_with(&format!("{key} ")))
            .unwrap_or_else(|| panic!("no line for {key}"))
    }

    #[test]
    fn config_labels_each_value_with_its_source() {
        let c = Config::from_env(&env_with(
            &[("OLLAMA_URL", "http://env:1")],
            dotenv("BACKUP_DIR=/dot/backups\n"),
        ))
        .unwrap();
        let line = report_line(&c, "OLLAMA_URL");
        assert!(
            line.contains("http://env:1") && line.ends_with("[env]"),
            "{line}"
        );
        let line = report_line(&c, "BACKUP_DIR");
        assert!(
            line.contains("/dot/backups") && line.ends_with("[.env]"),
            "{line}"
        );
        let line = report_line(&c, DB_ENV);
        assert!(
            line.contains("/home/tester/.local/share/turso/supercharged-memory.db")
                && line.ends_with("[default]"),
            "{line}"
        );
        let line = report_line(&c, VERSION_KEY);
        assert!(
            line.contains("(unset)") && line.ends_with("[default]"),
            "{line}"
        );
        // Every key gets exactly one line.
        for key in KEYS {
            report_line(&c, key);
        }
    }

    #[test]
    fn config_names_the_binary_and_the_dotenv() {
        let absent = report(&Config::from_env(&env(&[])).unwrap()).join("\n");
        assert!(absent.contains(EXE), "{absent}");
        assert!(absent.contains("/opt/sm/.env (not found)"), "{absent}");
        let present =
            report(&Config::from_env(&env_with(&[], dotenv("A=1\n"))).unwrap()).join("\n");
        assert!(present.contains("/opt/sm/.env (found)"), "{present}");
    }

    #[test]
    fn config_shows_a_shadowed_dotenv_value() {
        let c = Config::from_env(&env_with(
            &[(DB_ENV, "/legacy/mem.db")],
            dotenv("SUPERCHARGED_MEMORY_TURSO_PATH=/new/mem.db\n"),
        ))
        .unwrap();
        let line = report_line(&c, DB_ENV);
        assert!(line.contains("/legacy/mem.db"), "{line}");
        assert!(
            line.contains("overrides .env value \"/new/mem.db\""),
            "{line}"
        );
    }
}
