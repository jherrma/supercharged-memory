//! supercharged-memory: the single binary behind the long-term memory system.
//!
//! It replaces a set of Python scripts that shelled out to the `tursodb` CLI and
//! parsed its text output; see issue #17. Story 001 established this skeleton --
//! every subcommand is wired up and documented here, and each one is filled in by
//! its own story.

mod cli;
mod config;
mod embed;
mod error;
mod recall;
mod remember;
mod render;
mod status;
mod store;
mod version;

use std::process::ExitCode;

fn main() -> ExitCode {
    cli::main(std::env::args_os())
}
