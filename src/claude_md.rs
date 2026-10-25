//! The managed block in Claude Code's CLAUDE.md: its markers, the template it is
//! rendered from, and the rendering.
//!
//! `init` appends the block once and never rewrites text. Whether a block is
//! already there is decided by the template's headline, so a block written by the
//! legacy installer counts as present too. The markers stay anyway: `update`
//! needs the block's boundaries to diff it against a fresh render, and
//! `find-existing-memory` leaves the block out of the user's own content by them.

use std::path::Path;

/// The template, compiled in: no repository is needed at runtime.
pub const TEMPLATE: &str = include_str!("../CLAUDE.md.template");

/// The template's first line. Present anywhere in CLAUDE.md, it means the
/// instructions are installed, whoever wrote them.
pub const HEADLINE: &str = "# Long-term memory — Turso vector DB (supercharged-memory)";

/// The start of this system's managed block. A prefix, not a whole line: the
/// installer script wrote `(managed by install-claude-md.sh) -->` after it and
/// `init` writes its own ending, and both blocks are this system's instructions,
/// not the user's memory.
pub const BLOCK_BEGIN_PREFIX: &str = "<!-- BEGIN agentic-memory";
/// The BEGIN line `init` writes.
pub const BLOCK_BEGIN: &str = "<!-- BEGIN agentic-memory (managed by supercharged-memory) -->";
pub const BLOCK_END: &str = "<!-- END agentic-memory -->";

/// The legacy installer's stamp, `synced-at <sha>`: a block rendered from a
/// repository checkout, whose commands name the Python scripts.
pub const LEGACY_STAMP: &str = "<!-- supercharged-memory: synced-at ";

/// What a machine whose block was rendered by the legacy installer has to do to
/// move onto the binary. Printed by `init` (and `update`) when they find such a
/// block; `init` itself leaves the block untouched.
pub const CUTOVER_NOTE: &str = "\
This block was rendered by the legacy installer from a repository checkout; its
commands name the Python scripts. init leaves it untouched. To move this machine
onto the binary:
  1. Keep the binary in a permanent place under a stable name, and run it from there.
  2. Run init with --db-path <the database the block's machine uses>,
     --backup-dir <the repository's backups/ directory> and --episodic-mode <the
     block's mode>; relay the plan, then -y. It adopts the database and writes the .env.
  3. Move RECALL_ALPHA (if set) into the .env, then remove SUPERCHARGED_MEMORY_TURSO_PATH,
     BACKUP_DIR and RECALL_ALPHA from the \"env\" block of ~/.claude/settings.json: real
     environment variables override the .env on every run. `config` must then show
     every value coming from the .env.
  4. Run `update` and relay it; once the user agrees, replace this block, from its BEGIN
     line through its END line, with what `update --print-block` prints.
  5. Point the scheduled backup job at `<this binary> backup`.";

/// The first line inside a block `init` wrote: which binary version rendered it.
pub fn stamp(version: &str) -> String {
    format!("<!-- supercharged-memory: rendered-by {version} -->")
}

/// How much the agent records as episodic memory on this machine. Only the
/// active mode's rule is rendered: listing all four would cost context every
/// session to describe three modes the agent must ignore.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpisodicMode {
    EveryPrompt,
    MajorActions,
    MajorEvents,
    Manual,
}

impl EpisodicMode {
    pub const ALL: [EpisodicMode; 4] = [
        EpisodicMode::EveryPrompt,
        EpisodicMode::MajorActions,
        EpisodicMode::MajorEvents,
        EpisodicMode::Manual,
    ];
    pub const DEFAULT: EpisodicMode = EpisodicMode::MajorEvents;

    pub fn parse(s: &str) -> Option<EpisodicMode> {
        EpisodicMode::ALL.into_iter().find(|m| m.as_str() == s)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            EpisodicMode::EveryPrompt => "every-prompt",
            EpisodicMode::MajorActions => "major-actions",
            EpisodicMode::MajorEvents => "major-events",
            EpisodicMode::Manual => "manual",
        }
    }

    /// The rule rendered into the block, worded as the installer script had it.
    pub fn rule(self) -> &'static str {
        match self {
            EpisodicMode::EveryPrompt => {
                "store an episodic `note` for EVERY prompt/turn: what was asked, what you did."
            }
            EpisodicMode::MajorActions => {
                "store one for every substantive action (a change made, a task carried out); \
                 skip quick questions, clarifications, and trivial back-and-forth."
            }
            EpisodicMode::MajorEvents => {
                "store ONLY major events: feature completed, bug resolved, decision, milestone, \
                 incident."
            }
            EpisodicMode::Manual => {
                "store episodic memories ONLY when the user explicitly asks; never auto-store \
                 events."
            }
        }
    }
}

/// A path as a command line writes it: on Windows the mixed `C:/…` form, which
/// both PowerShell and Git Bash accept, without the `\\?\` prefix that
/// `canonicalize` adds. Unchanged elsewhere.
pub fn mixed_path(path: &str, windows: bool) -> String {
    if !windows {
        return path.to_string();
    }
    let path = if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else {
        path.strip_prefix(r"\\?\").unwrap_or(path).to_string()
    };
    path.replace('\\', "/")
}

/// Double quotes around a path that contains whitespace, so a shell keeps it
/// one word.
pub fn shell_word(s: &str) -> String {
    if s.contains(char::is_whitespace) {
        format!("\"{s}\"")
    } else {
        s.to_string()
    }
}

/// The binary's path as it is written into the instructions: the mixed form on
/// Windows, double-quoted when it contains whitespace. A path that cannot be
/// written that way is an error, never a mangled command.
pub fn exe_for_instructions(exe: &Path, windows: bool) -> Result<String, String> {
    let Some(raw) = exe.to_str() else {
        return Err(format!("{} is not valid UTF-8", exe.display()));
    };
    let path = mixed_path(raw, windows);
    if path.contains(['"', '\n', '\r']) {
        return Err(format!(
            "{raw} contains a double quote or a line break, which no command line can quote"
        ));
    }
    Ok(shell_word(&path))
}

/// Substitute every `{{NAME}}` in one pass. A placeholder without a value fails
/// the render, listing every such name: rendering `{{...}}` into the user's
/// instructions would break every command that names it, in a file nobody
/// re-reads. Values are never re-scanned.
pub fn render(template: &str, values: &[(&str, &str)]) -> Result<String, Vec<String>> {
    let mut out = String::with_capacity(template.len());
    let mut missing: Vec<String> = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after
            .find("}}")
            .map(|end| (&after[..end], &after[end + 2..]))
        {
            Some((name, tail)) if is_placeholder_name(name) => {
                match values.iter().find(|(key, _)| *key == name) {
                    Some((_, value)) => out.push_str(value),
                    None => {
                        let placeholder = format!("{{{{{name}}}}}");
                        if !missing.contains(&placeholder) {
                            missing.push(placeholder);
                        }
                    }
                }
                rest = tail;
            }
            _ => {
                out.push_str("{{");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    if missing.is_empty() {
        Ok(out)
    } else {
        Err(missing)
    }
}

fn is_placeholder_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// The whole block: BEGIN marker, version stamp, the rendered template, END
/// marker, newline-terminated.
pub fn block(rendered: &str, version: &str) -> String {
    let mut b = format!("{BLOCK_BEGIN}\n{}\n{rendered}", stamp(version));
    if !b.ends_with('\n') {
        b.push('\n');
    }
    b.push_str(BLOCK_END);
    b.push('\n');
    b
}

pub fn has_headline(text: &str) -> bool {
    text.contains(HEADLINE)
}

pub fn has_legacy_block(text: &str) -> bool {
    text.contains(LEGACY_STAMP)
}

/// What goes between existing text and an appended block: one blank line, and
/// nothing at all in an empty file.
pub fn separator(existing: &str) -> &'static str {
    if existing.is_empty() || existing.ends_with("\n\n") {
        ""
    } else if existing.ends_with('\n') {
        "\n"
    } else {
        "\n\n"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_template_starts_with_the_headline() {
        assert!(TEMPLATE.starts_with(&format!("{HEADLINE}\n")));
    }

    /// `find-existing-memory` matches a block by this prefix; a BEGIN line
    /// without it would be reported back to the user as their own content.
    #[test]
    fn the_begin_line_keeps_the_prefix_the_probe_matches() {
        assert!(BLOCK_BEGIN.starts_with(BLOCK_BEGIN_PREFIX));
        let block = block("# x\n", "0.1.0");
        assert_eq!(
            crate::find_existing::outside_managed_block(&block).trim(),
            ""
        );
    }

    #[test]
    fn the_block_has_its_markers_and_the_version_stamp() {
        let b = block("# Head\nbody\n", "0.3.0");
        let lines: Vec<&str> = b.lines().collect();
        assert_eq!(
            lines,
            vec![
                BLOCK_BEGIN,
                "<!-- supercharged-memory: rendered-by 0.3.0 -->",
                "# Head",
                "body",
                BLOCK_END,
            ]
        );
        assert!(b.ends_with(&format!("{BLOCK_END}\n")));
        // A template without a final newline still gets END on a line of its own.
        assert!(block("# Head", "0.3.0").contains(&format!("# Head\n{BLOCK_END}\n")));
        assert_eq!(
            stamp("1.2.3"),
            "<!-- supercharged-memory: rendered-by 1.2.3 -->"
        );
    }

    #[test]
    fn render_fills_every_known_placeholder() {
        let out = render(
            "run {{EXE}} status\n{{EPISODIC_MODE}}: {{EPISODIC_RULE}} ({{EXE}})\n",
            &[
                ("EXE", "/opt/sm/supercharged-memory"),
                ("EPISODIC_MODE", "manual"),
                ("EPISODIC_RULE", "only when asked"),
            ],
        )
        .unwrap();
        assert_eq!(
            out,
            "run /opt/sm/supercharged-memory status\nmanual: only when asked (/opt/sm/supercharged-memory)\n"
        );
    }

    #[test]
    fn a_placeholder_without_a_value_fails_rather_than_rendering() {
        let err = render(
            "{{EXE}} and {{BASE_PATH}}/x and {{PY}} and {{BASE_PATH}} again",
            &[("EXE", "/x")],
        )
        .unwrap_err();
        assert_eq!(err, vec!["{{BASE_PATH}}".to_string(), "{{PY}}".to_string()]);
    }

    /// A value is text, not template: a path that happens to contain braces is
    /// written as it is, and is not mistaken for a missing placeholder.
    #[test]
    fn a_value_is_never_rescanned() {
        assert_eq!(
            render("{{EXE}}", &[("EXE", "/odd/{{PY}}/bin")]).unwrap(),
            "/odd/{{PY}}/bin"
        );
        // Braces that are not a placeholder name are left alone.
        assert_eq!(render("a {{ b }} {x}", &[]).unwrap(), "a {{ b }} {x}");
    }

    /// The template still names the Python scripts through `{{BASE_PATH}}` and
    /// `{{PY}}`, which a binary without a repository cannot fill. Until it names
    /// the binary instead, rendering it must fail rather than install broken
    /// paths. When the template changes, this test changes with it.
    #[test]
    fn the_embedded_template_cannot_be_rendered_by_the_binary_yet() {
        let err = render(
            TEMPLATE,
            &[
                ("EXE", "/opt/sm/supercharged-memory"),
                ("EPISODIC_MODE", "major-events"),
                ("EPISODIC_RULE", "rule"),
            ],
        )
        .unwrap_err();
        assert!(err.contains(&"{{BASE_PATH}}".to_string()), "{err:?}");
        assert!(err.contains(&"{{PY}}".to_string()), "{err:?}");
    }

    #[test]
    fn episodic_modes_parse_and_carry_the_installer_rules() {
        let want = [
            (
                "every-prompt",
                "store an episodic `note` for EVERY prompt/turn: what was asked, what you did.",
            ),
            (
                "major-actions",
                "store one for every substantive action (a change made, a task carried out); \
                 skip quick questions, clarifications, and trivial back-and-forth.",
            ),
            (
                "major-events",
                "store ONLY major events: feature completed, bug resolved, decision, milestone, \
                 incident.",
            ),
            (
                "manual",
                "store episodic memories ONLY when the user explicitly asks; never auto-store \
                 events.",
            ),
        ];
        for (mode, (name, rule)) in EpisodicMode::ALL.iter().zip(want) {
            assert_eq!(mode.as_str(), name);
            assert_eq!(mode.rule(), rule);
            assert_eq!(EpisodicMode::parse(name), Some(*mode));
        }
        assert_eq!(EpisodicMode::DEFAULT.as_str(), "major-events");
        for bad in ["", "major", "Major-Events", "major_events", " manual"] {
            assert_eq!(EpisodicMode::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_exe_path_is_written_the_way_a_shell_can_run_it() {
        let cases: &[(&str, bool, &str)] = &[
            (
                "/opt/sm/supercharged-memory",
                false,
                "/opt/sm/supercharged-memory",
            ),
            (
                "/home/u/My Tools/supercharged-memory",
                false,
                "\"/home/u/My Tools/supercharged-memory\"",
            ),
            // canonicalize() on Windows yields the verbatim form.
            (
                r"\\?\C:\Users\me\bin\supercharged-memory.exe",
                true,
                "C:/Users/me/bin/supercharged-memory.exe",
            ),
            (
                r"\\?\C:\Program Files\sm\supercharged-memory.exe",
                true,
                "\"C:/Program Files/sm/supercharged-memory.exe\"",
            ),
            (
                r"\\?\UNC\server\share\sm.exe",
                true,
                "//server/share/sm.exe",
            ),
            (r"C:\tools\sm.exe", true, "C:/tools/sm.exe"),
        ];
        for (raw, windows, want) in cases {
            assert_eq!(
                exe_for_instructions(Path::new(raw), *windows).as_deref(),
                Ok(*want),
                "{raw}"
            );
        }
        // A double quote cannot be quoted: an error, not a broken command.
        assert!(exe_for_instructions(Path::new("/odd\"dir/sm"), false).is_err());
    }

    #[test]
    fn the_headline_and_a_legacy_block_are_recognised_anywhere() {
        let legacy = format!(
            "# Mine\n\n<!-- BEGIN agentic-memory (managed by install-claude-md.sh) -->\n\
             <!-- supercharged-memory: synced-at 1a2b3c -->\n{HEADLINE}\n\nbody\n{BLOCK_END}\n"
        );
        assert!(has_headline(&legacy));
        assert!(has_legacy_block(&legacy));
        let ours = block(&format!("{HEADLINE}\nbody\n"), "0.1.0");
        assert!(has_headline(&ours));
        assert!(!has_legacy_block(&ours));
        assert!(!has_headline("# Long-term memory\nsomething else\n"));
        assert!(!has_legacy_block("# Mine\n"));
    }

    #[test]
    fn an_appended_block_is_set_off_by_one_blank_line() {
        assert_eq!(separator(""), "");
        assert_eq!(separator("# Mine"), "\n\n");
        assert_eq!(separator("# Mine\n"), "\n");
        assert_eq!(separator("# Mine\n\n"), "");
    }
}
