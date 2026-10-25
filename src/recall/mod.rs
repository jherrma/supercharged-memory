//! `recall`: search, plus the session-start slots.
//!
//! The most-used command in the system, and the one whose failure is silent -- a
//! subtly different ranking still returns plausible rows. `rank` holds the parts
//! that can be tested without a database; this module wires them to a store and
//! prints what the runbooks quote.

pub mod rank;

use crate::config::Config;
use crate::embed::{self, Embedder};
use crate::error::{Error, Result};
use crate::render;
use crate::store::{self, Embedded, ResultSet, Scope, SearchQuery, Store, Table, candidates};

/// Above this, the topic index costs real context every single session.
const TOPIC_WARN_AT: u64 = 50;

#[derive(Debug, Clone)]
pub struct Args {
    pub query: Option<String>,
    pub table: TableArg,
    pub project: Option<String>,
    pub k: u32,
    pub coworker: Option<String>,
    pub baseline: bool,
    pub topics: bool,
    pub candidates: bool,
    pub count: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableArg {
    Semantic,
    Episodic,
    Both,
}

impl TableArg {
    fn tables(self) -> Vec<Table> {
        match self {
            TableArg::Semantic => vec![Table::Semantic],
            TableArg::Episodic => vec![Table::Episodic],
            TableArg::Both => vec![Table::Semantic, Table::Episodic],
        }
    }
}

pub fn run(cfg: &Config, args: &Args) -> Result<()> {
    // Refuse a meaningless invocation BEFORE opening anything: an argument error
    // should not depend on whether the database happens to be reachable.
    let has_mode = args.candidates || args.count || args.baseline || args.topics;
    let has_query = args.query.as_ref().is_some_and(|q| !q.trim().is_empty());
    if !has_mode && !has_query {
        return Err(Error::refused(
            "provide a query, or use --baseline / --topics / --candidates / --count",
        ));
    }

    // --candidates answers "is my real memory somewhere else?", so it is the one
    // mode that must work with no database at the configured path.
    if args.candidates {
        return list_candidates(cfg);
    }
    let store = store::open(cfg)?;

    if args.count {
        println!("{}", store.total_memories()?);
        return Ok(());
    }
    if args.baseline {
        return baseline(&store);
    }
    if args.topics {
        return topics(&store);
    }
    let out = search(
        cfg,
        &store,
        &embed::Ollama::new(cfg),
        args,
        args.query.as_deref().unwrap_or_default(),
    )?;
    print!("{out}");
    Ok(())
}

fn baseline(store: &impl Store) -> Result<()> {
    let rs = store.baseline()?;
    let n = rs.rows.len();
    println!(
        "===== baseline (load every session, follow for the whole session; {n} rule(s)) ====="
    );
    if n == 0 {
        // An empty result and a database still being written look identical, so
        // never let this pass as "there are no rules". A session that silently
        // starts without them drops every always-apply rule the user has.
        println!("WARNING: zero baseline rules loaded, which is NOT proof that none exist.");
        println!("A migration or restore still in progress, or the wrong DB path, looks the same.");
        println!("Say so to the user, then check `supercharged-memory status` and `--candidates`");
        println!("and re-run this before continuing without baseline rules.");
        return Ok(());
    }
    // println!, not print!: the Python implementation printed the rendered
    // block with print(), which appends a newline of its own. Runbooks quote
    // the result, so the trailing blank line is part of the format.
    println!("{}", render::line(&rs));
    Ok(())
}

fn topics(store: &impl Store) -> Result<()> {
    let rs = store.topics()?;
    let n = rs.rows.len() as u64;
    println!("===== topic index (load every session; {n} topic(s); rebuilt by sleep) =====");
    println!("{}", render::line(&rs));
    if n > TOPIC_WARN_AT {
        println!(
            "NOTE: {n} topics is a lot to hold in context every session — \
             consider consolidating harder next sleep (merge overlapping topics)."
        );
    }
    Ok(())
}

fn list_candidates(cfg: &Config) -> Result<()> {
    let found = candidates::find(cfg);
    println!(
        "configured path : {}{}",
        cfg.db_path.display(),
        if cfg.db_exists() {
            " (exists)"
        } else {
            " (MISSING)"
        }
    );
    for db in &found.dbs {
        println!(
            "CANDIDATE DB    : {} ({} memories)",
            db.path.display(),
            db.memories
        );
    }
    for b in found.backups.iter().take(5) {
        println!("CANDIDATE BACKUP: {}", b.display());
    }
    if found.is_empty() {
        println!("no other database or backup found in the usual locations");
    } else {
        println!("Ask the user before switching, restoring, or overwriting anything.");
    }
    Ok(())
}

/// The search output for every requested table, as printed.
pub(crate) fn search(
    cfg: &Config,
    store: &impl Store,
    embedder: &dyn Embedder,
    args: &Args,
    query: &str,
) -> Result<String> {
    let coworker = match &args.coworker {
        Some(name) => match store.resolve_coworker(name)? {
            Some(id) => Some(id),
            None => return Err(Error::refused(format!("no coworker named '{name}'."))),
        },
        None => None,
    };

    let cands = rank::candidate_tokens(query);
    let patterns: Vec<String> = cands.iter().map(|t| rank::like_pattern(t)).collect();

    // One embedding for the whole run; without one the search degrades to
    // keyword-only rather than failing, because the database is still useful.
    let vector = match embed::availability(cfg, embedder) {
        Ok(()) => Ok(embedder.embed(query)?),
        Err(why) => Err(why),
    };

    // One ranked search: the query's tokens weighted by IDF over `idf`'s rows,
    // then `rows` ranked with them.
    let ranked = |idf: &Scope, rows: &Scope, vector: Option<&[f32]>| -> Result<ResultSet> {
        let (dfs, total_rows) = store.document_frequencies(idf, &patterns)?;
        let tokens = rank::token_weights(&cands, &dfs, total_rows);
        let q = SearchQuery {
            tokens: &tokens,
            total: rank::kw_total(&tokens),
            alpha: cfg.recall_alpha,
            limit: args.k,
            vector,
        };
        store.search(rows, &q)
    };

    let mut out = String::new();
    for table in args.table.tables() {
        let scope = |embedded| Scope {
            table,
            project: args.project.clone(),
            coworker,
            embedded,
        };
        let (all, with, without) = (
            scope(Embedded::Any),
            scope(Embedded::With),
            scope(Embedded::Without),
        );
        match &vector {
            Ok(vector) => {
                out.push_str(&format!(
                    "===== {} (top {}; score = dist - {}*kw, lower is better; \
                     kw = IDF-weighted keyword share 0..1) =====\n",
                    table.as_str(),
                    args.k,
                    cfg.recall_alpha
                ));
                // The embedded rows alone, IDF included, so rows without a
                // vector leave this ranking exactly as it would be without them.
                out.push_str(&render::line(&ranked(&with, &with, Some(vector))?));
                out.push('\n');

                // Rows a vector search cannot see get a keyword-only section of
                // their own, so a write made without Ollama is not lost to
                // recall. No such rows, no section: the output stays as it was.
                let (_, unembedded) = store.document_frequencies(&without, &[])?;
                if unembedded > 0 {
                    out.push_str(&format!(
                        "===== {}: {unembedded} without embedding (keyword-only) =====\n",
                        table.as_str()
                    ));
                    // Weighted as keyword-only mode weighs them, over every row
                    // in scope: over the unembedded rows alone, a word they all
                    // share would weigh ln(1) = 0 and hide a lone row.
                    out.push_str(&render::line(&ranked(&all, &without, None)?));
                    out.push('\n');
                }
            }
            Err(why) => {
                out.push_str(&format!(
                    "===== {} ({} — keyword-only, top {}) =====\n",
                    table.as_str(),
                    why.label(),
                    args.k
                ));
                out.push_str(&render::line(&ranked(&all, &all, None)?));
                out.push('\n');
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::Unavailable;
    use crate::embed::fake::Fake;
    use crate::store::turso::TursoStore;
    use crate::store::turso::tests::{put, scratch};

    /// Rows every test database holds, each with a vector.
    const EMBEDDED: [&str; 4] = [
        "turso needs the multiprocess wal flag on every open",
        "ollama serves the bge-m3 embedding model",
        "zsh does not word-split an unquoted variable",
        "turso picks the iocp vfs on windows",
    ];

    fn args(k: u32) -> Args {
        Args {
            query: None,
            table: TableArg::Semantic,
            project: None,
            k,
            coworker: None,
            baseline: false,
            topics: false,
            candidates: false,
            count: false,
        }
    }

    fn db(name: &str, unembedded: &[&str]) -> Config {
        let cfg = scratch(name);
        for text in EMBEDDED {
            put(&cfg, Table::Semantic, text, true);
        }
        for text in unembedded {
            put(&cfg, Table::Semantic, text, false);
        }
        cfg
    }

    fn run_search(cfg: &Config, embedder: &dyn Embedder, k: u32, query: &str) -> String {
        search(
            cfg,
            &TursoStore::open(cfg).unwrap(),
            embedder,
            &args(k),
            query,
        )
        .expect("search")
    }

    fn headers(out: &str) -> Vec<&str> {
        out.lines().filter(|l| l.starts_with("=====")).collect()
    }

    /// The parity guarantee: rows without a vector change nothing about the
    /// vector ranking -- not its order, not its scores. IDF included, so the
    /// keyword share of an embedded row does not move because a blind row
    /// happens to contain the same word.
    #[test]
    fn the_main_ranking_equals_that_of_the_same_db_minus_the_unembedded_rows() {
        let mixed = db(
            "recall-mixed",
            &[
                "turso wal row stored without a vector",
                "turso again, blind",
            ],
        );
        let embedded_only = db("recall-embedded-only", &[]);
        let a = run_search(&mixed, &Fake::up(), 5, "turso wal");
        let b = run_search(&embedded_only, &Fake::up(), 5, "turso wal");
        let main = a.split("===== semantic: ").next().unwrap();
        assert_eq!(main, b);
        assert_eq!(headers(&a).len(), 2, "the mixed DB adds a section:\n{a}");
    }

    #[test]
    fn an_unembedded_row_with_a_keyword_hit_is_listed_and_one_without_is_not() {
        let cfg = db(
            "recall-section",
            &[
                "the wal flag, stored without a vector",
                "nothing relevant here",
            ],
        );
        let out = run_search(&cfg, &Fake::up(), 5, "wal flag");
        let section = out
            .split("===== semantic: 2 without embedding (keyword-only) =====\n")
            .nth(1)
            .unwrap_or_else(|| panic!("no unembedded section:\n{out}"));
        assert!(
            section.contains("memory_text = the wal flag, stored without a vector"),
            "{out}"
        );
        assert!(!out.contains("nothing relevant here"), "{out}");
        // Ranked by keyword alone: there is no distance to show.
        assert!(
            section.contains("kw = ") && !section.contains("dist = "),
            "{section}"
        );
    }

    /// IDF measured over the unembedded rows alone gives a word they all share
    /// weight ln(1) = 0, so a single blind row could never be listed. The
    /// section is weighted like keyword-only mode, over every row in scope.
    #[test]
    fn a_lone_unembedded_row_with_a_keyword_hit_is_listed() {
        let cfg = db(
            "recall-section-lone",
            &["the wal flag, stored without a vector"],
        );
        let out = run_search(&cfg, &Fake::up(), 5, "wal");
        let section = out
            .split("===== semantic: 1 without embedding (keyword-only) =====\n")
            .nth(1)
            .unwrap_or_else(|| panic!("no unembedded section:\n{out}"));
        assert!(section.contains("stored without a vector"), "{out}");
    }

    #[test]
    fn the_unembedded_section_lists_at_most_k_rows() {
        let cfg = db("recall-section-k", &["wal one", "wal two", "wal three"]);
        let out = run_search(&cfg, &Fake::up(), 2, "wal");
        let section = out
            .split("===== semantic: 3 without embedding (keyword-only) =====\n")
            .nth(1)
            .unwrap_or_else(|| panic!("no unembedded section:\n{out}"));
        assert_eq!(section.matches("memory_text = ").count(), 2, "{section}");
    }

    /// With every row embedded the output is what it was before rows without a
    /// vector existed, so the eval baseline and the Python parity still hold.
    #[test]
    fn with_every_row_embedded_there_is_no_extra_section() {
        let cfg = db("recall-all-embedded", &[]);
        let out = run_search(&cfg, &Fake::up(), 5, "turso wal");
        assert_eq!(
            headers(&out),
            vec![
                "===== semantic (top 5; score = dist - 0.15*kw, lower is better; \
                 kw = IDF-weighted keyword share 0..1) ====="
            ]
        );
    }

    #[test]
    fn keyword_only_mode_ranks_every_row_in_one_section() {
        let cfg = db("recall-kw-only", &["turso wal row stored without a vector"]);
        let down = Fake::down(Unavailable::Unreachable {
            url: "http://127.0.0.1:9".into(),
        });
        let out = run_search(&cfg, &down, 5, "turso wal");
        assert_eq!(
            headers(&out),
            vec!["===== semantic (Ollama down — keyword-only, top 5) ====="]
        );
        assert!(
            out.contains("turso wal row stored without a vector"),
            "{out}"
        );
        assert!(
            out.contains("turso needs the multiprocess wal flag"),
            "{out}"
        );
    }

    #[test]
    fn embeddings_off_recalls_by_keyword_without_contacting_ollama() {
        let mut cfg = db("recall-off", &[]);
        cfg.embeddings = false;
        let fake = Fake::up();
        let out = run_search(&cfg, &fake, 5, "turso");
        assert_eq!(
            fake.calls.get(),
            0,
            "EMBEDDINGS=off must not contact Ollama"
        );
        assert_eq!(
            headers(&out),
            vec!["===== semantic (EMBEDDINGS=off — keyword-only, top 5) ====="]
        );
        assert!(out.contains("turso picks the iocp vfs"), "{out}");
    }
}
