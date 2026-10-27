//! Eval: validation and scoring against the reference harness, then the
//! eval-case writes, which the reference left to raw SQL.

use crate::differences::{
    COMMAND_NAMES, EVAL_CACHE_PER_MODEL, EVAL_NO_CASES, EVAL_NO_EMBEDDINGS, EVAL_VALIDATE_TEXT,
};
use crate::harness::{Ctx, Fixture, Side, cmd};
use crate::runner::Test;

pub const TESTS: &[Test] = crate::tests![
    validation_and_scoring_match_the_reference,
    the_eval_cases_are_written_without_sql,
];

const T: &str = "eval";

fn validation_and_scoring_match_the_reference(ctx: &Ctx) {
    let p = ctx.pair(T, "scoring", Fixture::Embedded);
    let out = p.differ(
        "eval --validate",
        &cmd(&["eval", "--validate"]),
        &[&EVAL_VALIDATE_TEXT],
    );
    assert_eq!(
        out.code,
        Some(1),
        "a case points at a superseded row:\n{out}"
    );
    assert!(out.stdout.contains("x05"), "{out}");
    assert!(out.stdout.contains("4 -> 5 (superseded; repoint)"), "{out}");
    assert_eq!(out.stderr, "error: 1 eval case(s) need attention\n");

    let out = p.same("eval --sweep", &cmd(&["eval", "--sweep", "0.0,0.15,0.3"]));
    assert!(out.ok().contains("configured RECALL_ALPHA: 0.15"), "{out}");
    p.same("eval --variants", &cmd(&["eval", "--variants"]));
    // Each side keeps its own query-vector cache beside the database.
    assert_eq!(p.rs.list("data/turso/eval"), ["qvec-bge-m3.json"]);
    match &p.py {
        Some(py) => {
            assert_eq!(py.list("data/turso/eval"), ["qvec.json"]);
            ctx.asserted(&EVAL_CACHE_PER_MODEL);
        }
        None => ctx.skip(T, "the eval query-vector cache"),
    }
    p.same_state("after validation and scoring");

    let out = p.differ(
        "eval --report",
        &cmd(&["eval", "--report"]),
        &[&COMMAND_NAMES],
    );
    assert!(
        out.ok()
            .starts_with("shipped recall @ RECALL_ALPHA=0.15   (6 cases, "),
        "{out}"
    );
    p.same_state("after eval --report recorded its run");
    assert_eq!(p.rs.value("SELECT count(*) FROM eval_runs"), "2");

    // Without embeddings the binary refuses to score, even though every query
    // vector is cached by now; the reference scores from its cache.
    let sweep = cmd(&["eval", "--sweep", "0.15"]).without_ollama();
    let (rs, warm) = p.run(&sweep);
    assert!(
        rs.refused()
            .starts_with("refused: cannot score without embeddings"),
        "{rs}"
    );
    // With a query not cached yet, the reference cannot score either.
    let cold = ctx.pair(T, "cold-cache", Fixture::Embedded);
    let (rs, py) = cold.run(&sweep);
    assert!(
        rs.refused()
            .starts_with("refused: cannot score without embeddings"),
        "{rs}"
    );
    match (warm, py) {
        (Some(warm), Some(py)) => {
            assert!(warm.ok().contains("VERDICT: "), "{warm}");
            assert_eq!(py.code, Some(1), "{py}");
            assert_eq!(
                py.stderr, "Ollama is down and some queries are not cached — cannot score.\n",
                "{py}"
            );
            ctx.asserted(&EVAL_NO_EMBEDDINGS);
        }
        _ => ctx.skip(T, "eval scoring without Ollama"),
    }

    // Nothing authored yet: nothing to score.
    let empty = ctx.pair(T, "no-cases", Fixture::Schema);
    let (rs, py) = empty.run(&cmd(&["eval", "--report"]));
    assert!(rs.refused().contains("eval --add-case"), "{rs}");
    match py {
        Some(py) => {
            assert_eq!(py.code, Some(1), "{py}");
            assert!(
                py.stderr
                    .starts_with("no eval cases in the DB (table eval_cases is empty)."),
                "{py}"
            );
            ctx.asserted(&EVAL_NO_CASES);
        }
        None => ctx.skip(T, "eval --report without cases"),
    }
}

fn the_eval_cases_are_written_without_sql(ctx: &Ctx) {
    let m = ctx.machine(T, "cases", Side::Rust, Fixture::Embedded);
    assert_eq!(
        m.run(&cmd(&["eval", "--last-run"])).ok(),
        "2025-09-30 12:00:00\n"
    );
    m.run(&cmd(&["eval", "--repoint", "x05", "5"])).ok();
    assert_eq!(
        m.value("SELECT expect_ids || '|' || expect_stamps FROM eval_cases WHERE id = 'x05'"),
        "5|2025-06-01 10:00:00"
    );
    let out = m.run(&cmd(&["eval", "--validate"]));
    assert!(
        out.ok()
            .contains("all cases point at live, current rows with matching timestamps."),
        "{out}"
    );

    m.run(&cmd(&[
        "eval",
        "--add-case",
        "--id",
        "x07",
        "--class",
        "paraphrase",
        "--table",
        "semantic",
        "--query",
        "where does the embedding model run",
        "--expect",
        "9",
    ]))
    .ok();
    assert_eq!(
        m.value("SELECT expect_stamps FROM eval_cases WHERE id = 'x07'"),
        "2025-07-04 11:00:00"
    );
    let out = m.run(&cmd(&[
        "eval",
        "--add-case",
        "--id",
        "x08",
        "--class",
        "paraphrase",
        "--table",
        "semantic",
        "--query",
        "a query",
        "--expect",
        "4",
    ]));
    assert!(
        out.refused().contains("4"),
        "a superseded target is refused:\n{out}"
    );
    m.run(&cmd(&["eval", "--retire-case", "x07"])).ok();
    assert_eq!(
        m.value("SELECT retired_at IS NOT NULL FROM eval_cases WHERE id = 'x07'"),
        "1"
    );
    assert_eq!(
        m.value("SELECT count(*) FROM eval_cases"),
        "7",
        "nothing is ever deleted"
    );
}
