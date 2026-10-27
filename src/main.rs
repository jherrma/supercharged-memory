//! supercharged-memory: the single binary behind the long-term memory system.
//!
//! It replaces a set of Python scripts that shelled out to the `tursodb` CLI and
//! parsed its text output; see issue #17. Every subcommand is wired up and documented here, and each
//! one is filled in as it is ported.

mod backfill;
mod backup;
mod claude_md;
mod cli;
mod config;
mod coworkers;
mod embed;
mod error;
mod eval;
mod find_existing;
mod init;
mod instructions;
mod json;
mod recall;
mod remember;
mod render;
mod restore;
mod rows;
mod runbook;
mod sleep;
mod status;
mod store;
mod update;
mod version;

use std::process::ExitCode;

fn main() -> ExitCode {
    cli::main(std::env::args_os())
}
