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

    /// The cutover's checklist: the texts still name the repository checkout.
    /// It fails until they are rewritten for the binary, so it only runs on request
    /// (`cargo test -- --ignored`).
    #[test]
    #[ignore = "the embedded texts still name the repository checkout"]
    fn no_embedded_text_points_at_the_checkout() {
        let mut offenders = Vec::new();
        for (origin, text) in corpus() {
            for what in stale_references(text) {
                offenders.push(format!("{origin}: {what}"));
            }
        }
        assert!(offenders.is_empty(), "{}", offenders.join("\n"));
    }

    /// Placeholders the binary can fill: nothing but `{{EXE}}` may stay in a runbook.
    #[test]
    fn a_runbook_names_no_placeholder_but_exe() {
        for r in &RUNBOOKS {
            fill(r.text, "/x/sm").unwrap_or_else(|e| panic!("{}: {e}", r.name));
        }
    }
}
