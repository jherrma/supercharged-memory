//! The CLI surface: every subcommand the Python scripts exposed, plus dispatch.
//!
//! Each variant carries the constraint its implementation must honour, so the brief
//! is next to the signature.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};

use crate::config::{self, Config, ConfigError, Env};
use crate::error::{EXIT_OK, EXIT_USAGE, Error, Result};
use crate::remember;
use crate::store;
use crate::{backup, embed, find_existing, init, recall, restore, status, version};

#[derive(Parser, Debug)]
#[command(
    name = "supercharged-memory",
    about = "Long-term memory for AI coding agents",
    version = version::VERSION,
    long_version = version::long_version_static(),
    // Unknown flags are rejected, never ignored: a typo'd flag that is silently
    // dropped turns a refusal into a no-op that looks like success.
    disable_help_subcommand = false,
    arg_required_else_help = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// clap's view of `--table`, kept separate from the domain enum so the CLI can
/// change wording without touching the search path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum TableArg {
    Semantic,
    Episodic,
    Both,
}

/// `--table` where exactly one table is meant (the write side), as opposed to
/// `recall`'s three-way choice that also allows both at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OneTable {
    Semantic,
    Episodic,
}

impl From<OneTable> for store::Table {
    fn from(t: OneTable) -> Self {
        match t {
            OneTable::Semantic => store::Table::Semantic,
            OneTable::Episodic => store::Table::Episodic,
        }
    }
}

/// `embed --table`: one table that stores an embedding, or all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum EmbedTable {
    Semantic,
    Episodic,
    All,
}

impl EmbedTable {
    fn tables(self) -> Vec<store::Table> {
        match self {
            EmbedTable::Semantic => vec![store::Table::Semantic],
            EmbedTable::Episodic => vec![store::Table::Episodic],
            EmbedTable::All => vec![store::Table::Semantic, store::Table::Episodic],
        }
    }
}

impl From<TableArg> for recall::TableArg {
    fn from(t: TableArg) -> Self {
        match t {
            TableArg::Semantic => recall::TableArg::Semantic,
            TableArg::Episodic => recall::TableArg::Episodic,
            TableArg::Both => recall::TableArg::Both,
        }
    }
}

// One Command is built per process, and it is the process's entire argument
// state -- there is no array of them and nothing is cloned per row, so the size
// of the largest variant costs nothing worth boxing for.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Report database health: MISSING | EMPTY | DEGRADED n | ERROR | READY n.
    ///
    /// The vocabulary is a contract: CLAUDE.md.template tells the agent to react
    /// to each state differently, and MISSING must never be read as lost data.
    Status,

    /// Search memory, or load a session-start slot.
    ///
    /// `score = vector_distance_cos - RECALL_ALPHA * kw`, lowest first. A PURE
    /// READER: no hit counters, no last-accessed column. Ranking by true
    /// "hotness" would make every query a write, and that trade was refused.
    Recall {
        /// What to search for. Omit it when using one of the session-start slots.
        query: Option<String>,

        /// Which memories to search.
        #[arg(long, value_enum, default_value_t = TableArg::Both)]
        table: TableArg,

        /// Narrow to one tracking-tool work item id.
        #[arg(long)]
        project: Option<String>,

        /// How many hits per table.
        #[arg(long, default_value_t = 5)]
        k: u32,

        /// Scope to memories visible to this coworker persona.
        #[arg(long)]
        coworker: Option<String>,

        /// Load the must-always-apply rules (pure SQL; works without Ollama).
        #[arg(long)]
        baseline: bool,

        /// Load the topic index (pure SQL; load every session, like --baseline).
        #[arg(long)]
        topics: bool,

        /// List memory databases and backups found outside the configured path.
        #[arg(long)]
        candidates: bool,

        /// Print the total memory count and nothing else.
        #[arg(long)]
        count: bool,
    },

    /// Store one memory, or supersede existing ones.
    ///
    /// Owns every mechanical guard that keeps the corpus clean: near-duplicate
    /// refusal, the 2000-char cap, one embedding model per database, the
    /// baseline confirmation, the --created-at format.
    Remember {
        /// Timeless fact (semantic) or time-anchored event (episodic).
        #[arg(long, value_enum)]
        table: OneTable,

        /// The memory itself. NEVER PII — anonymize; store pointers instead.
        #[arg(long)]
        text: String,

        /// Short headline. Shown by recall and used to group a sleep pass.
        #[arg(long)]
        topic: Option<String>,

        /// Tracking-tool work item id. Omit if the memory is not ticket-bound.
        #[arg(long)]
        project: Option<String>,

        /// Appended INTO the text, so they are embedded and LIKE-searchable —
        /// and so they count against the 2000-char cap.
        #[arg(long)]
        keywords: Option<String>,

        /// Where the memory came from (user-stated, self-observed, migration…).
        #[arg(long)]
        source: Option<String>,

        /// The agent model that wrote it.
        #[arg(long)]
        model: Option<String>,

        /// Path this memory was derived from; the audit trail for an import.
        #[arg(long = "file-reference")]
        file_reference: Option<String>,

        /// 'YYYY-MM-DD HH:MM:SS', or a bare 'YYYY-MM-DD' (stored as that day at
        /// 00:00:00). Anything else is refused; omit the flag to date the row now.
        #[arg(long = "created-at")]
        created_at: Option<String>,

        /// Semantic only: baseline, user, feedback, project, reference, pattern.
        #[arg(long)]
        category: Option<String>,

        /// Episodic only: project_start, bug_fix, feature_complete, decision,
        /// milestone, incident, note.
        #[arg(long = "event-type")]
        event_type: Option<String>,

        /// Episodic only: routine, notable, major.
        #[arg(long)]
        importance: Option<String>,

        /// Required to store a 'baseline' memory: those load every session.
        #[arg(long = "confirm-baseline")]
        confirm_baseline: bool,

        /// Store even if a near-duplicate already exists.
        #[arg(long)]
        force: bool,

        /// Comma-separated id(s) of the semantic row(s) this replaces: inserts the
        /// new row and marks all of them superseded by it, in one transaction.
        #[arg(long)]
        supersedes: Option<String>,

        /// Comma-separated coworker name(s) to scope this memory to.
        #[arg(long)]
        coworker: Option<String>,
    },

    /// Fill in the embeddings of rows stored without one.
    ///
    /// Active rows only, embedded from their stored text. Refuses without Ollama
    /// and the model, or when the database holds another model. Each batch
    /// commits on its own, so an interrupted run resumes where it stopped; a
    /// run that leaves no active row unembedded switches EMBEDDINGS=off in the
    /// .env back on.
    Embed {
        /// Embed the active rows that have no embedding. Required: it is the
        /// only mode.
        #[arg(long, required = true)]
        missing: bool,

        /// Which table to fill.
        #[arg(long, value_enum, default_value_t = EmbedTable::All)]
        table: EmbedTable,

        /// Stop after this many rows, across tables.
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
        limit: Option<u32>,

        /// Rows per transaction.
        #[arg(
            long,
            default_value_t = embed::missing::DEFAULT_BATCH,
            value_parser = clap::value_parser!(u32).range(1..)
        )]
        batch: u32,
    },

    /// Add, appraise, retire or reactivate a coworker persona (writes only).
    ///
    /// Loading and listing stay ad-hoc SQL via the turso MCP server. --retire
    /// sets active=0; it never deletes.
    Coworkers,

    /// Consolidation primitives: mark-processed, retire, rebuild-topics, cluster, purge.
    ///
    /// Deliberately dumb -- judgment stays with the LLM driving SLEEP.md.
    /// --purge is the only hard-delete path in the system and keeps all four
    /// of its refusals.
    Sleep,

    /// Bulk-import a directory of files, one memory per file.
    ///
    /// Shares the write path and every refusal with `remember`.
    Backfill,

    /// Seed a fresh database with starter memories.
    ///
    /// Whether this survives is undecided: its Python interface was "edit
    /// the list inside the script", which a compiled binary cannot offer.
    Seed,

    /// Restore a dump into a fresh database file, verifying row counts.
    ///
    /// Reads tursodb's `.dump` and this binary's own dumps, .sql or .sql.gz.
    /// Every table the dump creates or writes is counted against the dump
    /// before the file takes its name; any mismatch, rejected statement or
    /// cut-off dump exits non-zero and leaves nothing at --out. Never over the
    /// configured database, not even with --force. A restore that is not
    /// counted is not a restore.
    Restore {
        /// The dump to restore. Default: the newest dump in BACKUP_DIR.
        #[arg(long, value_name = "FILE")]
        dump: Option<PathBuf>,

        /// The database file to create. It must not exist yet.
        #[arg(long, value_name = "FILE")]
        out: PathBuf,

        /// Replace an existing file at --out, once the restore is verified.
        /// This discards everything that file holds.
        #[arg(long)]
        force: bool,
    },

    /// Write a verified, gzipped SQL dump to BACKUP_DIR; keep 3 daily, 4 weekly.
    ///
    /// stdout is the dump's absolute path and nothing else, printed once the
    /// dump is written, verified and in place; everything else goes to stderr,
    /// and a failure prints nothing on stdout. One read snapshot, so writers
    /// are never blocked. Needs no tursodb.
    Backup,

    /// Probe for file-based Claude memory to migrate (read-only, no DB, no Ollama).
    ///
    /// Scans CLAUDE_CONFIG_DIR (default ~/.claude): CLAUDE.md outside the managed
    /// block, memory/ and projects/*/memory/, recursively, `.md` files of any case.
    /// Prints JSON. Every skipped path lands in a reported field, and
    /// `nothing_to_do` is the one field to gate a migration offer on.
    FindExistingMemory {
        /// Print only `found` or `nothing` -- `nothing` exactly when the JSON
        /// would say `nothing_to_do: true`. Exits 0 either way.
        #[arg(long)]
        quiet: bool,
    },

    /// First-run setup: create the database, write the .env, install the instructions.
    ///
    /// A dry run unless -y: prints the plan, ending `Ask the user to confirm`,
    /// and changes nothing. Never writes an existing database, never overwrites
    /// a differing .env value, never rewrites CLAUDE.md text -- it appends the
    /// instructions only when their headline is absent. Ollama and tursodb are
    /// optional; each missing one is listed with what the user loses.
    Init {
        /// The database file: adopted if it is a memory database, created from
        /// the embedded schema if absent. Default: the configured path.
        #[arg(long = "db-path", value_name = "FILE")]
        db_path: Option<PathBuf>,

        /// Where `backup` writes its dumps. Default: the configured directory.
        #[arg(long = "backup-dir", value_name = "DIR")]
        backup_dir: Option<PathBuf>,

        /// How much the agent stores as episodic memory: every-prompt,
        /// major-actions, major-events (default) or manual.
        #[arg(long = "episodic-mode", value_name = "MODE")]
        episodic_mode: Option<String>,

        /// Apply the plan. Also the acknowledgement of every missing dependency
        /// the plan lists.
        #[arg(short = 'y', long = "yes")]
        yes: bool,
    },

    /// Recall-quality harness: validate the eval set, report metrics, sweep RECALL_ALPHA.
    ///
    /// Deep sleep D6 runs this, and it is how the ranking is proven
    /// unchanged. eval_cases are AUTHORED and cannot be regenerated.
    Eval,

    /// Print every setting, its value and where it came from: env, .env or default.
    ///
    /// Read-only. Precedence is real environment > the .env beside the binary >
    /// built-in default; a real variable that overrides a differing .env value
    /// says so.
    Config,
}

impl Command {
    /// The subcommand's name as typed. Kept next to the enum so a rename cannot
    /// silently diverge from what the runbooks call it.
    pub fn name(&self) -> &'static str {
        match self {
            Command::Status => "status",
            Command::Recall { .. } => "recall",
            Command::Remember { .. } => "remember",
            Command::Embed { .. } => "embed",
            Command::Coworkers => "coworkers",
            Command::Sleep => "sleep",
            Command::Backfill => "backfill",
            Command::Seed => "seed",
            Command::Restore { .. } => "restore",
            Command::Backup => "backup",
            Command::FindExistingMemory { .. } => "find-existing-memory",
            Command::Init { .. } => "init",
            Command::Eval => "eval",
            Command::Config => "config",
        }
    }

    fn run(&self, cfg: &Config) -> Result<()> {
        match self {
            Command::Status => status::run(cfg),
            Command::Config => config::run(cfg),
            Command::Recall {
                query,
                table,
                project,
                k,
                coworker,
                baseline,
                topics,
                candidates,
                count,
            } => recall::run(
                cfg,
                &recall::Args {
                    query: query.clone(),
                    table: (*table).into(),
                    project: project.clone(),
                    k: *k,
                    coworker: coworker.clone(),
                    baseline: *baseline,
                    topics: *topics,
                    candidates: *candidates,
                    count: *count,
                },
            ),
            Command::Remember {
                table,
                text,
                topic,
                project,
                keywords,
                source,
                model,
                file_reference,
                created_at,
                category,
                event_type,
                importance,
                confirm_baseline,
                force,
                supersedes,
                coworker,
            } => remember::run(
                cfg,
                &remember::Args {
                    table: Some((*table).into()),
                    text: text.clone(),
                    topic: topic.clone(),
                    project: project.clone(),
                    keywords: keywords.clone(),
                    source: source.clone(),
                    model: model.clone(),
                    file_reference: file_reference.clone(),
                    created_at: created_at.clone(),
                    category: category.clone(),
                    event_type: event_type.clone(),
                    importance: importance.clone(),
                    confirm_baseline: *confirm_baseline,
                    force: *force,
                    supersedes: supersedes.clone(),
                    coworker: coworker.clone(),
                },
            ),
            Command::Embed {
                missing: _,
                table,
                limit,
                batch,
            } => embed::missing::run(
                cfg,
                &embed::missing::Args {
                    tables: table.tables(),
                    limit: *limit,
                    batch: *batch,
                },
            ),
            Command::Backup => backup::run(cfg),
            Command::Restore { dump, out, force } => restore::run(
                cfg,
                &restore::Args {
                    dump: dump.clone(),
                    out: out.clone(),
                    force: *force,
                },
            ),
            Command::FindExistingMemory { quiet } => find_existing::run(cfg, *quiet),
            Command::Init {
                db_path,
                backup_dir,
                episodic_mode,
                yes,
            } => init::run(
                cfg,
                &init::Args {
                    db_path: db_path.clone(),
                    backup_dir: backup_dir.clone(),
                    episodic_mode: episodic_mode.clone(),
                    yes: *yes,
                },
            ),
            _ => Err(Error::NotImplemented {
                command: self.name(),
            }),
        }
    }
}

/// Resolve configuration, then run the command with it.
///
/// Every command goes through here, stubs included, so a `.env` that cannot be
/// trusted stops the run before anything happens -- running on the built-in
/// defaults instead is how a skipped database-path line would silently open the
/// wrong database.
fn execute(command: &Command, env: std::result::Result<Env, ConfigError>) -> Result<()> {
    let cfg = Config::from_env(&env?)?;
    for warning in &cfg.warnings {
        eprintln!("warning: {warning}");
    }
    command.run(&cfg)
}

/// Parse and dispatch one invocation, returning the process exit code.
pub fn main<I, T>(args: I) -> ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(err) => {
            // clap already rendered the message and the usage text. --help and
            // --version are successful requests, not failures.
            let _ = err.print();
            return ExitCode::from(if err.use_stderr() {
                EXIT_USAGE
            } else {
                EXIT_OK
            });
        }
    };

    match execute(&cli.command, Env::system()) {
        Ok(()) => ExitCode::from(EXIT_OK),
        Err(err) => {
            eprintln!("{err}");
            ExitCode::from(err.exit_code())
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;
    use crate::error::{EXIT_ERROR, EXIT_REFUSED};

    #[test]
    fn clap_definition_is_wellformed() {
        Cli::command().debug_assert();
    }

    /// Every subcommand the rewrite promises must exist, so a runbook rewritten
    /// against this surface cannot name a command that was never wired up.
    #[test]
    fn every_promised_subcommand_is_registered() {
        let registered: Vec<String> = Cli::command()
            .get_subcommands()
            .map(|c| c.get_name().to_string())
            .collect();
        for want in [
            "status",
            "recall",
            "remember",
            "embed",
            "coworkers",
            "sleep",
            "backfill",
            "seed",
            "restore",
            "backup",
            "find-existing-memory",
            "init",
            "eval",
            "config",
        ] {
            assert!(
                registered.contains(&want.to_string()),
                "{want} is not registered"
            );
        }
        // `init` replaced the installer script's stand-in; nothing is left to
        // call by the old name.
        assert!(!registered.contains(&"install".to_string()));
    }

    /// Minimal valid arguments per subcommand, so each one gets past clap.
    fn invocation(name: &str) -> Vec<&str> {
        let mut args = vec!["sm", name];
        match name {
            "recall" => args.push("anything"),
            "remember" => args.extend(["--table", "semantic", "--text", "a memory"]),
            "embed" => args.push("--missing"),
            "restore" => args.extend(["--out", "/nonexistent/restored.db"]),
            _ => {}
        }
        args
    }

    /// A malformed `.env` must stop EVERY command, stubs included, before it does
    /// anything -- running on defaults is the silent wrong-database failure.
    #[test]
    fn a_malformed_dotenv_makes_every_command_refuse() {
        let dir = crate::config::dotenv::tests::tmpdir("cli-malformed");
        let exe = dir.join("supercharged-memory");
        std::fs::write(&exe, b"").unwrap();
        std::fs::write(
            dir.join(".env"),
            "SUPERCHARGED_MEMORY_TURSO_PATH=/x.db\nexport A=1\n",
        )
        .unwrap();

        let names: Vec<String> = Cli::command()
            .get_subcommands()
            .map(|c| c.get_name().to_string())
            .filter(|n| n != "help")
            .collect();
        assert!(names.contains(&"config".to_string()));
        for name in &names {
            let cli =
                Cli::try_parse_from(invocation(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
            let env = crate::config::DotEnv::beside(&exe).map(|dotenv| Env {
                vars: Default::default(),
                home: dir.clone(),
                windows: false,
                dotenv,
            });
            let err = execute(&cli.command, env).expect_err(name);
            assert_eq!(err.exit_code(), EXIT_REFUSED, "{name}: {err}");
            let msg = err.to_string();
            assert!(msg.starts_with("refused: "), "{name}: {msg}");
            assert!(msg.contains("line 2"), "{name}: {msg}");
        }
    }

    #[test]
    fn config_reads_only() {
        let cfg = Config::from_env(&Env {
            vars: Default::default(),
            home: std::path::PathBuf::from("/home/tester"),
            windows: false,
            dotenv: crate::config::DotEnv::absent(crate::config::tests::EXE.into()),
        })
        .unwrap();
        assert!(Command::Config.run(&cfg).is_ok());
        assert_eq!(Command::Config.name(), "config");
    }

    #[test]
    fn every_subcommand_reports_a_name() {
        let cfg = Config::from_env(&Env {
            vars: Default::default(),
            home: std::path::PathBuf::from("/home/tester"),
            windows: false,
            dotenv: crate::config::DotEnv::absent(crate::config::tests::EXE.into()),
        })
        .unwrap();
        for cmd in [
            Command::Status,
            Command::Recall {
                query: None,
                table: TableArg::Both,
                project: None,
                k: 5,
                coworker: None,
                baseline: false,
                topics: false,
                candidates: false,
                count: false,
            },
            Command::Remember {
                table: OneTable::Semantic,
                text: String::new(),
                topic: None,
                project: None,
                keywords: None,
                source: None,
                model: None,
                file_reference: None,
                created_at: None,
                category: None,
                event_type: None,
                importance: None,
                confirm_baseline: false,
                force: false,
                supersedes: None,
                coworker: None,
            },
            Command::Embed {
                missing: true,
                table: EmbedTable::All,
                limit: None,
                batch: 1,
            },
            Command::Coworkers,
            Command::Sleep,
            Command::Backfill,
            Command::Seed,
            Command::Restore {
                dump: None,
                out: PathBuf::from("/nonexistent/restored.db"),
                force: false,
            },
            Command::Backup,
            Command::FindExistingMemory { quiet: false },
            Command::Init {
                db_path: None,
                backup_dir: None,
                episodic_mode: None,
                yes: false,
            },
            Command::Eval,
            Command::Config,
        ] {
            assert!(!cmd.name().is_empty());
            if !matches!(
                cmd,
                Command::Status
                    | Command::Recall { .. }
                    | Command::Remember { .. }
                    | Command::Embed { .. }
                    | Command::Backup
                    | Command::Restore { .. }
                    | Command::FindExistingMemory { .. }
                    | Command::Init { .. }
                    | Command::Config
            ) {
                assert!(matches!(cmd.run(&cfg), Err(Error::NotImplemented { .. })));
            }
        }
    }

    fn code(args: &[&str]) -> u8 {
        // ExitCode has no accessor, so exercise the same paths it maps from.
        match Cli::try_parse_from(args) {
            Err(err) => {
                if err.use_stderr() {
                    EXIT_USAGE
                } else {
                    EXIT_OK
                }
            }
            Ok(cli) => match execute(&cli.command, Env::system()) {
                Ok(()) => EXIT_OK,
                Err(err) => err.exit_code(),
            },
        }
    }

    #[test]
    fn exit_codes() {
        assert_eq!(
            code(&["sm", "coworkers"]),
            EXIT_ERROR,
            "a stub is an error, not a refusal"
        );
        assert_eq!(
            code(&["sm", "remember"]),
            EXIT_USAGE,
            "remember without its required flags is a usage error"
        );
        assert_eq!(
            code(&["sm", "remember", "--table", "semantic", "--text", "  "]),
            EXIT_REFUSED,
            "an empty --text is refused before the database is opened"
        );
        assert_eq!(
            code(&["sm", "recall"]),
            EXIT_REFUSED,
            "recall with no query and no mode is a refusal, and must not open the database"
        );
        assert_eq!(
            code(&["sm", "recall", "--nope"]),
            EXIT_USAGE,
            "unknown flags are rejected"
        );
        assert_eq!(
            code(&["sm", "embed"]),
            EXIT_USAGE,
            "embed without --missing names no mode"
        );
        for bad in [["--batch", "0"], ["--limit", "0"]] {
            let mut args = vec!["sm", "embed", "--missing"];
            args.extend(bad);
            assert_eq!(code(&args), EXIT_USAGE, "{bad:?} asks for no work");
        }
        assert_eq!(
            code(&["sm", "find-existing-memory", "--quiet", "--json"]),
            EXIT_USAGE,
            "the probe rejects unknown flags too"
        );
        assert_eq!(
            code(&["sm", "init", "--force"]),
            EXIT_USAGE,
            "init rejects unknown flags like every command"
        );
        // Rejected by clap, so nothing runs: a scheduled `backup` with a stray
        // argument must fail, not back up something else.
        assert_eq!(
            code(&["sm", "backup", "--out", "/tmp/x"]),
            EXIT_USAGE,
            "backup takes no arguments"
        );
        // Rejected by clap before anything runs. Never add a restore here that
        // gets past clap: these run against the real environment.
        assert_eq!(
            code(&["sm", "restore"]),
            EXIT_USAGE,
            "restore names no target without --out"
        );
        assert_eq!(
            code(&["sm", "restore", "--out", "/nonexistent/x.db", "--into", "y"]),
            EXIT_USAGE,
            "restore rejects unknown flags"
        );
        // Validated before anything is probed or read, so this touches nothing.
        assert_eq!(
            code(&["sm", "init", "--episodic-mode", "sometimes"]),
            EXIT_REFUSED,
            "an invalid episodic mode is a refusal"
        );
        assert_eq!(
            code(&["sm", "init", "--db-path", "relative.db", "-y"]),
            EXIT_REFUSED,
            "a relative path cannot go into the .env"
        );
        assert_eq!(code(&["sm", "frobnicate"]), EXIT_USAGE, "unknown command");
        assert_eq!(code(&["sm"]), EXIT_USAGE, "no arguments prints usage");
        assert_eq!(code(&["sm", "--help"]), EXIT_OK, "--help is not a failure");
        assert_eq!(code(&["sm", "--version"]), EXIT_OK);
    }
}
