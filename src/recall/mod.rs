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
        print!("{}", baseline(&store)?);
        return Ok(());
    }
    if args.topics {
        print!("{}", topics(&store)?);
        return Ok(());
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

/// `--baseline`, as printed.
pub(crate) fn baseline(store: &impl Store) -> Result<String> {
    let rs = store.baseline()?;
    let n = rs.rows.len();
    let mut out = format!(
        "===== baseline (load every session, follow for the whole session; {n} rule(s)) =====\n"
    );
    if n == 0 {
        // An empty result and a database still being written look identical, so
        // never let this pass as "there are no rules". A session that silently
        // starts without them drops every always-apply rule the user has.
        out.push_str("WARNING: zero baseline rules loaded, which is NOT proof that none exist.\n");
        out.push_str(
            "A migration or restore still in progress, or the wrong DB path, looks the same.\n",
        );
        out.push_str(
            "Say so to the user, then check `supercharged-memory status` and `--candidates`\n",
        );
        out.push_str("and re-run this before continuing without baseline rules.\n");
        return Ok(out);
    }
    // A newline after the rendered block: the Python implementation printed it
    // with print(), which appends one of its own. Runbooks quote the result, so
    // the trailing blank line is part of the format.
    out.push_str(&render::line(&rs));
    out.push('\n');
    Ok(out)
}

/// `--topics`, as printed.
pub(crate) fn topics(store: &impl Store) -> Result<String> {
    let rs = store.topics()?;
    let n = rs.rows.len() as u64;
    let mut out =
        format!("===== topic index (load every session; {n} topic(s); rebuilt by sleep) =====\n");
    out.push_str(&render::line(&rs));
    out.push('\n');
    if n > TOPIC_WARN_AT {
        out.push_str(&format!(
            "NOTE: {n} topics is a lot to hold in context every session — \
             consider consolidating harder next sleep (merge overlapping topics).\n"
        ));
    }
    Ok(out)
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
    let within = Within {
        project: args.project.clone(),
        coworker,
        k: args.k,
    };
    Ok(search_in(cfg, store, embedder, &args.table.tables(), &within, query)?.text)
}

/// What a search is narrowed to, whichever tables it covers.
#[derive(Debug, Clone)]
pub(crate) struct Within {
    pub project: Option<String>,
    pub coworker: Option<i64>,
    /// Hits per table.
    pub k: u32,
}

/// A search as printed, and how many rows it listed.
pub(crate) struct Searched {
    pub text: String,
    pub hits: usize,
}

/// The ranked search over each of `tables`, as printed: one section per table,
/// plus one for its rows without an embedding. `instructions` ranks through
/// this same code, so the two searches cannot drift apart.
pub(crate) fn search_in(
    cfg: &Config,
    store: &impl Store,
    embedder: &dyn Embedder,
    tables: &[Table],
    within: &Within,
    query: &str,
) -> Result<Searched> {
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
            limit: within.k,
            vector,
        };
        store.search(rows, &q)
    };

    let mut out = String::new();
    let mut hits = 0;
    let mut section = |out: &mut String, rs: ResultSet| {
        hits += rs.rows.len();
        out.push_str(&render::line(&rs));
        out.push('\n');
    };
    for &table in tables {
        let scope = |embedded| Scope {
            table,
            project: within.project.clone(),
            coworker: within.coworker,
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
                    within.k,
                    cfg.recall_alpha
                ));
                // The embedded rows alone, IDF included, so rows without a
                // vector leave this ranking exactly as it would be without them.
                section(&mut out, ranked(&with, &with, Some(vector))?);

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
                    section(&mut out, ranked(&all, &without, None)?);
                }
            }
            Err(why) => {
                out.push_str(&format!(
                    "===== {} ({} — keyword-only, top {}) =====\n",
                    table.as_str(),
                    why.label(),
                    within.k
                ));
                section(&mut out, ranked(&all, &all, None)?);
            }
        }
    }
    Ok(Searched { text: out, hits })
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

    /// Scoped to a coworker, a search sees the untagged rows and the rows
    /// tagged to them -- shared ones included -- and never a row tagged only
    /// to someone else: in the ranking, in the section of rows without an
    /// embedding, and keyword-only, in both tables.
    #[test]
    fn a_search_scoped_to_a_coworker_sees_global_rows_and_theirs_never_anothers() {
        use crate::store::turso::tests::rows;
        let cfg = scratch("recall-coworker");
        rows(
            &cfg,
            "INSERT INTO coworkers (name, expertise, personality) \
             VALUES ('jeff', 'x', 'y'), ('ada', 'x', 'y');",
        );
        let tag = |table: Table, id: i64, coworker: i64| {
            rows(
                &cfg,
                &format!(
                    "INSERT INTO memory_coworkers (memory_table, memory_id, coworker_id) \
                     VALUES ('{}', {id}, {coworker});",
                    table.as_str()
                ),
            );
        };
        for table in [Table::Semantic, Table::Episodic] {
            for (embedded, kind) in [(true, "embedded"), (false, "blind")] {
                put(&cfg, table, &format!("global {kind}"), embedded);
                let jeff = put(&cfg, table, &format!("jeff {kind}"), embedded);
                tag(table, jeff, 1);
                let ada = put(&cfg, table, &format!("ada {kind}"), embedded);
                tag(table, ada, 2);
                let shared = put(&cfg, table, &format!("shared {kind}"), embedded);
                tag(table, shared, 1);
                tag(table, shared, 2);
            }
        }
        let texts = |out: &str| -> Vec<String> {
            let mut t: Vec<String> = out
                .lines()
                .filter_map(|l| l.trim_start().strip_prefix("memory_text = "))
                .map(str::to_string)
                .collect();
            t.sort();
            t
        };
        let every = |names: &[&str]| -> Vec<String> {
            let mut t = Vec::new();
            for _table in 0..2 {
                for name in names {
                    for kind in ["embedded", "blind"] {
                        t.push(format!("{name} {kind}"));
                    }
                }
            }
            t.sort();
            t
        };
        let down = Fake::down(Unavailable::Unreachable {
            url: "http://127.0.0.1:9".into(),
        });
        let store = TursoStore::open(&cfg).unwrap();
        for embedder in [&Fake::up() as &dyn Embedder, &down] {
            let search_as = |coworker: Option<&str>| {
                let mut a = args(10);
                a.table = TableArg::Both;
                a.coworker = coworker.map(str::to_string);
                search(&cfg, &store, embedder, &a, "global jeff ada shared").expect("search")
            };
            assert_eq!(
                texts(&search_as(Some("jeff"))),
                every(&["global", "jeff", "shared"])
            );
            assert_eq!(
                texts(&search_as(Some("ada"))),
                every(&["ada", "global", "shared"])
            );
            assert_eq!(
                texts(&search_as(None)),
                every(&["ada", "global", "jeff", "shared"])
            );
        }
        let mut a = args(10);
        a.coworker = Some("nobody".into());
        let e = search(&cfg, &store, &Fake::up(), &a, "global").expect_err("unknown coworker");
        assert_eq!(e.to_string(), "refused: no coworker named 'nobody'.");
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
