//! `sleep`: the mechanical primitives a sleep pass runs.
//!
//! Deliberately dumb. Promoting an episodic lesson into semantic memory and
//! merging semantic rows both go through `remember`; what is left is the writes
//! `remember` does not do -- stamping episodic rows as sifted, retiring a row,
//! replacing the topic index, the one hard delete -- and the read-only analyses
//! deep sleep hands to its workers. Judgment stays with the agent driving the
//! runbook: nothing here decides what to merge, retire or purge.
//!
//! None of it touches the `instructions` table: directives are outside sleep,
//! deep sleep and purge, and are retired only by their own command.

pub mod cluster;
pub mod purge;
pub mod review;

use std::io::Read;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::json::dumps;
use crate::store::{self, Store, Table};

/// The topic index's hard cap, in characters, topics and keywords together.
/// The whole table loads into every session's context, so this bounds a cost
/// every future session pays, not one paid when queried.
pub const TOPICS_CHAR_CAP: usize = 500;

/// One invocation. The CLI admits exactly one mode per run.
#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    /// Stamp `processed_at` on these episodic rows (comma-separated ids).
    MarkProcessed(String),
    /// Soft-delete one current semantic row.
    Retire(i64),
    /// Replace the topic index from a JSON array on stdin.
    RebuildTopics,
    /// Hard-delete these superseded or retired semantic rows.
    Purge { ids: String, confirm: bool },
    /// Embedding-similarity clusters, as JSON. `None` = the table's default.
    Cluster {
        table: Table,
        threshold: Option<f64>,
    },
    /// Pairs close enough to contradict each other, as JSON.
    ContradictionCandidates {
        table: Table,
        threshold: Option<f64>,
    },
    /// The age profile of the active rows, as JSON.
    Staleness { table: Table },
    /// Active rows quoting a checkable artifact, oldest first, as JSON.
    /// A `limit` of 0 lists them all.
    VerifyCandidates { table: Table, limit: u32 },
}

pub fn run(cfg: &Config, mode: &Mode) -> Result<()> {
    let out = execute(cfg, mode, &mut std::io::stdin().lock())?;
    println!("{out}");
    Ok(())
}

/// Run one mode and return what it prints on stdout. Only `RebuildTopics`
/// reads `stdin`.
pub fn execute(cfg: &Config, mode: &Mode, stdin: &mut dyn Read) -> Result<String> {
    match mode {
        Mode::MarkProcessed(csv) => mark_processed(cfg, csv),
        Mode::Retire(id) => retire(cfg, *id),
        Mode::RebuildTopics => rebuild_topics(cfg, stdin),
        Mode::Purge { ids, confirm } => purge::purge(cfg, ids, *confirm),
        Mode::Cluster { table, threshold } => {
            let store = store::open(cfg)?;
            let threshold = threshold.unwrap_or(cluster::default_threshold(*table));
            Ok(dumps(&cluster::clusters(&store, *table, threshold)?))
        }
        Mode::ContradictionCandidates { table, threshold } => {
            let store = store::open(cfg)?;
            let threshold = threshold.unwrap_or(cluster::CONTRADICTION_THRESHOLD);
            Ok(dumps(&cluster::contradictions(&store, *table, threshold)?))
        }
        Mode::Staleness { table } => Ok(dumps(&review::staleness(&store::open(cfg)?, *table)?)),
        Mode::VerifyCandidates { table, limit } => Ok(dumps(&review::verify_candidates(
            &store::open(cfg)?,
            *table,
            *limit,
        )?)),
    }
}

/// Kept and discarded rows alike: a sifted row is done either way. Only the
/// ids given -- a worker that died leaves its rows unprocessed, which is the
/// state the next pass recovers from -- and all of them or none.
fn mark_processed(cfg: &Config, csv: &str) -> Result<String> {
    let ids = parse_ids(csv)?;
    let store = store::open(cfg)?;
    let found = store.existing_ids(Table::Episodic, &ids)?;
    let missing: Vec<String> = ids
        .iter()
        .filter(|id| !found.contains(id))
        .map(i64::to_string)
        .collect();
    if !missing.is_empty() {
        return Err(Error::refused(format!(
            "no episodic row with id {} — nothing was marked.",
            missing.join(",")
        )));
    }
    let marked = store.mark_processed(&ids)?;
    Ok(format!("marked {marked} episodic row(s) processed"))
}

/// The only way `retired_at` is set on semantic memory. Never a delete.
fn retire(cfg: &Config, id: i64) -> Result<String> {
    if !store::open(cfg)?.retire_semantic(id)? {
        return Err(Error::refused(format!(
            "--retire {id}: no current (un-superseded, non-retired) semantic row with that id."
        )));
    }
    Ok(format!("retired semantic memory {id}"))
}

/// A full replace from stdin. This command does not derive topics or keywords
/// -- the agent groups current memory and writes the keyword lists; this is
/// only the atomic write, and the cap.
fn rebuild_topics(cfg: &Config, stdin: &mut dyn Read) -> Result<String> {
    let mut input = Vec::new();
    stdin
        .read_to_end(&mut input)
        .map_err(|e| Error::failed(format!("cannot read stdin: {e}")))?;
    let topics = parse_topics(&input)?;
    store::open(cfg)?.replace_topics(&topics)?;
    Ok(format!("rebuilt topic_keywords: {} topic(s)", topics.len()))
}

/// Comma-separated ids, deduplicated and sorted. Blank entries are skipped;
/// anything else that is not an integer is refused, so a stray word cannot
/// shrink the list a worker reported.
fn parse_ids(csv: &str) -> Result<Vec<i64>> {
    let mut ids = Vec::new();
    for part in csv.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        ids.push(part.parse::<i64>().map_err(|_| {
            Error::refused(format!(
                "not an id: {part:?} — ids are comma-separated integers."
            ))
        })?);
    }
    if ids.is_empty() {
        return Err(Error::refused("no ids given."));
    }
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

/// The topic index from the JSON on stdin: an array of `{topic, keywords}`
/// objects, validated whole before anything is written.
fn parse_topics(input: &[u8]) -> Result<Vec<(String, String)>> {
    let text = std::str::from_utf8(input)
        .map_err(|e| Error::refused(format!("stdin is not UTF-8 ({e}) — nothing was written.")))?;
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| Error::refused(format!("stdin is not valid JSON: {e}")))?;
    let items = value.as_array().ok_or_else(|| {
        Error::refused("stdin must be a JSON array of {topic, keywords} objects.")
    })?;

    let mut topics: Vec<(String, String)> = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let n = i + 1;
        let object = item.as_object().ok_or_else(|| {
            Error::refused(format!("item {n} is not a {{topic, keywords}} object."))
        })?;
        let field = |key: &str| {
            object
                .get(key)
                .and_then(|v| v.as_str())
                .ok_or_else(|| Error::refused(format!("item {n} has no string \"{key}\".")))
        };
        let (topic, keywords) = (field("topic")?, field("keywords")?);
        if topic.trim().is_empty() {
            return Err(Error::refused(format!("item {n} has an empty topic.")));
        }
        if topics.iter().any(|(t, _)| t == topic) {
            return Err(Error::refused(format!(
                "duplicate topic {topic:?} — the topic is the index's key."
            )));
        }
        topics.push((topic.to_string(), keywords.to_string()));
    }

    // Characters, as the reference's `len` counted them: an umlaut is one.
    let size = |(t, k): &(String, String)| t.chars().count() + k.chars().count();
    let total: usize = topics.iter().map(size).sum();
    if total > TOPICS_CHAR_CAP {
        let mut by_size: Vec<&(String, String)> = topics.iter().collect();
        by_size.sort_by_key(|t| std::cmp::Reverse(size(t)));
        let biggest: Vec<String> = by_size
            .iter()
            .take(5)
            .map(|t| format!("{} ({})", t.0, size(t)))
            .collect();
        return Err(Error::refused(format!(
            "{} topics total {total} chars, over the {TOPICS_CHAR_CAP}-char cap (this table \
             loads into every session's context). Drop or merge topics and retry. Largest: {}",
            topics.len(),
            biggest.join(", ")
        )));
    }
    Ok(topics)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::EXIT_REFUSED;
    use crate::store::Cell;
    use crate::store::turso::tests::{put, rows, scratch};

    fn run_mode(cfg: &Config, mode: Mode) -> Result<String> {
        execute(cfg, &mode, &mut std::io::empty())
    }

    fn rebuild(cfg: &Config, json: &str) -> Result<String> {
        execute(cfg, &Mode::RebuildTopics, &mut json.as_bytes())
    }

    fn refusal(r: Result<String>) -> String {
        let err = r.expect_err("expected a refusal");
        assert_eq!(err.exit_code(), EXIT_REFUSED, "{err}");
        let msg = err.to_string();
        assert!(msg.starts_with("refused: "), "{msg}");
        msg
    }

    fn topics(cfg: &Config) -> Vec<Vec<Cell>> {
        rows(
            cfg,
            "SELECT topic, keywords FROM topic_keywords ORDER BY topic;",
        )
    }

    fn processed(cfg: &Config) -> Vec<Vec<Cell>> {
        rows(
            cfg,
            "SELECT id, processed_at IS NOT NULL FROM episodic_memory ORDER BY id;",
        )
    }

    // ---- ids ----------------------------------------------------------------

    #[test]
    fn ids_are_parsed_deduplicated_and_sorted() {
        assert_eq!(parse_ids("3, 1,,3 ,2").unwrap(), vec![1, 2, 3]);
        assert_eq!(parse_ids("42").unwrap(), vec![42]);
    }

    #[test]
    fn no_ids_at_all_is_refused() {
        for csv in ["", " ", ",", " , ,"] {
            let msg = refusal(parse_ids(csv).map(|_| String::new()));
            assert_eq!(msg, "refused: no ids given.", "{csv:?}");
        }
    }

    /// Python died on a traceback here; a word in the list must not be read as
    /// a shorter list either.
    #[test]
    fn a_non_integer_id_is_refused_by_name() {
        for csv in ["1,all", "1,2x", "*", "1.5"] {
            let msg = refusal(parse_ids(csv).map(|_| String::new()));
            assert!(msg.contains("not an id"), "{csv}: {msg}");
        }
        let msg = refusal(parse_ids("1, two ,3").map(|_| String::new()));
        assert!(msg.contains("\"two\""), "{msg}");
    }

    // ---- --mark-processed ---------------------------------------------------

    #[test]
    fn mark_processed_stamps_exactly_the_listed_episodic_rows() {
        let cfg = scratch("sleep-mark");
        let a = put(&cfg, Table::Episodic, "a", false);
        let b = put(&cfg, Table::Episodic, "b", false);
        let c = put(&cfg, Table::Episodic, "c", false);
        put(&cfg, Table::Semantic, "a semantic row with id 1", false);
        let out = run_mode(&cfg, Mode::MarkProcessed(format!("{a},{c},{a}"))).unwrap();
        assert_eq!(out, "marked 2 episodic row(s) processed");
        assert_eq!(
            processed(&cfg),
            vec![
                vec![Cell::Int(a), Cell::Int(1)],
                vec![Cell::Int(b), Cell::Int(0)],
                vec![Cell::Int(c), Cell::Int(1)],
            ]
        );
    }

    /// A worker that reports a wrong id must not get a partial stamp that
    /// reads as done: the whole call is refused and nothing is marked.
    #[test]
    fn mark_processed_refuses_an_unknown_id_and_marks_nothing() {
        let cfg = scratch("sleep-mark-unknown");
        let a = put(&cfg, Table::Episodic, "a", false);
        let msg = refusal(run_mode(&cfg, Mode::MarkProcessed(format!("{a},77"))));
        assert!(msg.contains("77"), "{msg}");
        assert!(msg.contains("episodic"), "{msg}");
        assert_eq!(processed(&cfg), vec![vec![Cell::Int(a), Cell::Int(0)]]);
    }

    #[test]
    fn mark_processed_without_ids_is_refused() {
        let cfg = scratch("sleep-mark-empty");
        let msg = refusal(run_mode(&cfg, Mode::MarkProcessed(" , ".into())));
        assert_eq!(msg, "refused: no ids given.");
    }

    #[test]
    fn a_missing_database_is_refused_and_never_created() {
        let mut cfg = scratch("sleep-missing-db");
        cfg.db_path = cfg.db_path.with_file_name("gone.db");
        for mode in [
            Mode::MarkProcessed("1".into()),
            Mode::Retire(1),
            Mode::Staleness {
                table: Table::Semantic,
            },
            Mode::Cluster {
                table: Table::Semantic,
                threshold: None,
            },
        ] {
            let msg = refusal(run_mode(&cfg, mode.clone()));
            assert!(msg.contains("memory DB missing"), "{mode:?}: {msg}");
        }
        refusal(rebuild(&cfg, "[]"));
        assert!(!cfg.db_path.exists());
    }

    // ---- --retire -----------------------------------------------------------

    #[test]
    fn retire_soft_deletes_one_current_row() {
        let cfg = scratch("sleep-retire");
        let a = put(&cfg, Table::Semantic, "a", false);
        let out = run_mode(&cfg, Mode::Retire(a)).unwrap();
        assert_eq!(out, format!("retired semantic memory {a}"));
        assert_eq!(
            rows(
                &cfg,
                "SELECT count(*), sum(retired_at IS NOT NULL) FROM semantic_memory;"
            ),
            vec![vec![Cell::Int(1), Cell::Int(1)]],
            "soft delete: the row stays"
        );
    }

    #[test]
    fn retire_refuses_a_row_that_is_not_current() {
        let cfg = scratch("sleep-retire-refused");
        let a = put(&cfg, Table::Semantic, "a", false);
        let old = put(&cfg, Table::Semantic, "old", false);
        rows(
            &cfg,
            &format!("UPDATE semantic_memory SET superseded_by = {a} WHERE id = {old};"),
        );
        run_mode(&cfg, Mode::Retire(a)).unwrap();
        put(&cfg, Table::Episodic, "e1", false);
        put(&cfg, Table::Episodic, "e2", false);
        let episodic_only = put(&cfg, Table::Episodic, "only episodic has this id", false);
        assert_eq!(episodic_only, 3);
        for id in [a, old, 99, episodic_only] {
            let msg = refusal(run_mode(&cfg, Mode::Retire(id)));
            assert_eq!(
                msg,
                format!(
                    "refused: --retire {id}: no current (un-superseded, non-retired) \
                     semantic row with that id."
                )
            );
        }
    }

    // ---- --rebuild-topics ---------------------------------------------------

    /// The documented footgun: a partial set REPLACES the table, so every
    /// topic it omits is gone.
    #[test]
    fn rebuild_topics_with_a_partial_set_drops_every_omitted_topic() {
        let cfg = scratch("sleep-topics-partial");
        rows(
            &cfg,
            "INSERT INTO topic_keywords (topic, keywords) VALUES \
             ('alpha', 'a'), ('beta', 'b'), ('gamma', 'c');",
        );
        let out = rebuild(&cfg, r#"[{"topic": "beta", "keywords": "b, new"}]"#).unwrap();
        assert_eq!(out, "rebuilt topic_keywords: 1 topic(s)");
        assert_eq!(
            topics(&cfg),
            vec![vec![Cell::Text("beta".into()), Cell::Text("b, new".into())]]
        );
    }

    #[test]
    fn rebuild_topics_keeps_umlauts_and_ignores_extra_keys() {
        let cfg = scratch("sleep-topics-utf8");
        rebuild(
            &cfg,
            r#"[{"topic": "Schlaf", "keywords": "Tiefschlaf, Grüße", "note": "ignored"},
                {"topic": "zsh", "keywords": ""}]"#,
        )
        .unwrap();
        assert_eq!(
            topics(&cfg),
            vec![
                vec![
                    Cell::Text("Schlaf".into()),
                    Cell::Text("Tiefschlaf, Grüße".into())
                ],
                vec![Cell::Text("zsh".into()), Cell::Text(String::new())],
            ]
        );
    }

    #[test]
    fn an_empty_array_empties_the_index() {
        let cfg = scratch("sleep-topics-empty");
        rows(
            &cfg,
            "INSERT INTO topic_keywords (topic, keywords) VALUES ('alpha', 'a');",
        );
        assert_eq!(
            rebuild(&cfg, "[]").unwrap(),
            "rebuilt topic_keywords: 0 topic(s)"
        );
        assert!(topics(&cfg).is_empty());
    }

    fn sized(topic: &str, total: usize) -> String {
        format!(
            r#"{{"topic": "{topic}", "keywords": "{}"}}"#,
            "k".repeat(total - topic.chars().count())
        )
    }

    /// The cap counts characters, as Python's `len` did: an umlaut is one.
    #[test]
    fn the_cap_admits_exactly_500_characters() {
        let cfg = scratch("sleep-topics-cap");
        let json = format!("[{}, {}]", sized("Grüße", 250), sized("b", 250));
        rebuild(&cfg, &json).unwrap();
        assert_eq!(topics(&cfg).len(), 2);
    }

    #[test]
    fn an_over_cap_set_is_refused_and_the_index_is_left_alone() {
        let cfg = scratch("sleep-topics-over-cap");
        rows(
            &cfg,
            "INSERT INTO topic_keywords (topic, keywords) VALUES ('alpha', 'a');",
        );
        let parts: Vec<String> = [
            ("t1", 40),
            ("t2", 120),
            ("t3", 60),
            ("t4", 100),
            ("t5", 80),
            ("t6", 101),
        ]
        .iter()
        .map(|(t, n)| sized(t, *n))
        .collect();
        let msg = refusal(rebuild(&cfg, &format!("[{}]", parts.join(","))));
        assert_eq!(
            msg,
            "refused: 6 topics total 501 chars, over the 500-char cap (this table loads \
             into every session's context). Drop or merge topics and retry. Largest: \
             t2 (120), t6 (101), t4 (100), t5 (80), t3 (60)"
        );
        assert_eq!(
            topics(&cfg),
            vec![vec![Cell::Text("alpha".into()), Cell::Text("a".into())]]
        );
    }

    #[test]
    fn malformed_topic_input_is_refused_and_writes_nothing() {
        let cfg = scratch("sleep-topics-malformed");
        rows(
            &cfg,
            "INSERT INTO topic_keywords (topic, keywords) VALUES ('alpha', 'a');",
        );
        for (input, want) in [
            (r#"{"topic": "x", "keywords": "y"}"#, "JSON array"),
            ("not json", "not valid JSON"),
            ("", "not valid JSON"),
            (r#"[["x", "y"]]"#, "item 1"),
            (r#"[{"topic": "x"}]"#, "keywords"),
            (r#"[{"keywords": "y"}]"#, "topic"),
            (r#"[{"topic": 7, "keywords": "y"}]"#, "topic"),
            (r#"[{"topic": "x", "keywords": null}]"#, "keywords"),
            (r#"[{"topic": "  ", "keywords": "y"}]"#, "empty"),
            (
                r#"[{"topic": "x", "keywords": "a"}, {"topic": "x", "keywords": "b"}]"#,
                "duplicate topic \"x\"",
            ),
        ] {
            let msg = refusal(rebuild(&cfg, input));
            assert!(msg.contains(want), "{input}: {msg}");
        }
        let msg = refusal(execute(
            &cfg,
            &Mode::RebuildTopics,
            &mut &b"[{\"topic\": \"\xff\", \"keywords\": \"y\"}]"[..],
        ));
        assert!(msg.contains("UTF-8"), "{msg}");
        assert_eq!(
            topics(&cfg),
            vec![vec![Cell::Text("alpha".into()), Cell::Text("a".into())]]
        );
    }
}
