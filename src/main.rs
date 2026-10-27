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

/// Whether a panic message is std's report of a write to a closed stdout.
fn is_closed_stdout(message: &str) -> bool {
    message.starts_with("failed printing to stdout")
        && (message.contains("Broken pipe") || message.contains("pipe is being closed"))
}

/// Rust ignores SIGPIPE, so `print!` into a pipe whose reader has gone
/// (`... | head`) panics instead of ending the process quietly as other tools
/// do. The reader leaving is not an error, so that one panic exits with status
/// 0 and no message; every other panic keeps the default report. The hook
/// runs before an abort too, so it holds under `panic = "abort"`.
fn quiet_on_closed_stdout() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info.payload();
        let message = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&str>().copied())
            .unwrap_or("");
        if is_closed_stdout(message) {
            std::process::exit(0);
        }
        default(info);
    }));
}

fn main() -> ExitCode {
    quiet_on_closed_stdout();
    cli::main(std::env::args_os())
}

#[cfg(test)]
mod tests {
    use super::is_closed_stdout;

    #[test]
    fn only_a_closed_stdout_is_recognised() {
        assert!(is_closed_stdout(
            "failed printing to stdout: Broken pipe (os error 32)"
        ));
        assert!(!is_closed_stdout(
            "failed printing to stdout: No space left on device (os error 28)"
        ));
        assert!(!is_closed_stdout("index out of bounds"));
    }
}
