//! The migration probe and the install: `find-existing-memory` against the
//! reference on fixture trees, `init` and `update` (which replace the
//! reference's installer and are not compared line by line), and the dropped
//! `seed`.

use crate::differences::{
    COMMAND_NAMES, FIND_QUIET_EXIT, FIND_REPORTED_SKIPS, INSTALLER_REPLACED, SEED_DROPPED,
};
use crate::harness::{Ctx, Fixture, Pair, Side, cmd, repo};
use crate::runner::Test;

pub const TESTS: &[Test] = crate::tests![
    the_migration_probe_matches_the_reference_on_fixture_trees,
    a_skip_the_reference_drops_is_reported,
    init_plans_then_applies_on_a_clean_machine,
    update_migrates_a_database_from_before_the_instructions_table,
    seed_has_no_counterpart,
];

const T: &str = "install";

/// A config dir holding every kind the probe tells apart.
fn tree(p: &Pair) {
    p.write("claude/CLAUDE.md", "# My notes\n\nAlways answer briefly.\n");
    p.write("claude/memory/MEMORY.md", "- [a](a.md) one fact\n");
    p.write("claude/memory/a.md", "A fact worth keeping.\n");
    p.write("claude/memory/team/B.MD", "A shared fact.\n");
    p.write("claude/memory/empty.md", "  \n");
    p.write("claude/memory/big.md", &"y".repeat(1850));
    p.write("claude/memory/notes.txt", "not memory");
    p.write(
        "claude/projects/-home-user-repo/memory/p.md",
        "A project fact.\nSecond line.\r\nThird.\n",
    );
}

fn json(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("not JSON ({e}):\n{text}"))
}

fn the_migration_probe_matches_the_reference_on_fixture_trees(ctx: &Ctx) {
    let p = ctx.pair(T, "probe", Fixture::None);
    tree(&p);
    let out = p.differ(
        "find-existing-memory, a populated tree",
        &cmd(&["find-existing-memory"]),
        &[&COMMAND_NAMES],
    );
    let report = json(out.ok());
    assert_eq!(report["n_memory_files"], 4, "{out}");
    assert_eq!(report["nothing_to_do"], false, "{out}");
    assert_eq!(
        report["over_max_text"].as_array().map(Vec::len),
        Some(1),
        "{out}"
    );
    let quiet = p.same(
        "find-existing-memory --quiet, found",
        &cmd(&["find-existing-memory", "--quiet"]),
    );
    assert_eq!(quiet.ok(), "found\n");

    let p = ctx.pair(T, "probe-empty", Fixture::None);
    let quiet = p.differ(
        "find-existing-memory --quiet, nothing",
        &cmd(&["find-existing-memory", "--quiet"]),
        &[&FIND_QUIET_EXIT],
    );
    assert_eq!(quiet.ok(), "nothing\n");
    let out = p.differ(
        "find-existing-memory, an empty config dir",
        &cmd(&["find-existing-memory"]),
        &[&COMMAND_NAMES],
    );
    assert_eq!(json(out.ok())["nothing_to_do"], true);
    // Read-only: no database, no Ollama, nothing written.
    assert!(!p.rs.db().exists());
}

#[cfg(unix)]
fn a_skip_the_reference_drops_is_reported(ctx: &Ctx) {
    let p = ctx.pair(T, "dangling", Fixture::None);
    p.write("claude/CLAUDE.md", "");
    p.symlink("claude/memory", "gone");
    let (rs, py) = p.run(&cmd(&["find-existing-memory"]));
    let report = json(rs.ok());
    assert_eq!(report["nothing_to_do"], false, "{rs}");
    let unreadable = report["unreadable"].to_string();
    assert!(unreadable.contains("{ROOT}/claude/memory"), "{rs}");
    match py {
        Some(py) => {
            let reference = json(py.ok());
            assert_eq!(reference["nothing_to_do"], true, "{py}");
            assert_eq!(reference["unreadable"], serde_json::json!([]), "{py}");
            ctx.asserted(&FIND_REPORTED_SKIPS);
        }
        None => ctx.skip(T, "find-existing-memory over a dangling memory/ symlink"),
    }
}

#[cfg(not(unix))]
fn a_skip_the_reference_drops_is_reported(ctx: &Ctx) {
    ctx.skip(
        T,
        "find-existing-memory over a dangling memory/ symlink (needs unix symlinks)",
    );
}

fn init_plans_then_applies_on_a_clean_machine(ctx: &Ctx) {
    let m = ctx.bare_machine(T, "clean");
    let init = [
        "init",
        "--db-path",
        "{ROOT}/data/turso/supercharged-memory.db",
        "--backup-dir",
        "{ROOT}/backups",
        "--episodic-mode",
        "major-actions",
    ];
    let dry = m.run(&cmd(&init));
    let text = dry.ok();
    assert!(
        text.starts_with("init: dry run — nothing has been written."),
        "{dry}"
    );
    let last = text.lines().last().unwrap_or_default();
    assert!(last.starts_with("Ask the user to confirm"), "{dry}");
    assert!(!m.db().exists(), "a dry run created the database");
    assert!(!m.path("app/.env").exists(), "a dry run wrote the .env");
    assert!(
        !m.path("claude/CLAUDE.md").exists(),
        "a dry run wrote CLAUDE.md"
    );

    let mut apply = init.to_vec();
    apply.push("-y");
    let done = m.run(&cmd(&apply));
    let text = done.ok();
    assert!(text.starts_with("init -y: applying this plan."), "{done}");
    assert!(
        text.contains("created {ROOT}/data/turso/supercharged-memory.db from the embedded schema"),
        "{done}"
    );
    let dotenv = std::fs::read_to_string(m.path("app/.env")).unwrap();
    let root = m.root.display().to_string();
    assert_eq!(
        dotenv,
        format!(
            "SUPERCHARGED_MEMORY_TURSO_PATH={root}/data/turso/supercharged-memory.db\n\
             BACKUP_DIR={root}/backups\n\
             SUPERCHARGED_MEMORY_VERSION={}\n\
             EMBEDDINGS=on\n",
            env!("CARGO_PKG_VERSION")
        )
    );
    let claude = std::fs::read_to_string(m.path("claude/CLAUDE.md")).unwrap();
    assert!(
        claude.starts_with("<!-- BEGIN agentic-memory (managed by supercharged-memory) -->\n"),
        "{claude}"
    );
    assert!(
        claude.contains(&format!("{root}/app/supercharged-memory")),
        "the block names another binary"
    );

    // The machine now runs on its .env alone.
    assert_eq!(m.run(&cmd(&["status"])).ok(), "EMPTY\n");
    let stored = m.run(&cmd(&[
        "remember",
        "--table",
        "semantic",
        "--topic",
        "first",
        "--category",
        "user",
        "--text",
        "The first memory on a new machine.",
    ]));
    assert_eq!(stored.ok(), "stored semantic memory (first)\n");
    assert_eq!(m.run(&cmd(&["status"])).ok(), "READY 1\n");

    // A second init -y on the set-up machine changes nothing.
    let before = (m.read("app/.env"), m.read("claude/CLAUDE.md"), m.state());
    m.run(&cmd(&apply)).ok();
    assert_eq!(
        (m.read("app/.env"), m.read("claude/CLAUDE.md"), m.state()),
        before
    );

    assert!(repo().join("scripts/install-claude-md.sh").exists());
    ctx.asserted(&INSTALLER_REPLACED);
}

fn update_migrates_a_database_from_before_the_instructions_table(ctx: &Ctx) {
    let m = ctx.machine(T, "update", Side::Rust, Fixture::PreInstructions);
    // Without a recorded version there is nothing to update from.
    let out = m.run(&cmd(&["update"]));
    assert!(out.refused().contains("init"), "{out}");

    m.append(
        "app/.env",
        &format!(
            "SUPERCHARGED_MEMORY_VERSION={}\n",
            env!("CARGO_PKG_VERSION")
        ),
    );
    let block = m.run(&cmd(&[
        "update",
        "--print-block",
        "--episodic-mode",
        "major-actions",
    ]));
    // The block as printed, with the machine's real paths in it.
    let root = m.root.display().to_string();
    m.write("claude/CLAUDE.md", &block.ok().replace("{ROOT}", &root));

    let instructions =
        "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = 'instructions'";
    let dry = m.run(&cmd(&["update"]));
    let text = dry.ok();
    assert!(
        text.starts_with("update: dry run — nothing has been changed."),
        "{dry}"
    );
    assert!(
        text.contains("instructions"),
        "the pending migration is not listed:\n{dry}"
    );
    assert!(text.ends_with("Ask the user to confirm\n"), "{dry}");
    assert_eq!(m.value(instructions), "0", "a dry run migrated");

    let applied = m.run(&cmd(&["update", "-y"]));
    applied.ok();
    assert_eq!(
        m.value(instructions),
        "1",
        "update -y did not migrate:\n{applied}"
    );
    let dumps = m.list("backups");
    assert_eq!(
        dumps.len(),
        1,
        "update -y backs up before migrating: {dumps:?}"
    );
    assert!(
        dumps[0].ends_with("-supercharged-memory-native.sql.gz"),
        "{dumps:?}"
    );
    assert_eq!(
        m.run(&cmd(&["status"])).ok(),
        "READY 32\nunembedded: semantic 2, episodic 1, instructions 0 — run embed --missing\n"
    );

    let again = m.run(&cmd(&["update"]));
    let text = again.ok();
    assert!(text.contains("no schema migration pending"), "{again}");
    assert!(
        !text.contains("Ask the user to confirm"),
        "nothing left to confirm:\n{again}"
    );
}

fn seed_has_no_counterpart(ctx: &Ctx) {
    let m = ctx.machine(T, "seed", Side::Rust, Fixture::Schema);
    m.run(&cmd(&["seed"])).usage();
    assert!(
        repo().join("scripts/seed.py").exists(),
        "the reference script is gone"
    );
    ctx.asserted(&SEED_DROPPED);
}
