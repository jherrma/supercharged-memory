//! `runbook <name>` prints the runbook compiled into the binary, and the text
//! is the file in the repository byte for byte, with `{{EXE}}` replaced by the
//! path of the binary that printed it.

use crate::harness::{Ctx, Fixture, Side, cmd, repo};
use crate::runner::Test;

pub const TESTS: &[Test] = crate::tests![every_runbook_is_its_source_file];

const T: &str = "runbooks";

const RUNBOOKS: &[(&str, &str)] = &[
    ("setup", "SETUP.md"),
    ("update", "UPDATE.md"),
    ("sleep", "SLEEP.md"),
    ("deep-sleep", "DEEP-SLEEP.md"),
    ("migrate", "MIGRATE-EXISTING-MEMORY.md"),
    ("new-coworker", "NEW-COWORKER.md"),
];

fn every_runbook_is_its_source_file(ctx: &Ctx) {
    // No database: a runbook needs none.
    let m = ctx.machine(T, "print", Side::Rust, Fixture::None);
    for (name, file) in RUNBOOKS {
        let source = std::fs::read_to_string(repo().join("instructions").join(file)).unwrap();
        let expected = source.replace("{{EXE}}", "{ROOT}/app/supercharged-memory");
        let out = m.run(&cmd(&["runbook", name]));
        assert_eq!(
            out.ok(),
            expected,
            "runbook {name} differs from instructions/{file}"
        );
        assert_eq!(out.stderr, "");
    }
    let listing = m.run(&cmd(&["runbook"]));
    let names: Vec<&str> = listing
        .ok()
        .lines()
        .map(|l| l.split_whitespace().next().unwrap())
        .collect();
    let expected: Vec<&str> = RUNBOOKS.iter().map(|(n, _)| *n).collect();
    assert_eq!(names, expected);
    let unknown = m.run(&cmd(&["runbook", "nope"]));
    assert!(
        unknown.refused().contains("setup"),
        "an unknown name lists the real ones:\n{unknown}"
    );
    assert!(!m.db().exists());
}
