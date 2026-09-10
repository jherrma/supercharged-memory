//! Failure vocabulary, and the exit codes it maps to.
//!
//! The distinction that matters is REFUSAL vs. error. A refusal means a guard
//! rail fired and NOTHING HAPPENED; the runbooks teach the agent to read the word
//! `refused:` exactly that way, so it gets its own exit code rather than being
//! folded into a generic failure.

use std::fmt;

/// Exit codes. Anything non-zero means the command did not do what was asked.
pub const EXIT_OK: u8 = 0;
pub const EXIT_ERROR: u8 = 1;
pub const EXIT_REFUSED: u8 = 2;
/// sysexits.h EX_USAGE: the invocation itself was wrong.
pub const EXIT_USAGE: u8 = 64;

#[derive(Debug)]
pub enum Error {
    /// A guard rail fired: the command declined to act and changed nothing.
    /// Prints as `refused: <reason>`, matching the Python scripts' wording.
    Refused(String),
    /// Something went wrong while doing the work.
    Failed(String),
    /// The command exists but its story has not landed yet.
    NotImplemented {
        command: &'static str,
        story: &'static str,
    },
}

// The constructors are exercised by the tests below and used from story 005
// onward, when the first real guard rails land.
#[allow(dead_code)]
impl Error {
    pub fn refused(reason: impl Into<String>) -> Self {
        Error::Refused(reason.into())
    }

    pub fn failed(what: impl Into<String>) -> Self {
        Error::Failed(what.into())
    }

    pub fn exit_code(&self) -> u8 {
        match self {
            Error::Refused(_) => EXIT_REFUSED,
            Error::Failed(_) | Error::NotImplemented { .. } => EXIT_ERROR,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Refused(reason) => write!(f, "refused: {reason}"),
            Error::Failed(what) => write!(f, "error: {what}"),
            Error::NotImplemented { command, story } => {
                write!(
                    f,
                    "error: not implemented: {command} is filled in by story {story}"
                )
            }
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    // The wording and the codes are a contract the runbooks are written against.
    #[test]
    fn refusal_prints_the_prefix_the_runbooks_expect() {
        let e = Error::refused("memory is 2116 chars; hard cap 2000");
        assert_eq!(
            e.to_string(),
            "refused: memory is 2116 chars; hard cap 2000"
        );
        assert_eq!(e.exit_code(), EXIT_REFUSED);
    }

    #[test]
    fn a_failure_is_not_a_refusal() {
        assert_eq!(
            Error::failed("ollama is unreachable").exit_code(),
            EXIT_ERROR
        );
        assert_eq!(
            Error::failed("ollama is unreachable").to_string(),
            "error: ollama is unreachable"
        );
    }

    #[test]
    fn not_implemented_names_its_story() {
        let e = Error::NotImplemented {
            command: "recall",
            story: "004",
        };
        assert_eq!(
            e.to_string(),
            "error: not implemented: recall is filled in by story 004"
        );
        assert_eq!(e.exit_code(), EXIT_ERROR);
    }
}
