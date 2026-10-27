//! Store: `remember` for both tables, `--supersedes`, every refusal, the
//! write without an embedding, and `backfill`.

use crate::differences::{
    BACKFILL_SKIPS, COMMAND_NAMES, DEGRADED_WRITE, REFUSAL_EXIT, REFUSAL_PREFIX,
};
use crate::harness::{Cmd, Ctx, Fixture, cmd};
use crate::runner::Test;

pub const TESTS: &[Test] = crate::tests![
    semantic_and_episodic_writes_match_the_reference,
    supersedes_revises_one_row_or_merges_several,
    every_write_refusal_matches_the_reference,
    a_write_without_ollama_stores_the_row_without_a_vector,
    backfill_matches_the_reference_and_reports_every_skip,
];

const T: &str = "store";

fn remember(args: &[&str]) -> Cmd {
    let mut v = vec!["remember"];
    v.extend_from_slice(args);
    v.extend_from_slice(&["--source", "self-observed", "--model", "test-model"]);
    cmd(&v)
}

fn semantic_and_episodic_writes_match_the_reference(ctx: &Ctx) {
    let p = ctx.pair(T, "writes", Fixture::Embedded);
    let out = p.same(
        "remember a semantic row with keywords",
        &remember(&[
            "--table",
            "semantic",
            "--topic",
            "new-fact",
            "--category",
            "reference",
            "--keywords",
            "gamma, delta",
            "--text",
            "A brand new fact about gamma rays and delta waves.",
        ]),
    );
    assert_eq!(out.ok(), "stored semantic memory (new-fact)\n");
    p.same(
        "remember a dated project row",
        &remember(&[
            "--table",
            "semantic",
            "--topic",
            "dated",
            "--category",
            "project",
            "--project",
            "proj-alpha",
            "--created-at",
            "2025-01-05",
            "--text",
            "A dated fact about epsilon particles.",
        ]),
    );
    let out = p.same(
        "remember an episodic row",
        &remember(&[
            "--table",
            "episodic",
            "--topic",
            "an-event",
            "--event-type",
            "note",
            "--importance",
            "routine",
            "--text",
            "An event about zeta functions.",
        ]),
    );
    assert_eq!(out.ok(), "stored episodic memory (an-event)\n");
    p.same(
        "remember a row tagged to a coworker",
        &remember(&[
            "--table",
            "semantic",
            "--topic",
            "ada-note",
            "--category",
            "feedback",
            "--coworker",
            "ada",
            "--text",
            "Ada asks for a failing test before any fix.",
        ]),
    );
    p.same_state("after the writes");

    let m = &p.rs;
    assert_eq!(
        m.value("SELECT memory_text FROM semantic_memory WHERE topic = 'new-fact'"),
        "A brand new fact about gamma rays and delta waves.\n\nKeywords: gamma, delta"
    );
    assert_eq!(
        m.value("SELECT embed_model FROM semantic_memory WHERE topic = 'new-fact'"),
        "bge-m3"
    );
    assert_eq!(
        m.value("SELECT embedding IS NOT NULL FROM semantic_memory WHERE topic = 'new-fact'"),
        "1"
    );
    assert_eq!(
        m.value(
            "SELECT created_at || '|' || updated_at FROM semantic_memory WHERE topic = 'dated'"
        ),
        "2025-01-05 00:00:00|2025-01-05 00:00:00"
    );
    assert_eq!(m.value("SELECT count(*) FROM episodic_memory"), "11");
    assert_eq!(
        m.value(
            "SELECT count(*) FROM memory_coworkers WHERE coworker_id = 1 AND memory_id = \
             (SELECT id FROM semantic_memory WHERE topic = 'ada-note')"
        ),
        "1"
    );
}

fn supersedes_revises_one_row_or_merges_several(ctx: &Ctx) {
    let p = ctx.pair(T, "supersede", Fixture::Embedded);
    p.same(
        "remember --supersedes one row",
        &remember(&[
            "--table",
            "semantic",
            "--topic",
            "backup-retention",
            "--category",
            "project",
            "--supersedes",
            "5",
            "--text",
            "Backups keep three daily dumps, four weekly dumps and one monthly dump.",
        ]),
    );
    p.same(
        "remember --supersedes two rows",
        &remember(&[
            "--table",
            "semantic",
            "--topic",
            "gates",
            "--category",
            "reference",
            "--supersedes",
            "12,13",
            "--text",
            "Before handing work over, clippy and fmt must both pass on all targets.",
        ]),
    );
    let out = p.differ(
        "remember --supersedes a superseded row",
        &remember(&[
            "--table",
            "semantic",
            "--topic",
            "stale",
            "--category",
            "reference",
            "--supersedes",
            "4",
            "--text",
            "Something about stale rows.",
        ]),
        &[&REFUSAL_EXIT],
    );
    assert!(
        out.refused()
            .starts_with("refused: --supersedes 4: no current"),
        "{out}"
    );
    p.same_state("after the revisions");

    let m = &p.rs;
    assert_eq!(
        m.value("SELECT superseded_by FROM semantic_memory WHERE id = 5"),
        "23"
    );
    assert_eq!(
        m.value("SELECT group_concat(superseded_by) FROM semantic_memory WHERE id IN (12, 13)"),
        "24,24"
    );
    assert_eq!(
        m.value("SELECT superseded_by FROM semantic_memory WHERE id = 4"),
        "5"
    );
    assert_eq!(
        m.value("SELECT count(*) FROM semantic_memory WHERE superseded_by IS NULL"),
        "20"
    );
}

fn every_write_refusal_matches_the_reference(ctx: &Ctx) {
    let p = ctx.pair(T, "refusals", Fixture::Embedded);
    let near =
        "The embedding model is bge-m3 with 1024 dimensions, served by Ollama on port 11434.";
    let long = "x".repeat(1990);
    let cases: Vec<(&str, Cmd, Vec<&crate::differences::Difference>, &str)> = vec![
        (
            "a near-duplicate",
            remember(&[
                "--table",
                "semantic",
                "--topic",
                "dup",
                "--category",
                "reference",
                "--keywords",
                "ollama, embedding, dimensions",
                "--text",
                near,
            ]),
            vec![&REFUSAL_EXIT],
            "refused: a near-duplicate already exists (cosine ",
        ),
        (
            "over the cap once the keywords are on",
            remember(&[
                "--table",
                "semantic",
                "--topic",
                "long",
                "--category",
                "reference",
                "--keywords",
                "one, two, three",
                "--text",
                &long,
            ]),
            vec![&REFUSAL_EXIT],
            "refused: memory is 2017 chars; hard cap 2000",
        ),
        (
            "a table holding another model",
            remember(&[
                "--table",
                "semantic",
                "--topic",
                "om",
                "--category",
                "reference",
                "--text",
                "A fact stored under another model.",
            ])
            .env("EMBED_MODEL", "other-model"),
            vec![&REFUSAL_EXIT],
            "refused: semantic_memory has rows embedded with a different model than 'other-model'.",
        ),
        (
            "--created-at that is no date",
            remember(&[
                "--table",
                "semantic",
                "--topic",
                "bad",
                "--category",
                "reference",
                "--created-at",
                "March 2024",
                "--text",
                "A fact with a bad date.",
            ]),
            vec![&REFUSAL_EXIT, &COMMAND_NAMES],
            "refused: --created-at 'March 2024' is not a date this system can store.",
        ),
        (
            "an empty --created-at",
            remember(&[
                "--table",
                "semantic",
                "--topic",
                "bad",
                "--category",
                "reference",
                "--created-at",
                "",
                "--text",
                "A fact with no date.",
            ]),
            vec![&REFUSAL_EXIT],
            "refused: --created-at is empty.",
        ),
        (
            "an empty --text",
            remember(&[
                "--table",
                "semantic",
                "--topic",
                "bad",
                "--category",
                "reference",
                "--text",
                "  ",
            ]),
            vec![&REFUSAL_EXIT],
            "refused: --text is empty.",
        ),
        (
            "a baseline row without confirmation",
            remember(&[
                "--table",
                "semantic",
                "--topic",
                "rule",
                "--category",
                "baseline",
                "--text",
                "A new rule.",
            ]),
            vec![&REFUSAL_EXIT],
            "refused: 'baseline' memories load every session",
        ),
        (
            "an unknown coworker",
            remember(&[
                "--table",
                "semantic",
                "--topic",
                "c",
                "--category",
                "reference",
                "--coworker",
                "nobody",
                "--text",
                "A fact for nobody.",
            ]),
            vec![&REFUSAL_EXIT],
            "refused: no coworker named 'nobody'.",
        ),
    ];
    for (what, command, diffs, expect) in &cases {
        let out = p.differ(&format!("remember, {what}"), command, diffs);
        assert!(out.refused().starts_with(expect), "{what}:\n{out}");
    }
    p.same_state("after every refusal");
    assert_eq!(
        p.rs.state(),
        ctx.fixture_state(Fixture::Embedded),
        "a refusal wrote"
    );

    // --force stores the near-duplicate after all.
    let out = p.same(
        "remember a near-duplicate with --force",
        &remember(&[
            "--table",
            "semantic",
            "--topic",
            "dup",
            "--category",
            "reference",
            "--force",
            "--text",
            near,
        ]),
    );
    assert_eq!(out.ok(), "stored semantic memory (dup)\n");
    p.same_state("after --force");

    let p = ctx.pair(T, "missing", Fixture::None);
    let out = p.differ(
        "remember, no database",
        &remember(&[
            "--table",
            "semantic",
            "--topic",
            "c",
            "--category",
            "reference",
            "--text",
            "A fact.",
        ]),
        &[&REFUSAL_EXIT, &REFUSAL_PREFIX],
    );
    assert!(
        out.refused().starts_with("refused: memory DB missing"),
        "{out}"
    );
    assert!(!p.rs.db().exists());
}

fn a_write_without_ollama_stores_the_row_without_a_vector(ctx: &Ctx) {
    let p = ctx.pair(T, "degraded", Fixture::Embedded);
    let write = remember(&[
        "--table",
        "semantic",
        "--topic",
        "deg",
        "--category",
        "reference",
        "--text",
        "A fact stored while Ollama is down.",
    ])
    .without_ollama();
    let (rs, py) = p.run(&write);
    assert!(
        rs.ok()
            .starts_with("stored semantic memory (deg) WITHOUT embedding — Ollama unreachable at "),
        "{rs}"
    );
    assert!(rs.stderr.starts_with("warning: "), "{rs}");
    assert_eq!(
        p.rs.value("SELECT coalesce(embed_model, 'NULL') || '|' || (embedding IS NULL) FROM semantic_memory WHERE topic = 'deg'"),
        "NULL|1"
    );
    match py {
        Some(py) => {
            assert_eq!(py.code, Some(1), "{py}");
            assert!(
                py.stderr.starts_with("refused: Ollama not reachable at "),
                "{py}"
            );
            assert_eq!(
                p.py.as_ref()
                    .unwrap()
                    .value("SELECT count(*) FROM semantic_memory WHERE topic = 'deg'"),
                "0"
            );
            ctx.asserted(&DEGRADED_WRITE);
        }
        None => ctx.skip(T, "remember without Ollama"),
    }
}

fn notes(p: &crate::harness::Pair) {
    p.write(
        "notes/wal-notes.md",
        "Readers never block the multiprocess writer in tursodb.\n",
    );
    p.write(
        "notes/sub/retry_policy.md",
        "Retry three times with a growing backoff.\n",
    );
    p.write("notes/big.md", &"z".repeat(2100));
    p.write("notes/empty.md", "\n");
    p.write("notes/skip.txt", "not matched by the glob");
}

fn backfill_matches_the_reference_and_reports_every_skip(ctx: &Ctx) {
    let p = ctx.pair(T, "backfill", Fixture::Embedded);
    notes(&p);
    let import = cmd(&["backfill", "--dir", "{ROOT}/notes"]);
    let (rs, py) = p.run(&import);
    assert_eq!(
        rs.ok(),
        "SKIP big.md: 2100 chars > 2000 cap — split it manually\n\
         SKIP empty.md: empty — nothing to store\n\
         stored sub/retry_policy.md\n\
         stored wal-notes.md\n\
         backfilled 2 file(s); skipped 2\n"
    );
    p.same_state("after one backfill");
    let (again, py_again) = p.run(&import);
    let text = again.ok();
    assert!(
        text.contains("SKIP wal-notes.md: near-duplicate of id "),
        "{again}"
    );
    assert!(
        text.ends_with("backfilled 0 file(s); skipped 4\n"),
        "{again}"
    );
    assert_eq!(
        p.rs.value("SELECT count(*) FROM semantic_memory WHERE source = 'backfill'"),
        "2"
    );
    match (py, py_again) {
        (Some(py), Some(py_again)) => {
            assert_eq!(
                py.ok(),
                "SKIP big.md: 2100 chars > 2000 cap — split it manually\n\
                 stored retry_policy.md\n\
                 stored wal-notes.md\n\
                 backfilled 2 file(s); skipped 1\n"
            );
            assert!(
                py_again.ok().ends_with("backfilled 2 file(s); skipped 1\n"),
                "{py_again}"
            );
            let py_m = p.py.as_ref().unwrap();
            assert_eq!(
                py_m.value("SELECT count(*) FROM semantic_memory WHERE source = 'backfill'"),
                "4"
            );
            ctx.asserted(&BACKFILL_SKIPS);
        }
        _ => ctx.skip(T, "backfill"),
    }

    // A file that is not UTF-8 (Latin-1 bytes).
    let p = ctx.pair(T, "backfill-latin1", Fixture::Embedded);
    p.write_bytes("latin1/cafe.md", b"Caf\xe9 au lait is served at nine.\n");
    let (rs, py) = p.run(&cmd(&["backfill", "--dir", "{ROOT}/latin1"]));
    assert_eq!(
        rs.ok(),
        "SKIP cafe.md: not valid UTF-8 — convert it first\nbackfilled 0 file(s); skipped 1\n"
    );
    assert_eq!(p.rs.state(), ctx.fixture_state(Fixture::Embedded));
    match py {
        Some(py) => {
            assert_eq!(py.ok(), "stored cafe.md\nbackfilled 1 file(s); skipped 0\n");
            let py_m = p.py.as_ref().unwrap();
            assert!(
                py_m.value("SELECT memory_text FROM semantic_memory WHERE topic = 'cafe'")
                    .starts_with("Caf\u{fffd} au lait"),
            );
        }
        None => ctx.skip(T, "backfill of a non-UTF-8 file"),
    }
}
