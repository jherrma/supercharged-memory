//! `--cluster` and `--contradiction-candidates`: grouping by stored vectors.
//!
//! Mechanical, no LLM, and no Ollama either -- only the vectors already stored
//! are compared: every pair of active rows closer than the threshold in cosine
//! distance, then connected components. Deep sleep's compaction reads the
//! clusters (semantic, tight), its pattern mining the episodic ones (looser),
//! and the contradiction shortlist expands each cluster into pairs.
//!
//! A row without an embedding cannot be compared, so it is listed in
//! `no_embedding` with a pointer to `embed --missing`, never dropped. With no
//! embedded row at all the command refuses: an empty list there would read as
//! "nothing to merge" when nothing was looked at.

use std::cmp::Reverse;
use std::collections::BTreeMap;

use crate::error::{Error, Result};
use crate::json::Json;
use crate::store::{Store, Table};

/// `--cluster` defaults, measured against a real ~230-row bge-m3 corpus, not
/// guessed. Clusters are connected components, so distance chains: a-b close
/// and b-c close puts a and c together even if a and c are unrelated, which
/// makes the useful range much tighter than intuition suggests. Semantic: 0.10
/// finds nothing (that is `remember`'s own duplicate threshold), 0.22 gives ~11
/// pairs and triples, 0.30 already yields a 33-row blob and 0.35 collapses 154
/// of ~200 rows into one. Episodic runs slightly looser since it only needs
/// rows related enough to read together, but 0.40 there swallows the table.
pub const SEMANTIC_THRESHOLD: f64 = 0.22;
pub const EPISODIC_THRESHOLD: f64 = 0.25;

/// Contradictions live just above `remember`'s duplicate threshold (0.10): two
/// rows about the same subject that state different things are near-identical
/// in vector space, but not duplicates. Not yet measured on a real corpus.
pub const CONTRADICTION_THRESHOLD: f64 = 0.15;

/// A "cluster" bigger than this is chaining, not a group of near-duplicates,
/// and the output says so instead of handing back a blob that looks like a
/// finding.
const BLOB_MIN: usize = 12;
const BLOB_SHARE: f64 = 0.25;

pub fn default_threshold(table: Table) -> f64 {
    match table {
        Table::Semantic => SEMANTIC_THRESHOLD,
        Table::Episodic => EPISODIC_THRESHOLD,
    }
}

/// `--threshold`: a cosine distance, so above 0 and at most 2. Anything else
/// can only ever return nothing or everything, and an empty result reads as
/// "nothing to merge".
pub fn parse_threshold(s: &str) -> std::result::Result<f64, String> {
    let x: f64 = s
        .trim()
        .parse()
        .map_err(|_| format!("{s:?} is not a number"))?;
    // Written so that NaN fails it too.
    if x > 0.0 && x <= 2.0 {
        Ok(x)
    } else {
        Err(format!(
            "{s} is not a cosine distance above 0 and at most 2"
        ))
    }
}

/// Connected components of the pairs: each component's ids ascending, the
/// biggest component first and the lowest id first within a size. Ids in no
/// pair are in no component.
pub fn group(pairs: &[(i64, i64)]) -> Vec<Vec<i64>> {
    fn root(parent: &mut BTreeMap<i64, i64>, x: i64) -> i64 {
        let mut r = x;
        while parent[&r] != r {
            r = parent[&r];
        }
        // Point the whole path at the root, so later lookups stay short.
        let mut x = x;
        while x != r {
            x = parent
                .insert(x, r)
                .expect("every node on the path is known");
        }
        r
    }

    let mut parent: BTreeMap<i64, i64> = BTreeMap::new();
    for &(a, b) in pairs {
        parent.entry(a).or_insert(a);
        parent.entry(b).or_insert(b);
        let (ra, rb) = (root(&mut parent, a), root(&mut parent, b));
        if ra != rb {
            parent.insert(rb, ra);
        }
    }
    let mut components: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    let nodes: Vec<i64> = parent.keys().copied().collect();
    // Ascending node order, so each component comes out sorted.
    for node in nodes {
        let r = root(&mut parent, node);
        components.entry(r).or_default().push(node);
    }
    let mut clusters: Vec<Vec<i64>> = components.into_values().collect();
    clusters.sort_by_key(|c| (Reverse(c.len()), c[0]));
    clusters
}

/// The warning for a largest cluster that is chaining rather than grouping.
pub fn chaining_warning(clusters: &[Vec<i64>]) -> Option<String> {
    let clustered: usize = clusters.iter().map(Vec::len).sum();
    let biggest = clusters.iter().map(Vec::len).max().unwrap_or(0);
    (biggest as f64 > (BLOB_MIN as f64).max(BLOB_SHARE * clustered as f64)).then(|| {
        format!(
            "largest cluster holds {biggest} of {clustered} clustered rows — at this threshold \
             distance is chaining unrelated rows together (a-b close, b-c close pulls in a-c). \
             Re-run with a lower --threshold before treating these as merge candidates."
        )
    })
}

/// The grouping both commands print.
#[derive(Debug, Clone, PartialEq)]
struct Grouped {
    clusters: Vec<Vec<i64>>,
    /// Active rows without an embedding, ascending.
    no_embedding: Vec<i64>,
    warning: Option<String>,
}

/// Group the active rows of `table` at `threshold`. Refuses when rows are in
/// scope but none of them carries an embedding.
fn grouped(store: &impl Store, table: Table, threshold: f64) -> Result<Grouped> {
    let rows = store.active_rows(table)?;
    let mut no_embedding: Vec<i64> = rows.iter().filter(|r| !r.embedded).map(|r| r.id).collect();
    no_embedding.sort_unstable();
    if !rows.is_empty() && no_embedding.len() == rows.len() {
        let scope = match table {
            Table::Semantic => "current semantic",
            Table::Episodic => "episodic",
        };
        return Err(Error::refused(format!(
            "none of the {} {scope} row(s) has an embedding, so there is nothing to compare — \
             an empty result would read as \"nothing found\". Run embed --missing first; it \
             needs Ollama with the embedding model.",
            rows.len()
        )));
    }
    let clusters = group(&store.close_pairs(table, threshold)?);
    Ok(Grouped {
        warning: chaining_warning(&clusters),
        clusters,
        no_embedding,
    })
}

fn id_array(ids: &[i64]) -> Json {
    Json::Array(ids.iter().map(|id| Json::Int(*id)).collect())
}

/// `no_embedding`, and the pointer when it lists anything: a row without a
/// vector was skipped, not judged.
fn no_embedding(g: &Grouped) -> Vec<(&'static str, Json)> {
    let mut out = vec![("no_embedding", id_array(&g.no_embedding))];
    if !g.no_embedding.is_empty() {
        out.push((
            "no_embedding_note",
            format!(
                "{} row(s) have no embedding, so they were not compared — skipped, not \
                 judged. Run embed --missing first, then run this again.",
                g.no_embedding.len()
            )
            .into(),
        ));
    }
    out
}

/// `--cluster`'s JSON.
pub fn clusters(store: &impl Store, table: Table, threshold: f64) -> Result<Json> {
    let g = grouped(store, table, threshold)?;
    let clusters = g
        .clusters
        .iter()
        .enumerate()
        .map(|(n, c)| Json::object([("cluster", Json::from(n + 1)), ("ids", id_array(c))]))
        .collect();
    let mut out = vec![
        ("table", Json::from(table.as_str())),
        ("threshold", Json::Float(threshold)),
        ("clusters", Json::Array(clusters)),
    ];
    out.extend(no_embedding(&g));
    if let Some(w) = &g.warning {
        out.push(("warning", w.as_str().into()));
    }
    Ok(Json::object(out))
}

/// `--contradiction-candidates`' JSON: every pair within each cluster. A
/// shortlist only: whether two rows disagree is for an LLM to judge.
pub fn contradictions(store: &impl Store, table: Table, threshold: f64) -> Result<Json> {
    let g = grouped(store, table, threshold)?;
    let mut pairs = Vec::new();
    for c in &g.clusters {
        for (i, a) in c.iter().enumerate() {
            for b in &c[i + 1..] {
                pairs.push(Json::object([("ids", id_array(&[*a, *b]))]));
            }
        }
    }
    let mut out = vec![
        ("table", Json::from(table.as_str())),
        ("threshold", Json::Float(threshold)),
        ("n_pairs", Json::from(pairs.len())),
        ("pairs", Json::Array(pairs)),
    ];
    out.extend(no_embedding(&g));
    out.push((
        "note",
        "candidates only — a pair here shares a subject; whether the two rows CONTRADICT is \
         a judgment call, not a distance."
            .into(),
    ));
    if let Some(w) = &g.warning {
        out.push(("warning", w.as_str().into()));
    }
    Ok(Json::object(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::error::EXIT_REFUSED;
    use crate::sleep::{Mode, execute};
    use crate::store::turso::tests::{angle, put, put_vector, rows, scratch};

    fn run(cfg: &Config, mode: Mode) -> Result<String> {
        execute(cfg, &mode, &mut std::io::empty())
    }

    fn json(out: &str) -> serde_json::Value {
        serde_json::from_str(out).unwrap_or_else(|e| panic!("{e}: {out}"))
    }

    fn cluster(table: Table, threshold: Option<f64>) -> Mode {
        Mode::Cluster { table, threshold }
    }

    fn contradiction(table: Table, threshold: Option<f64>) -> Mode {
        Mode::ContradictionCandidates { table, threshold }
    }

    #[test]
    fn components_are_biggest_first_then_by_lowest_id() {
        assert_eq!(
            group(&[(1, 2), (2, 3), (10, 11), (6, 7), (5, 6), (7, 8)]),
            vec![vec![5, 6, 7, 8], vec![1, 2, 3], vec![10, 11]]
        );
        assert_eq!(group(&[(4, 9), (1, 3)]), vec![vec![1, 3], vec![4, 9]]);
        // Two chains that meet only late still end up as one component.
        assert_eq!(group(&[(1, 2), (3, 4), (2, 4)]), vec![vec![1, 2, 3, 4]]);
        assert!(group(&[]).is_empty());
    }

    #[test]
    fn a_blob_is_flagged_as_chaining() {
        let chain = |n: i64| (1..=n).collect::<Vec<i64>>();
        assert_eq!(chaining_warning(&[chain(12)]), None, "12 is the floor");
        let w = chaining_warning(&[chain(13)]).expect("13 of 13 is a blob");
        assert_eq!(
            w,
            "largest cluster holds 13 of 13 clustered rows — at this threshold distance is \
             chaining unrelated rows together (a-b close, b-c close pulls in a-c). Re-run with \
             a lower --threshold before treating these as merge candidates."
        );
        // Above the floor, a quarter of everything clustered is the bar.
        let mut many = vec![chain(20)];
        many.extend((0..40).map(|i| vec![1000 + 2 * i, 1001 + 2 * i]));
        assert_eq!(chaining_warning(&many), None, "20 of 100");
        let mut many = vec![chain(26)];
        many.extend((0..37).map(|i| vec![1000 + 2 * i, 1001 + 2 * i]));
        assert!(chaining_warning(&many).unwrap().contains("26 of 100"));
        assert_eq!(chaining_warning(&[]), None);
    }

    #[test]
    fn a_threshold_must_be_a_cosine_distance_above_zero() {
        assert_eq!(parse_threshold("0.22"), Ok(0.22));
        assert_eq!(parse_threshold("2"), Ok(2.0));
        for bad in ["0", "-0.1", "2.01", "nan", "inf", "x", ""] {
            assert!(parse_threshold(bad).is_err(), "{bad}");
        }
    }

    /// Plane 0: 0, 25 and 50 degrees -- neighbours 0.094 apart, the ends
    /// 0.357, so they chain at 0.15. Plane 1: a close pair. Plane 2: a loner.
    fn fixture(cfg: &Config) -> [i64; 6] {
        [
            put_vector(cfg, Table::Semantic, "a", &angle(0, 0.0)),
            put_vector(cfg, Table::Semantic, "b", &angle(0, 25.0)),
            put_vector(cfg, Table::Semantic, "c", &angle(0, 50.0)),
            put_vector(cfg, Table::Semantic, "d", &angle(1, 0.0)),
            put_vector(cfg, Table::Semantic, "e", &angle(1, 10.0)),
            put_vector(cfg, Table::Semantic, "f", &angle(2, 0.0)),
        ]
    }

    /// The layout is Python's `json.dumps(indent=1)` of the reference's dict.
    #[test]
    fn cluster_prints_connected_components_as_the_reference_did() {
        let cfg = scratch("cluster-layout");
        fixture(&cfg);
        let out = run(&cfg, cluster(Table::Semantic, Some(0.15))).unwrap();
        assert_eq!(
            out,
            "{\n \"table\": \"semantic\",\n \"threshold\": 0.15,\n \"clusters\": [\n  {\n   \
             \"cluster\": 1,\n   \"ids\": [\n    1,\n    2,\n    3\n   ]\n  },\n  {\n   \
             \"cluster\": 2,\n   \"ids\": [\n    4,\n    5\n   ]\n  }\n ],\n \
             \"no_embedding\": []\n}"
        );
    }

    #[test]
    fn the_default_threshold_depends_on_the_table() {
        let cfg = scratch("cluster-defaults");
        fixture(&cfg);
        put_vector(&cfg, Table::Episodic, "e", &angle(0, 0.0));
        let v = json(&run(&cfg, cluster(Table::Semantic, None)).unwrap());
        assert_eq!(v["threshold"], 0.22);
        let v = json(&run(&cfg, cluster(Table::Episodic, None)).unwrap());
        assert_eq!(v["threshold"], 0.25);
        assert_eq!(v["table"], "episodic");
        let v = json(&run(&cfg, contradiction(Table::Semantic, None)).unwrap());
        assert_eq!(v["threshold"], 0.15);
    }

    /// Superseded and retired rows are history, not merge candidates.
    #[test]
    fn only_current_semantic_rows_are_clustered() {
        let cfg = scratch("cluster-current");
        let [a, b, ..] = fixture(&cfg);
        let twin = put_vector(&cfg, Table::Semantic, "twin of f", &angle(2, 1.0));
        rows(
            &cfg,
            &format!("UPDATE semantic_memory SET superseded_by = {a} WHERE id = {twin};"),
        );
        rows(
            &cfg,
            &format!("UPDATE semantic_memory SET retired_at = '2026-02-02' WHERE id = {b};"),
        );
        let v = json(&run(&cfg, cluster(Table::Semantic, Some(0.15))).unwrap());
        assert_eq!(
            v["clusters"],
            serde_json::json!([{"cluster": 1, "ids": [4, 5]}])
        );
    }

    /// The degradation contract: the embedded rows are clustered, the rest are
    /// listed and the output says how to get them judged.
    #[test]
    fn mixed_rows_cluster_the_embedded_and_list_the_rest_with_the_pointer() {
        let cfg = scratch("cluster-mixed");
        fixture(&cfg);
        let blind_a = put(&cfg, Table::Semantic, "no vector a", false);
        let blind_b = put(&cfg, Table::Semantic, "no vector b", false);
        let blind_old = put(&cfg, Table::Semantic, "no vector, superseded", false);
        rows(
            &cfg,
            &format!("UPDATE semantic_memory SET superseded_by = 1 WHERE id = {blind_old};"),
        );
        for mode in [
            cluster(Table::Semantic, Some(0.15)),
            contradiction(Table::Semantic, Some(0.15)),
        ] {
            let v = json(&run(&cfg, mode.clone()).unwrap());
            assert_eq!(
                v["no_embedding"],
                serde_json::json!([blind_a, blind_b]),
                "{mode:?}"
            );
            let note = v["no_embedding_note"].as_str().expect("the pointer");
            assert!(note.contains("embed --missing"), "{note}");
            assert!(note.starts_with("2 row(s)"), "{note}");
        }
        let v = json(&run(&cfg, cluster(Table::Semantic, Some(0.15))).unwrap());
        assert_eq!(v["clusters"][0]["ids"], serde_json::json!([1, 2, 3]));
    }

    #[test]
    fn no_embedding_note_is_absent_when_every_row_has_a_vector() {
        let cfg = scratch("cluster-all-embedded");
        fixture(&cfg);
        let out = run(&cfg, cluster(Table::Semantic, Some(0.15))).unwrap();
        assert!(!out.contains("no_embedding_note"), "{out}");
    }

    #[test]
    fn with_no_embedding_at_all_both_refuse_with_the_pointer() {
        let cfg = scratch("cluster-none-embedded");
        put(&cfg, Table::Semantic, "blind a", false);
        put(&cfg, Table::Semantic, "blind b", false);
        put(&cfg, Table::Episodic, "blind event", false);
        for mode in [
            cluster(Table::Semantic, None),
            contradiction(Table::Semantic, None),
            cluster(Table::Episodic, None),
        ] {
            let err = run(&cfg, mode.clone()).unwrap_err();
            assert_eq!(err.exit_code(), EXIT_REFUSED, "{mode:?}: {err}");
            let msg = err.to_string();
            assert!(msg.starts_with("refused: "), "{msg}");
            assert!(msg.contains("embed --missing"), "{msg}");
            assert!(msg.contains("has an embedding"), "{msg}");
        }
    }

    /// Nothing stored is not "no embeddings": there is nothing to embed, and
    /// an empty list is the truth.
    #[test]
    fn an_empty_table_is_an_empty_result_not_a_refusal() {
        let cfg = scratch("cluster-empty");
        let v = json(&run(&cfg, cluster(Table::Semantic, None)).unwrap());
        assert_eq!(v["clusters"], serde_json::json!([]));
        assert_eq!(v["no_embedding"], serde_json::json!([]));
        assert!(v.get("no_embedding_note").is_none());
        assert!(v.get("warning").is_none());
    }

    #[test]
    fn episodic_clusters_every_row() {
        let cfg = scratch("cluster-episodic");
        let a = put_vector(&cfg, Table::Episodic, "a", &angle(0, 0.0));
        let b = put_vector(&cfg, Table::Episodic, "b", &angle(0, 5.0));
        rows(
            &cfg,
            "UPDATE episodic_memory SET processed_at = '2026-01-02';",
        );
        let blind = put(&cfg, Table::Episodic, "blind", false);
        let v = json(&run(&cfg, cluster(Table::Episodic, None)).unwrap());
        assert_eq!(v["clusters"][0]["ids"], serde_json::json!([a, b]));
        assert_eq!(v["no_embedding"], serde_json::json!([blind]));
    }

    #[test]
    fn a_chaining_blob_carries_the_warning() {
        let cfg = scratch("cluster-blob");
        for i in 0..13 {
            put_vector(
                &cfg,
                Table::Semantic,
                &format!("r{i}"),
                &angle(3, 10.0 * i as f64),
            );
        }
        let v = json(&run(&cfg, cluster(Table::Semantic, Some(0.05))).unwrap());
        assert_eq!(v["clusters"][0]["ids"].as_array().unwrap().len(), 13);
        assert!(
            v["warning"]
                .as_str()
                .unwrap()
                .contains("largest cluster holds 13 of 13")
        );
        let v = json(&run(&cfg, contradiction(Table::Semantic, Some(0.05))).unwrap());
        assert!(v["warning"].as_str().is_some());
        assert_eq!(v["n_pairs"], 78);
    }

    /// Each cluster is expanded into all its pairs -- including the chained
    /// ends, since "which two rows disagree" is what a judge can answer.
    #[test]
    fn contradiction_candidates_are_every_pair_within_a_cluster() {
        let cfg = scratch("contradictions");
        fixture(&cfg);
        let out = run(&cfg, contradiction(Table::Semantic, Some(0.15))).unwrap();
        assert_eq!(
            out,
            "{\n \"table\": \"semantic\",\n \"threshold\": 0.15,\n \"n_pairs\": 4,\n \
             \"pairs\": [\n  {\n   \"ids\": [\n    1,\n    2\n   ]\n  },\n  {\n   \"ids\": [\n    \
             1,\n    3\n   ]\n  },\n  {\n   \"ids\": [\n    2,\n    3\n   ]\n  },\n  {\n   \
             \"ids\": [\n    4,\n    5\n   ]\n  }\n ],\n \"no_embedding\": [],\n \"note\": \
             \"candidates only \\u2014 a pair here shares a subject; whether the two rows \
             CONTRADICT is a judgment call, not a distance.\"\n}"
        );
    }
}
