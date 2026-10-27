//! `update`: what changed since the binary that set this machine up, and the
//! schema migrations to apply.
//!
//! There is no repository at runtime, so "update" cannot mean `git pull`. The
//! user replaces the binary; the new one compares the version the `.env`
//! records with its own, prints the migration notes compiled into it for every
//! release in between, the schema migrations its probes find missing in the
//! database, and what to change in CLAUDE.md. Without `-y` it changes nothing
//! and ends `Ask the user to confirm`. With `-y` it backs up the database
//! before the first migration, applies each one in a transaction of its own,
//! and records its version in the `.env` only once every one has succeeded.
//! It never writes CLAUDE.md: the agent does, once the user agrees.

pub mod block;
pub mod migrations;
pub mod semver;

use std::fs;
use std::io;
use std::path::Path;

use chrono::NaiveDate;

use crate::backup;
use crate::claude_md::{self, EpisodicMode};
use crate::config::dotenv::{self, OnConflict};
use crate::config::{Config, Source, VERSION_KEY};
use crate::error::{Error, Result};
use crate::store::Store;
use crate::store::turso::TursoStore;
use crate::store::turso::migrate::Migrated;
use block::Report;
use migrations::{MIGRATIONS, Migration, NOTES, Note};
use semver::Version;

/// The flags, as given.
#[derive(Debug, Clone, Default)]
pub struct Args {
    pub yes: bool,
    /// Print the block this binary would write, and nothing else.
    pub print_block: bool,
    /// The user's answer when the block's own mode cannot be read. Validated by
    /// `update` itself, so an invalid one is a refusal.
    pub episodic_mode: Option<String>,
}

/// What a binary carries: its version and everything compiled in with it. A
/// test supplies its own.
#[derive(Debug, Clone, Copy)]
pub struct Release<'a> {
    pub version: &'a str,
    pub notes: &'a [Note],
    pub migrations: &'a [Migration],
    pub template: &'a str,
}

impl Release<'static> {
    pub fn this_binary() -> Release<'static> {
        Release {
            version: crate::version::VERSION,
            notes: NOTES,
            migrations: MIGRATIONS,
            template: claude_md::TEMPLATE,
        }
    }
}

/// The machine, beyond its configuration.
#[derive(Debug, Clone, Copy)]
pub struct Host {
    /// How the binary's path is written into the instructions.
    pub windows: bool,
    /// The local date, which names the backup taken before a migration.
    pub today: NaiveDate,
}

/// The whole command against a given release and machine; every line goes to
/// `out`.
pub fn execute(
    cfg: &Config,
    args: &Args,
    release: &Release,
    host: &Host,
    out: &mut dyn FnMut(String),
) -> Result<()> {
    let mode = validate(args)?;
    let exe = claude_md::exe_for_instructions(&cfg.exe, host.windows).map_err(|why| {
        Error::refused(format!(
            "the binary's path cannot be written into the instructions: {why}. Move the binary \
             — nothing was changed"
        ))
    })?;
    let md_path = cfg.claude_dir.join("CLAUDE.md");
    let md = read_claude_md(&md_path);
    let render = block::Render {
        exe: &exe,
        mode,
        template: release.template,
        version: release.version,
    };

    if args.print_block {
        let text = md
            .as_ref()
            .map_err(|why| Error::refused(format!("cannot read {}: {why}", md_path.display())))?;
        let printed = block::print_block(text.as_deref(), &render)
            .map_err(|why| Error::refused(format!("{why} — nothing was printed")))?;
        for line in printed.lines() {
            out(line.to_string());
        }
        return Ok(());
    }

    let report = match &md {
        Ok(text) => block::compare(text.as_deref(), &render),
        Err(why) => Report::Unreadable(why.clone()),
    };

    let recorded = recorded_version(cfg)?;
    if let Some(line) = env_version_warning(cfg, recorded.as_deref()) {
        out(line);
    }
    let Some(recorded) = recorded else {
        return not_set_up(cfg, args, &md_path, &report, out);
    };
    let installed = Version::parse(&recorded).map_err(|why| {
        Error::refused(format!(
            "{VERSION_KEY} in {}: {why} — nothing was changed",
            cfg.dotenv_path.display()
        ))
    })?;
    let own = Version::parse(release.version)
        .map_err(|why| Error::failed(format!("this binary's own version: {why}")))?;
    if installed > own {
        return Err(Error::refused(format!(
            "{} records {VERSION_KEY}={installed}, but this binary is {own}: an older binary \
             cannot know what a newer one changed. Run the newer binary — nothing was changed",
            cfg.dotenv_path.display()
        )));
    }

    let store = open_memory_database(cfg)?;
    let pending = pending_schema(&store, release)?;
    drop(store);
    let notes = migrations::between(release.notes, &installed, &own).map_err(Error::failed)?;

    let plan = Plan {
        installed,
        own,
        notes,
        pending,
        report,
    };
    out(if args.yes {
        "update -y: applying.".to_string()
    } else {
        "update: dry run — nothing has been changed. Relay this to the user; once they confirm, \
         re-run with -y."
            .to_string()
    });
    for line in plan.lines(cfg, &md_path) {
        out(line);
    }

    let bump = plan.installed < plan.own;
    if !bump && plan.pending.is_empty() && !plan.report.actionable() {
        out(format!(
            "Up to date: the .env and this binary are both {}, and no schema migration is pending.",
            plan.own
        ));
        return Ok(());
    }
    if !args.yes {
        out("Ask the user to confirm".to_string());
        return Ok(());
    }
    apply(cfg, host, &plan, out)
}

/// What `update` found.
struct Plan<'a> {
    installed: Version,
    own: Version,
    notes: Vec<&'a Note>,
    pending: Vec<&'a Migration>,
    report: Report,
}

impl Plan<'_> {
    /// Everything before the closing line: the version, the notes, the schema
    /// migrations, the `.env` step and the CLAUDE.md report, in that order.
    fn lines(&self, cfg: &Config, md_path: &Path) -> Vec<String> {
        let mut lines = Vec::new();
        let dotenv = cfg.dotenv_path.display();
        if self.installed == self.own {
            lines.push(format!("Version: {}, as {dotenv} records it.", self.own));
        } else {
            lines.push(format!(
                "Version: {dotenv} records {}; this binary is {}.",
                self.installed, self.own
            ));
            if self.notes.is_empty() {
                lines.push(format!(
                    "Migration notes: none between {} and {}.",
                    self.installed, self.own
                ));
            } else {
                lines.push(format!(
                    "Migration notes from {} to {}, oldest first ({}) — relay each one to the user:",
                    self.installed,
                    self.own,
                    self.notes.len()
                ));
            }
        }
        for note in &self.notes {
            let (title, body) = match migrations::parse_note(note.text) {
                Ok(s) => (s.title, s.body),
                Err(_) => ("(no title)", note.text),
            };
            lines.push(format!("=== {}: {title} ===", note.version));
            lines.extend(body.trim_matches('\n').lines().map(str::to_string));
            lines.push(format!("=== end of {} ===", note.version));
        }

        lines.push(format!("Database: {}", cfg.db_path.display()));
        if self.pending.is_empty() {
            lines.push("  no schema migration pending.".to_string());
        } else {
            lines.push(format!(
                "  {} schema migration(s) pending — -y backs the database up first, then applies \
                 each in a transaction of its own:",
                self.pending.len()
            ));
            for m in &self.pending {
                lines.push(format!(
                    "    - {} ({}): {} — applied once {}",
                    m.id, m.version, m.summary, m.probe.applied_when
                ));
            }
        }

        if self.installed < self.own {
            lines.push(format!(
                ".env: -y records {VERSION_KEY}={} (now {}) once every pending migration has \
                 succeeded.",
                self.own, self.installed
            ));
            if !self.notes.is_empty() {
                lines.push(
                    "  A step a note asks for by hand is the agent's, once the user agrees; -y \
                     does not do it."
                        .to_string(),
                );
            }
        }

        lines.push(format!("CLAUDE.md: {}", md_path.display()));
        lines.extend(self.report.lines());
        lines
    }
}

/// Back up, migrate, record the version. A failure stops there: the
/// migrations before it stay applied, and the version stays unrecorded.
fn apply(cfg: &Config, host: &Host, plan: &Plan, out: &mut dyn FnMut(String)) -> Result<()> {
    out("Applied:".to_string());
    if plan.pending.is_empty() && plan.installed == plan.own {
        out(
            "  nothing for -y to apply: the .env records this version and no schema migration is \
             pending."
                .to_string(),
        );
    }

    if !plan.pending.is_empty() {
        let done = backup::create(cfg, host.today).map_err(|e| match e {
            Error::Refused(why) => Error::refused(format!(
                "no backup, so no migration: {why} — nothing was changed"
            )),
            other => Error::failed(format!(
                "the backup before migrating failed, so nothing was migrated: {}",
                reason(other)
            )),
        })?;
        out("  backed up first:".to_string());
        for line in done.lines(cfg) {
            out(format!("    {line}"));
        }

        let store = TursoStore::open(cfg)?;
        for m in &plan.pending {
            let statements = migrations::statements(m.sql).map_err(|why| {
                Error::failed(format!("migration {} ({}): {why}", m.id, m.version))
            })?;
            match store.migrate(m.probe.query, &statements) {
                Ok(Migrated::Applied) => out(format!(
                    "  migration {} ({}): applied — {}",
                    m.id, m.version, m.probe.applied_when
                )),
                Ok(Migrated::AlreadyApplied) => out(format!(
                    "  migration {} ({}): already applied by another run meanwhile — nothing run",
                    m.id, m.version
                )),
                Err(e) => {
                    return Err(Error::failed(format!(
                        "migration {} ({}) failed: {}. The migrations listed as applied above \
                         stay applied; {VERSION_KEY} stays {}. The backup taken first: {}",
                        m.id,
                        m.version,
                        reason(e),
                        plan.installed,
                        done.path.display()
                    )));
                }
            }
        }
    }

    if plan.installed < plan.own {
        let own = plan.own.to_string();
        dotenv::set_keys(
            &cfg.dotenv_path,
            &[(VERSION_KEY, &own)],
            OnConflict::Overwrite,
        )?;
        out(format!(
            "  {VERSION_KEY}: {} → {own} in {}",
            plan.installed,
            cfg.dotenv_path.display()
        ));
    }

    if plan.report.actionable() {
        out(
            "CLAUDE.md is still the agent's to change, once the user agrees: see its report above \
             (update never writes CLAUDE.md)."
                .to_string(),
        );
    }
    Ok(())
}

/// A machine without a recorded version was not set up by `init`. A legacy
/// block gets the cutover note instead of a refusal; `-y` has nothing it
/// could apply there either way.
fn not_set_up(
    cfg: &Config,
    args: &Args,
    md_path: &Path,
    report: &Report,
    out: &mut dyn FnMut(String),
) -> Result<()> {
    let missing = if cfg.dotenv_present {
        format!("no {VERSION_KEY} in {}", cfg.dotenv_path.display())
    } else {
        format!(
            "no {} beside this binary ({})",
            dotenv::FILE_NAME,
            cfg.dotenv_path.display()
        )
    };
    if !report.legacy() || args.yes {
        return Err(Error::refused(format!(
            "{missing}: this machine was not set up by init, so there is no version to update \
             from. Run init first (a dry run, then -y){} — nothing was changed",
            if report.legacy() {
                "; `update` without -y prints the cutover note for this legacy block"
            } else {
                ""
            }
        )));
    }
    out(format!(
        "update: {missing}, and CLAUDE.md holds a block written by the legacy installer. This \
         machine has not moved onto the binary yet."
    ));
    out(format!("CLAUDE.md: {}", md_path.display()));
    for line in report.lines() {
        out(line);
    }
    out(
        "Nothing else can be checked until init has written the .env: follow the cutover note, \
         starting with init."
            .to_string(),
    );
    Ok(())
}

/// The flag `update` checks itself, before anything is read.
fn validate(args: &Args) -> Result<Option<EpisodicMode>> {
    match &args.episodic_mode {
        None => Ok(None),
        Some(m) => EpisodicMode::parse(m).map(Some).ok_or_else(|| {
            Error::refused(format!(
                "--episodic-mode {m:?} is not one of {} — nothing was changed",
                EpisodicMode::ALL.map(EpisodicMode::as_str).join(", ")
            ))
        }),
    }
}

/// CLAUDE.md's text: `None` when there is no file, the reason when it
/// cannot be read.
fn read_claude_md(path: &Path) -> std::result::Result<Option<String>, String> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// The schema migrations `release` carries that `store` does not hold yet, oldest
/// first. Read-only: `update` plans from it and `status` reports it.
pub(crate) fn pending_schema<'a>(
    store: &TursoStore,
    release: &Release<'a>,
) -> Result<Vec<&'a Migration>> {
    let mut ordered = Vec::with_capacity(release.migrations.len());
    for m in release.migrations {
        let version = Version::parse(m.version)
            .map_err(|why| Error::failed(format!("migration {}: {why}", m.id)))?;
        ordered.push((version, m));
    }
    ordered.sort_by(|a, b| a.0.cmp(&b.0));
    let mut pending = Vec::new();
    for (_, m) in ordered {
        if !store
            .holds(m.probe.query)
            .map_err(|e| Error::failed(format!("probing migration {}: {}", m.id, reason(e))))?
        {
            pending.push(m);
        }
    }
    Ok(pending)
}

/// A real environment variable that names a different version from the one the
/// `.env` records. Every other reader takes the environment first, so the
/// disagreement is said here: `update` compares the `.env` and ignores the
/// variable.
fn env_version_warning(cfg: &Config, recorded: Option<&str>) -> Option<String> {
    let setting = cfg.settings.iter().find(|s| s.key == VERSION_KEY)?;
    if setting.source != Source::Env {
        return None;
    }
    let env = setting.value.as_deref()?;
    if Some(env) == recorded {
        return None;
    }
    let recorded = recorded.unwrap_or("no version");
    Some(format!(
        "warn: {VERSION_KEY}={env} is set in the real environment, but {} records {recorded}; \
         update compares the .env and ignores the environment",
        cfg.dotenv_path.display()
    ))
}

/// The version the `.env` file itself records -- the machine's record, which
/// `-y` bumps -- or `None` without a file or the key.
fn recorded_version(cfg: &Config) -> Result<Option<String>> {
    Ok(dotenv::read(&cfg.dotenv_path)?
        .and_then(|entries| entries.into_iter().find(|e| e.key == VERSION_KEY))
        .map(|e| e.value)
        .filter(|v| !v.is_empty()))
}

/// The configured database, opened only once it is known to be a memory
/// database: a missing one is a wrong path, never something to create, and an
/// empty or foreign file is judged by its header without opening it (that
/// would create files beside it). An SQLite file without the memory tables
/// would otherwise be given a memory schema's table.
fn open_memory_database(cfg: &Config) -> Result<TursoStore> {
    let db = &cfg.db_path;
    if !db.exists() {
        return Err(Error::refused(format!(
            "no database at {} — update cannot check its schema. A missing database almost \
             always means a wrong path: check `status` and `config`. Nothing was changed",
            db.display()
        )));
    }
    match crate::store::turso::not_a_database(db) {
        Ok(None) => {}
        Ok(Some(why)) => {
            return Err(Error::refused(format!(
                "{} is not a memory database: {why}. Nothing was changed",
                db.display()
            )));
        }
        Err(e) => return Err(Error::failed(format!("cannot read {}: {e}", db.display()))),
    }
    let store = TursoStore::open(cfg)?;
    store.total_memories().map_err(|e| {
        Error::refused(format!(
            "{} is not a memory database ({}) — nothing was changed",
            db.display(),
            reason(e)
        ))
    })?;
    Ok(store)
}

/// An error's message without its `error:`/`refused:` prefix, to quote
/// inside another message.
fn reason(e: Error) -> String {
    match e {
        Error::Refused(why) | Error::Failed(why) => why,
    }
}

/// `update`.
pub fn run(cfg: &Config, args: &Args) -> Result<()> {
    let host = Host {
        windows: cfg!(windows),
        today: chrono::Local::now().date_naive(),
    };
    execute(cfg, args, &Release::this_binary(), &host, &mut |line| {
        println!("{line}")
    })
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::fs;
    use std::io::Read;
    use std::path::{Path, PathBuf};
    use std::time::SystemTime;

    use super::migrations::Probe;
    use super::*;
    use crate::claude_md::{CUTOVER_NOTE, EpisodicMode};
    use crate::config::{DB_ENV, DotEnv, Env, VERSION_KEY};
    use crate::error::{EXIT_ERROR, EXIT_REFUSED};
    use crate::store::turso::dump::tests::{batch, dump_text, restore};
    use crate::store::turso::{SCHEMA, TursoStore};

    const TEMPLATE: &str = "# Long-term memory — Turso vector DB (supercharged-memory)\n\
                            \n\
                            Run `{{EXE}} status` first; store with `{{EXE}} remember --table semantic …`.\n\
                            **Episodic policy on this machine — `{{EPISODIC_MODE}}`: {{EPISODIC_RULE}}**\n\
                            - \"backup\" → `{{EXE}} backup`.\n";

    const TUESDAY: &str = "2026-10-27";

    /// What turns today's schema into the one from before the instructions
    /// table.
    const PRE_INSTRUCTIONS: &str =
        "DROP INDEX IF EXISTS idx_instr_retired; DROP TABLE IF EXISTS instructions;";

    fn note(title: &'static str) -> &'static str {
        // Leaked once per title: a Note holds 'static text, as the embedded
        // ones do.
        Box::leak(
            format!(
                "# {title}\n\n## What broke\n\n{title} broke.\n\n## How to resolve\n\n\
                 Resolve {title}.\n\n## Verification\n\nVerify {title}.\n"
            )
            .into_boxed_str(),
        )
    }

    fn fixture_notes() -> &'static [Note] {
        Box::leak(Box::new([
            Note {
                version: "0.1.0",
                text: note("Zero-one"),
            },
            Note {
                version: "0.3.0",
                text: note("Zero-three"),
            },
            Note {
                version: "0.2.0",
                text: note("Zero-two"),
            },
        ]))
    }

    const WIDGETS: Migration = Migration {
        id: "widgets",
        version: "0.2.0",
        summary: "adds the widgets table",
        sql: "CREATE TABLE widgets (id INTEGER PRIMARY KEY);\nCREATE INDEX idx_widgets ON widgets (id);\n",
        probe: Probe {
            applied_when: "table `widgets` exists",
            query: "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = 'widgets'",
        },
    };

    /// Fails at its second statement, after creating a table.
    const BROKEN: Migration = Migration {
        id: "broken",
        version: "0.3.0",
        summary: "fails half-way",
        sql: "CREATE TABLE half (x);\nINSERT INTO no_such_table VALUES (1);\n",
        probe: Probe {
            applied_when: "table `half` exists",
            query: "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = 'half'",
        },
    };

    fn release(migrations: &'static [Migration]) -> Release<'static> {
        Release {
            version: "0.3.0",
            notes: fixture_notes(),
            migrations,
            template: TEMPLATE,
        }
    }

    /// This binary's notes and migrations, with the test template.
    fn real() -> Release<'static> {
        Release {
            template: TEMPLATE,
            ..Release::this_binary()
        }
    }

    fn host() -> Host {
        Host {
            windows: false,
            today: NaiveDate::parse_from_str(TUESDAY, "%Y-%m-%d").unwrap(),
        }
    }

    /// A machine in a directory of its own: the binary and its `.env` in
    /// `bin/`, the database in `data/`, dumps in `backups/`, CLAUDE.md in
    /// `claude/`.
    struct Fixture {
        root: PathBuf,
        exe: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Fixture {
            let root =
                std::env::temp_dir().join(format!("sm-update-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            for dir in ["bin", "data", "claude"] {
                fs::create_dir_all(root.join(dir)).unwrap();
            }
            let root = root.canonicalize().unwrap();
            let exe = root.join("bin/supercharged-memory");
            fs::write(&exe, b"").unwrap();
            Fixture { root, exe }
        }

        /// The usual machine: a database from the schema, a `.env` recording
        /// `version`, and CLAUDE.md with a block current for `TEMPLATE`.
        fn installed(name: &str, version: &str) -> Fixture {
            let f = Fixture::new(name);
            f.schema_db(&f.db());
            f.write_dotenv(&f.db(), Some(version));
            f.install_block(TEMPLATE, EpisodicMode::MajorActions, version);
            f
        }

        fn db(&self) -> PathBuf {
            self.root.join("data/memory.db")
        }

        fn backups(&self) -> PathBuf {
            self.root.join("backups")
        }

        fn dotenv(&self) -> PathBuf {
            self.root.join("bin/.env")
        }

        fn claude_md(&self) -> PathBuf {
            self.root.join("claude/CLAUDE.md")
        }

        /// A memory database as it stood before the instructions table, with
        /// one memory: dropped explicitly, so the tests that need it missing
        /// keep testing that once the schema itself carries the table.
        fn schema_db(&self, path: &Path) {
            let p = path.to_string_lossy().to_string();
            pollster::block_on(async {
                let db = turso::Builder::new_local(&p)
                    .experimental_multiprocess_wal(true)
                    .build()
                    .await
                    .unwrap();
                let conn = db.connect().unwrap();
                conn.execute_batch(SCHEMA).await.unwrap();
                conn.execute_batch(PRE_INSTRUCTIONS).await.unwrap();
                conn.execute(
                    "INSERT INTO semantic_memory (category, memory_text) VALUES ('reference', 'a fact');",
                    (),
                )
                .await
                .unwrap();
            });
        }

        fn write_dotenv(&self, db: &Path, version: Option<&str>) {
            let mut text = format!(
                "# written by init\n{DB_ENV}={}\nBACKUP_DIR={}\n",
                db.display(),
                self.backups().display()
            );
            if let Some(v) = version {
                text.push_str(&format!("{VERSION_KEY}={v}\n"));
            }
            text.push_str("EMBEDDINGS=off\n");
            fs::write(self.dotenv(), text).unwrap();
        }

        fn install_block(&self, template: &str, mode: EpisodicMode, version: &str) {
            let rendered = claude_md::render(
                template,
                &[
                    ("EXE", &self.exe.display().to_string()),
                    ("EPISODIC_MODE", mode.as_str()),
                    ("EPISODIC_RULE", mode.rule()),
                ],
            )
            .unwrap();
            fs::write(
                self.claude_md(),
                format!("# Mine\n\n{}", claude_md::block(&rendered, version)),
            )
            .unwrap();
        }

        fn cfg(&self) -> Config {
            self.cfg_with(&[])
        }

        /// The fixture's config with extra variables in the real environment.
        fn cfg_with(&self, extra: &[(&str, &str)]) -> Config {
            let mut vars: HashMap<String, String> = [
                (
                    "CLAUDE_CONFIG_DIR",
                    self.root.join("claude").display().to_string(),
                ),
                ("OLLAMA_URL", "http://127.0.0.1:9".to_string()),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
            for (k, v) in extra {
                vars.insert(k.to_string(), v.to_string());
            }
            Config::from_env(&Env {
                vars,
                home: self.root.join("home"),
                windows: false,
                dotenv: DotEnv::beside(&self.exe).unwrap(),
            })
            .unwrap()
        }

        /// The DB as the store sees it, for a config pointing at it.
        fn db_cfg(&self) -> Config {
            self.cfg()
        }

        /// Every directory and file under the fixture: bytes and mtime.
        fn snapshot(&self) -> BTreeMap<PathBuf, Option<(Vec<u8>, SystemTime)>> {
            fn walk(p: &Path, out: &mut BTreeMap<PathBuf, Option<(Vec<u8>, SystemTime)>>) {
                let meta = fs::symlink_metadata(p).unwrap();
                if meta.is_dir() {
                    out.insert(p.to_path_buf(), None);
                    for e in fs::read_dir(p).unwrap() {
                        walk(&e.unwrap().path(), out);
                    }
                } else {
                    out.insert(
                        p.to_path_buf(),
                        Some((fs::read(p).unwrap(), meta.modified().unwrap())),
                    );
                }
            }
            let mut out = BTreeMap::new();
            walk(&self.root, &mut out);
            out
        }

        fn dotenv_version(&self) -> Option<String> {
            crate::config::dotenv::read(&self.dotenv())
                .unwrap()
                .and_then(|e| e.into_iter().find(|e| e.key == VERSION_KEY))
                .map(|e| e.value)
        }

        fn holds(&self, query: &str) -> bool {
            TursoStore::open(&self.db_cfg())
                .unwrap()
                .holds(query)
                .unwrap()
        }

        /// The dumps in `backups/`.
        fn dumps(&self) -> Vec<PathBuf> {
            fs::read_dir(self.backups())
                .map(|rd| rd.map(|e| e.unwrap().path()).collect())
                .unwrap_or_default()
        }
    }

    fn update(cfg: &Config, args: &Args, release: &Release) -> Result<Vec<String>> {
        let mut lines = Vec::new();
        execute(cfg, args, release, &host(), &mut |l| lines.push(l))?;
        Ok(lines)
    }

    /// Like `update`, but keeps what was printed before an error.
    fn update_partial(cfg: &Config, args: &Args, release: &Release) -> (Vec<String>, Result<()>) {
        let mut lines = Vec::new();
        let r = execute(cfg, args, release, &host(), &mut |l| lines.push(l));
        (lines, r)
    }

    fn yes() -> Args {
        Args {
            yes: true,
            ..Args::default()
        }
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

    fn gunzip(path: &Path) -> String {
        let mut text = String::new();
        flate2::read::GzDecoder::new(fs::File::open(path).unwrap())
            .read_to_string(&mut text)
            .unwrap();
        text
    }

    const HAS_WIDGETS: &str = WIDGETS.probe.query;

    // ---- the dry run -----------------------------------------------------------

    #[test]
    fn a_dry_run_changes_nothing_and_ends_with_the_confirm_line() {
        let f = Fixture::installed("dry", "0.1.0");
        let before = f.snapshot();
        let out = update(&f.cfg(), &Args::default(), &release(&[WIDGETS])).unwrap();
        assert_eq!(f.snapshot(), before, "a dry run wrote something");
        assert!(out[0].starts_with("update: dry run"), "{}", text(&out));
        assert_eq!(last(&out), "Ask the user to confirm", "{}", text(&out));
        assert!(!f.backups().exists());
        assert!(!f.holds(HAS_WIDGETS));
    }

    /// Notes oldest first, only those above the recorded version; then the
    /// pending schema migrations; then the CLAUDE.md report.
    #[test]
    fn the_plan_lists_notes_then_migrations_then_the_claude_md_report() {
        let f = Fixture::installed("order", "0.1.0");
        let shown = text(&update(&f.cfg(), &Args::default(), &release(&[WIDGETS])).unwrap());
        assert!(!shown.contains("Zero-one"), "{shown}");
        let at = |s: &str| {
            shown
                .find(s)
                .unwrap_or_else(|| panic!("{s:?} missing from:\n{shown}"))
        };
        assert!(at("0.1.0") < at("Zero-two"), "{shown}");
        assert!(at("Zero-two broke.") < at("Zero-three broke."), "{shown}");
        assert!(at("Verify Zero-three.") < at("widgets"), "{shown}");
        assert!(at("adds the widgets table") < at("CLAUDE.md"), "{shown}");
        for want in [
            "Resolve Zero-two.",
            "table `widgets` exists",
            &f.db().display().to_string(),
            "back",
            &f.claude_md().display().to_string(),
            "instructions current",
        ] {
            assert!(shown.contains(want), "{want:?} missing from:\n{shown}");
        }
    }

    // ---- -y ----------------------------------------------------------------

    #[test]
    fn yes_backs_up_first_migrates_and_records_the_version() {
        let f = Fixture::installed("yes", "0.1.0");
        let out = update(&f.cfg(), &yes(), &release(&[WIDGETS])).unwrap();
        let shown = text(&out);
        assert!(!shown.contains("Ask the user to confirm"), "{shown}");

        // The backup: taken, its path printed, and taken BEFORE the migration.
        let dumps = f.dumps();
        assert_eq!(dumps.len(), 1, "{dumps:?}");
        assert!(shown.contains(&dumps[0].display().to_string()), "{shown}");
        let sql = gunzip(&dumps[0]);
        assert!(sql.contains("semantic_memory"), "not a dump");
        assert!(
            !sql.contains("widgets"),
            "the backup was taken after the migration"
        );

        assert!(f.holds(HAS_WIDGETS));
        assert_eq!(f.dotenv_version().as_deref(), Some("0.3.0"));

        // A second run: up to date, no confirm line, nothing written.
        for args in [Args::default(), yes()] {
            let before = f.snapshot();
            let again = update(&f.cfg(), &args, &release(&[WIDGETS])).unwrap();
            assert!(last(&again).starts_with("Up to date"), "{}", text(&again));
            assert!(
                !again.iter().any(|l| l == "Ask the user to confirm"),
                "{}",
                text(&again)
            );
            assert_eq!(f.snapshot(), before);
        }
    }

    #[test]
    fn the_version_bump_keeps_every_other_byte_of_the_dotenv() {
        let f = Fixture::installed("bump", "0.1.0");
        let original = format!(
            "# mine, keep me\r\n{DB_ENV}={}\r\nBACKUP_DIR={}\r\n{VERSION_KEY}=0.1.0  # set by init\r\n\
             RECALL_ALPHA=0.2\r\n",
            f.db().display(),
            f.backups().display()
        );
        fs::write(f.dotenv(), &original).unwrap();
        update(&f.cfg(), &yes(), &release(&[])).unwrap();
        assert_eq!(
            fs::read_to_string(f.dotenv()).unwrap(),
            original.replace("=0.1.0  #", "=0.3.0  #")
        );
    }

    /// A migration that fails half-way: the database as it was, the version
    /// not recorded, a non-zero exit naming the migration.
    #[test]
    fn a_migration_forced_to_fail_changes_nothing_and_is_named() {
        let f = Fixture::installed("fail", "0.1.0");
        let db = dump_text(&f.db_cfg());
        let dotenv = fs::read(f.dotenv()).unwrap();
        let (out, r) = update_partial(&f.cfg(), &yes(), &release(&[BROKEN]));
        let err = r.expect_err("the migration fails");
        assert_eq!(err.exit_code(), EXIT_ERROR, "{err}");
        let msg = err.to_string();
        assert!(msg.contains("broken"), "{msg}");
        assert!(msg.contains("no_such_table"), "{msg}");
        assert_eq!(dump_text(&f.db_cfg()), db, "the database changed");
        assert_eq!(fs::read(f.dotenv()).unwrap(), dotenv, "the .env changed");
        assert!(!text(&out).contains("Up to date"));
        // The backup taken first is still there: the way back.
        assert_eq!(f.dumps().len(), 1);
    }

    /// Each migration is a transaction of its own: one that succeeded stays,
    /// the run stops at the failure, and the version stays unrecorded.
    #[test]
    fn a_failure_stops_the_run_after_the_migrations_before_it() {
        let f = Fixture::installed("fail-second", "0.1.0");
        let (out, r) = update_partial(&f.cfg(), &yes(), &release(&[WIDGETS, BROKEN]));
        assert!(r.unwrap_err().to_string().contains("broken"));
        let shown = text(&out);
        assert!(shown.contains("widgets"), "{shown}");
        assert!(f.holds(HAS_WIDGETS));
        assert!(!f.holds(BROKEN.probe.query));
        assert_eq!(f.dotenv_version().as_deref(), Some("0.1.0"));
    }

    /// The probe decides, not the `.env`: a database that already has the
    /// table is not migrated again, though the `.env` predates the table.
    #[test]
    fn a_database_that_already_has_instructions_is_not_migrated_again() {
        let f = Fixture::installed("already", "0.0.9");
        for m in MIGRATIONS {
            batch(&f.db_cfg(), m.sql);
        }
        let shown = text(&update(&f.cfg(), &Args::default(), &real()).unwrap());
        assert!(shown.contains("no schema migration pending"), "{shown}");
        assert!(!shown.contains("table `instructions` exists"), "{shown}");

        let db = dump_text(&f.db_cfg());
        update(&f.cfg(), &yes(), &real()).unwrap();
        assert!(
            !f.backups().exists(),
            "a backup was taken for nothing to migrate"
        );
        assert_eq!(dump_text(&f.db_cfg()), db);
        assert_eq!(f.dotenv_version().as_deref(), Some(crate::version::VERSION));
    }

    /// A dump taken before the instructions table, restored: the `.env`
    /// already records this version, and the migration is pending anyway. A
    /// dump taken after it restores with nothing pending -- the probe survives
    /// the round trip, where `PRAGMA user_version` would not.
    #[test]
    fn a_restored_dump_from_before_the_instructions_table_lists_it_as_pending() {
        let f = Fixture::installed("restored", crate::version::VERSION);
        let old = restore(&f.db_cfg(), &dump_text(&f.db_cfg()), "restored-old");
        f.write_dotenv(&old.db_path, Some(crate::version::VERSION));
        let out = update(&f.cfg(), &Args::default(), &real()).unwrap();
        let shown = text(&out);
        assert!(shown.contains("instructions"), "{shown}");
        assert!(shown.contains("table `instructions` exists"), "{shown}");
        assert_eq!(last(&out), "Ask the user to confirm");

        update(&f.cfg(), &yes(), &real()).unwrap();
        let new = restore(&old, &dump_text(&old), "restored-new");
        f.write_dotenv(&new.db_path, Some(crate::version::VERSION));
        let out = update(&f.cfg(), &Args::default(), &real()).unwrap();
        assert!(last(&out).starts_with("Up to date"), "{}", text(&out));
    }

    // ---- nothing to do, or only CLAUDE.md --------------------------------------

    /// A real environment variable overrides the `.env` for every other reader,
    /// but `update` compares the `.env`, so the disagreement must be named.
    #[test]
    fn a_real_environment_version_that_disagrees_with_the_dotenv_is_named() {
        let f = Fixture::installed("env-version", "0.3.0");
        batch(&f.db_cfg(), WIDGETS.sql);
        let cfg = f.cfg_with(&[(VERSION_KEY, "0.2.0")]);
        let out = update(&cfg, &Args::default(), &release(&[WIDGETS])).unwrap();
        let warn = out
            .iter()
            .find(|l| l.starts_with("warn:"))
            .unwrap_or_else(|| panic!("no warn line in: {}", text(&out)));
        assert!(warn.contains(VERSION_KEY), "{warn}");
        assert!(warn.contains("0.2.0"), "{warn}");
        assert!(warn.contains("0.3.0"), "{warn}");
    }

    #[test]
    fn a_real_environment_version_that_agrees_with_the_dotenv_is_not_reported() {
        let f = Fixture::installed("env-agrees", "0.3.0");
        batch(&f.db_cfg(), WIDGETS.sql);
        let cfg = f.cfg_with(&[(VERSION_KEY, "0.3.0")]);
        let out = update(&cfg, &Args::default(), &release(&[WIDGETS])).unwrap();
        assert!(
            !out.iter().any(|l| l.starts_with("warn:")),
            "{}",
            text(&out)
        );
    }

    #[test]
    fn up_to_date_says_so_and_writes_nothing() {
        let f = Fixture::installed("current", "0.3.0");
        batch(&f.db_cfg(), WIDGETS.sql);
        let before = f.snapshot();
        for args in [Args::default(), yes()] {
            let out = update(&f.cfg(), &args, &release(&[WIDGETS])).unwrap();
            assert!(last(&out).starts_with("Up to date"), "{}", text(&out));
            assert!(!text(&out).contains("Ask the user to confirm"));
        }
        assert_eq!(f.snapshot(), before);
    }

    /// A CLAUDE.md change alone needs the user's agreement, but `-y` has
    /// nothing to apply: the agent edits CLAUDE.md, never `update`.
    #[test]
    fn a_claude_md_change_alone_asks_and_yes_writes_nothing() {
        let f = Fixture::installed("only-md", "0.3.0");
        batch(&f.db_cfg(), WIDGETS.sql);
        let md = fs::read_to_string(f.claude_md())
            .unwrap()
            .replace("first;", "first; Always answer in German;");
        fs::write(f.claude_md(), md).unwrap();
        let before = f.snapshot();
        let out = update(&f.cfg(), &Args::default(), &release(&[WIDGETS])).unwrap();
        assert_eq!(last(&out), "Ask the user to confirm", "{}", text(&out));
        let out = update(&f.cfg(), &yes(), &release(&[WIDGETS])).unwrap();
        let shown = text(&out);
        assert!(shown.contains("--print-block"), "{shown}");
        assert!(shown.contains("never writes CLAUDE.md"), "{shown}");
        assert_eq!(f.snapshot(), before);
    }

    #[test]
    fn print_block_prints_the_block_and_nothing_else() {
        let f = Fixture::installed("print", "0.1.0");
        fs::remove_file(f.dotenv()).unwrap();
        let before = f.snapshot();
        let args = Args {
            print_block: true,
            ..Args::default()
        };
        let out = update(&f.cfg(), &args, &release(&[WIDGETS])).unwrap();
        let rendered = claude_md::render(
            TEMPLATE,
            &[
                ("EXE", &f.exe.display().to_string()),
                ("EPISODIC_MODE", "major-actions"),
                ("EPISODIC_RULE", EpisodicMode::MajorActions.rule()),
            ],
        )
        .unwrap();
        let want = claude_md::block(&rendered, "0.3.0");
        assert_eq!(out, want.lines().map(String::from).collect::<Vec<_>>());
        assert_eq!(f.snapshot(), before);

        // Without a mode to render with, a refusal naming the flag.
        fs::write(f.claude_md(), "# Mine\n").unwrap();
        refused(update(&f.cfg(), &args, &release(&[])), "--episodic-mode");
    }

    // ---- refusals ----------------------------------------------------------

    #[test]
    fn a_downgrade_is_refused() {
        let f = Fixture::installed("downgrade", "0.4.0");
        let before = f.snapshot();
        for args in [Args::default(), yes()] {
            let msg = refused(update(&f.cfg(), &args, &release(&[WIDGETS])), "0.4.0");
            assert!(msg.contains("0.3.0") && msg.contains("older"), "{msg}");
        }
        assert_eq!(f.snapshot(), before);
    }

    #[test]
    fn a_malformed_recorded_version_is_refused() {
        let f = Fixture::installed("malformed", "0.3");
        let before = f.snapshot();
        refused(update(&f.cfg(), &yes(), &release(&[WIDGETS])), "\"0.3\"");
        assert_eq!(f.snapshot(), before);
    }

    #[test]
    fn without_a_recorded_version_update_points_at_init() {
        let f = Fixture::installed("no-version", "0.1.0");
        // A .env without the key, then no .env at all.
        f.write_dotenv(&f.db(), None);
        let before = f.snapshot();
        refused(update(&f.cfg(), &yes(), &release(&[WIDGETS])), "init");
        fs::remove_file(f.dotenv()).unwrap();
        let before_none = f.snapshot();
        for args in [Args::default(), yes()] {
            refused(update(&f.cfg(), &args, &release(&[WIDGETS])), "init");
        }
        assert_eq!(f.snapshot(), before_none);
        assert_ne!(before, before_none);
    }

    /// A legacy machine that never ran init: the dry run prints the cutover
    /// note instead of a refusal; `-y` has nothing it could apply.
    #[test]
    fn a_legacy_block_without_a_dotenv_gets_the_cutover_note() {
        let f = Fixture::new("legacy");
        f.schema_db(&f.db());
        fs::write(
            f.claude_md(),
            "# Mine\n\n<!-- BEGIN agentic-memory (managed by install-claude-md.sh) -->\n\
             <!-- supercharged-memory: synced-at 0123abcd -->\n\
             # Long-term memory — Turso vector DB (supercharged-memory)\n\
             **Episodic policy on this machine — `manual`: rule**\n\
             <!-- END agentic-memory -->\n",
        )
        .unwrap();
        let before = f.snapshot();
        let out = update(&f.cfg(), &Args::default(), &release(&[WIDGETS])).unwrap();
        let shown = text(&out);
        for line in CUTOVER_NOTE.lines() {
            assert!(shown.contains(line), "{line:?} missing from:\n{shown}");
        }
        assert!(!shown.contains("Ask the user to confirm"), "{shown}");
        refused(update(&f.cfg(), &yes(), &release(&[WIDGETS])), "init");
        assert_eq!(f.snapshot(), before);
    }

    #[test]
    fn a_missing_database_is_refused_and_not_created() {
        let f = Fixture::installed("no-db", "0.1.0");
        let gone = f.root.join("data/elsewhere.db");
        f.write_dotenv(&gone, Some("0.1.0"));
        let before = f.snapshot();
        for args in [Args::default(), yes()] {
            refused(update(&f.cfg(), &args, &release(&[WIDGETS])), "no database");
        }
        assert_eq!(f.snapshot(), before);
        assert!(!gone.exists());
    }

    /// Judged by its header, never opened: opening an empty file would create
    /// WAL files beside it.
    #[test]
    fn a_file_that_is_not_a_database_is_refused() {
        let f = Fixture::installed("not-a-db", "0.1.0");
        for (name, bytes) in [("empty.db", &b""[..]), ("text.db", &b"just some text"[..])] {
            let path = f.root.join("data").join(name);
            fs::write(&path, bytes).unwrap();
            f.write_dotenv(&path, Some("0.1.0"));
            let before = f.snapshot();
            refused(update(&f.cfg(), &yes(), &release(&[WIDGETS])), name);
            assert_eq!(f.snapshot(), before, "{name}");
        }
        // An SQLite file of something else: never given a memory schema's table.
        let other = f.root.join("data/other.db");
        let p = other.to_string_lossy().to_string();
        pollster::block_on(async {
            let db = turso::Builder::new_local(&p)
                .experimental_multiprocess_wal(true)
                .build()
                .await
                .unwrap();
            db.connect()
                .unwrap()
                .execute("CREATE TABLE recipes (name TEXT);", ())
                .await
                .unwrap();
        });
        f.write_dotenv(&other, Some("0.1.0"));
        refused(
            update(&f.cfg(), &yes(), &release(&[WIDGETS])),
            "not a memory database",
        );
        assert_eq!(f.dumps().len(), 0);
        let mut cfg = f.cfg();
        cfg.db_path = other;
        assert!(!TursoStore::open(&cfg).unwrap().holds(HAS_WIDGETS).unwrap());
    }

    #[test]
    fn an_invalid_episodic_mode_is_refused() {
        let f = Fixture::installed("bad-mode", "0.1.0");
        let before = f.snapshot();
        let args = Args {
            episodic_mode: Some("sometimes".into()),
            ..yes()
        };
        let msg = refused(update(&f.cfg(), &args, &release(&[WIDGETS])), "sometimes");
        assert!(msg.contains("major-events"), "{msg}");
        assert_eq!(f.snapshot(), before);
    }
}
