//! The CLI surface: every subcommand the Python scripts exposed, plus dispatch.
//!
//! Each variant carries the constraint its story must honour, so the brief is
//! next to the signature rather than only in stories/17-go-rewrite/.

use std::ffi::OsString;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::error::{EXIT_OK, EXIT_USAGE, Error, Result};
use crate::version;

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

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Report database health: MISSING | EMPTY | DEGRADED n | ERROR | READY n.
    ///
    /// The vocabulary is a contract: CLAUDE.md.template tells the agent to react
    /// to each state differently, and MISSING must never be read as lost data.
    Status,

    /// Search memory, or load a session-start slot.
    ///
    /// Story 004 must reproduce the ranking exactly:
    /// `score = vector_distance_cos - RECALL_ALPHA * kw`, lowest first. It stays
    /// a PURE READER -- no hit counters, no last-accessed column.
    Recall,

    /// Store one memory, or supersede existing ones.
    ///
    /// Owns every mechanical guard that keeps the corpus clean: near-duplicate
    /// refusal, the 2000-char cap, one embedding model per database, the
    /// baseline confirmation, the --created-at format.
    Remember,

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
    /// Story 009 decides whether this survives: its Python interface was "edit
    /// the list inside the script", which a compiled binary cannot offer.
    Seed,

    /// Restore a dump into a fresh database file, verifying row counts.
    ///
    /// Never over a live database. A restore that is not counted is not a restore.
    Restore,

    /// Write a validated gzipped dump to BACKUP_DIR, with daily+weekly retention.
    ///
    /// Folds in supercharged-memory-backup.sh, which was a second implementation
    /// of memlib's rules in shell and drifted from it.
    Backup,

    /// Probe for file-based Claude memory to migrate (read-only, no DB, no Ollama).
    ///
    /// SETUP.md Step 7 gates on this, so it must need nothing. Every skipped
    /// path lands in a reported field.
    FindExistingMemory,

    /// Render CLAUDE.md.template into ~/.claude/CLAUDE.md between the managed markers.
    ///
    /// Reads the existing block BEFORE it defaults, recovers EPISODIC_MODE and
    /// BASE_PATH from it, refuses a BASE_PATH that disagrees, and writes the
    /// sync stamp UPDATE.md is built on.
    Install,

    /// Recall-quality harness: validate the eval set, report metrics, sweep RECALL_ALPHA.
    ///
    /// Deep sleep D6 runs this, and it is how story 004 proves the ranking is
    /// unchanged. eval_cases are AUTHORED and cannot be regenerated.
    Eval,
}

impl Command {
    /// The subcommand's name as typed. Kept next to the enum so a rename cannot
    /// silently diverge from what the runbooks call it.
    pub fn name(&self) -> &'static str {
        match self {
            Command::Status => "status",
            Command::Recall => "recall",
            Command::Remember => "remember",
            Command::Coworkers => "coworkers",
            Command::Sleep => "sleep",
            Command::Backfill => "backfill",
            Command::Seed => "seed",
            Command::Restore => "restore",
            Command::Backup => "backup",
            Command::FindExistingMemory => "find-existing-memory",
            Command::Install => "install",
            Command::Eval => "eval",
        }
    }

    /// The story that fills this command in.
    pub fn story(&self) -> &'static str {
        match self {
            Command::Status | Command::Recall => "004",
            Command::Remember => "005",
            Command::Coworkers => "006",
            Command::Sleep => "007",
            Command::Backfill => "008",
            Command::Seed => "009",
            Command::Restore => "010",
            Command::Backup => "011",
            Command::FindExistingMemory => "012",
            Command::Install => "013",
            Command::Eval => "014",
        }
    }

    fn run(&self) -> Result<()> {
        Err(Error::NotImplemented {
            command: self.name(),
            story: self.story(),
        })
    }
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

    match cli.command.run() {
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
    use crate::error::EXIT_ERROR;

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
            "coworkers",
            "sleep",
            "backfill",
            "seed",
            "restore",
            "backup",
            "find-existing-memory",
            "install",
            "eval",
        ] {
            assert!(
                registered.contains(&want.to_string()),
                "{want} is not registered"
            );
        }
    }

    #[test]
    fn every_subcommand_reports_a_story_and_a_name() {
        for cmd in [
            Command::Status,
            Command::Recall,
            Command::Remember,
            Command::Coworkers,
            Command::Sleep,
            Command::Backfill,
            Command::Seed,
            Command::Restore,
            Command::Backup,
            Command::FindExistingMemory,
            Command::Install,
            Command::Eval,
        ] {
            assert!(!cmd.name().is_empty());
            assert!(!cmd.story().is_empty());
            assert!(matches!(cmd.run(), Err(Error::NotImplemented { .. })));
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
            Ok(cli) => match cli.command.run() {
                Ok(()) => EXIT_OK,
                Err(err) => err.exit_code(),
            },
        }
    }

    #[test]
    fn exit_codes() {
        assert_eq!(
            code(&["sm", "recall"]),
            EXIT_ERROR,
            "a stub is an error, not a refusal"
        );
        assert_eq!(
            code(&["sm", "recall", "--nope"]),
            EXIT_USAGE,
            "unknown flags are rejected"
        );
        assert_eq!(code(&["sm", "frobnicate"]), EXIT_USAGE, "unknown command");
        assert_eq!(code(&["sm"]), EXIT_USAGE, "no arguments prints usage");
        assert_eq!(code(&["sm", "--help"]), EXIT_OK, "--help is not a failure");
        assert_eq!(code(&["sm", "--version"]), EXIT_OK);
    }
}
