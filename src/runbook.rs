//! `runbook`: the procedures the instructions point at, printed on request.
//!
//! There is no checkout at runtime, so the texts under `instructions/` are
//! compiled in and printed from here. Nothing reads them from disk.

use std::path::Path;

use crate::claude_md;
use crate::config::Config;
use crate::error::{Error, Result};

/// One embedded procedure.
pub struct Runbook {
    /// What the user-facing command takes: `runbook <name>`.
    pub name: &'static str,
    /// One line for the listing.
    pub summary: &'static str,
    /// The source file, byte for byte.
    pub text: &'static str,
}

pub const RUNBOOKS: [Runbook; 6] = [
    Runbook {
        name: "setup",
        summary: "install and verify this machine's memory system",
        text: include_str!("../instructions/SETUP.md"),
    },
    Runbook {
        name: "update",
        summary: "bring an install up to date: migration notes, schema, instructions",
        text: include_str!("../instructions/UPDATE.md"),
    },
    Runbook {
        name: "sleep",
        summary: "consolidate episodic memory into semantic memory, rebuild the topic index",
        text: include_str!("../instructions/SLEEP.md"),
    },
    Runbook {
        name: "deep-sleep",
        summary: "sleep, then purge, merge, mine patterns and check recall quality",
        text: include_str!("../instructions/DEEP-SLEEP.md"),
    },
    Runbook {
        name: "migrate",
        summary: "import file-based Claude memory into the database",
        text: include_str!("../instructions/MIGRATE-EXISTING-MEMORY.md"),
    },
    Runbook {
        name: "new-coworker",
        summary: "create a named persona with scoped memory",
        text: include_str!("../instructions/NEW-COWORKER.md"),
    },
];

/// The listing: one line per runbook, the name padded so the summaries line up.
pub fn listing() -> String {
    let width = RUNBOOKS.iter().map(|r| r.name.len()).max().unwrap_or(0) + 2;
    RUNBOOKS
        .iter()
        .map(|r| format!("{:<width$}{}\n", r.name, r.summary))
        .collect()
}

/// Replace `{{EXE}}` and refuse to hand out text that still holds a placeholder:
/// a command line with `{{BASE_PATH}}` in it would be run as it stands.
pub fn fill(text: &str, exe: &str) -> Result<String> {
    claude_md::render(text, &[("EXE", exe)]).map_err(|missing| {
        Error::failed(format!(
            "the runbook still holds placeholders the binary cannot fill: {}",
            missing.join(", ")
        ))
    })
}

/// A text with `{{EXE}}` filled in as the binary's path is written on a command line.
pub fn text_for_source(text: &str, exe: &Path, windows: bool) -> Result<String> {
    let exe = claude_md::exe_for_instructions(exe, windows)
        .map_err(|why| Error::failed(format!("cannot write the binary's path: {why}")))?;
    fill(text, &exe)
}

/// One runbook ready to print, or the refusal that lists the valid names.
pub fn text_for(name: &str, exe: &Path, windows: bool) -> Result<String> {
    let Some(book) = RUNBOOKS.iter().find(|r| r.name == name) else {
        let names: Vec<&str> = RUNBOOKS.iter().map(|r| r.name).collect();
        return Err(Error::refused(format!(
            "unknown runbook {name:?}; the runbooks are: {}",
            names.join(", ")
        )));
    };
    text_for_source(book.text, exe, windows)
}

/// The runbook names a text tells the reader to print: `runbook <name>` after a
/// backtick or after `{{EXE}} `. Prose that merely says "runbook" is not one.
#[cfg(test)]
pub fn mentioned(text: &str) -> Vec<String> {
    const WORD: &str = "runbook ";
    let mut found = Vec::new();
    for (at, _) in text.match_indices(WORD) {
        let before = &text[..at];
        if !(before.ends_with('`') || before.ends_with("{{EXE}} ")) {
            continue;
        }
        let name: String = text[at + WORD.len()..]
            .chars()
            .take_while(|c| c.is_ascii_lowercase() || *c == '-')
            .collect();
        if !name.is_empty() {
            found.push(name);
        }
    }
    found
}

/// What in a text still points at the repository checkout instead of the binary:
/// `instructions/<file>.md`, `scripts/`, `{{BASE_PATH}}` and `{{PY}}`. In order of
/// first appearance, each once.
#[cfg(test)]
pub fn stale_references(text: &str) -> Vec<String> {
    let mut hits: Vec<(usize, String)> = Vec::new();
    for fixed in ["{{BASE_PATH}}", "{{PY}}"] {
        hits.extend(text.match_indices(fixed).map(|(at, s)| (at, s.to_string())));
    }
    for (at, _) in text.match_indices("scripts/") {
        if starts_a_word(text, at) {
            hits.push((at, "scripts/".to_string()));
        }
    }
    for (at, _) in text.match_indices("instructions/") {
        if !starts_a_word(text, at) {
            continue;
        }
        let rest = &text[at + "instructions/".len()..];
        let stem = rest.find(".md").filter(|end| {
            *end > 0
                && rest[..*end]
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        });
        if let Some(end) = stem {
            hits.push((
                at,
                text[at..at + "instructions/".len() + end + 3].to_string(),
            ));
        }
    }
    hits.sort();
    let mut out: Vec<String> = Vec::new();
    for (_, what) in hits {
        if !out.contains(&what) {
            out.push(what);
        }
    }
    out
}

/// Whether the match at `at` begins a word, so `transcripts/` is not `scripts/`.
#[cfg(test)]
fn starts_a_word(text: &str, at: usize) -> bool {
    text[..at]
        .chars()
        .next_back()
        .is_none_or(|c| !c.is_alphanumeric())
}

/// `runbook`: print one runbook, or with no name list them.
pub fn run(cfg: &Config, name: Option<&str>) -> Result<()> {
    let out = match name {
        None => listing(),
        Some(name) => text_for(name, &cfg.exe, cfg!(windows))?,
    };
    print!("{out}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{EXIT_ERROR, EXIT_REFUSED, Error};

    const FILES: [(&str, &str); 6] = [
        ("setup", "SETUP.md"),
        ("update", "UPDATE.md"),
        ("sleep", "SLEEP.md"),
        ("deep-sleep", "DEEP-SLEEP.md"),
        ("migrate", "MIGRATE-EXISTING-MEMORY.md"),
        ("new-coworker", "NEW-COWORKER.md"),
    ];

    fn source(file: &str) -> String {
        std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("instructions")
                .join(file),
        )
        .unwrap()
    }

    fn refusal(r: Result<String>) -> String {
        match r {
            Err(e @ Error::Refused(_)) => {
                assert_eq!(e.exit_code(), EXIT_REFUSED);
                e.to_string()
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn the_promised_names_exist_in_listing_order() {
        let names: Vec<&str> = RUNBOOKS.iter().map(|r| r.name).collect();
        assert_eq!(
            names,
            [
                "setup",
                "update",
                "sleep",
                "deep-sleep",
                "migrate",
                "new-coworker"
            ]
        );
    }

    #[test]
    fn every_runbook_is_its_source_file_byte_for_byte() {
        for (name, file) in FILES {
            let book = RUNBOOKS.iter().find(|r| r.name == name).unwrap();
            assert!(!book.text.is_empty(), "{name} is empty");
            assert_eq!(book.text, source(file), "{name} differs from {file}");
        }
    }

    #[test]
    fn printing_a_runbook_without_the_placeholder_changes_nothing() {
        for (name, file) in FILES {
            let out = text_for(name, Path::new("/opt/sm/supercharged-memory"), false).unwrap();
            // Today no source names {{EXE}}; the day one does, the rest of the
            // text must still be untouched.
            assert_eq!(
                out,
                source(file).replace("{{EXE}}", "/opt/sm/supercharged-memory"),
                "{name}"
            );
        }
    }

    #[test]
    fn the_exe_placeholder_is_replaced_everywhere() {
        let out = fill("a {{EXE}} b\n{{EXE}} c {{EXE}}\n", "/x/sm").unwrap();
        assert_eq!(out, "a /x/sm b\n/x/sm c /x/sm\n");
    }

    #[test]
    fn any_other_placeholder_fails_and_is_named() {
        let err = fill("{{EXE}} {{BASE_PATH}}/x {{PY}} {{BASE_PATH}}", "/x/sm").unwrap_err();
        assert_eq!(err.exit_code(), EXIT_ERROR);
        let msg = err.to_string();
        assert!(msg.starts_with("error: "), "{msg}");
        assert!(
            msg.contains("{{BASE_PATH}}") && msg.contains("{{PY}}"),
            "{msg}"
        );
    }

    #[test]
    fn text_that_only_looks_like_a_placeholder_is_left_alone() {
        let text = "a {{ b }} {x} {{lower}} ${{ matrix.os }}\n";
        assert_eq!(fill(text, "/x/sm").unwrap(), text);
    }

    #[test]
    fn the_binary_is_written_as_a_command_line_names_it() {
        let src = "{{EXE}} status";
        assert_eq!(
            text_for_source(src, Path::new("/opt/my tools/sm"), false).unwrap(),
            "\"/opt/my tools/sm\" status"
        );
        assert_eq!(
            text_for_source(src, Path::new(r"\\?\C:\tools\sm.exe"), true).unwrap(),
            "C:/tools/sm.exe status"
        );
    }

    #[test]
    fn a_binary_path_no_command_line_can_quote_fails() {
        let err = text_for_source("{{EXE}}", Path::new("/odd\"dir/sm"), false).unwrap_err();
        assert_eq!(err.exit_code(), EXIT_ERROR);
        assert!(err.to_string().contains("double quote"), "{err}");
    }

    #[test]
    fn an_unknown_name_is_refused_and_the_valid_names_are_listed() {
        for bad in ["nope", "SLEEP", "", "sleep ", "deep_sleep", "SLEEP.md"] {
            let msg = refusal(text_for(bad, Path::new("/x/sm"), false));
            assert!(msg.starts_with("refused: "), "{msg}");
            assert!(msg.contains(&format!("{bad:?}")), "{msg}");
            for r in &RUNBOOKS {
                assert!(msg.contains(r.name), "{bad:?}: {msg}");
            }
        }
    }

    #[test]
    fn the_listing_is_one_line_per_runbook() {
        let out = listing();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), RUNBOOKS.len(), "{out}");
        assert!(out.ends_with('\n'));
        for (line, book) in lines.iter().zip(&RUNBOOKS) {
            assert_eq!(line.split_whitespace().next(), Some(book.name), "{line}");
            assert!(!book.summary.is_empty(), "{} has no summary", book.name);
            assert!(line.contains(book.summary), "{line}");
            assert!(!book.summary.contains('\n'), "{}", book.name);
        }
    }

    #[test]
    fn mentions_are_runbook_commands_in_either_form() {
        let text = "Run `runbook setup` first, then `{{EXE}} runbook deep-sleep`.\n\
                    The runbook is long. A runbook for x. `runbook sleep`";
        assert_eq!(mentioned(text), ["setup", "deep-sleep", "sleep"]);
        assert!(mentioned("no mention, runbook alone `runbook`").is_empty());
    }

    #[test]
    fn stale_references_name_the_checkout() {
        let text = "see instructions/SLEEP.md and `{{BASE_PATH}}/x`, run scripts/recall.py\n\
                    and {{PY}} too; but `runbook sleep` is fine, as is our/instructions.";
        let found = stale_references(text);
        assert_eq!(
            found,
            [
                "instructions/SLEEP.md",
                "{{BASE_PATH}}",
                "scripts/",
                "{{PY}}"
            ]
        );
        assert!(stale_references("`{{EXE}} runbook sleep` and docs/x.md").is_empty());
    }

    /// Every embedded text: the runbooks and the instructions template.
    fn corpus() -> Vec<(String, &'static str)> {
        let mut all: Vec<(String, &'static str)> = RUNBOOKS
            .iter()
            .map(|r| (r.name.to_string(), r.text))
            .collect();
        all.push(("template".into(), crate::claude_md::TEMPLATE));
        all
    }

    /// A text that tells the reader to print a runbook must name one that exists.
    #[test]
    fn every_runbook_an_embedded_text_mentions_resolves() {
        for (origin, text) in corpus() {
            for name in mentioned(text) {
                assert!(
                    RUNBOOKS.iter().any(|r| r.name == name),
                    "{origin} names `runbook {name}`, which does not exist"
                );
            }
        }
    }

    /// Nothing an agent is told to run or open may point at the repository
    /// checkout: there is none on the machine the binary is installed on.
    #[test]
    fn no_embedded_text_points_at_the_checkout() {
        let mut offenders = Vec::new();
        for (origin, text) in corpus() {
            for what in stale_references(text) {
                offenders.push(format!("{origin}: {what}"));
            }
        }
        assert!(offenders.is_empty(), "{}", offenders.join("\n"));
    }

    /// The Python scripts, their interpreter, the installer script and the
    /// settings file the old setup edited are not part of the product any more.
    #[test]
    fn no_embedded_text_names_the_python_era() {
        const GONE: [&str; 8] = [
            "python3",
            "python ",
            ".py",
            "settings.json",
            "install-claude-md",
            "PYTHONUTF8",
            "schema.sql",
            "migration-steps",
        ];
        let mut offenders = Vec::new();
        for (origin, text) in corpus() {
            for gone in GONE {
                if text.contains(gone) {
                    offenders.push(format!("{origin}: {gone:?}"));
                }
            }
        }
        assert!(offenders.is_empty(), "{}", offenders.join("\n"));
    }

    /// The `{{EXE}} <command> --flags` a text hands the agent, each with the flags
    /// written in the same command line.
    fn command_lines(text: &str) -> Vec<(String, String, Vec<String>)> {
        let lines: Vec<&str> = text.lines().collect();
        let mut found = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            for (at, _) in line.match_indices("{{EXE}} ") {
                let mut rest = line[at + "{{EXE}} ".len()..].to_string();
                let mut next = i;
                while rest.trim_end().ends_with('\\') && next + 1 < lines.len() {
                    next += 1;
                    rest.push(' ');
                    rest.push_str(lines[next]);
                }
                let span = rest.split('`').next().unwrap_or("").to_string();
                let sub: String = span
                    .chars()
                    .take_while(|c| c.is_ascii_lowercase() || *c == '-')
                    .collect();
                let flags = span
                    .match_indices("--")
                    .map(|(f, _)| {
                        span[f + 2..]
                            .chars()
                            .take_while(|c| c.is_ascii_lowercase() || *c == '-')
                            .collect::<String>()
                    })
                    .filter(|f| !f.is_empty())
                    .collect();
                found.push((line.trim().to_string(), sub, flags));
            }
        }
        found
    }

    /// A command line in an embedded text must exist: a runbook that names a
    /// subcommand or a flag the binary does not have fails on the user's machine.
    #[test]
    fn every_command_an_embedded_text_gives_exists() {
        use clap::CommandFactory;
        let cli = crate::cli::Cli::command();
        let mut offenders = Vec::new();
        let mut seen = 0;
        for (origin, text) in corpus() {
            for (line, sub, flags) in command_lines(text) {
                if sub.is_empty() {
                    continue;
                }
                seen += 1;
                let Some(cmd) = cli.find_subcommand(&sub) else {
                    offenders.push(format!("{origin}: no subcommand {sub:?} in: {line}"));
                    continue;
                };
                for flag in flags {
                    let known = flag == "help"
                        || cmd
                            .get_arguments()
                            .any(|a| a.get_long() == Some(flag.as_str()));
                    if !known {
                        offenders.push(format!("{origin}: `{sub}` has no --{flag} in: {line}"));
                    }
                }
            }
        }
        assert!(seen > 40, "only {seen} command lines found");
        assert!(offenders.is_empty(), "{}", offenders.join("\n"));
    }

    fn book(name: &str) -> &'static str {
        RUNBOOKS.iter().find(|r| r.name == name).unwrap().text
    }

    #[test]
    fn the_template_runs_everything_through_the_binary() {
        let t = crate::claude_md::TEMPLATE;
        for want in [
            "{{EXE}} status",
            "{{EXE}} recall --baseline",
            "{{EXE}} recall --topics",
            "{{EXE}} recall --candidates",
            "{{EXE}} remember --table",
            "{{EXE}} backfill --dir",
            "{{EXE}} coworkers --list",
            "{{EXE}} coworkers --load",
            "{{EXE}} coworkers --appraise",
            "{{EXE}} instructions --get",
            "{{EXE}} runbook sleep",
            "{{EXE}} runbook deep-sleep",
            "{{EXE}} runbook migrate",
            "{{EXE}} backup",
            "{{EXE}} update",
        ] {
            assert!(t.contains(want), "the template lacks {want:?}");
        }
        // Reads are commands now, not SQL through a client.
        assert!(!t.contains("SELECT"), "the template still hands out SQL");
        assert!(!t.contains("MCP calls"), "{t}");
        // Never a quoted binary: the placeholder carries its own quotes.
        assert!(!t.contains("\"{{EXE}}\""));
    }

    /// The MCP, when registered read-only, cannot write; a read-write one makes
    /// "writes go through the CLI" a convention only.
    #[test]
    fn the_template_says_the_cli_only_rule_is_advisory_for_a_writable_mcp() {
        let t = crate::claude_md::TEMPLATE;
        assert!(t.contains("advisory"), "{t}");
        assert!(t.contains("read-only"), "{t}");
    }

    /// Without embeddings the agent stores anyway, and says what was skipped.
    #[test]
    fn the_template_no_longer_forbids_storing_without_embeddings() {
        let t = crate::claude_md::TEMPLATE;
        assert!(!t.contains("CANNOT store"), "{t}");
        assert!(t.contains("embed --missing"), "{t}");
    }

    #[test]
    fn the_template_did_not_grow_beyond_the_instructions_line() {
        let lines = crate::claude_md::TEMPLATE.lines().count();
        assert!(lines <= 122, "the template is {lines} lines; it was 121");
    }

    #[test]
    fn the_template_says_when_to_use_an_instruction() {
        let t = crate::claude_md::TEMPLATE;
        assert!(t.contains("use instruction"), "{t}");
        assert!(t.contains("{{EXE}} instruct "), "{t}");
    }

    #[test]
    fn the_runbooks_point_at_each_other_by_command() {
        assert!(mentioned(book("sleep")).contains(&"deep-sleep".to_string()));
        assert!(mentioned(book("deep-sleep")).contains(&"sleep".to_string()));
        assert!(mentioned(book("setup")).contains(&"migrate".to_string()));
        assert!(mentioned(book("setup")).contains(&"new-coworker".to_string()));
        assert!(mentioned(book("migrate")).contains(&"deep-sleep".to_string()));
    }

    #[test]
    fn setup_is_built_around_init() {
        let t = book("setup");
        for want in [
            "{{EXE}} init",
            "{{EXE}} init -y",
            "Missing",
            "{{EXE}} embed --missing",
            "{{EXE}} find-existing-memory",
            "{{EXE}} coworkers --add",
            "--readonly",
            "optional",
        ] {
            assert!(t.contains(want), "SETUP lacks {want:?}");
        }
        assert!(!t.contains("Work from the repository root"));
        assert!(
            !t.contains("Step 1 \u{2014} Turso"),
            "tursodb is not step 1 any more"
        );
    }

    #[test]
    fn update_is_built_around_the_update_command() {
        let t = book("update");
        for want in [
            "{{EXE}} update",
            "{{EXE}} update -y",
            "{{EXE}} update --print-block",
            "rendered-by",
            "synced-at",
            "restart",
        ] {
            assert!(t.contains(want), "UPDATE lacks {want:?}");
        }
        assert!(!t.contains("git pull"), "UPDATE still pulls a checkout");
        assert!(!t.contains("Work from the repository root"));
    }

    /// Embeddings are optional: a degraded status is not a stop sign.
    #[test]
    fn no_runbook_stops_on_degraded() {
        let deep = book("deep-sleep");
        assert!(!deep.contains("`DEGRADED n` \u{2192} stop here"), "D0");
        assert!(deep.contains("embed --missing"));
        let migrate = book("migrate");
        assert!(!migrate.contains("`DEGRADED n` \u{2192} stop"), "M0");
        assert!(migrate.contains("embed --missing"));
    }

    #[test]
    fn the_eval_set_is_edited_through_the_eval_command() {
        let t = book("deep-sleep");
        for want in [
            "{{EXE}} eval --validate",
            "{{EXE}} eval --report",
            "{{EXE}} eval --sweep",
            "{{EXE}} eval --add-case",
            "{{EXE}} eval --repoint",
            "{{EXE}} eval --retire-case",
            "{{EXE}} eval --last-run",
        ] {
            assert!(t.contains(want), "DEEP-SLEEP lacks {want:?}");
        }
        assert!(!t.contains("UPDATE eval_cases"));
        assert!(!t.contains("INSERT INTO eval_cases"));
    }

    #[test]
    fn rows_are_read_with_list_and_show_not_sql() {
        for name in ["sleep", "deep-sleep", "migrate"] {
            let t = book(name);
            assert!(!t.contains("tursodb"), "{name} still reads through tursodb");
            assert!(!t.contains("SELECT"), "{name} still hands out SQL");
        }
        assert!(book("sleep").contains("{{EXE}} list --table episodic --unprocessed"));
        assert!(book("sleep").contains("{{EXE}} show --table episodic"));
        assert!(book("deep-sleep").contains("{{EXE}} list --table semantic --not-current"));
        assert!(book("migrate").contains("{{EXE}} list --table semantic --source migration"));
    }

    #[test]
    fn the_coworker_runbook_states_the_schema_caps() {
        let t = book("new-coworker");
        assert!(t.contains("3000"), "{t}");
        assert!(!t.contains("256 chars") && !t.contains("1000 chars"));
        assert!(!t.contains("docs/") && !t.contains("coworkers/"));
        assert!(t.contains("{{EXE}} coworkers --add"));
    }

    /// Placeholders the binary can fill: nothing but `{{EXE}}` may stay in a runbook.
    #[test]
    fn a_runbook_names_no_placeholder_but_exe() {
        for r in &RUNBOOKS {
            fill(r.text, "/x/sm").unwrap_or_else(|e| panic!("{}: {e}", r.name));
        }
    }
}
