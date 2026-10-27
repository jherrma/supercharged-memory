//! Session start: `status` in every state, `config`, and the slots every
//! session loads (`recall --baseline`, `--topics`, `--count`, `--candidates`).

use crate::differences::{COMMAND_NAMES, STATUS_ERROR_TEXT, STATUS_REASON, STATUS_UNEMBEDDED};
use crate::harness::{Ctx, Fixture, Side, cmd};
use crate::runner::Test;

pub const TESTS: &[Test] = crate::tests![
    status_reports_every_state_as_the_reference_does,
    config_reads_the_machines_own_dotenv,
    the_session_slots_match_the_reference,
    zero_baseline_rules_are_a_warning_not_an_answer,
];

const T: &str = "session";

fn status_reports_every_state_as_the_reference_does(ctx: &Ctx) {
    let status = cmd(&["status"]);

    // MISSING names the configured path and what else exists, and creates nothing.
    let p = ctx.pair(T, "missing", Fixture::None);
    p.place(Fixture::Schema, "data/turso/old.db");
    p.write(
        "backups/2025-01-01-supercharged-memory.sql.gz",
        "not read, only found",
    );
    let out = p.same("status, no database", &status);
    assert_eq!(
        out.ok(),
        "MISSING\n  configured path : {ROOT}/data/turso/supercharged-memory.db\n  \
         path came from  : SUPERCHARGED_MEMORY_TURSO_PATH env var\n  \
         CANDIDATE DB    : {ROOT}/data/turso/old.db (0 memories)\n  \
         CANDIDATE BACKUP: {ROOT}/backups/2025-01-01-supercharged-memory.sql.gz\n  \
         DO NOT create or overwrite a database — ask the user first.\n"
    );
    assert!(!p.rs.db().exists(), "status created a database");

    let p = ctx.pair(T, "empty", Fixture::Schema);
    assert_eq!(p.same("status, empty database", &status).ok(), "EMPTY\n");
    p.same_state("after status on an empty database");

    let p = ctx.pair(T, "ready", Fixture::Embedded);
    assert_eq!(
        p.same("status, every row embedded", &status).ok(),
        "READY 32\n"
    );

    let p = ctx.pair(T, "unembedded", Fixture::Full);
    let out = p.differ(
        "status, rows without a vector",
        &status,
        &[&STATUS_UNEMBEDDED],
    );
    assert_eq!(
        out.ok(),
        "READY 32\nunembedded: semantic 2, episodic 1, instructions 1 — run embed --missing\n"
    );

    let p = ctx.pair(T, "degraded", Fixture::Embedded);
    let out = p.differ(
        "status, Ollama refused",
        &status.clone().without_ollama(),
        &[&STATUS_REASON],
    );
    let lines: Vec<&str> = out.ok().lines().collect();
    assert_eq!(lines[0], "DEGRADED 32");
    assert!(
        lines[1].starts_with("no embeddings: Ollama unreachable at http://127.0.0.1:"),
        "{out}"
    );

    // Without embeddings the answer is DEGRADED even when nothing is stored.
    let p = ctx.pair(T, "degraded-empty", Fixture::Schema);
    let out = p.differ(
        "status, empty and Ollama refused",
        &status.clone().without_ollama(),
        &[&STATUS_REASON],
    );
    assert!(out.ok().starts_with("DEGRADED 0\n"), "{out}");

    let p = ctx.pair(T, "error", Fixture::None);
    p.write(
        "data/turso/supercharged-memory.db",
        &"not a database ".repeat(300),
    );
    let out = p.differ(
        "status, a file that is no database",
        &status,
        &[&STATUS_ERROR_TEXT],
    );
    assert!(out.ok().starts_with("ERROR "), "{out}");
    assert_eq!(out.stdout.lines().count(), 1, "{out}");
}

fn config_reads_the_machines_own_dotenv(ctx: &Ctx) {
    let m = ctx
        .machine(T, "config", Side::Rust, Fixture::Embedded)
        .dotenv_only();
    let out = m.run(&cmd(&["config"]));
    let text = out.ok();
    let line = |key: &str| {
        text.lines()
            .find(|l| l.starts_with(&format!("{key} ")))
            .unwrap_or_else(|| panic!("no {key} line:\n{text}"))
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    assert_eq!(
        line("SUPERCHARGED_MEMORY_TURSO_PATH"),
        "SUPERCHARGED_MEMORY_TURSO_PATH = {ROOT}/data/turso/supercharged-memory.db [.env]"
    );
    assert_eq!(line("BACKUP_DIR"), "BACKUP_DIR = {ROOT}/backups [.env]");
    assert_eq!(line("EMBED_MODEL"), "EMBED_MODEL = bge-m3 [default]");
    assert_eq!(
        line("TURSO_BIN"),
        "TURSO_BIN = {ROOT}/home/.turso/tursodb [default]"
    );
    assert!(
        line("OLLAMA_URL").starts_with("OLLAMA_URL = http://127.0.0.1:"),
        "{text}"
    );
    assert!(text.contains(".env  : {ROOT}/app/.env (found)"), "{text}");
    // Nothing of the real machine shows through.
    if let Some(home) = std::env::var_os("HOME") {
        let home = home.to_string_lossy().to_string();
        assert!(
            !text.contains(&format!("{home}/")),
            "the real home leaked into config:\n{text}"
        );
    }
}

fn the_session_slots_match_the_reference(ctx: &Ctx) {
    let p = ctx.pair(T, "slots", Fixture::Embedded);
    let out = p.same("recall --baseline", &cmd(&["recall", "--baseline"]));
    let text = out.ok();
    assert!(
        text.starts_with(
            "===== baseline (load every session, follow for the whole session; 2 rule(s)) =====\n"
        ),
        "{text}"
    );
    assert!(text.contains("      topic = commit-policy\n"), "{text}");
    assert!(text.contains("      topic = pii-rule\n"), "{text}");

    let out = p.same("recall --topics", &cmd(&["recall", "--topics"]));
    let text = out.ok();
    assert!(text.starts_with(
        "===== topic index (load every session; 3 topic(s); rebuilt by sleep) =====\n"
    ));
    // Newest first.
    let order: Vec<&str> = text
        .lines()
        .filter_map(|l| l.strip_prefix("   topic = "))
        .collect();
    assert_eq!(order, ["rust", "backup", "turso"], "{text}");

    assert_eq!(
        p.same("recall --count", &cmd(&["recall", "--count"])).ok(),
        "32\n"
    );

    p.place(Fixture::Schema, "data/turso/old.db");
    let out = p.same("recall --candidates", &cmd(&["recall", "--candidates"]));
    assert_eq!(
        out.ok(),
        "configured path : {ROOT}/data/turso/supercharged-memory.db (exists)\n\
         CANDIDATE DB    : {ROOT}/data/turso/old.db (0 memories)\n\
         Ask the user before switching, restoring, or overwriting anything.\n"
    );
    p.same_state("after the session slots");
    assert_eq!(
        p.rs.state(),
        ctx.fixture_state(Fixture::Embedded),
        "a slot wrote to the database"
    );
}

fn zero_baseline_rules_are_a_warning_not_an_answer(ctx: &Ctx) {
    let p = ctx.pair(T, "no-baseline", Fixture::Schema);
    let out = p.differ(
        "recall --baseline, none stored",
        &cmd(&["recall", "--baseline"]),
        &[&COMMAND_NAMES],
    );
    let text = out.ok();
    assert!(
        text.contains("WARNING: zero baseline rules loaded, which is NOT proof that none exist."),
        "{text}"
    );
    assert!(text.contains("`supercharged-memory status`"), "{text}");
    let out = p.same(
        "recall --topics, none stored",
        &cmd(&["recall", "--topics"]),
    );
    assert!(
        out.ok().starts_with(
            "===== topic index (load every session; 0 topic(s); rebuilt by sleep) ====="
        )
    );
}
