//! The two documents for humans, README.md and CLAUDE.md, must describe the
//! binary, not the Python scripts they were written around.

use std::fs;
use std::path::Path;

fn read(name: &str) -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(name)).unwrap()
}

fn lacks(doc: &str, text: &str, gone: &[&str]) {
    let offenders: Vec<String> = gone
        .iter()
        .filter(|g| text.contains(**g))
        .map(|g| format!("{doc} still mentions {g:?}"))
        .collect();
    assert!(offenders.is_empty(), "{}", offenders.join("\n"));
}

fn has(doc: &str, text: &str, want: &[&str]) {
    let missing: Vec<String> = want
        .iter()
        .filter(|w| !text.contains(**w))
        .map(|w| format!("{doc} lacks {w:?}"))
        .collect();
    assert!(missing.is_empty(), "{}", missing.join("\n"));
}

#[test]
fn the_seed_script_is_not_mentioned() {
    for doc in ["README.md", "CLAUDE.md"] {
        lacks(doc, &read(doc), &["seed.py", "seeded start"]);
    }
}

#[test]
fn the_readme_installs_the_binary() {
    let t = read("README.md");
    lacks(
        "README.md",
        &t,
        &[
            "python3",
            "install-claude-md.sh",
            "BASE_PATH",
            "synced-at",
            "settings.json",
            "SETUP.md",
            "{{PY}}",
            "tursodb \"$SUPERCHARGED_MEMORY_TURSO_PATH\" --experimental-multiprocess-wal < schema.sql",
            "Neither dependency is bundled",
        ],
    );
    has(
        "README.md",
        &t,
        &[
            "init",
            ".env",
            "rendered-by",
            "runbook setup",
            "migrations/",
            "update -y",
            "backup",
            "reference implementation",
            "keyword-only",
            "EMBEDDINGS",
            "precedence",
        ],
    );
}

#[test]
fn the_readme_says_nothing_is_required() {
    let t = read("README.md");
    assert!(
        t.contains("None of them is required") || t.contains("no dependency is required"),
        "the dependency section must say that nothing is required"
    );
}

#[test]
fn the_readme_documents_every_dotenv_key() {
    let t = read("README.md");
    for key in [
        "SUPERCHARGED_MEMORY_TURSO_PATH",
        "BACKUP_DIR",
        "SUPERCHARGED_MEMORY_VERSION",
        "EMBEDDINGS",
        "OLLAMA_URL",
        "EMBED_MODEL",
        "RECALL_ALPHA",
        "TURSO_VFS",
        "TURSO_BIN",
        "SUPERCHARGED_MEMORY_EVAL_DIR",
    ] {
        assert!(t.contains(&format!("| `{key}`")), "the table lacks {key}");
    }
}

#[test]
fn the_contributor_guide_states_each_changed_invariant() {
    let t = read("CLAUDE.md");
    lacks(
        "CLAUDE.md",
        &t,
        &[
            "python3 scripts/",
            "memlib.py` is the shared core",
            "`BASE_PATH` in `CLAUDE.md.template` is machine-specific",
            "reads the block before it defaults",
            "Pipe `schema.sql`",
            "must live in `~/.claude/settings.json`",
            "note goes in the commit immediately after",
        ],
    );
    has(
        "CLAUDE.md",
        &t,
        &[
            "{{EXE}}",
            "rendered-by",
            "`.env`",
            "migrations/",
            "reference implementation",
            "cargo test",
            "update -y",
            "read-only",
        ],
    );
}
