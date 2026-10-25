//! `embed --missing`: fill in the embeddings of rows stored without one.
//!
//! A row goes in without a vector whenever Ollama is unavailable at write time;
//! this command catches it up once Ollama is there. Only active rows are filled
//! -- a superseded or retired row is never recalled, and deep sleep may purge it
//! -- each from its stored text, keywords included, which is exactly what the
//! writer would have embedded.
//!
//! Batches commit one at a time and the selection is `embedding IS NULL`, so a
//! run that stops part-way keeps what it committed and a re-run carries on.

use std::path::{Path, PathBuf};

use crate::config::dotenv::{self, OnConflict};
use crate::config::{Config, Source};
use crate::embed::{Embedder, Ollama};
use crate::error::{Error, Result};
use crate::remember::DUP_DIST;
use crate::store::{self, Store, Table};
use crate::{render, status};

/// Rows per transaction when `--batch` is not given.
pub const DEFAULT_BATCH: u32 = 50;

/// Every table that stores an embedding. The one-model check covers all of
/// them whatever `--table` says: another model in any one of them already
/// mixes the database's vector space.
const EMBEDDED_TABLES: [Table; 2] = [Table::Semantic, Table::Episodic];

#[derive(Debug, Clone)]
pub struct Args {
    /// The tables to fill, in this order.
    pub tables: Vec<Table>,
    /// At most this many rows in total, across the tables.
    pub limit: Option<u32>,
    /// Rows per transaction.
    pub batch: u32,
}

/// What one table's run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Counts {
    pub table: Table,
    pub embedded: u64,
    /// Rows whose embed failed. They keep no embedding, so they are counted in
    /// `remaining` too.
    pub failed: Vec<i64>,
    /// Rows another process embedded, superseded or retired between selection
    /// and update. Left exactly as they are.
    pub skipped: u64,
    /// Active rows of the table still without an embedding after the run.
    pub remaining: u64,
}

/// A newly embedded row within the dedup distance of another current row.
#[derive(Debug, Clone, PartialEq)]
pub struct NearDuplicate {
    pub table: Table,
    /// The row this run embedded.
    pub id: i64,
    pub other: i64,
    pub dist: f64,
}

/// What became of `EMBEDDINGS` in the `.env`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Switch {
    /// The `.env` does not say `EMBEDDINGS=off`: there is nothing to switch.
    Untouched,
    /// `EMBEDDINGS=off` in this file was set to `on`.
    On(PathBuf),
    /// `EMBEDDINGS=off` stays, because these active rows still lack a vector.
    StaysOff(PathBuf, Vec<(&'static str, u64)>),
}

#[derive(Debug)]
pub struct Report {
    pub tables: Vec<Counts>,
    pub near: Vec<NearDuplicate>,
    pub switch: Switch,
    /// Why the command exits non-zero after printing the report: a row that
    /// could not be embedded, or Ollama going away mid-run.
    pub failure: Option<String>,
    /// A stderr line when the real environment keeps `EMBEDDINGS=off`.
    pub warning: Option<String>,
}

impl Report {
    /// The summary, for stdout. "None" is said rather than left blank, so an
    /// empty report never reads like a report that was cut off.
    pub fn lines(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .tables
            .iter()
            .map(|c| {
                format!(
                    "{}: embedded {}, failed {}, skipped {}, remaining {}",
                    c.table.as_str(),
                    c.embedded,
                    c.failed.len(),
                    c.skipped,
                    c.remaining
                )
            })
            .collect();
        let below = render::format_real(DUP_DIST);
        if self.near.is_empty() {
            out.push(format!(
                "near-duplicate candidates (cosine < {below}): none"
            ));
        } else {
            out.push(format!(
                "near-duplicate candidates (cosine < {below}; listed only, the next sleep pass decides):"
            ));
            for n in &self.near {
                out.push(format!(
                    "  {} {} ~ {} (cosine {})",
                    n.table.as_str(),
                    n.id,
                    n.other,
                    render::format_real(n.dist)
                ));
            }
        }
        match &self.switch {
            Switch::Untouched => {}
            Switch::On(path) => out.push(format!(
                "EMBEDDINGS=on written to {}: no active row lacks an embedding",
                path.display()
            )),
            Switch::StaysOff(path, left) => {
                let left: Vec<String> = left.iter().map(|(t, n)| format!("{t} {n}")).collect();
                out.push(format!(
                    "EMBEDDINGS=off kept in {}: active rows still lack an embedding ({})",
                    path.display(),
                    left.join(", ")
                ));
            }
        }
        out
    }
}

pub fn run(cfg: &Config, args: &Args) -> Result<()> {
    let report = fill(cfg, args, &Ollama::new(cfg))?;
    for line in report.lines() {
        println!("{line}");
    }
    if let Some(warning) = &report.warning {
        eprintln!("{warning}");
    }
    match report.failure {
        Some(why) => Err(Error::failed(why)),
        None => Ok(()),
    }
}

/// Fill the missing embeddings with the given embedder; everything but the
/// printing of the summary.
pub fn fill(cfg: &Config, args: &Args, embedder: &dyn Embedder) -> Result<Report> {
    let store = store::open(cfg)?;

    // Asked directly, not through `availability`, which answers EMBEDDINGS=off
    // without asking -- and that is the very state this command exists to end.
    if let Err(why) = embedder.probe() {
        return Err(Error::refused(format!(
            "embed --missing needs Ollama at {} with {} pulled — nothing was changed ({why})",
            cfg.ollama_url, cfg.embed_model
        )));
    }
    for table in EMBEDDED_TABLES {
        if store.rows_with_other_embed_model(table, &cfg.embed_model)? > 0 {
            return Err(Error::refused(format!(
                "{}_memory has rows embedded with a different model than '{}' — nothing was \
                 changed. Filling in the rest with it would mix vector spaces, and cosine \
                 across two is meaningless: re-embed the DB (rebuild) before switching models.",
                table.as_str(),
                cfg.embed_model
            )));
        }
    }

    let mut left = args.limit;
    let mut tables = Vec::new();
    let mut near: Vec<NearDuplicate> = Vec::new();
    let mut stopped = None;
    for &table in &args.tables {
        let mut counts = Counts {
            table,
            embedded: 0,
            failed: Vec::new(),
            skipped: 0,
            remaining: 0,
        };
        let mut filled: Vec<i64> = Vec::new();
        let mut after = 0;
        while stopped.is_none() {
            let take = left.map_or(args.batch, |n| n.min(args.batch));
            if take == 0 {
                break;
            }
            let batch = store.unembedded_rows(table, after, take)?;
            let Some(&(last, _)) = batch.last() else {
                break;
            };
            after = last;
            if let Some(n) = left.as_mut() {
                *n -= batch.len() as u32;
            }

            // Embedded before the transaction opens, so a slow Ollama never
            // holds the write lock other sessions are waiting for.
            let failed_before = counts.failed.len();
            let mut vectors = Vec::with_capacity(batch.len());
            for (id, text) in batch {
                match embedder.embed(&text) {
                    Ok(v) => vectors.push((id, v)),
                    Err(Error::Failed(why)) => {
                        eprintln!("{} {id}: not embedded — {why}", table.as_str());
                        counts.failed.push(id);
                        // A row can fail on its own, and the run goes on past
                        // it; with Ollama gone every further row would fail too.
                        if let Err(gone) = embedder.probe() {
                            stopped = Some(gone);
                            break;
                        }
                    }
                    // A refusal (a model of the wrong dimension) stops the run.
                    Err(e) => return Err(e),
                }
            }

            let written = store.fill_embeddings(table, &cfg.embed_model, &vectors)?;
            counts.embedded += written.len() as u64;
            counts.skipped += (vectors.len() - written.len()) as u64;
            filled.extend(&written);
            eprintln!(
                "{}: batch committed — {} embedded, {} skipped, {} failed",
                table.as_str(),
                written.len(),
                vectors.len() - written.len(),
                counts.failed.len() - failed_before
            );

            // Semantic rows only: episodic events recur legitimately.
            if table == Table::Semantic {
                for (id, v) in vectors.iter().filter(|(id, _)| written.contains(id)) {
                    for (other, dist) in store.semantic_within(*id, v, DUP_DIST)? {
                        // A pair this run filled in both halves of is listed
                        // once, under the later id, whatever the batching.
                        if other > *id && filled.contains(&other) {
                            continue;
                        }
                        near.push(NearDuplicate {
                            table,
                            id: *id,
                            other,
                            dist,
                        });
                    }
                }
            }
        }
        tables.push(counts);
        if stopped.is_some() {
            break;
        }
    }

    // Counted in every table, not only the ones this run filled: EMBEDDINGS
    // goes back on only when no active row anywhere lacks a vector.
    let unembedded = status::unembedded(&store)?;
    for c in &mut tables {
        c.remaining = unembedded
            .iter()
            .find(|(t, _)| *t == c.table.as_str())
            .map_or(0, |(_, n)| *n);
    }
    let complete = unembedded.iter().all(|(_, n)| *n == 0);

    let switch = if !embeddings_off_in(&cfg.dotenv_path)? {
        Switch::Untouched
    } else if complete {
        dotenv::set_keys(
            &cfg.dotenv_path,
            &[("EMBEDDINGS", "on")],
            OnConflict::Overwrite,
        )?;
        Switch::On(cfg.dotenv_path.clone())
    } else {
        Switch::StaysOff(cfg.dotenv_path.clone(), unembedded)
    };

    // The real environment overrides the `.env`, so switching the file does
    // not switch this setting.
    let off_in_env = cfg
        .setting("EMBEDDINGS")
        .is_some_and(|s| s.source == Source::Env && s.value.as_deref() == Some("off"));
    let warning = (complete && off_in_env).then(|| {
        "warning: EMBEDDINGS=off is set in the environment, which overrides the .env — \
         unset it there, or recall stays keyword-only"
            .to_string()
    });

    let failed: Vec<String> = tables
        .iter()
        .flat_map(|c| {
            c.failed
                .iter()
                .map(|id| format!("{} {id}", c.table.as_str()))
        })
        .collect();
    let failure = match stopped {
        Some(gone) => Some(format!(
            "Ollama stopped answering mid-run ({gone}); the committed batches are kept — \
             run embed --missing again to continue"
        )),
        None if !failed.is_empty() => Some(format!(
            "could not embed {}; they keep no embedding — run embed --missing again to retry \
             them",
            failed.join(", ")
        )),
        None => None,
    };

    Ok(Report {
        tables,
        near,
        switch,
        failure,
        warning,
    })
}

/// Whether the `.env` says `EMBEDDINGS=off`. The file is read afresh, because
/// it is the file that gets switched: with no such line there is nothing to
/// undo, and no `.env` is created to say what the default already says.
fn embeddings_off_in(path: &Path) -> Result<bool> {
    Ok(dotenv::read(path)?.is_some_and(|entries| {
        entries
            .iter()
            .any(|e| e.key == "EMBEDDINGS" && e.value.trim().eq_ignore_ascii_case("off"))
    }))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::embed::Unavailable;
    use crate::embed::fake::Fake;
    use crate::error::EXIT_REFUSED;
    use crate::recall;
    use crate::store::Cell;
    use crate::store::turso::TursoStore;
    use crate::store::turso::tests::{distance, put, rows, scratch, set_vector};

    fn args(tables: &[Table], limit: Option<u32>, batch: u32) -> Args {
        Args {
            tables: tables.to_vec(),
            limit,
            batch,
        }
    }

    fn all(batch: u32) -> Args {
        args(&[Table::Semantic, Table::Episodic], None, batch)
    }

    /// Ids of the rows of a table that have no embedding, superseded and
    /// retired ones included.
    fn null_ids(cfg: &Config, table: Table) -> Vec<i64> {
        rows(
            cfg,
            &format!(
                "SELECT id FROM {}_memory WHERE embedding IS NULL ORDER BY id;",
                table.as_str()
            ),
        )
        .into_iter()
        .map(|r| match r[0] {
            Cell::Int(id) => id,
            ref other => panic!("{other:?}"),
        })
        .collect()
    }

    /// Every column of every memory row, the vector as hex: what "the database
    /// is unchanged" means.
    fn dump(cfg: &Config) -> Vec<Vec<Cell>> {
        let mut out = rows(
            cfg,
            "SELECT id, created_at, updated_at, project, topic, category, source, model, \
             embed_model, memory_text, file_reference, hex(embedding), superseded_by, \
             retired_at FROM semantic_memory ORDER BY id;",
        );
        out.extend(rows(
            cfg,
            "SELECT id, created_at, project, topic, event_type, importance, source, model, \
             embed_model, memory_text, file_reference, hex(embedding), processed_at \
             FROM episodic_memory ORDER BY id;",
        ));
        out
    }

    fn counts(
        table: Table,
        embedded: u64,
        failed: Vec<i64>,
        skipped: u64,
        remaining: u64,
    ) -> Counts {
        Counts {
            table,
            embedded,
            failed,
            skipped,
            remaining,
        }
    }

    fn refused(r: Result<Report>) -> String {
        let e = r.expect_err("must be refused");
        assert_eq!(e.exit_code(), EXIT_REFUSED, "{e}");
        let msg = e.to_string();
        assert!(msg.starts_with("refused: "), "{msg}");
        msg
    }

    /// A `.env` beside the scratch database, holding `text`, as the one this
    /// configuration was read from.
    fn with_dotenv(cfg: &mut Config, text: &str) -> PathBuf {
        let path = cfg.db_path.with_file_name(".env");
        fs::write(&path, text).unwrap();
        cfg.dotenv_path = path.clone();
        cfg.embeddings = !text.contains("EMBEDDINGS=off");
        path
    }

    /// Answers a probe, given how many embeds came before it.
    type ProbeFn<'a> = Box<dyn Fn(usize) -> std::result::Result<(), Unavailable> + 'a>;
    /// Answers an embed, given the call's number (from 1) and the text.
    type EmbedFn<'a> = Box<dyn Fn(usize, &str) -> Result<Vec<f32>> + 'a>;

    /// An embedder a test scripts.
    struct Scripted<'a> {
        embeds: std::cell::Cell<usize>,
        probe: ProbeFn<'a>,
        embed: EmbedFn<'a>,
    }

    impl Embedder for Scripted<'_> {
        fn probe(&self) -> std::result::Result<(), Unavailable> {
            (self.probe)(self.embeds.get())
        }

        fn embed(&self, text: &str) -> Result<Vec<f32>> {
            self.embeds.set(self.embeds.get() + 1);
            (self.embed)(self.embeds.get(), text)
        }
    }

    fn gone() -> Unavailable {
        Unavailable::Unreachable {
            url: "http://127.0.0.1:9".into(),
        }
    }

    /// Ollama answering the first `n` embeds, then gone for good.
    fn dies_after(n: usize) -> Scripted<'static> {
        Scripted {
            embeds: std::cell::Cell::new(0),
            probe: Box::new(move |done| if done > n { Err(gone()) } else { Ok(()) }),
            embed: Box::new(move |call, text| {
                if call > n {
                    Err(Error::failed("ollama at http://127.0.0.1:9 is unreachable"))
                } else {
                    Ok(Fake::vector(text))
                }
            }),
        }
    }

    /// Ollama up throughout; `embed` decides per text.
    fn scripted<'a>(embed: impl Fn(&str) -> Result<Vec<f32>> + 'a) -> Scripted<'a> {
        Scripted {
            embeds: std::cell::Cell::new(0),
            probe: Box::new(|_| Ok(())),
            embed: Box::new(move |_, text| embed(text)),
        }
    }

    // ---- refusals: exit 2, `refused:`, nothing changed ---------------------

    #[test]
    fn ollama_unreachable_is_refused_and_changes_nothing() {
        let cfg = scratch("embed-unreachable");
        put(&cfg, Table::Semantic, "blind", false);
        let before = dump(&cfg);
        let msg = refused(fill(&cfg, &all(10), &Fake::down(gone())));
        assert!(
            msg.contains(&format!(
                "embed --missing needs Ollama at {} with {} pulled — nothing was changed",
                cfg.ollama_url, cfg.embed_model
            )),
            "{msg}"
        );
        assert!(msg.contains("Ollama unreachable"), "{msg}");
        assert_eq!(dump(&cfg), before);
    }

    #[test]
    fn a_model_not_pulled_is_refused_and_changes_nothing() {
        let cfg = scratch("embed-not-pulled");
        put(&cfg, Table::Episodic, "blind", false);
        let before = dump(&cfg);
        let fake = Fake::down(Unavailable::NotPulled {
            url: cfg.ollama_url.clone(),
            model: cfg.embed_model.clone(),
        });
        let msg = refused(fill(&cfg, &all(10), &fake));
        assert!(msg.contains("nothing was changed"), "{msg}");
        assert!(msg.contains("ollama pull bge-m3"), "{msg}");
        assert_eq!(dump(&cfg), before);
    }

    /// One model per database ACROSS tables: a row embedded with another model
    /// anywhere refuses the run, whatever `--table` says.
    #[test]
    fn a_db_holding_another_model_is_refused_and_changes_nothing() {
        let cfg = scratch("embed-other-model");
        put(&cfg, Table::Semantic, "blind", false);
        let e = put(&cfg, Table::Episodic, "embedded event", true);
        rows(
            &cfg,
            &format!("UPDATE episodic_memory SET embed_model = 'nomic-embed-text' WHERE id = {e};"),
        );
        let before = dump(&cfg);
        let fake = Fake::up();
        let msg = refused(fill(&cfg, &args(&[Table::Semantic], None, 10), &fake));
        assert!(msg.contains("episodic_memory"), "{msg}");
        assert!(msg.contains("'bge-m3'"), "{msg}");
        assert!(msg.contains("nothing was changed"), "{msg}");
        assert_eq!(dump(&cfg), before);
        assert_eq!(fake.calls.get(), 1, "probed, never embedded");
    }

    /// A missing database is refused as everywhere else, and none is created.
    #[test]
    fn a_missing_database_is_refused_and_none_is_created() {
        let mut cfg = scratch("embed-no-db");
        cfg.db_path = cfg.db_path.with_file_name("absent.db");
        let msg = refused(fill(&cfg, &all(10), &Fake::up()));
        assert!(msg.contains("memory DB missing"), "{msg}");
        assert!(!cfg.db_path.exists());
    }

    // ---- what gets embedded ------------------------------------------------

    /// The vector comes from the stored text, keywords included -- exactly what
    /// `remember` would have embedded -- and is recorded with the configured
    /// model.
    #[test]
    fn every_active_row_is_embedded_from_its_stored_text() {
        let cfg = scratch("embed-fills");
        let text = "turso needs the wal flag\n\nKeywords: turso, wal";
        let s = put(&cfg, Table::Semantic, text, false);
        let e = put(&cfg, Table::Episodic, "an event", false);
        let report = fill(&cfg, &all(10), &Fake::up()).unwrap();

        assert_eq!(
            report.tables,
            vec![
                counts(Table::Semantic, 1, vec![], 0, 0),
                counts(Table::Episodic, 1, vec![], 0, 0),
            ]
        );
        assert!(report.failure.is_none());
        assert!(null_ids(&cfg, Table::Semantic).is_empty());
        assert!(null_ids(&cfg, Table::Episodic).is_empty());
        assert!(distance(&cfg, Table::Semantic, s, &Fake::vector(text)) < 1e-6);
        assert!(
            distance(
                &cfg,
                Table::Semantic,
                s,
                &Fake::vector("turso needs the wal flag")
            ) > 0.01,
            "the keywords are part of what is embedded"
        );
        assert!(distance(&cfg, Table::Episodic, e, &Fake::vector("an event")) < 1e-6);
        assert_eq!(
            rows(&cfg, "SELECT DISTINCT embed_model FROM semantic_memory;"),
            vec![vec![Cell::Text("bge-m3".into())]]
        );
    }

    #[test]
    fn superseded_and_retired_rows_are_neither_embedded_nor_counted() {
        let cfg = scratch("embed-active-only");
        let current = put(&cfg, Table::Semantic, "current", false);
        let superseded = put(&cfg, Table::Semantic, "superseded", false);
        let retired = put(&cfg, Table::Semantic, "retired", false);
        rows(
            &cfg,
            &format!(
                "UPDATE semantic_memory SET superseded_by = {current} WHERE id = {superseded};"
            ),
        );
        rows(
            &cfg,
            &format!("UPDATE semantic_memory SET retired_at = '2026-01-02' WHERE id = {retired};"),
        );
        let report = fill(&cfg, &all(10), &Fake::up()).unwrap();
        assert_eq!(null_ids(&cfg, Table::Semantic), vec![superseded, retired]);
        assert_eq!(report.tables[0], counts(Table::Semantic, 1, vec![], 0, 0));
    }

    /// `EMBEDDINGS=off` is the state this command exists to end, so its own
    /// probe decides, not the setting.
    #[test]
    fn embeddings_off_still_asks_ollama_and_runs() {
        let mut cfg = scratch("embed-off-runs");
        cfg.embeddings = false;
        put(&cfg, Table::Semantic, "blind", false);
        let fake = Fake::up();
        fill(&cfg, &all(10), &fake).unwrap();
        assert!(
            fake.calls.get() >= 2,
            "probe and embed: {}",
            fake.calls.get()
        );
        assert!(null_ids(&cfg, Table::Semantic).is_empty());
    }

    #[test]
    fn limit_bounds_the_count_exactly_across_tables() {
        let cfg = scratch("embed-limit");
        for i in 0..2 {
            put(&cfg, Table::Semantic, &format!("fact {i}"), false);
        }
        for i in 0..3 {
            put(&cfg, Table::Episodic, &format!("event {i}"), false);
        }
        let report = fill(
            &cfg,
            &args(&[Table::Semantic, Table::Episodic], Some(3), 2),
            &Fake::up(),
        )
        .unwrap();
        assert_eq!(
            report.tables,
            vec![
                counts(Table::Semantic, 2, vec![], 0, 0),
                counts(Table::Episodic, 1, vec![], 0, 2),
            ]
        );
        assert_eq!(null_ids(&cfg, Table::Episodic).len(), 2);
    }

    #[test]
    fn table_scopes_the_run() {
        let cfg = scratch("embed-table");
        let s = put(&cfg, Table::Semantic, "blind fact", false);
        put(&cfg, Table::Episodic, "blind event", false);
        let report = fill(&cfg, &args(&[Table::Episodic], None, 10), &Fake::up()).unwrap();
        assert_eq!(null_ids(&cfg, Table::Semantic), vec![s]);
        assert!(null_ids(&cfg, Table::Episodic).is_empty());
        assert_eq!(
            report.tables,
            vec![counts(Table::Episodic, 1, vec![], 0, 0)]
        );
    }

    // ---- idempotent, resumable, concurrent ---------------------------------

    #[test]
    fn a_second_run_embeds_nothing_and_changes_nothing() {
        let mut cfg = scratch("embed-idempotent");
        let env = with_dotenv(&mut cfg, "EMBEDDINGS=off\n");
        for i in 0..3 {
            put(&cfg, Table::Semantic, &format!("fact {i}"), false);
        }
        put(&cfg, Table::Episodic, "event", false);
        fill(&cfg, &all(2), &Fake::up()).unwrap();
        let (db, dotenv) = (dump(&cfg), fs::read(&env).unwrap());
        let files = |cfg: &Config| -> Vec<Option<Vec<u8>>> {
            ["", "-wal"]
                .iter()
                .map(|s| fs::read(format!("{}{s}", cfg.db_path.display())).ok())
                .collect()
        };
        let bytes = files(&cfg);

        let fake = Fake::up();
        let again = fill(&cfg, &all(2), &fake).unwrap();
        assert_eq!(fake.calls.get(), 1, "the probe, and no embed");
        assert_eq!(
            again.tables,
            vec![
                counts(Table::Semantic, 0, vec![], 0, 0),
                counts(Table::Episodic, 0, vec![], 0, 0),
            ]
        );
        assert!(again.near.is_empty());
        assert!(again.failure.is_none());
        assert_eq!(again.switch, Switch::Untouched, "already on");
        assert_eq!(dump(&cfg), db);
        assert_eq!(files(&cfg), bytes, "the database files, byte for byte");
        assert_eq!(fs::read(&env).unwrap(), dotenv);
    }

    /// Ollama goes away after the first batch: that batch stays committed, the
    /// rest stays NULL, and the run says so and exits non-zero. A re-run
    /// completes them.
    #[test]
    fn a_failure_after_the_first_batch_keeps_it_and_a_rerun_completes_the_rest() {
        let cfg = scratch("embed-resume");
        let ids: Vec<i64> = (0..5)
            .map(|i| put(&cfg, Table::Semantic, &format!("fact {i}"), false))
            .collect();
        put(&cfg, Table::Episodic, "event", false);

        let report = fill(&cfg, &all(2), &dies_after(2)).expect("committed work is reported");
        assert_eq!(null_ids(&cfg, Table::Semantic), ids[2..].to_vec());
        assert_eq!(
            report.tables,
            vec![counts(Table::Semantic, 2, vec![ids[2]], 0, 3)],
            "a dead Ollama stops the run; the next table is not attempted"
        );
        let why = report.failure.expect("a stopped run must not exit 0");
        assert!(why.contains("Ollama stopped answering"), "{why}");
        assert!(why.contains("run embed --missing again"), "{why}");
        assert_eq!(null_ids(&cfg, Table::Episodic).len(), 1);

        let again = fill(&cfg, &all(2), &Fake::up()).unwrap();
        assert!(again.failure.is_none());
        assert!(null_ids(&cfg, Table::Semantic).is_empty());
        assert!(null_ids(&cfg, Table::Episodic).is_empty());
        assert_eq!(again.tables[0], counts(Table::Semantic, 3, vec![], 0, 0));
    }

    /// A row that fails on its own while Ollama still answers is counted and
    /// left NULL, and the run carries on past it -- otherwise one bad row would
    /// stop every later run at the same place.
    #[test]
    fn a_row_that_fails_alone_is_counted_and_the_run_goes_on() {
        let cfg = scratch("embed-poison");
        put(&cfg, Table::Semantic, "fine a", false);
        let bad = put(&cfg, Table::Semantic, "poison", false);
        put(&cfg, Table::Semantic, "fine b", false);
        let embedder = scripted(|text| {
            if text == "poison" {
                Err(Error::failed("ollama returned 500"))
            } else {
                Ok(Fake::vector(text))
            }
        });
        let report = fill(&cfg, &all(1), &embedder).unwrap();
        assert_eq!(null_ids(&cfg, Table::Semantic), vec![bad]);
        assert_eq!(
            report.tables[0],
            counts(Table::Semantic, 2, vec![bad], 0, 1)
        );
        let why = report.failure.expect("a failed row must not exit 0");
        assert!(why.contains(&format!("semantic {bad}")), "{why}");
    }

    /// Another process embeds a row after this run selected it and before it
    /// wrote: the row is skipped, its vector untouched.
    #[test]
    fn a_row_embedded_concurrently_is_skipped_not_overwritten() {
        let cfg = scratch("embed-concurrent");
        put(&cfg, Table::Semantic, "blind a", false);
        let b = put(&cfg, Table::Semantic, "blind b", false);
        let embedder = scripted(|text| {
            if text == "blind b" {
                set_vector(
                    &cfg,
                    Table::Semantic,
                    b,
                    &Fake::vector("elsewhere"),
                    "bge-m3",
                );
            }
            Ok(Fake::vector(text))
        });
        let report = fill(&cfg, &all(10), &embedder).unwrap();
        assert_eq!(report.tables[0], counts(Table::Semantic, 1, vec![], 1, 0));
        assert!(
            distance(&cfg, Table::Semantic, b, &Fake::vector("elsewhere")) < 1e-6,
            "never overwritten"
        );
        assert!(report.failure.is_none(), "a skipped row is not a failure");
    }

    // ---- near-duplicates ---------------------------------------------------

    /// A newly embedded semantic row within the dedup distance of another
    /// current row is listed -- and nothing is superseded or retired: the next
    /// sleep pass decides.
    #[test]
    fn a_near_duplicate_is_listed_and_nothing_is_acted_on() {
        let cfg = scratch("embed-near");
        let old = put(&cfg, Table::Semantic, "turso needs the wal flag", true);
        put(&cfg, Table::Semantic, "something else entirely", true);
        let twin = put(&cfg, Table::Semantic, "turso needs the wal flag", false);
        put(&cfg, Table::Semantic, "a fact of its own", false);
        let report = fill(&cfg, &all(10), &Fake::up()).unwrap();

        assert_eq!(report.near.len(), 1, "{:?}", report.near);
        let n = &report.near[0];
        assert_eq!((n.table, n.id, n.other), (Table::Semantic, twin, old));
        assert!(n.dist < DUP_DIST, "{n:?}");
        assert_eq!(
            rows(
                &cfg,
                "SELECT count(*) FROM semantic_memory \
                 WHERE superseded_by IS NOT NULL OR retired_at IS NOT NULL;"
            ),
            vec![vec![Cell::Int(0)]]
        );
    }

    /// Two rows filled in by the same run that duplicate each other are one
    /// pair, listed once. Episodic rows recur legitimately and are not compared.
    #[test]
    fn a_pair_embedded_in_one_run_is_listed_once_and_episodic_rows_never() {
        let cfg = scratch("embed-near-pair");
        let a = put(&cfg, Table::Semantic, "twin", false);
        let b = put(&cfg, Table::Semantic, "twin", false);
        put(&cfg, Table::Episodic, "the same event", false);
        put(&cfg, Table::Episodic, "the same event", false);
        let report = fill(&cfg, &all(10), &Fake::up()).unwrap();
        let pairs: Vec<(i64, i64)> = report.near.iter().map(|n| (n.id, n.other)).collect();
        assert_eq!(pairs, vec![(b, a)]);
    }

    // ---- recall sees the result --------------------------------------------

    #[test]
    fn after_a_run_recall_ranks_the_row_in_the_main_list() {
        let cfg = scratch("embed-recall");
        put(
            &cfg,
            Table::Semantic,
            "ollama serves the bge-m3 model",
            true,
        );
        put(&cfg, Table::Semantic, "zsh does not word-split", true);
        put(
            &cfg,
            Table::Semantic,
            "the wal flag, stored without a vector",
            false,
        );
        let search = || {
            let args = recall::Args {
                query: None,
                table: recall::TableArg::Semantic,
                project: None,
                k: 10,
                coworker: None,
                baseline: false,
                topics: false,
                candidates: false,
                count: false,
            };
            recall::search(
                &cfg,
                &TursoStore::open(&cfg).unwrap(),
                &Fake::up(),
                &args,
                "wal flag",
            )
            .unwrap()
        };
        let before = search();
        assert!(
            before.contains("1 without embedding (keyword-only)"),
            "{before}"
        );

        fill(&cfg, &all(10), &Fake::up()).unwrap();
        let after = search();
        assert!(!after.contains("without embedding"), "{after}");
        assert_eq!(after.lines().filter(|l| l.starts_with("=====")).count(), 1);
        assert!(
            after.contains("memory_text = the wal flag, stored without a vector"),
            "{after}"
        );
    }

    // ---- EMBEDDINGS back on ------------------------------------------------

    #[test]
    fn a_run_that_leaves_nothing_unembedded_switches_embeddings_on() {
        let mut cfg = scratch("embed-switch-on");
        let env = with_dotenv(
            &mut cfg,
            "# written by init\nEMBEDDINGS=off  # no Ollama yet\nBACKUP_DIR=/b\n",
        );
        put(&cfg, Table::Semantic, "blind", false);
        put(&cfg, Table::Episodic, "blind", false);
        let report = fill(&cfg, &all(10), &Fake::up()).unwrap();
        assert_eq!(report.switch, Switch::On(env.clone()));
        assert_eq!(
            fs::read_to_string(&env).unwrap(),
            "# written by init\nEMBEDDINGS=on  # no Ollama yet\nBACKUP_DIR=/b\n"
        );
    }

    #[test]
    fn a_limited_run_that_leaves_rows_keeps_embeddings_off() {
        let mut cfg = scratch("embed-switch-limit");
        let env = with_dotenv(&mut cfg, "EMBEDDINGS=off\n");
        put(&cfg, Table::Semantic, "one", false);
        put(&cfg, Table::Semantic, "two", false);
        let report = fill(
            &cfg,
            &args(&[Table::Semantic, Table::Episodic], Some(1), 10),
            &Fake::up(),
        )
        .unwrap();
        assert_eq!(
            report.switch,
            Switch::StaysOff(env.clone(), vec![("semantic", 1), ("episodic", 0)])
        );
        assert_eq!(fs::read_to_string(&env).unwrap(), "EMBEDDINGS=off\n");
    }

    /// The switch counts every table, not only the ones this run filled.
    #[test]
    fn rows_left_in_a_table_outside_the_run_keep_embeddings_off() {
        let mut cfg = scratch("embed-switch-scope");
        let env = with_dotenv(&mut cfg, "EMBEDDINGS=off\n");
        put(&cfg, Table::Semantic, "fact", false);
        put(&cfg, Table::Episodic, "event", false);
        let report = fill(&cfg, &args(&[Table::Semantic], None, 10), &Fake::up()).unwrap();
        assert_eq!(
            report.switch,
            Switch::StaysOff(env.clone(), vec![("semantic", 0), ("episodic", 1)])
        );
        assert_eq!(fs::read_to_string(&env).unwrap(), "EMBEDDINGS=off\n");
    }

    /// A superseded row without a vector is not active: it is not embedded,
    /// not counted, and does not hold the switch back.
    #[test]
    fn a_superseded_row_without_a_vector_does_not_keep_embeddings_off() {
        let mut cfg = scratch("embed-switch-superseded");
        let env = with_dotenv(&mut cfg, "EMBEDDINGS=off\n");
        let current = put(&cfg, Table::Semantic, "current", false);
        let old = put(&cfg, Table::Semantic, "old", false);
        rows(
            &cfg,
            &format!("UPDATE semantic_memory SET superseded_by = {current} WHERE id = {old};"),
        );
        let report = fill(&cfg, &all(10), &Fake::up()).unwrap();
        assert_eq!(report.switch, Switch::On(env.clone()));
        assert_eq!(null_ids(&cfg, Table::Semantic), vec![old]);
        assert_eq!(report.tables[0], counts(Table::Semantic, 1, vec![], 0, 0));
        assert_eq!(fs::read_to_string(&env).unwrap(), "EMBEDDINGS=on\n");
    }

    /// Without `EMBEDDINGS=off` in the `.env` there is nothing to switch: no
    /// file is created, and an existing one is left byte for byte.
    #[test]
    fn without_embeddings_off_in_the_dotenv_no_file_is_written() {
        let mut cfg = scratch("embed-switch-none");
        cfg.dotenv_path = cfg.db_path.with_file_name(".env");
        put(&cfg, Table::Semantic, "blind", false);
        let report = fill(&cfg, &all(10), &Fake::up()).unwrap();
        assert_eq!(report.switch, Switch::Untouched);
        assert!(!cfg.dotenv_path.exists());

        let env = with_dotenv(&mut cfg, "BACKUP_DIR=/b\n");
        put(&cfg, Table::Semantic, "another", false);
        let report = fill(&cfg, &all(10), &Fake::up()).unwrap();
        assert_eq!(report.switch, Switch::Untouched);
        assert_eq!(fs::read_to_string(&env).unwrap(), "BACKUP_DIR=/b\n");
    }

    /// The real environment overrides the `.env`, so `EMBEDDINGS=off` there
    /// keeps every run keyword-only however the file reads -- said on stderr.
    #[test]
    fn embeddings_off_from_the_environment_is_warned_about() {
        let mut cfg = scratch("embed-switch-env");
        let env = with_dotenv(&mut cfg, "EMBEDDINGS=off\n");
        let setting = cfg
            .settings
            .iter_mut()
            .find(|s| s.key == "EMBEDDINGS")
            .unwrap();
        setting.source = Source::Env;
        setting.value = Some("off".into());
        put(&cfg, Table::Semantic, "blind", false);
        let report = fill(&cfg, &all(10), &Fake::up()).unwrap();
        assert_eq!(report.switch, Switch::On(env));
        let warning = report.warning.expect("the override must be named");
        assert!(warning.starts_with("warning: "), "{warning}");
        assert!(warning.contains("environment"), "{warning}");
    }

    // ---- the summary ---------------------------------------------------------

    #[test]
    fn the_summary_names_every_count_the_near_duplicates_and_the_switch() {
        let report = Report {
            tables: vec![
                counts(Table::Semantic, 3, vec![7], 1, 1),
                counts(Table::Episodic, 2, vec![], 0, 0),
            ],
            near: vec![NearDuplicate {
                table: Table::Semantic,
                id: 12,
                other: 4,
                dist: 0.0312,
            }],
            switch: Switch::On(PathBuf::from("/opt/sm/.env")),
            failure: None,
            warning: None,
        };
        assert_eq!(
            report.lines(),
            vec![
                "semantic: embedded 3, failed 1, skipped 1, remaining 1",
                "episodic: embedded 2, failed 0, skipped 0, remaining 0",
                "near-duplicate candidates (cosine < 0.1; listed only, the next sleep pass decides):",
                "  semantic 12 ~ 4 (cosine 0.0312)",
                "EMBEDDINGS=on written to /opt/sm/.env: no active row lacks an embedding",
            ]
        );
    }

    /// No near-duplicates and rows left over are both said, never left blank.
    #[test]
    fn the_summary_says_none_and_why_embeddings_stays_off() {
        let report = Report {
            tables: vec![counts(Table::Semantic, 1, vec![], 0, 1)],
            near: vec![],
            switch: Switch::StaysOff(
                PathBuf::from("/opt/sm/.env"),
                vec![("semantic", 1), ("episodic", 0)],
            ),
            failure: None,
            warning: None,
        };
        let lines = report.lines();
        assert_eq!(
            lines[1..],
            [
                "near-duplicate candidates (cosine < 0.1): none",
                "EMBEDDINGS=off kept in /opt/sm/.env: active rows still lack an embedding \
                 (semantic 1, episodic 0)",
            ]
        );
    }
}
