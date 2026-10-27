//! `embed --missing`: no Python reference (rows without a vector did not
//! exist there). Active rows only, in batches, up to a limit, and the vector
//! is the one the fake Ollama gives the stored text.

use crate::harness::{Ctx, Fixture, Side, cmd};
use crate::runner::Test;
use crate::vectors;

pub const TESTS: &[Test] = crate::tests![
    embed_missing_fills_active_rows_in_batches_up_to_a_limit,
    embed_missing_refuses_without_ollama,
];

const T: &str = "embed";

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

fn embed_missing_fills_active_rows_in_batches_up_to_a_limit(ctx: &Ctx) {
    let m = ctx.machine(T, "fill", Side::Rust, Fixture::Full);
    // A retired row, a superseded row and a retired instruction without a
    // vector: none of them is active, so none may be filled.
    m.sql(
        "UPDATE semantic_memory SET embedding = NULL, embed_model = NULL WHERE id IN (4, 6);
         UPDATE instructions SET embedding = NULL, embed_model = NULL WHERE id = 4;",
    );
    let status = m.run(&cmd(&["status"]));
    assert_eq!(
        status.ok(),
        "READY 32\nunembedded: semantic 2, episodic 1, instructions 1 — run embed --missing\n"
    );

    let first = m.run(&cmd(&[
        "embed",
        "--missing",
        "--batch",
        "1",
        "--limit",
        "2",
    ]));
    first.ok();
    let left = m.run(&cmd(&["status"]));
    let line = left.ok().lines().nth(1).unwrap_or_default().to_string();
    let remaining: u64 = line
        .trim_start_matches("unembedded: ")
        .split(" — ")
        .next()
        .unwrap()
        .split(", ")
        .map(|part| part.rsplit(' ').next().unwrap().parse::<u64>().unwrap())
        .sum();
    assert_eq!(
        remaining, 2,
        "--limit 2 filled a different number of rows:\n{first}\n{left}"
    );

    m.run(&cmd(&["embed", "--missing"])).ok();
    assert_eq!(m.run(&cmd(&["status"])).ok(), "READY 32\n");

    for (table, id) in [
        ("semantic_memory", 21),
        ("semantic_memory", 22),
        ("episodic_memory", 9),
        ("instructions", 6),
    ] {
        let text = m.value(&format!("SELECT memory_text FROM {table} WHERE id = {id}"));
        assert_eq!(
            m.value(&format!(
                "SELECT hex(embedding) || '|' || embed_model FROM {table} WHERE id = {id}"
            )),
            format!("{}|bge-m3", hex(&vectors::blob(&text))),
            "{table} {id}"
        );
    }
    for (table, id) in [
        ("semantic_memory", 4),
        ("semantic_memory", 6),
        ("instructions", 4),
    ] {
        assert_eq!(
            m.value(&format!(
                "SELECT embedding IS NULL FROM {table} WHERE id = {id}"
            )),
            "1",
            "{table} {id} is not active and was filled"
        );
    }

    // The filled row now ranks in the main list, and no section is left over.
    let out = m.run(&cmd(&[
        "recall",
        "backup daily timer",
        "--table",
        "semantic",
    ]));
    let text = out.ok();
    assert!(!text.contains("without embedding"), "{text}");
    assert!(text.contains("      topic = backup-timer\n"), "{text}");
}

fn embed_missing_refuses_without_ollama(ctx: &Ctx) {
    let m = ctx.machine(T, "refused", Side::Rust, Fixture::Full);
    let out = m.run(&cmd(&["embed", "--missing"]).without_ollama());
    assert!(out.refused().contains("Ollama"), "{out}");
    assert_eq!(
        m.state(),
        ctx.fixture_state(Fixture::Full),
        "a refused embed wrote"
    );
}
