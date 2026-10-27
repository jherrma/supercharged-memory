//! Instructions: no Python reference (the table is new). Store, replace,
//! retire, search, `--get` and `--list`, and the state each leaves.

use crate::harness::{Ctx, Fixture, Side, cmd};
use crate::runner::Test;

pub const TESTS: &[Test] = crate::tests![instructions_are_stored_replaced_retired_and_found];

const T: &str = "instructions";

fn instructions_are_stored_replaced_retired_and_found(ctx: &Ctx) {
    let m = ctx.machine(T, "lifecycle", Side::Rust, Fixture::Embedded);
    let stored = m.run(&cmd(&[
        "instruct",
        "--topic",
        "deploy",
        "--source",
        "user-stated",
        "--model",
        "test-model",
        "--text",
        "Deploy only from a tagged commit, after the full suite passed.",
    ]));
    stored.ok();
    assert_eq!(m.value("SELECT max(id) FROM instructions"), "8");
    assert_eq!(
        m.value("SELECT embed_model FROM instructions WHERE id = 8"),
        "bge-m3"
    );

    let got = m.run(&cmd(&["instructions", "--get", "deploy"]));
    assert!(
        got.ok().contains("Deploy only from a tagged commit"),
        "{got}"
    );

    let replaced = m.run(&cmd(&[
        "instruct",
        "--topic",
        "deploy",
        "--replaces",
        "8",
        "--source",
        "user-stated",
        "--text",
        "Deploy only from a signed tag, after the full suite passed on the release build.",
    ]));
    replaced.ok();
    assert_eq!(
        m.value("SELECT retired_at IS NOT NULL FROM instructions WHERE id = 8"),
        "1"
    );
    let got = m.run(&cmd(&["instructions", "--get", "deploy"]));
    let text = got.ok();
    assert!(text.contains("signed tag"), "{text}");
    assert!(
        !text.contains("tagged commit"),
        "a replaced instruction is still current:\n{text}"
    );

    m.run(&cmd(&["instruct", "--retire", "3"])).ok();
    assert_eq!(
        m.value("SELECT retired_at IS NOT NULL FROM instructions WHERE id = 3"),
        "1"
    );
    let again = m.run(&cmd(&["instruct", "--retire", "3"]));
    again.refused();

    let found = m.run(&cmd(&["instructions", "review findings severity"]));
    let text = found.ok();
    let first = text
        .lines()
        .find_map(|l| l.strip_prefix("      topic = "))
        .unwrap_or_default();
    assert_eq!(first, "review-style", "{text}");
    assert!(
        !text.contains("in file order"),
        "a retired instruction was found:\n{text}"
    );

    let listed = m.run(&cmd(&["instructions", "--list"]));
    let text = listed.ok();
    for topic in [
        "release-checklist",
        "review-style",
        "bench-report",
        "standup",
        "handover",
        "deploy",
    ] {
        assert!(text.contains(topic), "{topic} missing from --list:\n{text}");
    }
    assert!(
        !text.contains("commit-message"),
        "a retired instruction is listed:\n{text}"
    );
    assert_eq!(
        m.value("SELECT count(*) FROM instructions"),
        "9",
        "nothing is ever deleted"
    );

    // Instructions are never loaded at session start, and sleep never sees them.
    let baseline = m.run(&cmd(&["recall", "--baseline"]));
    assert!(!baseline.ok().contains("signed tag"));
    m.run(&cmd(&["sleep", "--cluster", "--table", "instructions"]))
        .usage();
}
