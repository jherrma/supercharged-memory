//! `recall`: search, plus the session-start slots.
//!
//! The most-used command in the system, and the one whose failure is silent -- a
//! subtly different ranking still returns plausible rows. `rank` holds the parts
//! that can be tested without a database; this module wires them to a store and
//! prints what the runbooks quote.

pub mod rank;

use crate::config::Config;
use crate::embed;
use crate::error::{Error, Result};
use crate::render;
use crate::store::{self, Scope, SearchQuery, Store, Table, candidates};

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
    search(cfg, &store, args, args.query.as_deref().unwrap_or_default())
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

fn search(cfg: &Config, store: &impl Store, args: &Args, query: &str) -> Result<()> {
    let coworker = match &args.coworker {
        Some(name) => match store.resolve_coworker(name)? {
            Some(id) => Some(id),
            None => return Err(Error::refused(format!("no coworker named '{name}'."))),
        },
        None => None,
    };

    let cands = rank::candidate_tokens(query);
    let patterns: Vec<String> = cands.iter().map(|t| rank::like_pattern(t)).collect();

    // One embedding for the whole run; with Ollama down the search degrades to
    // keyword-only rather than failing, because the database is still useful.
    let vector = embed::ollama_up(cfg)
        .then(|| embed::embed(cfg, query))
        .transpose()?;

    for table in args.table.tables() {
        let scope = Scope {
            table,
            project: args.project.clone(),
            coworker,
        };
        let (dfs, total_rows) = store.document_frequencies(&scope, &patterns)?;
        let tokens = rank::token_weights(&cands, &dfs, total_rows);
        let total = rank::kw_total(&tokens);

        let q = SearchQuery {
            tokens: &tokens,
            total,
            alpha: cfg.recall_alpha,
            limit: args.k,
            vector: vector.as_deref(),
        };
        match vector {
            Some(_) => println!(
                "===== {} (top {}; score = dist - {}*kw, lower is better; \
                 kw = IDF-weighted keyword share 0..1) =====",
                table.as_str(),
                args.k,
                cfg.recall_alpha
            ),
            None => println!(
                "===== {} (Ollama down — keyword-only, top {}) =====",
                table.as_str(),
                args.k
            ),
        }
        println!("{}", render::line(&store.search(&scope, &q)?));
    }
    Ok(())
}
