//! `status`: the first thing every session runs.
//!
//! The vocabulary is a contract. `CLAUDE.md.template` tells the agent to react to
//! each state differently -- DEGRADED means "recall works, do not try to store",
//! ERROR means "something is wrong with this file", and MISSING means "you are
//! probably pointed at the wrong path", never "your memory is gone".

use crate::config::Config;
use crate::embed;
use crate::error::Result;
use crate::store::{Store, candidates, turso::TursoStore};

#[derive(Debug, PartialEq, Eq)]
pub enum Status {
    /// No database at the configured path. Carries the report explaining what
    /// else was found, because "MISSING" alone invites creating a fresh one.
    Missing,
    /// Readable and empty, with Ollama up.
    Empty,
    /// Database fine, Ollama unreachable: recall degrades to keyword-only and
    /// writes must be refused.
    Degraded(u64),
    Ready(u64),
    /// The file exists but could not be read as a memory database. The classic
    /// cause is a stale process re-creating a 0-byte file at a renamed path, so
    /// the message matters as much as the state.
    Error(String),
}

impl Status {
    /// The single line the runbooks parse.
    pub fn line(&self) -> String {
        match self {
            Status::Missing => "MISSING".into(),
            Status::Empty => "EMPTY".into(),
            Status::Degraded(n) => format!("DEGRADED {n}"),
            Status::Ready(n) => format!("READY {n}"),
            Status::Error(e) => format!("ERROR {e}"),
        }
    }
}

/// The decision itself, separated from every side effect so it can be tested
/// without a database or an Ollama.
///
/// Note the order: with Ollama down the answer is DEGRADED even when the corpus
/// is empty. EMPTY is reserved for "everything works, there is just nothing
/// stored yet", which is a setup state, not a health state.
pub fn classify(
    db_exists: bool,
    count: std::result::Result<u64, String>,
    ollama_up: bool,
) -> Status {
    if !db_exists {
        return Status::Missing;
    }
    match count {
        Err(e) => Status::Error(e),
        Ok(n) if !ollama_up => Status::Degraded(n),
        Ok(0) => Status::Empty,
        Ok(n) => Status::Ready(n),
    }
}

pub fn run(cfg: &Config) -> Result<()> {
    let count = if cfg.db_exists() {
        TursoStore::open(cfg)
            .and_then(|s| s.total_memories())
            .map_err(|e| e.to_string().trim_start_matches("error: ").to_string())
    } else {
        Ok(0)
    };

    let status = classify(cfg.db_exists(), count, embed::ollama_up(cfg));
    println!("{}", status.line());
    if status == Status::Missing {
        for line in candidates::missing_report(cfg) {
            println!("  {line}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_database_outranks_everything_else() {
        assert_eq!(classify(false, Ok(300), true), Status::Missing);
        assert_eq!(classify(false, Err("boom".into()), false), Status::Missing);
    }

    #[test]
    fn an_unreadable_database_reports_the_underlying_message() {
        // The stale-MCP-server symptom: a 0-byte file at a renamed path. The
        // runbooks key on this exact text, so it must survive into the output.
        let s = classify(true, Err("no such table: semantic_memory".into()), true);
        assert_eq!(s.line(), "ERROR no such table: semantic_memory");
    }

    #[test]
    fn ollama_down_is_degraded_even_when_the_corpus_is_empty() {
        assert_eq!(classify(true, Ok(0), false), Status::Degraded(0));
        assert_eq!(classify(true, Ok(309), false), Status::Degraded(309));
    }

    #[test]
    fn empty_means_healthy_but_unpopulated() {
        assert_eq!(classify(true, Ok(0), true), Status::Empty);
    }

    #[test]
    fn ready_carries_the_count() {
        assert_eq!(classify(true, Ok(309), true).line(), "READY 309");
    }
}
