//! Coworkers: the writes against the reference, then `--list` and `--load`,
//! which the reference left to ad-hoc SQL.

use crate::differences::{COWORKER_COMMA, COWORKER_NOOP, REFUSAL_EXIT, TRUST_REFUSED, USAGE_EXIT};
use crate::harness::{Ctx, Fixture, Side, cmd};
use crate::runner::Test;

pub const TESTS: &[Test] = crate::tests![
    coworker_writes_match_the_reference,
    list_and_load_show_the_current_state,
];

const T: &str = "coworkers";

fn coworker_writes_match_the_reference(ctx: &Ctx) {
    let p = ctx.pair(T, "writes", Fixture::Full);
    let out = p.same(
        "coworkers --add",
        &cmd(&[
            "coworkers",
            "--add",
            "--name",
            "cy",
            "--expertise",
            "test design",
            "--personality",
            "calm, thorough",
        ]),
    );
    assert_eq!(out.ok(), "added coworker 'cy'\n");
    let out = p.same(
        "coworkers --appraise, superseding the current appraisal",
        &cmd(&[
            "coworkers",
            "--appraise",
            "ada",
            "--trust",
            "autonomous",
            "--text",
            "Third review: autonomous on recovery work.",
        ]),
    );
    assert_eq!(
        out.ok(),
        "appraised 'ada': trust=autonomous (superseded appraisal 2)\n"
    );
    let out = p.same(
        "coworkers --appraise, the first appraisal",
        &cmd(&[
            "coworkers",
            "--appraise",
            "bob",
            "--trust",
            "trusted",
            "--text",
            "First review of bob.",
        ]),
    );
    assert_eq!(
        out.ok(),
        "appraised 'bob': trust=trusted (first appraisal)\n"
    );
    p.same(
        "coworkers --set-trust",
        &cmd(&["coworkers", "--set-trust", "cy", "trusted"]),
    );
    p.same(
        "coworkers --retire",
        &cmd(&["coworkers", "--retire", "ada"]),
    );
    p.same(
        "coworkers --reactivate",
        &cmd(&["coworkers", "--reactivate", "bob"]),
    );
    p.same_state("after the coworker writes");
    let m = &p.rs;
    assert_eq!(
        m.value(
            "SELECT group_concat(name || ':' || trust_level || ':' || active, ' ') FROM coworkers"
        ),
        "ada:autonomous:0 bob:trusted:1 cy:trusted:1"
    );
    assert_eq!(
        m.value("SELECT count(*) FROM appraisals WHERE superseded_by IS NULL"),
        "2"
    );
    assert_eq!(
        m.value("SELECT superseded_by FROM appraisals WHERE id = 2"),
        "3"
    );
    assert_eq!(
        m.value("SELECT period_start FROM appraisals WHERE id = 3"),
        "2025-06-01 08:00:00"
    );

    let out = p.differ(
        "coworkers --add of an existing name",
        &cmd(&[
            "coworkers",
            "--add",
            "--name",
            "ada",
            "--expertise",
            "x",
            "--personality",
            "y",
        ]),
        &[&REFUSAL_EXIT],
    );
    assert!(
        out.refused()
            .starts_with("refused: coworker 'ada' already exists (id 1)."),
        "{out}"
    );
    let out = p.differ(
        "coworkers --appraise of an unknown name",
        &cmd(&[
            "coworkers",
            "--appraise",
            "zed",
            "--trust",
            "trusted",
            "--text",
            "x",
        ]),
        &[&REFUSAL_EXIT],
    );
    assert_eq!(out.refused(), "refused: no coworker named 'zed'.\n");
    let (rs, py) = p.run(&cmd(&[
        "coworkers",
        "--appraise",
        "ada",
        "--trust",
        "boss",
        "--text",
        "x",
    ]));
    assert_eq!(
        rs.refused(),
        "refused: --trust must be one of ('supervised', 'trusted', 'autonomous').\n"
    );
    match py {
        Some(py) => {
            assert_eq!(py.code, Some(2), "{py}");
            assert!(
                py.stderr
                    .contains("error: argument --trust: invalid choice: 'boss'"),
                "{py}"
            );
            ctx.asserted(&TRUST_REFUSED);
        }
        None => ctx.skip(T, "coworkers --appraise with an unknown trust level"),
    }
    let out = p.differ(
        "coworkers with an unknown flag",
        &cmd(&["coworkers", "--promote", "ada"]),
        &[&USAGE_EXIT],
    );
    out.usage();
    p.same_state("after the refusals");

    let (rs, py) = p.run(&cmd(&[
        "coworkers",
        "--add",
        "--name",
        "ann,bea",
        "--expertise",
        "x",
        "--personality",
        "y",
    ]));
    assert!(rs.refused().contains("comma"), "{rs}");
    match py {
        Some(py) => {
            assert_eq!(py.ok(), "added coworker 'ann,bea'\n");
            ctx.asserted(&COWORKER_COMMA);
        }
        None => ctx.skip(T, "coworkers --add with a comma in the name"),
    }

    let (rs, py) = p.run(&cmd(&["coworkers", "--retire", "ada"]));
    assert!(rs.refused().contains("already retired"), "{rs}");
    match py {
        Some(py) => {
            assert_eq!(py.ok(), "'ada' retired\n");
            ctx.asserted(&COWORKER_NOOP);
        }
        None => ctx.skip(T, "coworkers --retire of a retired coworker"),
    }
}

fn list_and_load_show_the_current_state(ctx: &Ctx) {
    let m = ctx.machine(T, "reads", Side::Rust, Fixture::Full);
    let out = m.run(&cmd(&["coworkers", "--list"]));
    let text = out.ok();
    assert!(text.contains("ada"), "{text}");
    assert!(text.contains("bob"), "{text}");

    let out = m.run(&cmd(&["coworkers", "--load", "ada"]));
    let text = out.ok();
    assert!(
        text.contains("Second review: reliable on recovery work."),
        "the current appraisal:\n{text}"
    );
    assert!(
        !text.contains("First review"),
        "a superseded appraisal:\n{text}"
    );
    assert!(
        text.contains("Every refusal needs a test"),
        "the feedback tagged to ada since then:\n{text}"
    );
    assert!(
        !text.contains("Commit messages are short"),
        "bob's feedback:\n{text}"
    );

    let out = m.run(&cmd(&["coworkers", "--load", "bob"]));
    let first = out.ok().lines().next().unwrap_or_default().to_lowercase();
    assert!(
        first.contains("retired") || first.contains("inactive"),
        "an inactive coworker says so first:\n{out}"
    );
    assert_eq!(m.state(), ctx.fixture_state(Fixture::Full), "a read wrote");
}
