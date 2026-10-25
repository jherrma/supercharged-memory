//! `status`: the first thing every session runs.
//!
//! The vocabulary is a contract. `CLAUDE.md.template` tells the agent to react to
//! each state differently -- DEGRADED means "no embeddings: recall is
//! keyword-only and writes store rows without one", ERROR means "something is
//! wrong with this file", and MISSING means "you are probably pointed at the
//! wrong path", never "your memory is gone".

use crate::config::Config;
use crate::embed::{self, Embedder, Unavailable};
use crate::error::Result;
use crate::store::{Embedded, Scope, Store, Table, candidates, turso::TursoStore};

#[derive(Debug, PartialEq, Eq)]
pub enum Status {
    /// No database at the configured path. Carries the report explaining what
    /// else was found, because "MISSING" alone invites creating a fresh one.
    Missing,
    /// Readable and empty, with embeddings available.
    Empty,
    /// Database fine, no embeddings (Ollama unreachable, the model not pulled,
    /// or `EMBEDDINGS=off`): recall is keyword-only and writes store their row
    /// without a vector.
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
/// Note the order: without embeddings the answer is DEGRADED even when the
/// corpus is empty. EMPTY is reserved for "everything works, there is just
/// nothing stored yet", which is a setup state, not a health state.
pub fn classify(
    db_exists: bool,
    count: std::result::Result<u64, String>,
    embeddings: &std::result::Result<(), Unavailable>,
) -> Status {
    if !db_exists {
        return Status::Missing;
    }
    match count {
        Err(e) => Status::Error(e),
        Ok(n) if embeddings.is_err() => Status::Degraded(n),
        Ok(0) => Status::Empty,
        Ok(n) => Status::Ready(n),
    }
}

/// Everything `status` prints for a database that exists: the state line, then
/// why there are no embeddings, then how many active rows lack one.
///
/// The first line never changes spelling -- everything that parses `status`
/// matches on it -- so the explanations go on lines of their own.
pub fn lines(
    status: &Status,
    embeddings: &std::result::Result<(), Unavailable>,
    unembedded: &[(&str, u64)],
) -> Vec<String> {
    let mut out = vec![status.line()];
    if let (Status::Degraded(_), Err(why)) = (status, embeddings) {
        out.push(format!(
            "no embeddings: {why} — recall is keyword-only, writes store rows without one"
        ));
    }
    if unembedded.iter().any(|(_, n)| *n > 0) {
        let counts: Vec<String> = unembedded
            .iter()
            .map(|(table, n)| format!("{table} {n}"))
            .collect();
        out.push(format!(
            "unembedded: {} — run embed --missing",
            counts.join(", ")
        ));
    }
    out
}

/// Active rows without an embedding, per table: current semantic rows and every
/// episodic row -- the rows recall can return, and so the ones worth embedding.
pub(crate) fn unembedded(store: &impl Store) -> Result<Vec<(&'static str, u64)>> {
    [Table::Semantic, Table::Episodic]
        .into_iter()
        .map(|table| {
            let scope = Scope {
                table,
                project: None,
                coworker: None,
                embedded: Embedded::Without,
            };
            // With no patterns, the probe is just the count of rows in scope.
            let (_, n) = store.document_frequencies(&scope, &[])?;
            Ok((table.as_str(), n))
        })
        .collect()
}

fn report(cfg: &Config, embedder: &dyn Embedder) -> Vec<String> {
    if !cfg.db_exists() {
        let mut out = vec![Status::Missing.line()];
        for line in candidates::missing_report(cfg) {
            out.push(format!("  {line}"));
        }
        return out;
    }
    let counts = TursoStore::open(cfg)
        .and_then(|s| Ok((s.total_memories()?, unembedded(&s)?)))
        .map_err(|e| e.to_string().trim_start_matches("error: ").to_string());
    let (count, missing) = match counts {
        Ok((n, missing)) => (Ok(n), missing),
        Err(e) => (Err(e), Vec::new()),
    };
    let embeddings = embed::availability(cfg, embedder);
    lines(&classify(true, count, &embeddings), &embeddings, &missing)
}

pub fn run(cfg: &Config) -> Result<()> {
    for line in report(cfg, &embed::Ollama::new(cfg)) {
        println!("{line}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::fake::Fake;
    use crate::store::turso::tests::{put, rows, scratch};

    const UP: std::result::Result<(), Unavailable> = Ok(());

    fn down() -> std::result::Result<(), Unavailable> {
        Err(Unavailable::Unreachable {
            url: "http://127.0.0.1:9".into(),
        })
    }

    #[test]
    fn a_missing_database_outranks_everything_else() {
        assert_eq!(classify(false, Ok(300), &UP), Status::Missing);
        assert_eq!(
            classify(false, Err("boom".into()), &down()),
            Status::Missing
        );
    }

    #[test]
    fn an_unreadable_database_reports_the_underlying_message() {
        // The stale-MCP-server symptom: a 0-byte file at a renamed path. The
        // runbooks key on this exact text, so it must survive into the output.
        let s = classify(true, Err("no such table: semantic_memory".into()), &UP);
        assert_eq!(s.line(), "ERROR no such table: semantic_memory");
    }

    #[test]
    fn no_embeddings_is_degraded_even_when_the_corpus_is_empty() {
        assert_eq!(classify(true, Ok(0), &down()), Status::Degraded(0));
        assert_eq!(classify(true, Ok(309), &down()), Status::Degraded(309));
        assert_eq!(
            classify(true, Ok(309), &Err(Unavailable::Off)),
            Status::Degraded(309)
        );
    }

    #[test]
    fn empty_means_healthy_but_unpopulated() {
        assert_eq!(classify(true, Ok(0), &UP), Status::Empty);
    }

    #[test]
    fn ready_carries_the_count() {
        assert_eq!(classify(true, Ok(309), &UP).line(), "READY 309");
    }

    /// The first line is what every runbook and the template match on; the
    /// lines added after it must not change it.
    #[test]
    fn the_first_line_keeps_its_spelling() {
        let some = [("semantic", 3), ("episodic", 1)];
        for (status, embeddings, want) in [
            (Status::Ready(309), UP, "READY 309"),
            (Status::Degraded(309), down(), "DEGRADED 309"),
            (Status::Degraded(0), Err(Unavailable::Off), "DEGRADED 0"),
            (Status::Empty, UP, "EMPTY"),
            (Status::Error("boom".into()), UP, "ERROR boom"),
        ] {
            assert_eq!(lines(&status, &embeddings, &some)[0], want);
        }
    }

    #[test]
    fn ready_with_every_row_embedded_is_one_line() {
        let out = lines(&Status::Ready(5), &UP, &[("semantic", 0), ("episodic", 0)]);
        assert_eq!(out, vec!["READY 5".to_string()]);
    }

    /// DEGRADED no longer means "cannot store": the next line says what it does
    /// mean, and why.
    #[test]
    fn degraded_names_its_reason_on_the_next_line() {
        for (why, want) in [
            (
                Unavailable::Unreachable {
                    url: "http://127.0.0.1:9".into(),
                },
                "Ollama unreachable at http://127.0.0.1:9",
            ),
            (
                Unavailable::NotPulled {
                    url: "http://127.0.0.1:9".into(),
                    model: "bge-m3".into(),
                },
                "bge-m3 is not pulled",
            ),
            (Unavailable::Off, "EMBEDDINGS=off"),
        ] {
            let out = lines(&Status::Degraded(7), &Err(why), &[("semantic", 0)]);
            assert_eq!(out.len(), 2, "{out:?}");
            assert_eq!(out[0], "DEGRADED 7");
            assert!(out[1].starts_with("no embeddings: "), "{}", out[1]);
            assert!(out[1].contains(want), "{}", out[1]);
            assert!(out[1].contains("keyword-only"), "{}", out[1]);
            assert!(out[1].contains("without one"), "{}", out[1]);
        }
    }

    #[test]
    fn the_unembedded_line_appears_only_when_a_count_is_above_zero() {
        let counts = [("semantic", 3), ("episodic", 0)];
        let out = lines(&Status::Ready(9), &UP, &counts);
        assert_eq!(
            out,
            vec![
                "READY 9".to_string(),
                "unembedded: semantic 3, episodic 0 — run embed --missing".to_string(),
            ]
        );
        let out = lines(&Status::Degraded(9), &down(), &counts);
        assert_eq!(out.len(), 3, "{out:?}");
        assert_eq!(
            out[2],
            "unembedded: semantic 3, episodic 0 — run embed --missing"
        );
        let out = lines(
            &Status::Degraded(9),
            &down(),
            &[("semantic", 0), ("episodic", 0)],
        );
        assert!(!out.iter().any(|l| l.starts_with("unembedded")), "{out:?}");
    }

    /// Counted against the real tables: only ACTIVE rows lacking a vector count,
    /// because a superseded row is never recalled and need not be embedded.
    #[test]
    fn status_counts_active_unembedded_rows_per_table() {
        let cfg = scratch("status-unembedded");
        put(&cfg, Table::Semantic, "embedded", true);
        let old = put(&cfg, Table::Semantic, "blind and superseded", false);
        put(&cfg, Table::Semantic, "blind one", false);
        put(&cfg, Table::Semantic, "blind two", false);
        put(&cfg, Table::Episodic, "blind event", false);
        rows(
            &cfg,
            &format!("UPDATE semantic_memory SET superseded_by = 1 WHERE id = {old};"),
        );

        let out = report(&cfg, &Fake::up());
        assert_eq!(
            out,
            vec![
                "READY 5".to_string(),
                "unembedded: semantic 2, episodic 1 — run embed --missing".to_string(),
            ]
        );
    }

    #[test]
    fn status_with_embeddings_off_never_contacts_the_embedder() {
        let mut cfg = scratch("status-off");
        cfg.embeddings = false;
        let fake = Fake::up();
        let out = report(&cfg, &fake);
        assert_eq!(fake.calls.get(), 0);
        assert_eq!(out[0], "DEGRADED 0");
        assert!(out[1].contains("EMBEDDINGS=off"), "{out:?}");
    }
}
