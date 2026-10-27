//! Recall: ranked output for a fixed query set, every flag, the keyword-only
//! path with Ollama refused, rows without a vector, and the refusals.

use crate::differences::{
    COMMAND_NAMES, RECALL_ALPHA_INVALID, RECALL_UNEMBEDDED, REFUSAL_EXIT, REFUSAL_PREFIX,
    USAGE_EXIT,
};
use crate::harness::{Cmd, Ctx, Fixture, cmd};
use crate::runner::Test;

pub const TESTS: &[Test] = crate::tests![
    ranked_output_matches_the_reference_for_a_fixed_query_set,
    keyword_only_output_matches_the_reference_with_ollama_refused,
    rows_without_a_vector_get_a_section_of_their_own,
    recall_refusals_match_the_reference,
];

const T: &str = "recall";

/// Every IDF scope these search holds at least five rows.
const QUERIES: &[(&str, &[&str])] = &[
    ("wal lock reader", &[]),
    ("backup retention weekly", &["--table", "semantic"]),
    ("busy retry backoff", &["--table", "episodic", "--k", "3"]),
    ("turso wal checkpoint", &["--project", "proj-alpha"]),
    ("refusal test exit code", &["--coworker", "ada"]),
    ("commit messages imperative", &["--coworker", "bob"]),
    ("gate", &["--k", "2"]),
    ("zsh bash word split", &[]),
    (
        "ollama embedding 1024",
        &["--table", "semantic", "--k", "1"],
    ),
];

fn search(query: &str, flags: &[&str]) -> Cmd {
    let mut args = vec!["recall", query];
    args.extend_from_slice(flags);
    cmd(&args)
}

/// The topics of a recall output, in order, per section header.
fn topics(out: &str) -> Vec<(String, Vec<String>)> {
    let mut sections: Vec<(String, Vec<String>)> = Vec::new();
    for line in out.lines() {
        if line.starts_with("===== ") {
            sections.push((line.to_string(), Vec::new()));
        } else if let Some(t) = line.strip_prefix("      topic = ") {
            sections
                .last_mut()
                .expect("a topic before any header")
                .1
                .push(t.to_string());
        }
    }
    sections
}

fn ranked_output_matches_the_reference_for_a_fixed_query_set(ctx: &Ctx) {
    let p = ctx.pair(T, "ranked", Fixture::Embedded);
    for (query, flags) in QUERIES {
        p.same(
            &format!("recall {query:?} {flags:?}"),
            &search(query, flags),
        );
    }
    let out = p.rs.run(&search("wal lock reader", &[]));
    let sections = topics(out.ok());
    assert_eq!(
        sections[0].0,
        "===== semantic (top 5; score = dist - 0.15*kw, lower is better; kw = IDF-weighted keyword share 0..1) ====="
    );
    assert_eq!(sections[0].1[0], "turso-wal", "{out}");
    assert_eq!(sections.len(), 2, "{out}");

    let out = p.rs.run(&search(
        "turso wal checkpoint",
        &["--project", "proj-alpha"],
    ));
    for line in out.ok().lines().filter(|l| l.starts_with("    project = ")) {
        assert_eq!(line, "    project = proj-alpha", "{out}");
    }
    let out = p.rs.run(&search(
        "commit messages imperative",
        &["--coworker", "ada"],
    ));
    assert!(
        !out.ok().contains("topic = commit-style"),
        "ada sees bob's row:\n{out}"
    );
    let out = p.rs.run(&search(
        "commit messages imperative",
        &["--coworker", "bob"],
    ));
    assert!(
        out.ok().contains("topic = commit-style"),
        "bob misses his own row:\n{out}"
    );

    // Another alpha reaches both the same way.
    p.same(
        "recall at RECALL_ALPHA=0.3",
        &search("wal lock reader", &[]).env("RECALL_ALPHA", "0.3"),
    );
    p.same_state("after the searches");
    assert_eq!(
        p.rs.state(),
        ctx.fixture_state(Fixture::Embedded),
        "a search wrote to the database"
    );
}

fn keyword_only_output_matches_the_reference_with_ollama_refused(ctx: &Ctx) {
    for fixture in [Fixture::Embedded, Fixture::Full] {
        let p = ctx.pair(T, &format!("keyword-only-{fixture:?}"), fixture);
        for (query, flags) in QUERIES {
            p.same(
                &format!("{fixture:?}: recall {query:?} {flags:?}, Ollama refused"),
                &search(query, flags).without_ollama(),
            );
        }
        let out =
            p.rs.run(&search("backup daily timer", &[]).without_ollama());
        let sections = topics(out.ok());
        assert_eq!(
            sections[0].0,
            "===== semantic (Ollama down — keyword-only, top 5) ====="
        );
        assert_eq!(
            sections[1].0,
            "===== episodic (Ollama down — keyword-only, top 5) ====="
        );
        if fixture == Fixture::Full {
            // Keyword-only ranks every row, embedded or not, in one section.
            assert!(sections[0].1.contains(&"backup-timer".to_string()), "{out}");
        }
    }
}

fn rows_without_a_vector_get_a_section_of_their_own(ctx: &Ctx) {
    let p = ctx.pair(T, "unembedded", Fixture::Full);
    // The binary's main ranking is the reference's over the rows that carry a
    // vector: drop the others on the reference's side.
    if let Some(py) = &p.py {
        py.sql(
            "DELETE FROM semantic_memory WHERE embedding IS NULL;
             DELETE FROM episodic_memory WHERE embedding IS NULL;",
        );
    }
    let query = search("backup daily timer", &[]);
    let out = p.differ(
        "recall over rows without a vector",
        &query,
        &[&RECALL_UNEMBEDDED],
    );
    let sections = topics(out.ok());
    let headers: Vec<&str> = sections.iter().map(|(h, _)| h.as_str()).collect();
    assert_eq!(
        headers,
        [
            "===== semantic (top 5; score = dist - 0.15*kw, lower is better; kw = IDF-weighted keyword share 0..1) =====",
            "===== semantic: 2 without embedding (keyword-only) =====",
            "===== episodic (top 5; score = dist - 0.15*kw, lower is better; kw = IDF-weighted keyword share 0..1) =====",
            "===== episodic: 1 without embedding (keyword-only) =====",
        ],
        "{out}"
    );
    assert_eq!(sections[1].1, ["backup-timer"], "{out}");
    assert!(
        !sections[0].1.contains(&"backup-timer".to_string()),
        "{out}"
    );
}

fn recall_refusals_match_the_reference(ctx: &Ctx) {
    let p = ctx.pair(T, "refusals", Fixture::Embedded);
    let out = p.differ(
        "recall --coworker nobody",
        &search("anything", &["--coworker", "nobody"]),
        &[&REFUSAL_EXIT],
    );
    assert_eq!(out.refused(), "refused: no coworker named 'nobody'.\n");

    let out = p.differ(
        "recall without a query",
        &cmd(&["recall"]),
        &[&REFUSAL_EXIT, &REFUSAL_PREFIX, &COMMAND_NAMES],
    );
    assert!(
        out.refused().starts_with("refused: provide a query"),
        "{out}"
    );

    let out = p.differ(
        "recall --table bogus",
        &search("x", &["--table", "bogus"]),
        &[&USAGE_EXIT],
    );
    out.usage();

    let (rs, py) = p.run(&search("x", &[]).env("RECALL_ALPHA", "abc"));
    assert!(
        rs.refused().starts_with("refused: RECALL_ALPHA=\"abc\""),
        "{rs}"
    );
    match py {
        Some(py) => {
            assert_eq!(py.code, Some(1), "{py}");
            assert!(
                py.stderr
                    .contains("ValueError: could not convert string to float: 'abc'"),
                "{py}"
            );
            ctx.asserted(&RECALL_ALPHA_INVALID);
        }
        None => ctx.skip(T, "recall with RECALL_ALPHA=abc"),
    }
    p.same_state("after the refusals");

    let p = ctx.pair(T, "missing", Fixture::None);
    let out = p.differ(
        "recall, no database",
        &search("x", &[]),
        &[&REFUSAL_EXIT, &REFUSAL_PREFIX],
    );
    assert!(
        out.refused().starts_with(
            "refused: memory DB missing — refusing to silently create an empty one.\n"
        ),
        "{out}"
    );
    assert!(!p.rs.db().exists());
    if let Some(py) = &p.py {
        assert!(!py.db().exists());
    }
}
