//! `init`: first-run setup from the binary alone.
//!
//! Only an agent ever runs this binary, and an agent cannot answer a prompt. So
//! every choice the user owns is a flag; without `-y`, `init` changes nothing and
//! prints a plan whose last line starts with `Ask the user to confirm`; with `-y`
//! it recomputes the plan (the world may have moved), prints it, and applies it.
//!
//! It creates the database from the embedded schema, writes the `.env` beside
//! the binary, and appends the rendered instructions to CLAUDE.md when their
//! headline is absent. It never rewrites text, never writes an existing
//! database, and never overwrites a `.env` value that differs. Every outside
//! dependency is optional: a missing one is listed with exactly what the user
//! loses, and the confirm line carries that loss so it cannot be relayed without
//! it. `-y` is the acknowledgement.

pub mod location;
pub mod tursodb;

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::claude_md::{self, EpisodicMode};
use crate::config::dotenv::{self, OnConflict};
use crate::config::{Config, DB_ENV, VERSION_KEY};
use crate::embed::{self, Embedder, Unavailable};
use crate::error::{Error, Result};
use crate::find_existing;
use crate::store::turso::TursoStore;
use crate::store::{Store, candidates};
use crate::version;
use location::Place;
use tursodb::{Mcp, Tursodb};

/// What the user loses without Ollama and the embedding model.
const OLLAMA_LOSS: &str = "semantic (vector) recall (keyword-only instead), the vector \
     near-duplicate guard (a weaker text-only one instead), clustering for sleep's compaction \
     and pattern mining (D3, D4), and the eval harness";
/// What the user loses without tursodb.
const TURSODB_LOSS: &str = "broad ad-hoc reads (only the CLI's fixed retrieval commands remain)";

/// The flags, as given.
#[derive(Debug, Clone, Default)]
pub struct Args {
    pub db_path: Option<PathBuf>,
    pub backup_dir: Option<PathBuf>,
    /// Validated by `init` itself, so an invalid mode is a refusal like every
    /// other guard rather than a usage error.
    pub episodic_mode: Option<String>,
    pub yes: bool,
}

/// Everything outside the three files `init` manages that the plan depends on:
/// the machine, and the dependency probes. A test supplies it.
pub struct Host {
    pub windows: bool,
    /// `std::env::consts::OS`: which backup scheduler to recommend.
    pub os: String,
    pub home: PathBuf,
    /// The variables the location classifier reads (see `location::VARS`).
    pub vars: HashMap<String, String>,
    /// Ollama and the model, probed even under `EMBEDDINGS=off`: the probe is
    /// what decides the value written.
    pub ollama: std::result::Result<(), Unavailable>,
    pub tursodb: Option<Tursodb>,
    /// Claude Code's global config, where a user-scope MCP server is registered.
    pub claude_json: PathBuf,
    /// Its text: `None` when there is no such file, `Err` when it cannot be read.
    pub claude_json_text: Option<std::result::Result<String, String>>,
}

impl Host {
    /// The real machine. Every probe here only reads.
    pub fn system(cfg: &Config) -> Host {
        let windows = cfg!(windows);
        let vars = location::VARS
            .iter()
            .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
            .collect();
        let claude_json_text = match fs::read(&cfg.claude_json) {
            Ok(bytes) => Some(String::from_utf8(bytes).map_err(|_| "not valid UTF-8".to_string())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => Some(Err(e.to_string())),
        };
        let tursodb = tursodb::find(&cfg.turso_bin, std::env::var_os("PATH").as_deref(), windows)
            .map(|path| Tursodb {
                version: tursodb::version(&path),
                path,
            });
        Host {
            windows,
            os: std::env::consts::OS.to_string(),
            home: cfg.home.clone(),
            vars,
            ollama: embed::Ollama::new(cfg).probe(),
            tursodb,
            claude_json: cfg.claude_json.clone(),
            claude_json_text,
        }
    }
}

/// What `init` would do, and the text that says so.
#[derive(Debug, Default)]
pub struct Plan {
    /// Everything printed before the closing line.
    lines: Vec<String>,
    /// Printed last: what the agent should offer once `init` is through.
    next: Vec<String>,
    db: PathBuf,
    create_db: bool,
    dotenv: PathBuf,
    dotenv_updates: Vec<(&'static str, String)>,
    /// Keys the merge adds; a differing value is never among them.
    dotenv_added: Vec<String>,
    claude_md: PathBuf,
    /// The block to append, when the headline is absent.
    block: Option<String>,
    /// Each absent dependency, with its loss, for the confirm line.
    missing: Vec<String>,
}

impl Plan {
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// Nothing would be written: the database exists, the `.env` has every key,
    /// and the instructions are installed.
    pub fn nothing_to_do(&self) -> bool {
        !self.create_db && self.dotenv_added.is_empty() && self.block.is_none()
    }

    /// The dry run's last line. Plain when nothing is missing; otherwise it
    /// names every absent dependency with its loss.
    pub fn confirm_line(&self) -> String {
        if self.missing.is_empty() {
            "Ask the user to confirm".to_string()
        } else {
            format!(
                "Ask the user to confirm, and to acknowledge continuing without: {}",
                self.missing.join("; ")
            )
        }
    }
}

/// The flags `init` checks itself, before anything is probed or read.
fn validate(args: &Args) -> Result<EpisodicMode> {
    for (flag, path) in [
        ("--db-path", &args.db_path),
        ("--backup-dir", &args.backup_dir),
    ] {
        if let Some(p) = path.as_ref().filter(|p| !p.is_absolute()) {
            return Err(relative(flag, p));
        }
    }
    match &args.episodic_mode {
        None => Ok(EpisodicMode::DEFAULT),
        Some(m) => EpisodicMode::parse(m).ok_or_else(|| {
            Error::refused(format!(
                "--episodic-mode {m:?} is not one of {} — nothing was written",
                EpisodicMode::ALL.map(EpisodicMode::as_str).join(", ")
            ))
        }),
    }
}

/// A path written into the `.env` is read from every working directory, and
/// the `.env` refuses a relative one on read.
fn relative(what: &str, path: &Path) -> Error {
    Error::refused(format!(
        "{what} {} is relative; the .env is read from every working directory, so give an \
         absolute path — nothing was written",
        path.display()
    ))
}

/// A value for the plan and where it came from: the flag, else the
/// configuration's own resolution (env, .env or default).
fn choose(
    flag: &Option<PathBuf>,
    cfg: &Config,
    key: &str,
    resolved: &Path,
) -> (PathBuf, &'static str) {
    match flag {
        Some(p) => (p.clone(), "flag"),
        None => (
            resolved.to_path_buf(),
            cfg.setting(key).map_or("default", |s| s.source.as_str()),
        ),
    }
}

/// Compute the plan. Reads, never writes: no file, no directory, no database
/// opened for writing. `template` is the CLAUDE.md template to render.
pub fn plan(cfg: &Config, args: &Args, host: &Host, template: &str) -> Result<Plan> {
    let mode = validate(args)?;
    let (db, db_source) = choose(&args.db_path, cfg, DB_ENV, &cfg.db_path);
    let (backups, backups_source) = choose(&args.backup_dir, cfg, "BACKUP_DIR", &cfg.backup_dir);
    for (what, path, source) in [
        ("the database path", &db, db_source),
        ("the backup directory", &backups, backups_source),
    ] {
        if !path.is_absolute() {
            return Err(relative(&format!("{what} (from {source})"), path));
        }
    }
    let places = location::Context {
        home: &host.home,
        windows: host.windows,
        vars: &host.vars,
    };

    let mut p = Plan {
        db: db.clone(),
        dotenv: cfg.dotenv_path.clone(),
        claude_md: cfg.claude_dir.join("CLAUDE.md"),
        ..Plan::default()
    };
    p.lines.push(if args.yes {
        "init -y: applying this plan.".to_string()
    } else {
        "init: dry run — nothing has been written. Relay this plan to the user; once they \
         confirm, re-run the same command with -y."
            .to_string()
    });

    binary(&mut p.lines, cfg, &places);
    p.create_db = database(&mut p.lines, cfg, &db, db_source, &backups, &places)?;

    let embeddings = if host.ollama.is_ok() { "on" } else { "off" };
    let updates = [
        (DB_ENV, db.display().to_string(), db_source),
        ("BACKUP_DIR", backups.display().to_string(), backups_source),
        (VERSION_KEY, version::VERSION.to_string(), "this binary"),
        ("EMBEDDINGS", embeddings.to_string(), "Ollama probe"),
    ];
    p.dotenv_added = dotenv_step(&mut p.lines, cfg, &updates)?;
    p.dotenv_updates = updates.into_iter().map(|(k, v, _)| (k, v)).collect();

    let mode_source = if args.episodic_mode.is_some() {
        "flag"
    } else {
        "default"
    };
    p.block = claude_md_step(
        &mut p.lines,
        &p.claude_md,
        cfg,
        host,
        (mode, mode_source),
        template,
    )?;

    p.lines.push(format!(
        "Ollama + {} (optional, recommended):",
        cfg.embed_model
    ));
    match &host.ollama {
        Ok(()) => p
            .lines
            .push(format!("  available at {} — EMBEDDINGS=on", cfg.ollama_url)),
        Err(why) => {
            p.lines.push(format!(
                "  not available: {why} — listed under Missing; EMBEDDINGS=off. Install steps: \
                 `runbook setup`."
            ));
            p.missing.push(format!(
                "Ollama + {} ({why}) — loses {OLLAMA_LOSS}",
                cfg.embed_model
            ));
        }
    }

    turso_step(&mut p.lines, &mut p.missing, cfg, host, &db);

    if !p.missing.is_empty() {
        p.lines.push(
            "Missing — optional dependencies; continuing without one is a loss the user must \
             acknowledge, and -y is that acknowledgement:"
                .to_string(),
        );
        for m in &p.missing {
            p.lines.push(format!("  - {m}"));
        }
        if host.ollama.is_err() {
            p.lines.push(
                "  Rows are stored without embeddings meanwhile; `embed --missing` fills them \
                 in once Ollama and the model are available."
                    .to_string(),
            );
        }
    }

    let exe = claude_md::mixed_path(&cfg.exe.to_string_lossy(), host.windows);
    p.lines.push(
        "Backup schedule (recommended; init never installs one) — once the user agrees, set \
         up a daily run of this binary's `backup`:"
            .to_string(),
    );
    for line in schedule(&host.os, &exe) {
        p.lines.push(format!("  {line}"));
    }

    if !find_existing::scan(&cfg.claude_dir).nothing_to_do() {
        p.next.push(format!(
            "File-based memory was found under {} (find-existing-memory: nothing_to_do is \
             false). Once init is through, offer the user to migrate it: `runbook migrate`.",
            cfg.claude_dir.display()
        ));
    }
    Ok(p)
}

/// The binary's own place: the instructions and the `.env` are tied to it.
fn binary(lines: &mut Vec<String>, cfg: &Config, places: &location::Context) {
    lines.push(format!("Binary: {}", cfg.exe.display()));
    let mut findings = Vec::new();
    if let Some(place) = cfg
        .exe
        .parent()
        .and_then(|dir| location::classify(dir, places))
    {
        findings.push(format!("it is in {}", describe(&place)));
    }
    if let Some(name) = cfg.exe.file_name().map(|n| n.to_string_lossy())
        && location::name_carries_version(&name)
    {
        findings.push(format!("its file name {name} carries a version"));
    }
    if !findings.is_empty() {
        lines.push(format!(
            "  warning: {} — the instructions and the .env are tied to this path, so move or \
             rename the binary to a permanent place under a stable name (such as \
             ~/.local/bin/supercharged-memory) first, and run init from there.",
            findings.join(", and ")
        ));
    }
}

fn describe(place: &Place) -> String {
    match place {
        Place::Cloud(service) => format!("a cloud-synced folder ({service})"),
        Place::Downloads => "a Downloads folder".to_string(),
        Place::Temp => "a temporary directory".to_string(),
    }
}

/// The database step: adopt a memory database, refuse anything else, or plan
/// to create one. Returns whether `-y` creates it.
fn database(
    lines: &mut Vec<String>,
    cfg: &Config,
    db: &Path,
    source: &str,
    backups: &Path,
    places: &location::Context,
) -> Result<bool> {
    lines.push(format!("Database: {}  [{source}]", db.display()));
    if source == "default" {
        lines.push(
            "  Ask the user where the database should live (--db-path <file>); this is the \
             built-in default."
                .to_string(),
        );
    }
    // symlink_metadata: a dangling link is something at that path, not nothing.
    let exists = fs::symlink_metadata(db).is_ok();
    if exists {
        let n = memory_count(cfg, db).map_err(|why| {
            Error::refused(format!(
                "{} exists but is not a memory database ({why}) — point --db-path at a memory \
                 database, or at a path that does not exist yet. Nothing was written",
                db.display()
            ))
        })?;
        lines.push(format!(
            "  exists: a memory database ({n} memories) — adopted as it is; opened only to \
             count them, never written"
        ));
    } else {
        lines.push(
            "  absent: -y creates it from the embedded schema, with any missing parent \
             directories"
                .to_string(),
        );
        candidate_lines(lines, cfg, db, backups);
    }
    if let Some(place) = location::classify(db, places) {
        let consequence = match place {
            Place::Cloud(_) => "cloud sync corrupts a live SQLite file",
            Place::Downloads => "it is easily cleaned out along with the downloads",
            Place::Temp => "it is emptied on reboot or by the system",
        };
        lines.push(format!(
            "  warning: it is in {} — {consequence}. Relay this before the user confirms; -y \
             proceeds with this path.",
            describe(&place)
        ));
    }
    Ok(!exists)
}

/// The number of memories in an existing file, or why it is not a memory
/// database. An empty or non-SQLite file is judged by its header alone: opening
/// an empty one would create WAL files beside it.
fn memory_count(cfg: &Config, db: &Path) -> std::result::Result<u64, String> {
    if let Some(why) = crate::store::turso::not_a_database(db).map_err(|e| e.to_string())? {
        return Err(why.into());
    }
    TursoStore::open(&at(cfg, db))
        .and_then(|s| s.total_memories())
        .map_err(reason)
}

/// The configuration, pointed at another database file.
fn at(cfg: &Config, db: &Path) -> Config {
    Config {
        db_path: db.to_path_buf(),
        ..cfg.clone()
    }
}

/// An error's message without its `error:`/`refused:` prefix, to quote inside
/// another message.
fn reason(e: Error) -> String {
    match e {
        Error::Refused(why) | Error::Failed(why) => why,
        other => other.to_string(),
    }
}

/// Other memory databases and dumps, so a new, empty database is never
/// confirmed while the real one sits somewhere else.
fn candidate_lines(lines: &mut Vec<String>, cfg: &Config, db: &Path, backups: &Path) {
    let mut search_dirs = cfg.search_dirs.clone();
    if let Some(parent) = db.parent()
        && !search_dirs.iter().any(|d| d == parent)
    {
        search_dirs.insert(0, parent.to_path_buf());
    }
    let found = candidates::find(&Config {
        db_path: db.to_path_buf(),
        backup_dir: backups.to_path_buf(),
        search_dirs,
        ..cfg.clone()
    });
    if found.is_empty() {
        return;
    }
    lines.push(
        "  Other memory was found. Ask the user whether one of these is their real memory \
         before confirming a new, empty database — an empty database strands the real one. To \
         use a database, re-run with --db-path <it>:"
            .to_string(),
    );
    for c in &found.dbs {
        lines.push(format!(
            "    CANDIDATE DB    : {} ({} memories)",
            c.path.display(),
            c.memories
        ));
    }
    for b in found.backups.iter().take(3) {
        lines.push(format!("    CANDIDATE BACKUP: {}", b.display()));
    }
}

/// The `.env` step: which keys a merge adds, which it leaves, and which values
/// the real environment overrides anyway. Returns the keys it would add.
fn dotenv_step(
    lines: &mut Vec<String>,
    cfg: &Config,
    updates: &[(&'static str, String, &str)],
) -> Result<Vec<String>> {
    let original = match fs::read_to_string(&cfg.dotenv_path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(Error::refused(format!(
                "cannot read {}: {e} — nothing was written",
                cfg.dotenv_path.display()
            )));
        }
    };
    let pairs: Vec<(&str, &str)> = updates.iter().map(|(k, v, _)| (*k, v.as_str())).collect();
    let merged = dotenv::merge(&cfg.dotenv_path, &original, &pairs, OnConflict::Keep)?;

    lines.push(format!(
        ".env: {} ({})",
        cfg.dotenv_path.display(),
        if cfg.dotenv_present {
            "exists"
        } else {
            "-y creates it"
        }
    ));
    for (key, value, source) in updates {
        let action = if merged.added.iter().any(|k| k == key) {
            "-y adds it".to_string()
        } else if let Some(c) = merged.kept.iter().find(|c| c.key == *key) {
            format!(
                "the .env holds {:?} — left alone, never overwritten (edit the .env by hand to \
                 change it)",
                c.existing
            )
        } else {
            "already set".to_string()
        };
        lines.push(format!("  {key} = {value}  [{source}] — {action}"));
    }
    // A legacy `~/.claude/settings.json` `env` entry is real environment for
    // every Claude Code tool call, so it beats the `.env` on every run.
    for (key, value, _) in updates {
        let Some(setting) = cfg
            .setting(key)
            .filter(|s| s.source == crate::config::Source::Env)
        else {
            continue;
        };
        let real = setting.value.clone().unwrap_or_default();
        let mut line = format!(
            "  warning: the real environment sets {key}={real} (typically an \"env\" entry in \
             ~/.claude/settings.json); it overrides the .env on every run"
        );
        if real != *value {
            line.push_str(&format!(
                " — and differs from {value}: every run uses {real} until that variable is \
                 removed"
            ));
        }
        lines.push(line);
    }
    Ok(merged.added)
}

/// The CLAUDE.md step: leave existing instructions alone, or render the block
/// to append. Returns the block.
fn claude_md_step(
    lines: &mut Vec<String>,
    path: &Path,
    cfg: &Config,
    host: &Host,
    (mode, mode_source): (EpisodicMode, &str),
    template: &str,
) -> Result<Option<String>> {
    let existing = match fs::read(path) {
        Ok(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(Error::refused(format!(
                "cannot read {}: {e} — nothing was written",
                path.display()
            )));
        }
    };
    lines.push(format!("CLAUDE.md: {}", path.display()));

    if let Some(text) = existing.as_deref().filter(|t| claude_md::has_headline(t)) {
        lines.push(
            "  the instructions' headline is present — left untouched (init never rewrites \
             text). To bring an existing block up to date, run `update`."
                .to_string(),
        );
        if claude_md::has_legacy_block(text) {
            lines.push("  It is a legacy block (synced-at <sha>). The cutover note:".to_string());
            for line in claude_md::CUTOVER_NOTE.lines() {
                lines.push(format!("    {line}"));
            }
        }
        return Ok(None);
    }

    let exe = claude_md::exe_for_instructions(&cfg.exe, host.windows).map_err(|why| {
        Error::refused(format!(
            "the binary's path cannot be written into the instructions: {why}. Move the binary \
             — nothing was written"
        ))
    })?;
    let rendered = claude_md::render(
        template,
        &[
            ("EXE", &exe),
            ("EPISODIC_MODE", mode.as_str()),
            ("EPISODIC_RULE", mode.rule()),
        ],
    )
    .map_err(|names| {
        Error::refused(format!(
            "the embedded CLAUDE.md template has placeholders this binary cannot fill: {} — \
             rendering them would install broken commands. Nothing was written",
            names.join(", ")
        ))
    })?;
    let block = claude_md::block(&rendered, version::VERSION);

    let creates = match (&existing, path.parent()) {
        (Some(_), _) => "after the text already there, which stays as it is",
        (None, Some(dir)) if !dir.exists() => "-y creates the file and its directory",
        (None, _) => "-y creates the file",
    };
    lines.push(format!(
        "  will append the managed block ({} lines; {creates})",
        block.lines().count()
    ));
    lines.push(format!("  binary path in the block: {exe}"));
    let ask = if mode_source == "default" {
        " — ask the user which one"
    } else {
        ""
    };
    lines.push(format!(
        "  EPISODIC_MODE = {}  [{mode_source}] — how much the agent stores as episodic \
         memory{ask}:",
        mode.as_str()
    ));
    for m in EpisodicMode::ALL {
        lines.push(format!("    {:<13} — {}", m.as_str(), m.rule()));
    }
    Ok(Some(block))
}

/// tursodb and the `turso` MCP server: detected and recommended, never
/// installed or registered.
fn turso_step(
    lines: &mut Vec<String>,
    missing: &mut Vec<String>,
    cfg: &Config,
    host: &Host,
    db: &Path,
) {
    lines.push("tursodb + the turso MCP server (optional, highly recommended):".to_string());
    match &host.tursodb {
        Some(t) => lines.push(format!(
            "  tursodb: {} ({}); this binary links Turso {} — both open the same file",
            t.path.display(),
            t.version.as_deref().unwrap_or("version unknown"),
            version::TURSO_VERSION
        )),
        None => {
            lines.push(format!(
                "  tursodb: not found at {} (TURSO_BIN) or on PATH — listed under Missing. \
                 Install steps: `runbook setup`.",
                cfg.turso_bin.display()
            ));
            missing.push(format!("tursodb — loses {TURSODB_LOSS}"));
        }
    }

    let register = tursodb::register_command(
        host.tursodb.as_ref().map(|t| t.path.as_path()),
        db,
        host.windows,
    );
    let at = format!("MCP (user scope, {})", host.claude_json.display());
    match tursodb::check(host.claude_json_text.as_ref(), db, host.windows) {
        Mcp::Registered(problems) if problems.is_empty() => lines.push(format!(
            "  {at}: registered read-only on this database, with multiprocess WAL"
        )),
        Mcp::Registered(problems) => {
            lines.push(format!("  {at}: registered, but:"));
            for problem in problems {
                lines.push(format!("    - {}", problem.describe()));
            }
            lines.push("  Once the user agrees, re-register it:".to_string());
            lines.push("    claude mcp remove turso --scope user".to_string());
            lines.push(format!("    {register}"));
        }
        Mcp::NotRegistered => {
            lines.push(format!(
                "  {at}: not registered. With it the agent can run broad ad-hoc reads; without \
                 it the user must be content with the CLI's limited retrieval (recall, list, \
                 show, coworkers --load, …). Recommend it; once the user agrees, run:"
            ));
            lines.push(format!("    {register}"));
            lines.push(
                "  --readonly makes the MCP's write tools fail, so writes go through the CLI; \
                 without --experimental-multiprocess-wal the server would lock this binary out."
                    .to_string(),
            );
        }
        Mcp::Unknown(why) => lines.push(format!(
            "  {at}: unknown — {why}. Check it by hand; do not add a second registration."
        )),
    }
}

/// Apply a plan just computed, re-checking each step's precondition as it goes:
/// a database file that appeared since the plan is refused rather than touched,
/// and a headline that appeared is left alone. Returns what was done.
pub fn apply(cfg: &Config, plan: &Plan) -> Result<Vec<String>> {
    let mut done = vec!["Applied:".to_string()];

    if plan.create_db {
        if let Some(dir) = plan.db.parent() {
            fs::create_dir_all(dir)
                .map_err(|e| Error::failed(format!("cannot create {}: {e}", dir.display())))?;
        }
        // create_new: a file that appeared since the plan is refused, never
        // touched.
        match fs::File::create_new(&plan.db) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                return Err(Error::refused(format!(
                    "{} appeared after the plan was made; it was not touched and nothing else \
                     was written. Re-run init to see the new plan",
                    plan.db.display()
                )));
            }
            Err(e) => {
                return Err(Error::failed(format!(
                    "cannot create {}: {e}",
                    plan.db.display()
                )));
            }
        }
        TursoStore::open(&at(cfg, &plan.db))
            .and_then(|store| store.create_schema())
            .map_err(|e| {
                Error::failed(format!(
                    "{} was created by this run, but creating its schema failed: {}. It holds \
                     no memories",
                    plan.db.display(),
                    reason(e)
                ))
            })?;
        done.push(format!(
            "  created {} from the embedded schema",
            plan.db.display()
        ));
    }

    if !plan.dotenv_added.is_empty() {
        let pairs: Vec<(&str, &str)> = plan
            .dotenv_updates
            .iter()
            .map(|(k, v)| (*k, v.as_str()))
            .collect();
        let merged = dotenv::set_keys(&plan.dotenv, &pairs, OnConflict::Keep)?;
        if merged.added != plan.dotenv_added {
            done.push(
                "  note: the .env changed since the plan was made; this is what was written:"
                    .to_string(),
            );
        }
        if merged.wrote {
            done.push(format!(
                "  wrote {}: added {}",
                plan.dotenv.display(),
                merged.added.join(", ")
            ));
        }
        for c in &merged.kept {
            done.push(format!(
                "  left {} = {:?} in the .env, not {:?}",
                c.key, c.existing, c.wanted
            ));
        }
    }

    if let Some(block) = &plan.block {
        let current = match fs::read(&plan.claude_md) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => {
                return Err(Error::failed(format!(
                    "cannot read {}: {e}",
                    plan.claude_md.display()
                )));
            }
        };
        if claude_md::has_headline(&current) {
            done.push(format!(
                "  {}: the instructions' headline appeared since the plan was made — left \
                 untouched",
                plan.claude_md.display()
            ));
        } else {
            append(
                &plan.claude_md,
                &format!("{}{block}", claude_md::separator(&current)),
            )
            .map_err(|e| {
                Error::failed(format!(
                    "appending to {} failed: {e}",
                    plan.claude_md.display()
                ))
            })?;
            done.push(format!(
                "  appended the managed block to {}",
                plan.claude_md.display()
            ));
            done.push("Restart the Claude Code session so it loads the instructions.".to_string());
        }
    }
    Ok(done)
}

/// Append, never rewrite: the user's text keeps its bytes, and a CLAUDE.md that
/// is a symlink (a dotfile manager's) stays one.
fn append(path: &Path, text: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut file = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()
}

/// The whole command against a given machine and template; every line goes to
/// `out`. Without `-y` it ends with the confirm line, or says there is nothing
/// to do.
pub fn execute(
    cfg: &Config,
    args: &Args,
    host: &Host,
    template: &str,
    out: &mut dyn FnMut(String),
) -> Result<()> {
    let plan = plan(cfg, args, host, template)?;
    for line in plan.lines() {
        out(line.clone());
    }
    if plan.nothing_to_do() {
        out(
            "Nothing to do: the database exists, the .env holds every key, and the \
             instructions are installed."
                .to_string(),
        );
        for line in &plan.next {
            out(line.clone());
        }
        return Ok(());
    }
    if args.yes {
        for line in apply(cfg, &plan)? {
            out(line);
        }
        for line in &plan.next {
            out(line.clone());
        }
    } else {
        for line in &plan.next {
            out(line.clone());
        }
        out(plan.confirm_line());
    }
    Ok(())
}

/// `init`.
pub fn run(cfg: &Config, args: &Args) -> Result<()> {
    // A bad flag is refused before anything is probed or read.
    validate(args)?;
    let host = Host::system(cfg);
    execute(cfg, args, &host, claude_md::TEMPLATE, &mut |line| {
        println!("{line}")
    })
}

/// The daily backup entry for this OS, invoking `<exe> backup` by absolute path.
/// Printed for the agent to set up once the user agrees; never installed.
fn schedule(os: &str, exe: &str) -> Vec<String> {
    let word = claude_md::shell_word(exe);
    match os {
        "linux" => vec![
            "systemd user timer — ~/.config/systemd/user/supercharged-memory-backup.service:"
                .to_string(),
            "  [Service]".to_string(),
            "  Type=oneshot".to_string(),
            format!("  ExecStart={word} backup"),
            "~/.config/systemd/user/supercharged-memory-backup.timer:".to_string(),
            "  [Timer]".to_string(),
            "  OnCalendar=daily".to_string(),
            "  Persistent=true".to_string(),
            "  [Install]".to_string(),
            "  WantedBy=timers.target".to_string(),
            "then: systemctl --user enable --now supercharged-memory-backup.timer".to_string(),
        ],
        "macos" => {
            let xml = exe
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;");
            vec![
                "launchd agent — ~/Library/LaunchAgents/com.supercharged-memory.backup.plist:"
                    .to_string(),
                "  <?xml version=\"1.0\" encoding=\"UTF-8\"?>".to_string(),
                "  <plist version=\"1.0\"><dict>".to_string(),
                "    <key>Label</key><string>com.supercharged-memory.backup</string>".to_string(),
                format!(
                    "    <key>ProgramArguments</key><array><string>{xml}</string>\
                     <string>backup</string></array>"
                ),
                "    <key>StartCalendarInterval</key><dict><key>Hour</key><integer>3</integer>\
                 <key>Minute</key><integer>30</integer></dict>"
                    .to_string(),
                "  </dict></plist>".to_string(),
                "then: launchctl load ~/Library/LaunchAgents/com.supercharged-memory.backup.plist"
                    .to_string(),
            ]
        }
        "windows" => vec![
            "Task Scheduler:".to_string(),
            format!(
                "schtasks /Create /SC DAILY /ST 03:30 /TN supercharged-memory-backup /TR \
                 \"\\\"{exe}\\\" backup\""
            ),
        ],
        _ => vec![
            "cron — crontab -e, then add:".to_string(),
            format!("30 3 * * * {word} backup"),
        ],
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;

    use super::*;
    use crate::claude_md::{BLOCK_BEGIN, BLOCK_END, HEADLINE};
    use crate::config::{DB_ENV, DotEnv, Env, VERSION_KEY};
    use crate::embed::fake::Fake;
    use crate::error::EXIT_REFUSED;

    /// A template shaped like the real one, naming only what the binary can fill.
    const TEMPLATE: &str = "# Long-term memory — Turso vector DB (supercharged-memory)\n\
                            \n\
                            Run `{{EXE}} status` first.\n\
                            **Episodic policy on this machine — `{{EPISODIC_MODE}}`: {{EPISODIC_RULE}}**\n";

    const DEAD: &str = "http://127.0.0.1:9";

    /// A machine in a directory of its own: the binary in `bin/`, a home, a
    /// Claude config dir and an XDG data dir. Nothing outside it is touched.
    struct Fixture {
        root: PathBuf,
        exe: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Fixture {
            let root = std::env::temp_dir().join(format!("sm-init-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join("bin")).unwrap();
            fs::create_dir_all(root.join("home")).unwrap();
            let root = root.canonicalize().unwrap();
            let exe = root.join("bin/supercharged-memory");
            fs::write(&exe, b"").unwrap();
            Fixture { root, exe }
        }

        fn path(&self, rel: &str) -> PathBuf {
            self.root.join(rel)
        }

        fn claude_md(&self) -> PathBuf {
            self.path("claude/CLAUDE.md")
        }

        fn dotenv(&self) -> PathBuf {
            self.path("bin/.env")
        }

        fn default_db(&self) -> PathBuf {
            self.path("data/turso/supercharged-memory.db")
        }

        fn default_backups(&self) -> PathBuf {
            self.path("data/supercharged-memory/backups")
        }

        /// Config as the binary at `exe` would resolve it, plus `vars`.
        fn cfg_for(&self, exe: &Path, vars: &[(&str, &str)]) -> Config {
            let mut all: HashMap<String, String> = [
                ("XDG_DATA_HOME", self.path("data")),
                ("CLAUDE_CONFIG_DIR", self.path("claude")),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string_lossy().into_owned()))
            .collect();
            all.insert("OLLAMA_URL".into(), DEAD.into());
            for (k, v) in vars {
                all.insert(k.to_string(), v.to_string());
            }
            Config::from_env(&Env {
                vars: all,
                home: self.path("home"),
                windows: false,
                dotenv: DotEnv::beside(exe).unwrap(),
            })
            .unwrap()
        }

        fn cfg(&self) -> Config {
            self.cfg_for(&self.exe, &[])
        }

        fn host(&self, ollama: std::result::Result<(), Unavailable>, tursodb: bool) -> Host {
            Host {
                windows: false,
                os: "linux".into(),
                home: self.path("home"),
                vars: HashMap::new(),
                ollama,
                tursodb: tursodb.then(|| Tursodb {
                    path: PathBuf::from("/opt/turso/tursodb"),
                    version: Some("Turso 0.7.2".into()),
                }),
                claude_json: self.path("home/.claude.json"),
                claude_json_text: None,
            }
        }

        /// Every directory and file under the fixture, with each file's bytes.
        fn snapshot(&self) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
            fn walk(p: &Path, out: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
                let meta = fs::symlink_metadata(p).unwrap();
                if meta.is_dir() {
                    out.insert(p.to_path_buf(), None);
                    for e in fs::read_dir(p).unwrap() {
                        walk(&e.unwrap().path(), out);
                    }
                } else {
                    out.insert(p.to_path_buf(), Some(fs::read(p).unwrap_or_default()));
                }
            }
            let mut out = BTreeMap::new();
            walk(&self.root, &mut out);
            out
        }
    }

    fn up() -> std::result::Result<(), Unavailable> {
        Ok(())
    }

    fn unreachable() -> std::result::Result<(), Unavailable> {
        Err(Unavailable::Unreachable { url: DEAD.into() })
    }

    fn not_pulled() -> std::result::Result<(), Unavailable> {
        Err(Unavailable::NotPulled {
            url: DEAD.into(),
            model: "bge-m3".into(),
        })
    }

    fn args(pairs: &[(&str, &str)], yes: bool) -> Args {
        let mut a = Args {
            yes,
            ..Args::default()
        };
        for (k, v) in pairs {
            match *k {
                "db" => a.db_path = Some(PathBuf::from(v)),
                "backups" => a.backup_dir = Some(PathBuf::from(v)),
                "mode" => a.episodic_mode = Some(v.to_string()),
                other => panic!("{other}"),
            }
        }
        a
    }

    /// Run the whole command and collect what it prints.
    fn init(cfg: &Config, args: &Args, host: &Host) -> Result<Vec<String>> {
        init_with(cfg, args, host, TEMPLATE)
    }

    fn init_with(cfg: &Config, args: &Args, host: &Host, template: &str) -> Result<Vec<String>> {
        let mut lines = Vec::new();
        execute(cfg, args, host, template, &mut |l| lines.push(l))?;
        Ok(lines)
    }

    fn text(lines: &[String]) -> String {
        lines.join("\n")
    }

    fn last(lines: &[String]) -> &str {
        lines.last().map(String::as_str).unwrap_or("")
    }

    fn refused(r: Result<Vec<String>>, phrase: &str) -> String {
        let err = r.expect_err(&format!("expected a refusal mentioning {phrase:?}"));
        assert_eq!(err.exit_code(), EXIT_REFUSED, "{err}");
        let msg = err.to_string();
        assert!(msg.starts_with("refused: "), "{msg}");
        assert!(msg.contains(phrase), "{phrase:?} not in: {msg}");
        msg
    }

    fn dotenv_value(f: &Fixture, key: &str) -> Option<String> {
        crate::config::dotenv::read(&f.dotenv())
            .unwrap()
            .and_then(|e| e.into_iter().find(|e| e.key == key).map(|e| e.value))
    }

    /// A memory database with `n` rows, outside the code under test.
    fn memory_db(path: &Path, n: usize) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let p = path.to_string_lossy().to_string();
        pollster::block_on(async {
            let db = turso::Builder::new_local(&p)
                .experimental_multiprocess_wal(true)
                .build()
                .await
                .unwrap();
            let conn = db.connect().unwrap();
            conn.execute_batch(crate::store::turso::SCHEMA)
                .await
                .unwrap();
            for i in 0..n {
                conn.execute(
                    &format!(
                        "INSERT INTO semantic_memory (category, memory_text) \
                         VALUES ('reference', 'fact {i}');"
                    ),
                    (),
                )
                .await
                .unwrap();
            }
        });
    }

    fn mtime(p: &Path) -> std::time::SystemTime {
        fs::metadata(p).unwrap().modified().unwrap()
    }

    // ---- the dry run ---------------------------------------------------------

    #[test]
    fn a_dry_run_changes_nothing_and_ends_with_the_confirm_line() {
        let f = Fixture::new("dry");
        let before = f.snapshot();
        let out = init(&f.cfg(), &Args::default(), &f.host(up(), true)).unwrap();
        assert_eq!(f.snapshot(), before, "a dry run wrote something");
        assert_eq!(last(&out), "Ask the user to confirm", "{}", text(&out));
        assert!(!f.default_db().exists());
        assert!(!f.dotenv().exists());
        assert!(!f.claude_md().exists());
        assert!(!f.path("claude").exists() && !f.path("data").exists());
    }

    #[test]
    fn the_plan_says_what_it_would_do() {
        let f = Fixture::new("plan-text");
        let out = text(&init(&f.cfg(), &Args::default(), &f.host(up(), true)).unwrap());
        for phrase in [
            "dry run",
            "-y",
            &f.default_db().display().to_string(),
            "embedded schema",
            &f.dotenv().display().to_string(),
            &f.claude_md().display().to_string(),
            "will append",
            // The database's location and the episodic mode are the user's.
            "Ask the user where the database should live",
            "every-prompt",
            "major-actions",
            "major-events",
            "manual",
        ] {
            assert!(out.contains(phrase), "{phrase:?} missing from:\n{out}");
        }
    }

    // ---- -y on a clean machine ------------------------------------------------

    #[test]
    fn yes_on_a_clean_fixture_creates_the_database_the_dotenv_and_one_block() {
        let f = Fixture::new("clean");
        let cfg = f.cfg();
        let out = init(&cfg, &args(&[], true), &f.host(up(), true)).unwrap();
        let shown = text(&out);
        assert!(!shown.contains("Ask the user to confirm"), "{shown}");

        // The database: created from the embedded schema, and healthy.
        let db_cfg = Config {
            db_path: f.default_db(),
            ..cfg.clone()
        };
        // The embedded schema carries every migration, the instructions table
        // included, so nothing is pending on a fresh database.
        assert_eq!(
            crate::status::report(&db_cfg, &Fake::up()),
            vec!["EMPTY".to_string()]
        );
        assert!(
            TursoStore::open(&db_cfg)
                .unwrap()
                .has_table(crate::store::Table::Instructions)
                .unwrap()
        );

        // The .env: the four keys.
        assert_eq!(
            fs::read_to_string(f.dotenv()).unwrap(),
            format!(
                "{DB_ENV}={}\nBACKUP_DIR={}\n{VERSION_KEY}={}\nEMBEDDINGS=on\n",
                f.default_db().display(),
                f.default_backups().display(),
                crate::version::VERSION
            )
        );

        // CLAUDE.md: exactly one block, naming the canonical binary.
        let md = fs::read_to_string(f.claude_md()).unwrap();
        assert_eq!(md.matches(BLOCK_BEGIN).count(), 1, "{md}");
        assert_eq!(md.matches(BLOCK_END).count(), 1, "{md}");
        assert!(md.starts_with(BLOCK_BEGIN), "{md}");
        assert!(
            md.contains(&format!("Run `{} status` first.", f.exe.display())),
            "{md}"
        );
        assert!(
            md.contains("`major-events`: store ONLY major events"),
            "{md}"
        );
        assert!(
            md.contains(&format!(
                "<!-- supercharged-memory: rendered-by {} -->",
                crate::version::VERSION
            )),
            "{md}"
        );
    }

    #[test]
    fn the_chosen_episodic_mode_is_rendered() {
        let f = Fixture::new("mode");
        init(
            &f.cfg(),
            &args(&[("mode", "major-actions")], true),
            &f.host(up(), true),
        )
        .unwrap();
        let md = fs::read_to_string(f.claude_md()).unwrap();
        assert!(
            md.contains("`major-actions`: store one for every substantive action"),
            "{md}"
        );
    }

    /// Text already in CLAUDE.md stays exactly as it was; the block is appended
    /// after one blank line.
    #[test]
    fn the_block_is_appended_after_the_users_own_text() {
        let f = Fixture::new("append");
        fs::create_dir_all(f.path("claude")).unwrap();
        fs::write(f.claude_md(), "# Mine\n\nAlways use pnpm.").unwrap();
        init(&f.cfg(), &args(&[], true), &f.host(up(), true)).unwrap();
        let md = fs::read_to_string(f.claude_md()).unwrap();
        assert!(
            md.starts_with(&format!("# Mine\n\nAlways use pnpm.\n\n{BLOCK_BEGIN}\n")),
            "{md}"
        );
    }

    // ---- missing dependencies ---------------------------------------------------

    #[test]
    fn each_missing_dependency_is_named_with_its_loss_in_the_plan_and_the_last_line() {
        type Case = (
            &'static str,
            fn() -> std::result::Result<(), Unavailable>,
            bool,
            &'static [&'static str],
            &'static str,
        );
        let cases: &[Case] = &[
            (
                "ollama",
                unreachable,
                true,
                &["Ollama unreachable at"],
                "off",
            ),
            ("model", not_pulled, true, &["bge-m3 is not pulled"], "off"),
            ("tursodb", up, false, &["tursodb"], "on"),
            (
                "both",
                unreachable,
                false,
                &["Ollama unreachable at", "tursodb"],
                "off",
            ),
        ];
        for (name, ollama, tursodb, named, embeddings) in cases {
            let f = Fixture::new(&format!("missing-{name}"));
            let out = init(&f.cfg(), &Args::default(), &f.host(ollama(), *tursodb)).unwrap();
            let shown = text(&out);
            let line = last(&out);
            assert!(
                line.starts_with(
                    "Ask the user to confirm, and to acknowledge continuing without: "
                ),
                "{name}: {line}"
            );
            assert!(shown.contains("\nMissing"), "{name}: {shown}");
            for n in *named {
                assert!(
                    line.contains(n),
                    "{name}: {n:?} not in the last line: {line}"
                );
            }
            if ollama().is_err() {
                assert!(line.contains("keyword-only"), "{name}: {line}");
                assert!(shown.contains("embed --missing"), "{name}: {shown}");
            } else {
                assert!(!line.contains("keyword-only"), "{name}: {line}");
            }
            if *tursodb {
                assert!(!line.contains("ad-hoc"), "{name}: {line}");
            } else {
                assert!(line.contains("ad-hoc"), "{name}: {line}");
            }

            init(&f.cfg(), &args(&[], true), &f.host(ollama(), *tursodb)).unwrap();
            assert_eq!(
                dotenv_value(&f, "EMBEDDINGS").as_deref(),
                Some(*embeddings),
                "{name}"
            );
        }
    }

    #[test]
    fn with_every_dependency_present_the_last_line_is_plain() {
        let f = Fixture::new("all-present");
        let out = init(&f.cfg(), &Args::default(), &f.host(up(), true)).unwrap();
        assert_eq!(last(&out), "Ask the user to confirm");
        assert!(!text(&out).contains("\nMissing"), "{}", text(&out));
    }

    // ---- idempotency --------------------------------------------------------------

    #[test]
    fn a_second_yes_changes_nothing_and_says_so() {
        let f = Fixture::new("second");
        let cfg = f.cfg();
        init(&cfg, &args(&[], true), &f.host(unreachable(), false)).unwrap();
        let md = fs::read(f.claude_md()).unwrap();
        let env = fs::read(f.dotenv()).unwrap();
        let db = fs::read(f.default_db()).unwrap();
        let db_mtime = mtime(&f.default_db());

        // The .env now exists, so the binary would resolve from it.
        let cfg = f.cfg();
        for yes in [false, true] {
            let out = init(&cfg, &args(&[], yes), &f.host(unreachable(), false)).unwrap();
            let shown = text(&out);
            assert!(
                out.iter().any(|l| l.starts_with("Nothing to do")),
                "yes={yes}: {shown}"
            );
            assert!(
                !shown.contains("Ask the user to confirm"),
                "yes={yes}: {shown}"
            );
        }
        assert_eq!(fs::read(f.claude_md()).unwrap(), md);
        assert_eq!(fs::read(f.dotenv()).unwrap(), env);
        assert_eq!(fs::read(f.default_db()).unwrap(), db);
        assert_eq!(mtime(&f.default_db()), db_mtime);
    }

    // ---- an existing CLAUDE.md block -------------------------------------------

    const LEGACY_BLOCK: &str = "<!-- BEGIN agentic-memory (managed by install-claude-md.sh) -->\n\
                                <!-- supercharged-memory: synced-at 0123abcd -->\n\
                                # Long-term memory — Turso vector DB (supercharged-memory)\n\
                                \n\
                                them as `python3 \"/repo/scripts/<cmd>\"`.\n\
                                <!-- END agentic-memory -->\n";

    #[test]
    fn an_existing_headline_is_left_alone_and_points_at_update() {
        let f = Fixture::new("legacy");
        fs::create_dir_all(f.path("claude")).unwrap();
        let original = format!("# Mine\n\n{LEGACY_BLOCK}");
        fs::write(f.claude_md(), &original).unwrap();

        let dry = text(&init(&f.cfg(), &Args::default(), &f.host(up(), true)).unwrap());
        assert!(dry.contains("`update`"), "{dry}");
        assert!(
            dry.contains(crate::claude_md::CUTOVER_NOTE.lines().next().unwrap()),
            "a legacy block shows the cutover note:\n{dry}"
        );
        // Nothing about rendering a block the user will not get.
        assert!(!dry.contains("will append"), "{dry}");

        init(&f.cfg(), &args(&[], true), &f.host(up(), true)).unwrap();
        assert_eq!(fs::read_to_string(f.claude_md()).unwrap(), original);
        // The rest of the plan still happened.
        assert!(f.default_db().exists());
        assert!(f.dotenv().exists());
    }

    /// A block `init` wrote itself is not a legacy block: no cutover note.
    #[test]
    fn a_block_init_wrote_gets_no_cutover_note() {
        let f = Fixture::new("own-block");
        init(&f.cfg(), &args(&[], true), &f.host(up(), true)).unwrap();
        let out = text(&init(&f.cfg(), &Args::default(), &f.host(up(), true)).unwrap());
        assert!(out.contains("`update`"), "{out}");
        assert!(
            !out.contains(crate::claude_md::CUTOVER_NOTE.lines().next().unwrap()),
            "{out}"
        );
    }

    // ---- an existing database ---------------------------------------------------

    #[test]
    fn an_existing_memory_database_is_adopted_and_never_written() {
        let f = Fixture::new("adopt");
        let db = f.path("mine/memory.db");
        memory_db(&db, 3);
        let bytes = fs::read(&db).unwrap();
        let when = mtime(&db);
        let a = args(&[("db", &db.to_string_lossy())], false);

        let out = text(&init(&f.cfg(), &a, &f.host(up(), true)).unwrap());
        assert!(out.contains("adopt"), "{out}");
        assert!(out.contains("3 memories"), "{out}");
        assert!(!out.contains("embedded schema"), "{out}");

        init(&f.cfg(), &Args { yes: true, ..a }, &f.host(up(), true)).unwrap();
        assert_eq!(fs::read(&db).unwrap(), bytes, "the database was written");
        assert_eq!(mtime(&db), when, "the database was written");
        assert_eq!(
            dotenv_value(&f, DB_ENV).as_deref(),
            Some(db.to_str().unwrap())
        );
    }

    #[test]
    fn candidates_are_listed_with_their_counts_when_the_target_is_absent() {
        let f = Fixture::new("candidates");
        // Another memory database in a searched directory, and a dump.
        memory_db(&f.path("data/turso/old-memory.db"), 2);
        fs::create_dir_all(f.default_backups()).unwrap();
        fs::write(
            f.default_backups()
                .join("2026-10-01-supercharged-memory.sql.gz"),
            b"x",
        )
        .unwrap();

        let out = text(&init(&f.cfg(), &Args::default(), &f.host(up(), true)).unwrap());
        assert!(out.contains("old-memory.db (2 memories)"), "{out}");
        assert!(
            out.contains("2026-10-01-supercharged-memory.sql.gz"),
            "{out}"
        );
        assert!(out.contains("real memory"), "{out}");
    }

    // ---- refusals -------------------------------------------------------------------

    /// Garbage, an empty file (what a stale process re-creating a renamed path
    /// leaves), and an SQLite database without the memory tables. The first two
    /// are judged by their header alone: opening an empty file would create WAL
    /// files beside it.
    #[test]
    fn a_file_that_is_not_a_memory_database_is_refused() {
        let f = Fixture::new("not-memory");
        let garbage = f.path("garbage.db");
        fs::write(&garbage, b"this is not a database at all").unwrap();
        let empty = f.path("empty.db");
        fs::write(&empty, b"").unwrap();
        let before = f.snapshot();
        for db in [&garbage, &empty] {
            for yes in [false, true] {
                refused(
                    init(
                        &f.cfg(),
                        &args(&[("db", &db.to_string_lossy())], yes),
                        &f.host(up(), true),
                    ),
                    "not a memory database",
                );
            }
        }
        assert_eq!(f.snapshot(), before, "a refusal wrote something");

        let other = f.path("other/app.db");
        fs::create_dir_all(other.parent().unwrap()).unwrap();
        pollster::block_on(async {
            let db = turso::Builder::new_local(&other.to_string_lossy())
                .experimental_multiprocess_wal(true)
                .build()
                .await
                .unwrap();
            db.connect()
                .unwrap()
                .execute_batch("CREATE TABLE t (x);")
                .await
                .unwrap();
        });
        let bytes = fs::read(&other).unwrap();
        refused(
            init(
                &f.cfg(),
                &args(&[("db", &other.to_string_lossy())], true),
                &f.host(up(), true),
            ),
            "not a memory database",
        );
        assert_eq!(fs::read(&other).unwrap(), bytes);
        assert!(!f.dotenv().exists());
    }

    /// The window between the plan and the create: a file that appeared there
    /// is refused, not touched.
    #[test]
    fn a_file_that_appeared_after_the_plan_is_refused_and_left_alone() {
        let f = Fixture::new("appeared");
        let cfg = f.cfg();
        let p = plan(&cfg, &args(&[], true), &f.host(up(), true), TEMPLATE).unwrap();
        fs::create_dir_all(f.default_db().parent().unwrap()).unwrap();
        fs::write(f.default_db(), b"someone else's").unwrap();

        let err = apply(&cfg, &p).unwrap_err();
        assert_eq!(err.exit_code(), EXIT_REFUSED, "{err}");
        assert!(err.to_string().starts_with("refused: "), "{err}");
        assert!(err.to_string().contains("appeared"), "{err}");
        assert_eq!(fs::read(f.default_db()).unwrap(), b"someone else's");
        // Nothing after the database step ran.
        assert!(!f.dotenv().exists());
        assert!(!f.claude_md().exists());
    }

    #[test]
    fn an_invalid_episodic_mode_is_refused() {
        let f = Fixture::new("bad-mode");
        let before = f.snapshot();
        for yes in [false, true] {
            let msg = refused(
                init(
                    &f.cfg(),
                    &args(&[("mode", "sometimes")], yes),
                    &f.host(up(), true),
                ),
                "sometimes",
            );
            assert!(msg.contains("major-events"), "lists the valid modes: {msg}");
        }
        assert_eq!(f.snapshot(), before);
    }

    /// The `.env` is read from every working directory, so a path written into
    /// it must be absolute -- and the `.env` refuses a relative one on read.
    #[test]
    fn a_relative_path_is_refused() {
        let f = Fixture::new("relative");
        let before = f.snapshot();
        for pairs in [&[("db", "mem.db")][..], &[("backups", "backups")]] {
            refused(
                init(&f.cfg(), &args(pairs, true), &f.host(up(), true)),
                "absolute",
            );
        }
        assert_eq!(f.snapshot(), before);
    }

    #[test]
    fn a_template_placeholder_without_a_value_refuses_before_anything_is_written() {
        let f = Fixture::new("placeholder");
        let before = f.snapshot();
        let template = format!("{HEADLINE}\n\nRun {{{{PY}}}} \"{{{{BASE_PATH}}}}/x\".\n");
        let msg = refused(
            init_with(&f.cfg(), &args(&[], true), &f.host(up(), true), &template),
            "{{BASE_PATH}}",
        );
        assert!(msg.contains("{{PY}}"), "{msg}");
        assert_eq!(f.snapshot(), before, "nothing may be written");
    }

    /// A double quote cannot be quoted on a command line, nor written into the
    /// `.env`: refused, not mangled.
    #[test]
    fn a_path_that_cannot_be_written_is_refused() {
        let f = Fixture::new("quote");
        fs::create_dir_all(f.path("odd\"dir")).unwrap();
        let exe = f.path("odd\"dir/supercharged-memory");
        fs::write(&exe, b"").unwrap();
        let cfg = f.cfg_for(&exe, &[]);
        let before = f.snapshot();
        refused(
            init(&cfg, &args(&[], true), &f.host(up(), true)),
            "cannot be written into the instructions",
        );
        assert_eq!(f.snapshot(), before);

        let db = f.path("my\"data/memory.db");
        refused(
            init(
                &f.cfg(),
                &args(&[("db", &db.to_string_lossy())], true),
                &f.host(up(), true),
            ),
            "cannot be written",
        );
        assert_eq!(f.snapshot(), before);
    }

    /// The `.env` was readable when the configuration was resolved and is not
    /// any more: refused before anything is written.
    #[cfg(unix)]
    #[test]
    fn a_dotenv_that_turns_unreadable_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new("dotenv-gone");
        fs::write(f.dotenv(), "BACKUP_DIR=/srv/backups\n").unwrap();
        let cfg = f.cfg();
        fs::set_permissions(f.dotenv(), fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(f.dotenv()).is_ok() {
            return; // running as root: permissions do not apply
        }
        let result = init(&cfg, &args(&[], true), &f.host(up(), true));
        fs::set_permissions(f.dotenv(), fs::Permissions::from_mode(0o644)).unwrap();
        refused(result, ".env");
        assert!(!f.default_db().exists());
        assert!(!f.claude_md().exists());
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_claude_md_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new("unreadable-md");
        fs::create_dir_all(f.path("claude")).unwrap();
        fs::write(f.claude_md(), "# Mine\n").unwrap();
        fs::set_permissions(f.claude_md(), fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(f.claude_md()).is_ok() {
            return; // running as root: permissions do not apply
        }
        let result = init(&f.cfg(), &args(&[], true), &f.host(up(), true));
        fs::set_permissions(f.claude_md(), fs::Permissions::from_mode(0o644)).unwrap();
        refused(result, "CLAUDE.md");
        assert!(!f.default_db().exists());
        assert!(!f.dotenv().exists());
    }

    // ---- locations ------------------------------------------------------------------

    #[test]
    fn a_flagged_location_warns_and_yes_still_creates_the_database_there() {
        let f = Fixture::new("dropbox");
        let db = f.path("home/Dropbox/memory.db");
        let a = args(&[("db", &db.to_string_lossy())], false);
        let out = text(&init(&f.cfg(), &a, &f.host(up(), true)).unwrap());
        assert!(out.contains("Dropbox"), "{out}");
        assert!(
            out.contains("cloud sync corrupts a live SQLite file"),
            "{out}"
        );

        init(&f.cfg(), &Args { yes: true, ..a }, &f.host(up(), true)).unwrap();
        assert!(db.exists());
    }

    #[test]
    fn a_binary_in_downloads_or_with_a_versioned_name_is_warned_about() {
        let f = Fixture::new("exe-place");
        fs::create_dir_all(f.path("home/Downloads")).unwrap();
        let exe = f.path("home/Downloads/supercharged-memory_0.3.0_linux_amd64");
        fs::write(&exe, b"").unwrap();
        let cfg = f.cfg_for(&exe, &[]);
        let out = text(&init(&cfg, &Args::default(), &f.host(up(), true)).unwrap());
        assert!(out.contains("Downloads"), "{out}");
        assert!(out.contains("version"), "{out}");
        assert!(out.contains("move"), "{out}");
    }

    // ---- the .env -----------------------------------------------------------------

    #[test]
    fn the_dotenv_merge_leaves_a_differing_value_and_unrelated_lines_alone() {
        let f = Fixture::new("merge");
        let original = "# hand-written\nOLLAMA_URL=http://box:11434   # the GPU box\n\
                        BACKUP_DIR=/srv/backups\n";
        fs::write(f.dotenv(), original).unwrap();
        let cfg = f.cfg();
        let other = f.path("elsewhere");
        let a = args(&[("backups", &other.to_string_lossy())], false);

        let out = text(&init(&cfg, &a, &f.host(up(), true)).unwrap());
        assert!(out.contains("/srv/backups"), "{out}");
        assert!(out.contains("left alone"), "{out}");

        init(&cfg, &Args { yes: true, ..a }, &f.host(up(), true)).unwrap();
        let now = fs::read_to_string(f.dotenv()).unwrap();
        assert!(now.starts_with(original), "{now}");
        assert_eq!(
            dotenv_value(&f, "BACKUP_DIR").as_deref(),
            Some("/srv/backups")
        );
        assert_eq!(
            dotenv_value(&f, "OLLAMA_URL").as_deref(),
            Some("http://box:11434")
        );
        let added: Vec<&str> = now[original.len()..].lines().collect();
        assert_eq!(added.len(), 3, "{now}");
        assert!(added.iter().all(|l| !l.starts_with("BACKUP_DIR")), "{now}");
    }

    #[test]
    fn every_value_names_its_source() {
        let f = Fixture::new("sources");
        fs::write(f.dotenv(), "BACKUP_DIR=/srv/backups\n").unwrap();
        let db = f.path("chosen/memory.db");
        let out = init(
            &f.cfg(),
            &args(&[("db", &db.to_string_lossy())], false),
            &f.host(up(), true),
        )
        .unwrap();
        let line = |needle: &str| {
            out.iter()
                .find(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("no line with {needle:?} in:\n{}", text(&out)))
                .clone()
        };
        assert!(line(&format!("{DB_ENV} = ")).contains("[flag]"));
        assert!(line("BACKUP_DIR = /srv/backups").contains("[.env]"));
        assert!(line("major-events").contains("[default]"));

        let env_db = f.path("from-env/memory.db");
        let cfg = f.cfg_for(&f.exe, &[(DB_ENV, &env_db.to_string_lossy())]);
        let out = text(&init(&cfg, &Args::default(), &f.host(up(), true)).unwrap());
        assert!(
            out.contains(&format!("{DB_ENV} = {}  [env]", env_db.display())),
            "{out}"
        );
    }

    /// A legacy `~/.claude/settings.json` `env` entry is real environment for
    /// every Claude Code tool call, so it overrides the `.env` on every run.
    #[test]
    fn a_real_environment_variable_that_overrides_the_choice_is_a_loud_warning() {
        let f = Fixture::new("env-override");
        let legacy = f.path("legacy/memory.db");
        let chosen = f.path("chosen/memory.db");
        let cfg = f.cfg_for(&f.exe, &[(DB_ENV, &legacy.to_string_lossy())]);
        let out = text(
            &init(
                &cfg,
                &args(&[("db", &chosen.to_string_lossy())], false),
                &f.host(up(), true),
            )
            .unwrap(),
        );
        assert!(out.contains("overrides the .env on every run"), "{out}");
        assert!(out.contains(&legacy.display().to_string()), "{out}");
        assert!(out.contains("warning"), "{out}");
    }

    // ---- the binary's path -----------------------------------------------------

    /// A binary linked into `~/.local/bin` keeps its `.env` beside the real file,
    /// and the instructions name the real file.
    #[cfg(unix)]
    #[test]
    fn through_a_symlink_the_dotenv_and_the_rendered_path_are_the_targets() {
        let f = Fixture::new("symlink");
        fs::create_dir_all(f.path("home/.local/bin")).unwrap();
        let link = f.path("home/.local/bin/supercharged-memory");
        std::os::unix::fs::symlink(&f.exe, &link).unwrap();
        let cfg = f.cfg_for(&link, &[]);

        init(&cfg, &args(&[], true), &f.host(up(), true)).unwrap();
        assert!(f.dotenv().exists());
        assert!(!f.path("home/.local/bin/.env").exists());
        let md = fs::read_to_string(f.claude_md()).unwrap();
        assert!(
            md.contains(&format!("Run `{} status`", f.exe.display())),
            "{md}"
        );
        assert!(!md.contains(".local/bin"), "{md}");
    }

    // ---- recommendations ------------------------------------------------------------

    #[test]
    fn the_migration_offer_follows_the_probe() {
        let f = Fixture::new("migrate");
        let out = init(&f.cfg(), &Args::default(), &f.host(up(), true)).unwrap();
        assert!(!text(&out).contains("runbook migrate"), "{}", text(&out));

        fs::create_dir_all(f.path("claude/memory")).unwrap();
        fs::write(f.path("claude/memory/fact.md"), "A fact.\n").unwrap();
        let out = init(&f.cfg(), &Args::default(), &f.host(up(), true)).unwrap();
        let n = out.len();
        assert!(out[n - 2].contains("runbook migrate"), "{}", text(&out));
        assert_eq!(last(&out), "Ask the user to confirm");
    }

    #[test]
    fn a_missing_mcp_registration_is_recommended_read_only() {
        let f = Fixture::new("mcp");
        let out = text(&init(&f.cfg(), &Args::default(), &f.host(up(), true)).unwrap());
        assert!(
            out.contains(&format!(
                "claude mcp add turso --scope user -- /opt/turso/tursodb {} --mcp --readonly \
                 --experimental-multiprocess-wal",
                f.default_db().display()
            )),
            "{out}"
        );
        assert!(out.contains("Turso 0.7.2"), "{out}");

        // A registration missing the WAL flag is named, with the fix.
        let mut host = f.host(up(), true);
        host.claude_json_text = Some(Ok(serde_json::json!({
            "mcpServers": {"turso": {"command": "/opt/turso/tursodb",
                "args": [f.default_db(), "--mcp", "--readonly"]}}
        })
        .to_string()));
        let out = text(&init(&f.cfg(), &Args::default(), &host).unwrap());
        assert!(out.contains("--experimental-multiprocess-wal"), "{out}");
        assert!(out.contains("claude mcp remove turso"), "{out}");

        // An unreadable file is unknown, never "not registered".
        host.claude_json_text = Some(Err("Permission denied".into()));
        let out = text(&init(&f.cfg(), &Args::default(), &host).unwrap());
        assert!(out.contains("unknown"), "{out}");
        assert!(!out.contains("claude mcp add"), "{out}");
    }

    #[test]
    fn the_backup_schedule_is_printed_never_installed() {
        let f = Fixture::new("schedule");
        let out = text(&init(&f.cfg(), &Args::default(), &f.host(up(), true)).unwrap());
        assert!(
            out.contains(&format!("{} backup", f.exe.display())),
            "{out}"
        );
        for os in ["linux", "macos", "windows", "freebsd"] {
            let lines = schedule(os, "/opt/sm/supercharged-memory").join("\n");
            assert!(
                lines.contains("/opt/sm/supercharged-memory") && lines.contains("backup"),
                "{os}: {lines}"
            );
        }
        assert!(schedule("linux", "x").join("\n").contains("systemd"));
        assert!(schedule("macos", "x").join("\n").contains("launchd"));
        assert!(schedule("windows", "x").join("\n").contains("schtasks"));
        assert!(schedule("freebsd", "x").join("\n").contains("crontab"));
    }
}
