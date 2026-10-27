//! `eval`: the recall-quality harness.
//!
//! Cases live in the database (`eval_cases`, `eval_runs`), so the dump covers
//! both: they are authored, cannot be regenerated, and a past corpus cannot be
//! re-measured. Only the query-embedding cache lives on disk, and it is derived.
//!
//! `--validate` and the case writes need no embeddings. The scoring modes
//! (`--report`, `--sweep`, `--variants`) need query vectors and an embedded
//! corpus, and refuse -- naming why -- when they have neither. Nothing here
//! writes to the memory tables.

pub mod cache;
mod cases;
mod import;
mod measure;
pub mod rank;
pub mod score;
mod validate;

use std::path::PathBuf;

use crate::config::Config;
use crate::embed::{self, Embedder};
use crate::error::{Error, Result};
use crate::store::{self, Store, Table};

/// One invocation. The CLI admits exactly one per run.
#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    /// Check every live case still points at current rows with matching stamps.
    Validate,
    /// Score the shipped ranking at the configured alpha and record the run.
    Report,
    /// Score the shipped ranking across these alphas; never records, never
    /// changes the configured value.
    Sweep(Vec<f64>),
    /// Compare the frozen legacy ranker, pure cosine, IDF-lexicographic and the
    /// shipped blend.
    Variants,
    /// One-shot migration of a legacy `eval.jsonl` / `history.jsonl` directory.
    Import(PathBuf),
    /// Point a case at a new target and refresh its stamp.
    Repoint {
        case: String,
        to: String,
        from: Option<i64>,
    },
    /// Retire a case; never deletes.
    RetireCase(String),
    /// Author a case; the stamps are computed here, never typed.
    AddCase {
        id: String,
        class: String,
        table: Table,
        query: String,
        expect: String,
    },
    /// The newest `eval_runs.ran_at`.
    LastRun,
}

/// What one invocation prints, and whether it counts as having failed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    pub text: String,
    /// Set when the run did what was asked and found something wrong: the
    /// text is still printed, the exit code is 1.
    pub failure: Option<String>,
    /// Derived-data trouble that did not stop the run.
    pub warnings: Vec<String>,
}

pub fn run(cfg: &Config, mode: &Mode) -> Result<()> {
    let out = execute(cfg, mode, &embed::Ollama::new(cfg))?;
    for w in &out.warnings {
        eprintln!("warning: {w}");
    }
    print!("{}", out.text);
    match out.failure {
        Some(why) => Err(Error::failed(why)),
        None => Ok(()),
    }
}

/// What one invocation prints, or a refusal. Argument errors are refused
/// before the database is opened.
pub fn execute(cfg: &Config, mode: &Mode, embedder: &dyn Embedder) -> Result<Output> {
    // What can be judged from the arguments alone, before the database is
    // looked at: a malformed invocation should not depend on whether the
    // database happens to be reachable.
    let new_case = match mode {
        Mode::AddCase {
            id,
            class,
            table,
            query,
            expect,
        } => Some(cases::parse(&cases::NewCase {
            id,
            class,
            table: *table,
            query,
            expect,
        })?),
        _ => None,
    };
    let new_target = match mode {
        Mode::Repoint { to, .. } => Some(cases::parse_id("the new id", to)?),
        _ => None,
    };

    let store = store::open(cfg)?;
    if !store.has_eval_tables()? {
        return Err(Error::refused(format!(
            "{} has no eval_cases / eval_runs tables: it predates the eval harness, and no \
             migration creates them. Nothing was changed.",
            cfg.db_path.display()
        )));
    }
    match mode {
        Mode::Validate => validate::validate(cfg, &store),
        Mode::Report => measure::measure(cfg, &store, embedder, measure::Scoring::Report),
        Mode::Sweep(alphas) => {
            measure::measure(cfg, &store, embedder, measure::Scoring::Sweep(alphas))
        }
        Mode::Variants => measure::measure(cfg, &store, embedder, measure::Scoring::Variants),
        Mode::Import(dir) => import::import(&store, dir),
        Mode::Repoint { case, from, .. } => cases::repoint(
            &store,
            case,
            new_target.expect("parsed above for this mode"),
            *from,
        ),
        Mode::RetireCase(case) => cases::retire(&store, case),
        Mode::AddCase { .. } => cases::add(&store, &new_case.expect("parsed above for this mode")),
        Mode::LastRun => Ok(Output {
            text: match store.newest_eval_run_time()? {
                Some(at) => format!("{at}\n"),
                None => "none\n".to_string(),
            },
            ..Output::default()
        }),
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use crate::embed::fake::Fake;
    use crate::store::Cell;
    use crate::store::turso::tests::{put, rows, scratch};

    /// Rows every test database holds, each with a vector.
    pub const FACTS: [&str; 7] = [
        "turso needs the multiprocess wal flag on every open",
        "ollama serves the bge-m3 embedding model",
        "zsh does not word-split an unquoted variable",
        "turso picks the iocp vfs on windows",
        "backups are gzipped sql dumps kept three days",
        "the topic index is rebuilt wholesale by sleep",
        "a coworker is retired, never deleted",
    ];

    /// A scratch database holding `FACTS` as semantic rows, and their ids.
    pub fn db(name: &str) -> (Config, Vec<i64>) {
        let cfg = scratch(name);
        let ids = FACTS
            .iter()
            .map(|t| put(&cfg, Table::Semantic, t, true))
            .collect();
        (cfg, ids)
    }

    pub fn exec(cfg: &Config, mode: Mode) -> Result<Output> {
        execute(cfg, &mode, &Fake::up())
    }

    /// An eval case row, written directly so a test of one mode does not
    /// depend on another.
    pub fn raw_case(cfg: &Config, id: &str, class: &str, table: &str, ids: &str, stamps: &str) {
        rows(
            cfg,
            &format!(
                "INSERT INTO eval_cases (id, class, memory_table, query, expect_ids, expect_stamps) \
                 VALUES ('{id}', '{class}', '{table}', 'query {id}', '{ids}', '{stamps}');"
            ),
        );
    }

    pub fn restamp(cfg: &Config, table: Table, id: i64, created_at: &str) {
        rows(
            cfg,
            &format!(
                "UPDATE {} SET created_at = '{created_at}' WHERE id = {id};",
                table.name()
            ),
        );
    }

    pub fn supersede(cfg: &Config, old: i64, by: i64) {
        rows(
            cfg,
            &format!("UPDATE semantic_memory SET superseded_by = {by} WHERE id = {old};"),
        );
    }

    pub fn count(cfg: &Config, sql: &str) -> i64 {
        match rows(cfg, sql)[0][0] {
            Cell::Int(n) => n,
            ref other => panic!("{other:?}"),
        }
    }

    /// The text of a refusal; fails the test for anything else.
    pub fn refusal(result: Result<Output>) -> String {
        match result {
            Err(e) => {
                assert_eq!(e.exit_code(), crate::error::EXIT_REFUSED, "{e}");
                let msg = e.to_string();
                assert!(msg.starts_with("refused: "), "{msg}");
                msg
            }
            Ok(out) => panic!("expected a refusal, got: {out:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use crate::store::turso::TursoStore;
    use crate::store::{EvalRun, Store};

    fn run_at(ran_at: &str) -> EvalRun {
        EvalRun {
            ran_at: Some(ran_at.into()),
            alpha: 0.15,
            embed_model: "bge-m3".into(),
            n_cases: 1,
            r1: 1.0,
            r5: 1.0,
            mrr: 1.0,
        }
    }

    #[test]
    fn the_last_run_is_the_newest_ran_at_or_the_word_none() {
        let (cfg, _) = db("last-run");
        assert_eq!(exec(&cfg, Mode::LastRun).unwrap().text, "none\n");
        let store = TursoStore::open(&cfg).unwrap();
        store
            .insert_eval_run(&run_at("2026-05-01 10:00:00"))
            .unwrap();
        store
            .insert_eval_run(&run_at("2026-04-01 10:00:00"))
            .unwrap();
        let out = exec(&cfg, Mode::LastRun).unwrap();
        assert_eq!(out.text, "2026-05-01 10:00:00\n");
        assert_eq!(out.failure, None);
    }

    fn every_mode() -> Vec<Mode> {
        vec![
            Mode::Validate,
            Mode::Report,
            Mode::Sweep(vec![0.1]),
            Mode::Variants,
            Mode::Import(PathBuf::from("/nonexistent")),
            Mode::Repoint {
                case: "s01".into(),
                to: "1".into(),
                from: None,
            },
            Mode::RetireCase("s01".into()),
            Mode::AddCase {
                id: "s01".into(),
                class: "semantic".into(),
                table: Table::Semantic,
                query: "q".into(),
                expect: "1".into(),
            },
            Mode::LastRun,
        ]
    }

    #[test]
    fn every_mode_refuses_a_missing_database_and_none_creates_one() {
        let (mut cfg, _) = db("missing-db");
        cfg.db_path = cfg.db_path.with_file_name("not-there.db");
        for mode in every_mode() {
            let msg = refusal(exec(&cfg, mode.clone()));
            assert!(msg.contains("memory DB missing"), "{mode:?}: {msg}");
        }
        assert!(!cfg.db_path.exists());
    }

    #[test]
    fn an_argument_error_is_refused_before_the_database_is_looked_at() {
        let (mut cfg, _) = db("args-first");
        cfg.db_path = cfg.db_path.with_file_name("not-there.db");
        let msg = refusal(exec(
            &cfg,
            Mode::AddCase {
                id: "s01".into(),
                class: "semantic".into(),
                table: Table::Semantic,
                query: " ".into(),
                expect: "1".into(),
            },
        ));
        assert!(msg.contains("--query"), "{msg}");
        let msg = refusal(exec(
            &cfg,
            Mode::Repoint {
                case: "s01".into(),
                to: "abc".into(),
                from: None,
            },
        ));
        assert!(msg.contains("integer"), "{msg}");
    }

    #[test]
    fn a_database_from_before_the_eval_tables_is_refused_not_crashed() {
        let (cfg, _) = db("no-eval-tables");
        crate::store::turso::tests::rows(&cfg, "DROP TABLE eval_runs;");
        crate::store::turso::tests::rows(&cfg, "DROP TABLE eval_cases;");
        for mode in [Mode::Validate, Mode::LastRun] {
            let msg = refusal(exec(&cfg, mode));
            assert!(
                msg.contains("eval_cases") || msg.contains("eval_runs"),
                "{msg}"
            );
        }
    }
}
