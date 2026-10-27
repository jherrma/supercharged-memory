use std::collections::HashMap;
use std::collections::hash_map::Entry;

use crate::config::Config;
use crate::embed::{self, Embedder};
use crate::error::{Error, Result};
use crate::eval::Output;
use crate::eval::cache::QueryVectors;
use crate::eval::rank::{self, Candidate};
use crate::eval::score::{self, Judged, Scored};
use crate::json::python_float;
use crate::store::{EvalCase, EvalRun, Store, Table};

pub enum Scoring<'a> {
    Report,
    Sweep(&'a [f64]),
    Variants,
}

/// The alphas `--variants` shows the shipped blend at.
const VARIANT_ALPHAS: [f64; 3] = [0.10, 0.15, 0.20];

/// The rows of a query's distances that have a text, in id order.
fn candidates<'a>(dists: &[(i64, f64)], texts: &HashMap<i64, &'a str>) -> Vec<Candidate<'a>> {
    dists
        .iter()
        .filter_map(|&(id, dist)| texts.get(&id).map(|&text| Candidate { id, dist, text }))
        .collect()
}

/// Score the cases and print what the mode asks for.
///
/// Refuses, before anything is embedded or written, when there is nothing to
/// score, no way to embed, or a target that no vector search could ever rank:
/// that one would read as a regression.
pub fn measure(
    cfg: &Config,
    store: &impl Store,
    embedder: &dyn Embedder,
    what: Scoring,
) -> Result<Output> {
    let cases = store.eval_cases()?;
    if cases.is_empty() {
        return Err(Error::refused(
            "no eval cases in the database: eval_cases holds no live case, so there is no \
             metric to compute. Author cases with `eval --add-case`, or migrate a legacy file \
             set with `eval --import <dir>`. Do NOT generate them from memory_text: a query \
             written from the row it should retrieve biases every future alpha.",
        ));
    }
    if let Err(why) = embed::availability(cfg, embedder) {
        return Err(Error::refused(format!(
            "cannot score without embeddings: {why}. `eval --validate` and the case writes \
             need none and still work."
        )));
    }
    let blind = unembedded_targets(store, &cases)?;
    if !blind.is_empty() {
        return Err(Error::refused(format!(
            "these targets have no embedding, so no vector search can rank them and they \
             would read as misses: {}. Run `embed --missing` first.",
            blind.join(", ")
        )));
    }

    let (texts, dists, warnings) = prepare(cfg, store, embedder, &cases)?;
    let by_id: HashMap<Table, HashMap<i64, &str>> = texts
        .iter()
        .map(|(t, rows)| (*t, rows.iter().map(|(i, x)| (*i, x.as_str())).collect()))
        .collect();
    let judge = |ranker: &Ranker| -> Scored {
        let judged: Vec<Judged> = cases
            .iter()
            .map(|c| {
                let rows = candidates(&dists[&(c.table, c.query.clone())], &by_id[&c.table]);
                Judged {
                    id: &c.id,
                    class: &c.class,
                    query: &c.query,
                    expect: &c.expect,
                    ranked: ranker(&c.query, &rows),
                }
            })
            .collect();
        score::score(&judged)
    };
    let classes = {
        let mut c: Vec<String> = cases.iter().map(|c| c.class.clone()).collect();
        c.sort();
        c.dedup();
        c
    };

    let text = match what {
        Scoring::Report => report(cfg, store, &cases, &classes, &judge)?,
        Scoring::Sweep(alphas) => sweep(cfg, &cases, &classes, alphas, &judge),
        Scoring::Variants => variants(&classes, &judge),
    };
    Ok(Output {
        text,
        warnings,
        ..Output::default()
    })
}

/// `case (id)` for every target that is current but holds no vector. A target
/// that is not current is `--validate`'s business, not this one's.
fn unembedded_targets(store: &impl Store, cases: &[EvalCase]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for c in cases {
        for t in store.targets(c.table, &c.expect)? {
            if t.current && !t.embedded {
                out.push(format!("{} ({})", c.id, t.id));
            }
        }
    }
    Ok(out)
}

/// Orders a query's candidate rows, best first.
type Ranker<'a> = dyn Fn(&str, &[Candidate]) -> Vec<i64> + 'a;
/// Scores every case with one ranker.
type Judge<'a> = dyn Fn(&Ranker) -> Scored + 'a;

type Texts = HashMap<Table, Vec<(i64, String)>>;
type Dists = HashMap<(Table, String), Vec<(i64, f64)>>;

/// Each used table's corpus, and each distinct (table, query)'s distances to
/// it, with the query embedded once -- or not at all, when the cache has it.
fn prepare(
    cfg: &Config,
    store: &impl Store,
    embedder: &dyn Embedder,
    cases: &[EvalCase],
) -> Result<(Texts, Dists, Vec<String>)> {
    let mut texts = Texts::new();
    let mut dists = Dists::new();
    let mut cache = QueryVectors::open(&cfg.eval_dir, &cfg.embed_model);
    for c in cases {
        if let Entry::Vacant(e) = texts.entry(c.table) {
            e.insert(store.eval_corpus(c.table)?);
        }
        if let Entry::Vacant(e) = dists.entry((c.table, c.query.clone())) {
            let vector = cache.get(&c.query, embedder)?;
            e.insert(store.eval_distances(c.table, &vector)?);
        }
    }
    let mut warnings = cache.warnings.clone();
    warnings.extend(cache.save());
    Ok((texts, dists, warnings))
}

fn report(
    cfg: &Config,
    store: &impl Store,
    cases: &[EvalCase],
    classes: &[String],
    judge: &Judge,
) -> Result<String> {
    let alpha = cfg.recall_alpha;
    let n = cases.len();
    let r = judge(&|q, rows| rank::shipped(q, rows, alpha));
    let mut out = format!(
        "shipped recall @ RECALL_ALPHA={}   ({n} cases, noise floor {:.1}pp per case)\n\n",
        python_float(alpha),
        100.0 / n as f64
    );
    out.push_str(&score::header(classes));
    out.push('\n');
    out.push_str(&score::row("current", &r, classes));
    out.push('\n');

    if let Some(prev) = store.last_eval_run()? {
        let d = (r.r1 - prev.r1, r.r5 - prev.r5, r.mrr - prev.mrr);
        out.push_str(&format!(
            "\nvs {} (alpha {}, {} cases): R@1 {:+.2}  R@5 {:+.2}  MRR {:+.2}\n",
            prev.ran_at.as_deref().unwrap_or("?"),
            python_float(prev.alpha),
            prev.n_cases,
            d.0,
            d.1,
            d.2
        ));
        if prev.n_cases != n as i64 {
            out.push_str("  NOTE: case count changed — the deltas are not like-for-like.\n");
        }
        if d.1 <= -2.0 / n as f64 {
            out.push_str(
                "  REGRESSION: recall@5 dropped by more than one case. Check the eval set \
                 with `eval --validate` first\n  (purged or superseded targets look like a \
                 ranking regression), then consider `eval --sweep`.\n",
            );
        }
    }
    store.insert_eval_run(&EvalRun {
        ran_at: None,
        alpha,
        embed_model: cfg.embed_model.clone(),
        n_cases: n as i64,
        r1: r.r1,
        r5: r.r5,
        mrr: r.mrr,
    })?;
    if !r.misses.is_empty() {
        out.push_str("\nmisses @5:\n");
        for m in &r.misses {
            let top = m
                .top3
                .iter()
                .map(i64::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&format!("  {}  {:<46} top3=[{top}]\n", m.id, m.query));
        }
    }
    Ok(out)
}

fn sweep(
    cfg: &Config,
    cases: &[EvalCase],
    classes: &[String],
    alphas: &[f64],
    judge: &Judge,
) -> String {
    let mut out = score::header(classes);
    out.push('\n');
    let mut results = Vec::new();
    for &a in alphas {
        let s = judge(&|q, rows| rank::shipped(q, rows, a));
        out.push_str(&score::row(
            &format!("alpha={}", python_float(a)),
            &s,
            classes,
        ));
        out.push('\n');
        results.push((a, s));
    }
    out.push_str(&score::verdict(&results, cfg.recall_alpha, cases.len()));
    out
}

fn variants(classes: &[String], judge: &Judge) -> String {
    let mut out = score::header(classes);
    out.push('\n');
    let rows: [(&str, Scored); 3] = [
        (
            "V0_legacy_lexicographic",
            judge(&|q, rows| rank::legacy(q, rows)),
        ),
        ("V_vector_only", judge(&|_, rows| rank::vector_only(rows))),
        (
            "V1_idf_lexicographic",
            judge(&|q, rows| rank::idf_lexicographic(q, rows)),
        ),
    ];
    for (name, s) in &rows {
        out.push_str(&score::row(name, s, classes));
        out.push('\n');
    }
    for a in VARIANT_ALPHAS {
        let s = judge(&|q, rows| rank::shipped(q, rows, a));
        out.push_str(&score::row(
            &format!("V3_shipped_blend a={}", python_float(a)),
            &s,
            classes,
        ));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::candidates;
    use crate::config::Config;
    use crate::embed::Unavailable;
    use crate::embed::fake::Fake;
    use crate::error::Result;
    use crate::eval::rank;
    use crate::eval::testing::*;
    use crate::eval::{Mode, execute};
    use crate::recall::{Within, search_in};
    use crate::store::turso::TursoStore;
    use crate::store::turso::tests::{put, rows};
    use crate::store::{Cell, EvalRun, Store, Table};

    /// The ids of one table's embedded rows in the order `recall` would list them
    /// for this query and its vector, at this alpha.
    fn shipped_order(
        store: &impl Store,
        table: Table,
        query: &str,
        vector: &[f32],
        alpha: f64,
    ) -> Result<Vec<i64>> {
        let texts = store.eval_corpus(table)?;
        let dists = store.eval_distances(table, vector)?;
        let texts: HashMap<i64, &str> = texts.iter().map(|(i, t)| (*i, t.as_str())).collect();
        Ok(rank::shipped(query, &candidates(&dists, &texts), alpha))
    }

    const STAMP: &str = "2026-01-01 00:00:00";

    /// A case whose query is the text of a fact, so the fake embedder puts the
    /// query exactly on that row.
    fn case_q(cfg: &Config, id: &str, class: &str, query: &str, ids: &[i64]) {
        let list = ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
        let stamps = vec![STAMP; ids.len()].join(",");
        rows(
            cfg,
            &format!(
                "INSERT INTO eval_cases (id, class, memory_table, query, expect_ids, expect_stamps) \
                 VALUES ('{id}', '{class}', 'semantic', '{}', '{list}', '{stamps}');",
                query.replace('\'', "''")
            ),
        );
    }

    /// Two cases that hit and one that cannot: its target was superseded, so it
    /// is never in the ranking.
    fn hits_and_a_miss(name: &str) -> (Config, Vec<i64>) {
        let (cfg, ids) = db(name);
        supersede(&cfg, ids[6], ids[5]);
        case_q(&cfg, "c1", "exact", FACTS[0], &[ids[0]]);
        case_q(&cfg, "c2", "exact", FACTS[1], &[ids[1]]);
        case_q(&cfg, "c3", "other", FACTS[2], &[ids[6]]);
        (cfg, ids)
    }

    fn runs(cfg: &Config) -> i64 {
        count(cfg, "SELECT count(*) FROM eval_runs;")
    }

    // ---- report -----------------------------------------------------------

    #[test]
    fn a_report_scores_the_cases_prints_the_table_and_records_one_run() {
        let (cfg, ids) = hits_and_a_miss("rep-ok");
        let out = exec(&cfg, Mode::Report).expect("report");
        assert_eq!(out.failure, None);
        assert!(
            out.text.starts_with(
                "shipped recall @ RECALL_ALPHA=0.15   (3 cases, noise floor 33.3pp per case)\n\n\
                 variant                      R@1   R@5   MRR        exact      other\n"
            ),
            "{}",
            out.text
        );
        let row = out
            .text
            .lines()
            .find(|l| l.starts_with("current"))
            .expect("a row");
        assert!(
            row.ends_with("0.67  0.67  0.67         1.00       0.00"),
            "{row}"
        );
        assert!(out.text.contains("\nmisses @5:\n  c3  "), "{}", out.text);
        assert!(
            out.text.contains(&format!("top3=[{}, ", ids[2])),
            "the query's own row ranks first: {}",
            out.text
        );

        assert_eq!(runs(&cfg), 1);
        let r = TursoStore::open(&cfg)
            .unwrap()
            .last_eval_run()
            .unwrap()
            .unwrap();
        assert_eq!(
            (r.alpha, r.embed_model.as_str(), r.n_cases),
            (0.15, "bge-m3", 3)
        );
        assert!((r.r1 - 2.0 / 3.0).abs() < 1e-9 && (r.mrr - 2.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn a_report_diffs_against_the_previous_run_and_flags_a_changed_case_count() {
        let (cfg, ids) = hits_and_a_miss("rep-diff");
        exec(&cfg, Mode::Report).unwrap();
        let out = exec(&cfg, Mode::Report).unwrap();
        assert!(
            out.text
                .contains("(alpha 0.15, 3 cases): R@1 +0.00  R@5 +0.00  MRR +0.00\n"),
            "{}",
            out.text
        );
        assert!(!out.text.contains("NOTE"), "{}", out.text);

        case_q(&cfg, "c4", "exact", FACTS[3], &[ids[3]]);
        let out = exec(&cfg, Mode::Report).unwrap();
        assert!(
            out.text
                .contains("  NOTE: case count changed — the deltas are not like-for-like."),
            "{}",
            out.text
        );
        assert_eq!(runs(&cfg), 3);
    }

    #[test]
    fn a_drop_of_more_than_one_case_in_recall_at_five_is_flagged_as_a_regression() {
        let (cfg, ids) = db("rep-regression");
        supersede(&cfg, ids[5], ids[4]);
        for (n, fact) in [(0usize, 0usize), (1, 1), (2, 2)] {
            // All three targets are the superseded row: nothing can hit.
            case_q(&cfg, &format!("c{n}"), "exact", FACTS[fact], &[ids[5]]);
        }
        TursoStore::open(&cfg)
            .unwrap()
            .insert_eval_run(&EvalRun {
                ran_at: Some("2026-01-01 00:00:00".into()),
                alpha: 0.15,
                embed_model: "bge-m3".into(),
                n_cases: 3,
                r1: 1.0,
                r5: 1.0,
                mrr: 1.0,
            })
            .unwrap();
        let out = exec(&cfg, Mode::Report).unwrap();
        assert!(
            out.text.contains(
                "vs 2026-01-01 00:00:00 (alpha 0.15, 3 cases): R@1 -1.00  R@5 -1.00  MRR -1.00"
            ),
            "{}",
            out.text
        );
        assert!(
            out.text
                .contains("  REGRESSION: recall@5 dropped by more than one case."),
            "{}",
            out.text
        );
        assert!(
            out.text.contains("--validate"),
            "points at the eval set first: {}",
            out.text
        );
    }

    #[test]
    fn a_retired_case_is_not_counted() {
        let (cfg, _) = hits_and_a_miss("rep-retired");
        exec(&cfg, Mode::RetireCase("c3".into())).unwrap();
        let out = exec(&cfg, Mode::Report).unwrap();
        assert!(
            out.text.contains("(2 cases, noise floor 50.0pp per case)"),
            "{}",
            out.text
        );
        assert!(!out.text.contains("misses @5"), "{}", out.text);
        let r = TursoStore::open(&cfg)
            .unwrap()
            .last_eval_run()
            .unwrap()
            .unwrap();
        assert_eq!((r.n_cases, r.r5), (2, 1.0));
    }

    #[test]
    fn scoring_never_writes_to_the_memory_tables() {
        let (cfg, _) = hits_and_a_miss("rep-readonly");
        let before = rows(
            &cfg,
            "SELECT id, memory_text, embedding, updated_at FROM semantic_memory ORDER BY id;",
        );
        for mode in [Mode::Report, Mode::Sweep(vec![0.1, 0.2]), Mode::Variants] {
            exec(&cfg, mode).unwrap();
        }
        assert_eq!(
            before,
            rows(
                &cfg,
                "SELECT id, memory_text, embedding, updated_at FROM semantic_memory ORDER BY id;"
            )
        );
    }

    // ---- the cache --------------------------------------------------------

    #[test]
    fn a_second_run_embeds_nothing_it_has_already_embedded() {
        let (cfg, _) = hits_and_a_miss("rep-cache");
        let fake = Fake::up();
        execute(&cfg, &Mode::Report, &fake).unwrap();
        assert_eq!(fake.calls.get(), 1 + 3, "one probe, three distinct queries");
        execute(&cfg, &Mode::Report, &fake).unwrap();
        assert_eq!(fake.calls.get(), 1 + 3 + 1, "only the probe");
        let files: Vec<_> = std::fs::read_dir(&cfg.eval_dir)
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(files.len(), 1, "{files:?}");
        assert!(files[0].file_name().to_string_lossy().starts_with("qvec"));
    }

    #[test]
    fn two_cases_with_one_query_embed_it_once() {
        let (cfg, ids) = db("rep-cache-shared");
        case_q(&cfg, "c1", "exact", FACTS[0], &[ids[0]]);
        case_q(&cfg, "c2", "exact", FACTS[0], &[ids[1]]);
        let fake = Fake::up();
        execute(&cfg, &Mode::Report, &fake).unwrap();
        assert_eq!(fake.calls.get(), 2);
    }

    // ---- refusals ---------------------------------------------------------

    fn scoring() -> Vec<Mode> {
        vec![Mode::Report, Mode::Sweep(vec![0.1]), Mode::Variants]
    }

    #[test]
    fn with_no_cases_the_scoring_modes_refuse_and_point_at_add_case() {
        let (cfg, _) = db("ref-empty");
        for mode in scoring() {
            let fake = Fake::up();
            let msg = refusal(execute(&cfg, &mode, &fake));
            assert!(
                msg.contains("no eval cases") && msg.contains("--add-case"),
                "{mode:?}: {msg}"
            );
            assert_eq!(fake.calls.get(), 0, "{mode:?}: asked nobody");
        }
        assert_eq!(runs(&cfg), 0);
    }

    #[test]
    fn without_embeddings_the_scoring_modes_refuse_naming_the_reason_and_write_nothing() {
        let (cfg, ids) = hits_and_a_miss("ref-no-embeddings");
        let reasons = [
            (
                Unavailable::Unreachable {
                    url: "http://127.0.0.1:9".into(),
                },
                "unreachable at http://127.0.0.1:9",
            ),
            (
                Unavailable::NotPulled {
                    url: "http://x".into(),
                    model: "bge-m3".into(),
                },
                "bge-m3 is not pulled",
            ),
        ];
        for mode in scoring() {
            for (why, needle) in &reasons {
                let fake = Fake::down(why.clone());
                let msg = refusal(execute(&cfg, &mode, &fake));
                assert!(msg.contains(needle), "{mode:?}: {msg}");
                assert!(msg.contains("--validate"), "says what still works: {msg}");
            }
            // EMBEDDINGS=off: the embedder is not even asked.
            let mut off = cfg.clone();
            off.embeddings = false;
            let fake = Fake::up();
            let msg = refusal(execute(&off, &mode, &fake));
            assert!(msg.contains("EMBEDDINGS=off"), "{mode:?}: {msg}");
            assert_eq!(fake.calls.get(), 0, "{mode:?}");
        }
        assert_eq!(runs(&cfg), 0);
        assert!(!cfg.eval_dir.exists(), "no cache written either");
        let _ = ids;
    }

    #[test]
    fn a_target_without_a_vector_is_refused_with_its_id_and_the_way_to_fix_it() {
        let (cfg, ids) = db("ref-unembedded");
        let blind = put(&cfg, Table::Semantic, "stored without a vector", false);
        let blind_old = put(&cfg, Table::Semantic, "an old blind row", false);
        supersede(&cfg, blind_old, blind);
        case_q(&cfg, "c1", "exact", FACTS[0], &[ids[0]]);
        case_q(&cfg, "c2", "exact", "stored without a vector", &[blind]);
        case_q(&cfg, "c3", "exact", "an old blind row", &[blind_old]);
        for mode in scoring() {
            let fake = Fake::up();
            let msg = refusal(execute(&cfg, &mode, &fake));
            assert!(msg.contains(&format!("c2 ({blind})")), "{mode:?}: {msg}");
            assert!(msg.contains("embed --missing"), "{mode:?}: {msg}");
            assert!(
                !msg.contains("c3") && !msg.contains("c1"),
                "a superseded target is validate's business, a vector-holding one is fine: {msg}"
            );
            assert_eq!(
                fake.calls.get(),
                1,
                "{mode:?}: only the probe, no query was embedded"
            );
        }
        assert_eq!(runs(&cfg), 0);
    }

    // ---- sweep and variants -----------------------------------------------

    #[test]
    fn a_sweep_prints_a_row_per_alpha_a_verdict_and_records_nothing() {
        let (cfg, _) = hits_and_a_miss("sweep");
        let out = exec(&cfg, Mode::Sweep(vec![0.05, 0.1, 0.15, 0.2])).unwrap();
        for a in ["0.05", "0.1", "0.15", "0.2"] {
            assert!(
                out.text.contains(&format!("alpha={a} ")),
                "{a}: {}",
                out.text
            );
        }
        assert!(out.text.contains("variant "), "{}", out.text);
        assert!(
            out.text
                .contains("best recall@5 0.67 on alpha [0.05, 0.1, 0.15, 0.2]"),
            "{}",
            out.text
        );
        assert!(
            out.text
                .contains("configured alpha is still inside the plateau"),
            "{}",
            out.text
        );
        assert_eq!(runs(&cfg), 0);
    }

    #[test]
    fn a_sweep_whose_plateau_misses_the_configured_alpha_asks_and_does_not_change_it() {
        let (cfg, _) = hits_and_a_miss("sweep-outside");
        let out = exec(&cfg, Mode::Sweep(vec![0.05, 0.1])).unwrap();
        assert!(out.text.contains("OUTSIDE the plateau"), "{}", out.text);
        assert!(out.text.contains("ASK THE USER"), "{}", out.text);
        assert_eq!(cfg.recall_alpha, 0.15);
    }

    #[test]
    fn variants_compare_the_four_rankers_and_record_nothing() {
        let (cfg, _) = hits_and_a_miss("variants");
        let out = exec(&cfg, Mode::Variants).unwrap();
        for name in [
            "V0_legacy_lexicographic",
            "V_vector_only",
            "V1_idf_lexicographic",
        ] {
            assert!(out.text.contains(name), "{name}: {}", out.text);
        }
        for a in ["0.1", "0.15", "0.2"] {
            assert!(
                out.text.contains(&format!("V3_shipped_blend a={a} ")),
                "{a}: {}",
                out.text
            );
        }
        assert_eq!(runs(&cfg), 0);
    }

    // ---- the shipped ranking is recall's ----------------------------------

    /// The harness measures what `recall` does, not a copy that could drift.
    #[test]
    fn the_shipped_order_is_the_order_recall_lists() {
        let (cfg, _) = db("parity");
        let store = TursoStore::open(&cfg).unwrap();
        for query in [
            "turso wal flag",
            "zsh variable",
            "the sleep index",
            "windows",
            "qqqq",
        ] {
            let vector = Fake::vector(query);
            let order =
                shipped_order(&store, Table::Semantic, query, &vector, cfg.recall_alpha).unwrap();
            let by_id: std::collections::HashMap<i64, &str> = store
                .eval_corpus(Table::Semantic)
                .unwrap()
                .into_iter()
                .map(|(i, _)| i)
                .zip(FACTS)
                .collect();
            let ours: Vec<&str> = order.iter().map(|i| by_id[i]).collect();

            let within = Within {
                project: None,
                coworker: None,
                k: 7,
            };
            let searched = search_in(
                &cfg,
                &store,
                &Fake::up(),
                &[Table::Semantic],
                &within,
                query,
            )
            .unwrap();
            let theirs: Vec<&str> = searched
                .text
                .lines()
                .filter_map(|l| l.trim_start().strip_prefix("memory_text = "))
                .collect();
            assert_eq!(theirs.len(), 7);
            assert_eq!(ours, theirs, "query {query:?}");
        }
        let _ = Cell::Null;
    }
}
