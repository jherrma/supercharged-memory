//! The sleep and deep-sleep primitives: the writes, the read-only reports,
//! and `--purge` with and without its backup gate.

use crate::differences::{
    BACKUP_OUTPUT, CLUSTER_NO_VECTORS, COMMAND_NAMES, MARK_COUNTS_ROWS, MARK_UNKNOWN, PURGE_GATE,
    REFUSAL_EXIT, SLEEP_NO_EMBEDDING_NOTE, SLEEP_ONE_MODE, STALENESS_UNEMBEDDED, THRESHOLD_RANGE,
    TOPICS_VALIDATED, VERIFY_CLASSES,
};
use crate::harness::{Ctx, Fixture, cmd};
use crate::runner::Test;

pub const TESTS: &[Test] = crate::tests![
    the_sleep_writes_match_the_reference,
    the_read_only_reports_match_the_reference,
    purge_needs_confirmation_and_a_fresh_backup,
    one_mode_per_run,
    the_guards_the_reference_lacks,
];

const T: &str = "sleep";

fn json(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("not JSON ({e}):\n{text}"))
}

fn the_sleep_writes_match_the_reference(ctx: &Ctx) {
    let p = ctx.pair(T, "writes", Fixture::Full);
    let out = p.same(
        "sleep --mark-processed",
        &cmd(&["sleep", "--mark-processed", "3,4"]),
    );
    assert_eq!(out.ok(), "marked 2 episodic row(s) processed\n");
    let out = p.same("sleep --retire", &cmd(&["sleep", "--retire", "14"]));
    assert_eq!(out.ok(), "retired semantic memory 14\n");
    let topics = r#"[{"topic": "turso", "keywords": "wal, busy"}, {"topic": "rust", "keywords": "clippy — fmt"}]"#;
    let out = p.same(
        "sleep --rebuild-topics",
        &cmd(&["sleep", "--rebuild-topics"]).stdin(topics),
    );
    assert_eq!(out.ok(), "rebuilt topic_keywords: 2 topic(s)\n");
    p.same_state("after the sleep writes");
    let m = &p.rs;
    assert_eq!(
        m.value("SELECT count(*) FROM episodic_memory WHERE processed_at IS NOT NULL"),
        "4"
    );
    assert_eq!(
        m.value("SELECT retired_at IS NOT NULL FROM semantic_memory WHERE id = 14"),
        "1"
    );
    assert_eq!(m.value("SELECT group_concat(t, ';') FROM (SELECT topic || '=' || keywords AS t FROM topic_keywords ORDER BY topic)"), "rust=clippy — fmt;turso=wal, busy");

    let out = p.differ(
        "sleep --retire a superseded row",
        &cmd(&["sleep", "--retire", "4"]),
        &[&REFUSAL_EXIT],
    );
    assert_eq!(
        out.refused(),
        "refused: --retire 4: no current (un-superseded, non-retired) semantic row with that id.\n"
    );
    let big: Vec<String> = (0..10)
        .map(|i| format!(r#"{{"topic": "t{i}", "keywords": "{}"}}"#, "k".repeat(60)))
        .collect();
    let out = p.differ(
        "sleep --rebuild-topics over the cap",
        &cmd(&["sleep", "--rebuild-topics"]).stdin(&format!("[{}]", big.join(", "))),
        &[&REFUSAL_EXIT],
    );
    assert!(
        out.refused()
            .starts_with("refused: 10 topics total 620 chars, over the 500-char cap"),
        "{out}"
    );
    p.same_state("after the refusals");

    let p = ctx.pair(T, "mark-unknown", Fixture::Full);
    let (rs, py) = p.run(&cmd(&["sleep", "--mark-processed", "5,99"]));
    assert_eq!(
        rs.refused(),
        "refused: no episodic row with id 99 — nothing was marked.\n"
    );
    assert_eq!(
        p.rs.value("SELECT processed_at IS NULL FROM episodic_memory WHERE id = 5"),
        "1"
    );
    match py {
        Some(py) => {
            assert_eq!(py.ok(), "marked 2 episodic row(s) processed\n");
            let py_m = p.py.as_ref().unwrap();
            assert_eq!(
                py_m.value("SELECT processed_at IS NULL FROM episodic_memory WHERE id = 5"),
                "0"
            );
            ctx.asserted(&MARK_UNKNOWN);
        }
        None => ctx.skip(T, "sleep --mark-processed with an unknown id"),
    }
}

fn the_read_only_reports_match_the_reference(ctx: &Ctx) {
    let p = ctx.pair(T, "reports", Fixture::Embedded);
    for table in ["semantic", "episodic"] {
        let out = p.same(
            &format!("sleep --cluster {table}"),
            &cmd(&["sleep", "--cluster", "--table", table]),
        );
        json(out.ok());
        p.same(
            &format!("sleep --cluster {table} at 0.5"),
            &cmd(&["sleep", "--cluster", "--table", table, "--threshold", "0.5"]),
        );
        p.same(
            &format!("sleep --contradiction-candidates {table}"),
            &cmd(&["sleep", "--contradiction-candidates", "--table", table]),
        );
        let out = p.differ(
            &format!("sleep --staleness {table}"),
            &cmd(&["sleep", "--staleness", "--table", table]),
            &[&STALENESS_UNEMBEDDED],
        );
        assert_eq!(
            json(out.ok())["n_without_embedding"],
            serde_json::json!({"semantic": 0, "episodic": 0})
        );
    }
    p.same(
        "sleep --verify-candidates episodic",
        &cmd(&["sleep", "--verify-candidates", "--table", "episodic"]),
    );

    let cluster = json(p.rs.run(&cmd(&["sleep", "--cluster"])).ok());
    let ids: Vec<serde_json::Value> = cluster["clusters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["ids"].clone())
        .collect();
    assert!(
        ids.contains(&serde_json::json!([9, 10])),
        "the two near-identical rows cluster: {cluster}"
    );
    let pairs = json(
        p.rs.run(&cmd(&["sleep", "--contradiction-candidates"]))
            .ok(),
    );
    assert_eq!(
        pairs["pairs"],
        serde_json::json!([{"ids": [12, 13]}, {"ids": [12, 14]}, {"ids": [13, 14]}, {"ids": [9, 10]}]),
        "{pairs}"
    );

    // Two more classes: every candidate of the reference's is the binary's
    // too, with the same classes and possibly more.
    let (rs, py) = p.run(&cmd(&["sleep", "--verify-candidates"]));
    let binary = json(rs.ok());
    let of = |report: &serde_json::Value, id: i64| -> Option<Vec<String>> {
        report["candidates"]
            .as_array()?
            .iter()
            .find(|c| c["id"] == id)
            .map(|c| {
                c["artifacts"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|a| a.as_str().unwrap().to_string())
                    .collect()
            })
    };
    assert_eq!(
        of(&binary, 15),
        Some(vec!["paths".into(), "flags".into(), "python".into()]),
        "{binary}"
    );
    assert_eq!(of(&binary, 21), Some(vec!["python".into()]), "{binary}");
    match py {
        Some(py) => {
            let reference = json(py.ok());
            for c in reference["candidates"].as_array().unwrap() {
                let id = c["id"].as_i64().unwrap();
                let theirs = of(&reference, id).unwrap();
                let ours =
                    of(&binary, id).unwrap_or_else(|| panic!("candidate {id} missing:\n{binary}"));
                assert!(
                    theirs.iter().all(|a| ours.contains(a)),
                    "{id}: {theirs:?} vs {ours:?}"
                );
                assert!(
                    ours.iter()
                        .all(|a| theirs.contains(a) || a == "commands" || a == "python"),
                    "{id}: {ours:?}"
                );
            }
            assert_eq!(of(&reference, 21), None, "{reference}");
            ctx.asserted(&VERIFY_CLASSES);
        }
        None => ctx.skip(T, "sleep --verify-candidates semantic"),
    }
    p.same_state("after the reports");
    assert_eq!(
        p.rs.state(),
        ctx.fixture_state(Fixture::Embedded),
        "a report wrote"
    );

    let p = ctx.pair(T, "reports-unembedded", Fixture::Full);
    for args in [
        ["sleep", "--cluster"],
        ["sleep", "--contradiction-candidates"],
    ] {
        let out = p.differ(&args.join(" "), &cmd(&args), &[&SLEEP_NO_EMBEDDING_NOTE]);
        let report = json(out.ok());
        assert_eq!(report["no_embedding"], serde_json::json!([21, 22]), "{out}");
        assert!(
            report["no_embedding_note"]
                .as_str()
                .unwrap()
                .starts_with("2 row(s) have no embedding"),
            "{out}"
        );
    }
    let out = p.differ(
        "sleep --staleness, rows without a vector",
        &cmd(&["sleep", "--staleness"]),
        &[&STALENESS_UNEMBEDDED],
    );
    assert_eq!(
        json(out.ok())["n_without_embedding"],
        serde_json::json!({"semantic": 2, "episodic": 1})
    );
}

fn purge_needs_confirmation_and_a_fresh_backup(ctx: &Ctx) {
    let p = ctx.pair(T, "purge", Fixture::Full);
    let purge = cmd(&["sleep", "--purge", "4,6", "--confirm-purge"]);
    let out = p.differ(
        "sleep --purge without --confirm-purge",
        &cmd(&["sleep", "--purge", "4,6"]),
        &[&REFUSAL_EXIT],
    );
    assert!(
        out.refused()
            .starts_with("refused: --purge hard-DELETEs 2 memory row(s)"),
        "{out}"
    );
    let out = p.differ(
        "sleep --purge without a backup",
        &purge,
        &[&REFUSAL_EXIT, &COMMAND_NAMES],
    );
    assert_eq!(
        out.refused(),
        "refused: no backup found in {ROOT}/backups. Run backup first — a purge has no other undo.\n"
    );
    p.same_state("after the refused purges");

    p.run(&cmd(&["backup"]));
    let out = p.differ(
        "sleep --purge of a current row",
        &cmd(&["sleep", "--purge", "5", "--confirm-purge"]),
        &[&REFUSAL_EXIT],
    );
    assert!(
        out.refused().starts_with("refused: id 5 is CURRENT truth"),
        "{out}"
    );
    let out = p.differ("sleep --purge after a backup", &purge, &[&BACKUP_OUTPUT]);
    assert!(
        out.ok().starts_with(
            "purged 2 semantic row(s): 4,6\ncleared 0 memory_coworkers row(s)\nbackup verified: "
        ),
        "{out}"
    );
    p.same_state("after the purge");
    assert_eq!(
        p.rs.value("SELECT count(*) FROM semantic_memory WHERE id IN (4, 6)"),
        "0"
    );
    assert_eq!(p.rs.value("SELECT count(*) FROM semantic_memory"), "20");

    // The purge was a write: the backup no longer covers the database.
    let out = p.differ(
        "a second sleep --purge after the same backup",
        &cmd(&["sleep", "--purge", "4", "--confirm-purge"]),
        &[&REFUSAL_EXIT, &PURGE_GATE, &BACKUP_OUTPUT],
    );
    assert!(
        out.refused()
            .contains("Pass ALL selected ids in ONE --purge call"),
        "{out}"
    );
}

fn one_mode_per_run(ctx: &Ctx) {
    let p = ctx.pair(T, "modes", Fixture::Embedded);
    let (rs, py) = p.run(&cmd(&["sleep", "--staleness", "--cluster"]));
    rs.usage();
    match py {
        Some(py) => {
            let ran = json(py.ok());
            assert!(
                ran.get("clusters").is_some(),
                "the reference ran --cluster: {py}"
            );
            ctx.asserted(&SLEEP_ONE_MODE);
        }
        None => ctx.skip(T, "sleep with two modes"),
    }
}

fn the_guards_the_reference_lacks(ctx: &Ctx) {
    let p = ctx.pair(T, "guards", Fixture::Full);

    // A threshold no distance can meet.
    let (rs, py) = p.run(&cmd(&["sleep", "--cluster", "--threshold", "0"]));
    rs.usage();
    match py {
        Some(py) => {
            assert_eq!(json(py.ok())["clusters"], serde_json::json!([]), "{py}");
            ctx.asserted(&THRESHOLD_RANGE);
        }
        None => ctx.skip(T, "sleep --cluster --threshold 0"),
    }

    // A payload the replace must not start on.
    for payload in ["not json", r#"[{"topic": "turso"}]"#] {
        let (rs, py) = p.run(&cmd(&["sleep", "--rebuild-topics"]).stdin(payload));
        rs.refused();
        match py {
            Some(py) => {
                assert_eq!(py.code, Some(1), "{py}");
                assert!(
                    py.stderr.contains("Traceback (most recent call last)"),
                    "{py}"
                );
                ctx.asserted(&TOPICS_VALIDATED);
            }
            None => ctx.skip(T, "sleep --rebuild-topics with a malformed payload"),
        }
    }
    p.same_state("after the refused topic payloads");

    // The same id twice is one row stamped.
    let (rs, py) = p.run(&cmd(&["sleep", "--mark-processed", "5,5"]));
    assert_eq!(rs.ok(), "marked 1 episodic row(s) processed\n");
    match py {
        Some(py) => {
            assert_eq!(py.ok(), "marked 2 episodic row(s) processed\n");
            ctx.asserted(&MARK_COUNTS_ROWS);
        }
        None => ctx.skip(T, "sleep --mark-processed with a repeated id"),
    }
    p.same_state("after marking a repeated id");

    // A table none of whose rows carries a vector.
    let p = ctx.pair(T, "no-vectors", Fixture::Full);
    let strip = "UPDATE semantic_memory SET embedding = NULL, embed_model = NULL;";
    p.rs.sql(strip);
    if let Some(py) = &p.py {
        py.sql(strip);
    }
    let (rs, py) = p.run(&cmd(&["sleep", "--cluster"]));
    assert!(rs.refused().contains("embed --missing"), "{rs}");
    match py {
        Some(py) => {
            let report = json(py.ok());
            assert_eq!(report["clusters"], serde_json::json!([]), "{py}");
            assert_eq!(
                report["no_embedding"].as_array().map(Vec::len),
                Some(20),
                "{py}"
            );
            ctx.asserted(&CLUSTER_NO_VECTORS);
        }
        None => ctx.skip(T, "sleep --cluster over a table without vectors"),
    }
}
