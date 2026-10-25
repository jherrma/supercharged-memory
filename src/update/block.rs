//! `update`'s CLAUDE.md report: the managed block, compared with what this
//! binary renders for the same machine.
//!
//! `update` never writes CLAUDE.md. It says what differs and what to replace,
//! and the agent edits once the user agrees. Two findings are named on their
//! own rather than left in the diff: the block naming another binary (it
//! moved, or a second copy is running), and text inside the markers that the
//! render lacks (a local hand-edit the replacement would destroy).

use crate::claude_md::{self, EpisodicMode};

/// The anchor of the episodic policy line; the mode is the word in backticks
/// right after it. The installer script and the old UPDATE.md read it the same
/// way.
pub const MODE_ANCHOR: &str = "Episodic policy on this machine — `";

/// How every stamp line starts: `rendered-by <version>` (`init`) or
/// `synced-at <sha>` (the installer script).
const STAMP_PREFIX: &str = "<!-- supercharged-memory: ";
const RENDERED_BY: &str = "<!-- supercharged-memory: rendered-by ";

/// Lines of context around each change in the diff.
const CONTEXT: usize = 3;

/// The managed block, cut out of CLAUDE.md.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block<'a> {
    /// The BEGIN line: `init`'s, or the installer script's.
    pub begin: &'a str,
    /// The first line inside, when it is a stamp (`rendered-by` or `synced-at`).
    pub stamp: Option<&'a str>,
    /// Everything else between the markers, one entry per line, without the
    /// line ending.
    pub body: Vec<&'a str>,
}

impl Block<'_> {
    /// Written by the installer script from a repository checkout: its
    /// commands name the Python scripts.
    pub fn legacy(&self) -> bool {
        self.begin.trim() != claude_md::BLOCK_BEGIN
            || self
                .stamp
                .is_some_and(|s| s.starts_with(claude_md::LEGACY_STAMP))
    }

    /// The version a `rendered-by` stamp names.
    pub fn rendered_by(&self) -> Option<&str> {
        self.stamp?
            .strip_prefix(RENDERED_BY)?
            .trim_end()
            .strip_suffix("-->")
            .map(str::trim)
    }
}

/// Where the block is, if anywhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Located<'a> {
    /// No markers. `headline`: the instructions' headline is there anyway.
    Absent {
        headline: bool,
    },
    /// Markers that do not delimit exactly one block.
    Ambiguous(String),
    Found(Block<'a>),
}

/// Find the block: a line starting with the BEGIN prefix, through the next
/// END line.
pub fn locate(text: &str) -> Located<'_> {
    let lines: Vec<&str> = text.lines().collect();
    let begins: Vec<usize> = (0..lines.len())
        .filter(|&i| {
            lines[i]
                .trim_start()
                .starts_with(claude_md::BLOCK_BEGIN_PREFIX)
        })
        .collect();
    let ends: Vec<usize> = (0..lines.len())
        .filter(|&i| lines[i].trim() == claude_md::BLOCK_END)
        .collect();
    let (begin, end) = match (begins.as_slice(), ends.as_slice()) {
        ([], []) => {
            return Located::Absent {
                headline: claude_md::has_headline(text),
            };
        }
        ([], _) => return Located::Ambiguous("an END marker without a BEGIN marker".into()),
        ([b], [e]) if e > b => (*b, *e),
        ([_], []) => {
            return Located::Ambiguous("a BEGIN marker without an END marker after it".into());
        }
        ([_], _) => {
            return Located::Ambiguous(format!(
                "{} END markers around one BEGIN marker",
                ends.len()
            ));
        }
        (_, _) => {
            return Located::Ambiguous(format!(
                "{} BEGIN markers: two blocks or more, or a stray marker",
                begins.len()
            ));
        }
    };
    let inside = &lines[begin + 1..end];
    let (stamp, body) = match inside.split_first() {
        Some((first, rest)) if first.trim_start().starts_with(STAMP_PREFIX) => {
            (Some(first.trim()), rest)
        }
        _ => (None, inside),
    };
    Located::Found(Block {
        begin: lines[begin],
        stamp,
        body: body.to_vec(),
    })
}

/// The episodic mode the block was installed with, read from its policy line.
/// `None` when no such line names a mode, or two of them disagree: the caller
/// asks the user, and never falls back to a default.
pub fn episodic_mode(body: &[&str]) -> Option<EpisodicMode> {
    let mut found = None;
    for line in body {
        let mut rest = *line;
        while let Some(at) = rest.find(MODE_ANCHOR) {
            let after = &rest[at + MODE_ANCHOR.len()..];
            let end = after.find('`')?;
            let mode = EpisodicMode::parse(&after[..end])?;
            if found.is_some_and(|f| f != mode) {
                return None;
            }
            found = Some(mode);
            rest = &after[end..];
        }
    }
    found
}

/// The binary the block's commands run: the most common first word, in a
/// backtick span, that is an absolute path followed by a subcommand. `None`
/// when no span looks like that -- a legacy block's spans start with
/// `python3` or `bash`.
pub fn binary_path(body: &[&str]) -> Option<String> {
    // In order of first appearance, so a tie goes to the first one named.
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for line in body {
        let parts: Vec<&str> = line.split('`').collect();
        // Odd parts are inside backticks; the last one only if it was closed.
        for span in parts.iter().skip(1).step_by(2).take((parts.len() - 1) / 2) {
            if let Some(word) = command_word(span) {
                match counts.iter_mut().find(|(w, _)| *w == word) {
                    Some((_, n)) => *n += 1,
                    None => counts.push((word, 1)),
                }
            }
        }
    }
    let most = counts.iter().map(|(_, n)| *n).max()?;
    counts
        .into_iter()
        .find(|(_, n)| *n == most)
        .map(|(w, _)| w.to_string())
}

/// The first word of a command, as written -- quoted when it is -- if it is an
/// absolute path and a subcommand follows it.
fn command_word(span: &str) -> Option<&str> {
    let span = span.trim();
    let (word, rest) = match span.strip_prefix('"') {
        Some(quoted) => {
            let end = quoted.find('"')?;
            (&span[..end + 2], &quoted[end + 1..])
        }
        None => span.split_at(span.find(char::is_whitespace).unwrap_or(span.len())),
    };
    if !absolute(word.trim_matches('"')) {
        return None;
    }
    let next = rest.split_whitespace().next()?;
    let mut chars = next.chars();
    let subcommand = chars.next()?.is_ascii_lowercase()
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    subcommand.then_some(word)
}

/// `/…`, or a Windows drive path in either slash.
fn absolute(path: &str) -> bool {
    let b = path.as_bytes();
    path.starts_with('/')
        || (b.len() >= 3
            && b[0].is_ascii_alphabetic()
            && b[1] == b':'
            && matches!(b[2], b'/' | b'\\'))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Same,
    Del,
    Add,
}

/// The edit script from `old` to `new` over their longest common
/// subsequence; at a change, deletions come before additions.
fn ops(old: &[&str], new: &[&str]) -> Vec<Op> {
    let (n, m) = (old.len(), new.len());
    // lcs[i][j]: the longest common subsequence of old[i..] and new[j..].
    let mut lcs = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if old[i] == new[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::with_capacity(n + m);
    while i < n || j < m {
        if i < n && j < m && old[i] == new[j] {
            out.push(Op::Same);
            i += 1;
            j += 1;
        } else if i < n && (j == m || lcs[i + 1][j] >= lcs[i][j + 1]) {
            out.push(Op::Del);
            i += 1;
        } else {
            out.push(Op::Add);
            j += 1;
        }
    }
    out
}

/// A unified diff of `old` against `new`, with three lines of context: hunk
/// headers and lines only, no file headers. Empty when they are equal.
pub fn diff(old: &[&str], new: &[&str]) -> Vec<String> {
    let ops = ops(old, new);
    let changes: Vec<usize> = (0..ops.len()).filter(|&k| ops[k] != Op::Same).collect();
    let Some((&first, rest)) = changes.split_first() else {
        return Vec::new();
    };
    // Changes closer than twice the context share a hunk.
    let mut hunks = vec![(first, first)];
    for &k in rest {
        let last = hunks.last_mut().expect("one hunk at least");
        if k - last.1 - 1 <= 2 * CONTEXT {
            last.1 = k;
        } else {
            hunks.push((k, k));
        }
    }

    let mut out = Vec::new();
    for (a, b) in hunks {
        let start = a.saturating_sub(CONTEXT);
        let end = (b + CONTEXT + 1).min(ops.len());
        let consumed = |range: std::ops::Range<usize>, side: Op| {
            ops[range]
                .iter()
                .filter(|o| **o == Op::Same || **o == side)
                .count()
        };
        let (old_before, new_before) = (consumed(0..start, Op::Del), consumed(0..start, Op::Add));
        let (old_len, new_len) = (consumed(start..end, Op::Del), consumed(start..end, Op::Add));
        out.push(format!(
            "@@ -{} +{} @@",
            range(old_before, old_len),
            range(new_before, new_len)
        ));
        let (mut i, mut j) = (old_before, new_before);
        for op in &ops[start..end] {
            match op {
                Op::Same => {
                    out.push(format!(" {}", old[i]));
                    i += 1;
                    j += 1;
                }
                Op::Del => {
                    out.push(format!("-{}", old[i]));
                    i += 1;
                }
                Op::Add => {
                    out.push(format!("+{}", new[j]));
                    j += 1;
                }
            }
        }
    }
    out
}

/// A hunk's line range as diff writes it: the count left out when it is one,
/// and an empty range named by the line before it.
fn range(before: usize, len: usize) -> String {
    match len {
        0 => format!("{before},0"),
        1 => format!("{}", before + 1),
        _ => format!("{},{len}", before + 1),
    }
}

/// What the block is compared with: the template rendered with this binary's
/// path and the block's episodic mode, unless the user named one.
#[derive(Debug, Clone, Copy)]
pub struct Render<'a> {
    /// The binary's path as the instructions write it.
    pub exe: &'a str,
    /// `--episodic-mode`, when given: the user's answer overrides the block.
    pub mode: Option<EpisodicMode>,
    pub template: &'a str,
    /// This binary's version, for the stamp.
    pub version: &'a str,
}

impl Render<'_> {
    fn render(&self, mode: EpisodicMode) -> Result<String, Vec<String>> {
        claude_md::render(
            self.template,
            &[
                ("EXE", self.exe),
                ("EPISODIC_MODE", mode.as_str()),
                ("EPISODIC_RULE", mode.rule()),
            ],
        )
    }
}

/// The report on CLAUDE.md.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Report {
    /// No file, or no managed block and no headline in it.
    NoBlock,
    Unreadable(String),
    /// The headline is there, but no markers around it.
    Unmanaged,
    Ambiguous(String),
    /// No readable policy line, and no `--episodic-mode`.
    ModeUnreadable {
        legacy: bool,
    },
    /// The embedded template names placeholders this binary cannot fill.
    Unrenderable {
        names: Vec<String>,
        legacy: bool,
    },
    Compared(Box<Compared>),
}

/// A block compared with the render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compared {
    pub legacy: bool,
    /// The version that rendered the block, from its stamp.
    pub rendered_by: Option<String>,
    /// This binary's version.
    pub version: String,
    pub mode: EpisodicMode,
    /// The mode came from `--episodic-mode`, not from the block.
    pub mode_from_flag: bool,
    /// What the block's policy line says, if anything.
    pub block_mode: Option<EpisodicMode>,
    /// This binary, as the instructions write it.
    pub exe: String,
    /// The binary the block's commands name.
    pub block_exe: Option<String>,
    /// The unified diff of the block against the render; empty when equal.
    pub diff: Vec<String>,
    /// Non-blank lines inside the markers that the render does not have,
    /// once a moved binary path is accounted for.
    pub lacks: Vec<String>,
    /// The template, rendered.
    pub render: String,
}

/// Compare CLAUDE.md's block (`text`; `None` when there is no file) with the
/// render.
pub fn compare(text: Option<&str>, r: &Render) -> Report {
    let Some(text) = text else {
        return Report::NoBlock;
    };
    let block = match locate(text) {
        Located::Absent { headline: false } => return Report::NoBlock,
        Located::Absent { headline: true } => return Report::Unmanaged,
        Located::Ambiguous(why) => return Report::Ambiguous(why),
        Located::Found(block) => block,
    };
    let legacy = block.legacy();
    let block_mode = episodic_mode(&block.body);
    let Some(mode) = r.mode.or(block_mode) else {
        return Report::ModeUnreadable { legacy };
    };
    let render = match r.render(mode) {
        Ok(render) => render,
        Err(names) => return Report::Unrenderable { names, legacy },
    };
    let new: Vec<&str> = render.lines().collect();
    let block_exe = binary_path(&block.body);

    // A moved binary changes every command line. With the paths aligned, what
    // the render still lacks is text it never had.
    let aligned: Vec<String> = block
        .body
        .iter()
        .map(|line| match &block_exe {
            Some(old) if old != r.exe => line.replace(old.as_str(), r.exe),
            _ => line.to_string(),
        })
        .collect();
    let aligned: Vec<&str> = aligned.iter().map(String::as_str).collect();
    let lacks = ops(&aligned, &new)
        .iter()
        .filter(|o| **o != Op::Add)
        .zip(&aligned)
        .filter(|(o, line)| **o == Op::Del && !line.trim().is_empty())
        .map(|(_, line)| line.to_string())
        .collect();

    Report::Compared(Box::new(Compared {
        legacy,
        rendered_by: block.rendered_by().map(str::to_string),
        version: r.version.to_string(),
        mode,
        mode_from_flag: r.mode.is_some(),
        block_mode,
        exe: r.exe.to_string(),
        block_exe,
        diff: diff(&block.body, &new),
        lacks,
        render,
    }))
}

impl Report {
    /// The agent has something to relay and ask about. A template this
    /// binary cannot render is not that: nothing the user does changes it.
    pub fn actionable(&self) -> bool {
        match self {
            Report::Unrenderable { legacy, .. } => *legacy,
            Report::Compared(c) => c.legacy || !c.diff.is_empty(),
            _ => true,
        }
    }

    /// The block was written by the legacy installer script.
    pub fn legacy(&self) -> bool {
        match self {
            Report::ModeUnreadable { legacy } | Report::Unrenderable { legacy, .. } => *legacy,
            Report::Compared(c) => c.legacy,
            _ => false,
        }
    }

    /// The report's lines, indented two spaces; diff lines verbatim.
    pub fn lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        match self {
            Report::NoBlock => out.push(
                "  no managed block, and no instructions' headline: the memory instructions are \
                 not installed in this file. Once the user agrees, run init to append them."
                    .to_string(),
            ),
            Report::Unreadable(why) => out.push(format!(
                "  cannot read it: {why} — the instructions were not compared."
            )),
            Report::Unmanaged => out.push(
                "  the instructions' headline is there, but no managed-block markers around it: \
                 an unmanaged block, cannot diff. Ask the user how it was installed before \
                 changing anything in it."
                    .to_string(),
            ),
            Report::Ambiguous(why) => out.push(format!(
                "  the managed block cannot be delimited: {why}. Ask the user which text is the \
                 block before changing anything."
            )),
            Report::ModeUnreadable { legacy } => {
                if *legacy {
                    cutover(&mut out);
                }
                out.push(
                    "  the block has no readable `Episodic policy on this machine` line, so the \
                     episodic mode it was installed with is unknown — never assume the default. \
                     Ask the user which one, then re-run with --episodic-mode <mode>:"
                        .to_string(),
                );
                for m in EpisodicMode::ALL {
                    out.push(format!("    {:<13} — {}", m.as_str(), m.rule()));
                }
            }
            Report::Unrenderable { names, legacy } => {
                if *legacy {
                    cutover(&mut out);
                }
                out.push(format!(
                    "  not compared: the embedded template names {}, which this binary cannot \
                     fill.",
                    names.join(", ")
                ));
            }
            Report::Compared(c) => c.lines(&mut out),
        }
        out
    }
}

impl Compared {
    fn lines(&self, out: &mut Vec<String>) {
        if self.legacy {
            cutover(out);
        } else if self.diff.is_empty() {
            out.push(format!(
                "  instructions current: the block matches this binary's render (episodic mode \
                 {}).",
                self.mode.as_str()
            ));
            return;
        }

        out.push(match (self.mode_from_flag, self.block_mode) {
            (false, _) => format!("  episodic mode: {} (from the block)", self.mode.as_str()),
            (true, Some(b)) => format!(
                "  episodic mode: {} (from --episodic-mode; the block says {})",
                self.mode.as_str(),
                b.as_str()
            ),
            (true, None) => format!(
                "  episodic mode: {} (from --episodic-mode; the block names none)",
                self.mode.as_str()
            ),
        });

        if !self.legacy {
            match &self.block_exe {
                Some(old) if *old != self.exe => out.push(format!(
                    "  FINDING — binary path: the block's commands run {old}, but this binary is \
                     {}: it moved, or a second copy is running. Ask the user which one is real \
                     before replacing the block — the replacement points every command at {}.",
                    self.exe, self.exe
                )),
                Some(_) => {}
                None => out.push(
                    "  FINDING — binary path: no command in the block names a binary, so whether \
                     it moved cannot be checked."
                        .to_string(),
                ),
            }
        }

        if self.legacy {
            out.push(
                "  FINDING — local edits: a legacy block differs from the render on every command \
                 line, so text edited by hand cannot be told apart here. Compare it by hand, with \
                 the diff below, before replacing it."
                    .to_string(),
            );
        } else if !self.lacks.is_empty() {
            let origin = match &self.rendered_by {
                Some(v) if *v == self.version => {
                    "a local hand-edit (this version's template rendered the block)".to_string()
                }
                Some(v) => format!("a local hand-edit, or text the template of {v} had"),
                None => "a local hand-edit, or text an older template had".to_string(),
            };
            out.push(format!(
                "  FINDING — text inside the markers that the render lacks ({} lines): {origin}. \
                 Replacing the block removes it. Show each line to the user and ask whether to \
                 keep it (outside the markers) or drop it:",
                self.lacks.len()
            ));
            for line in &self.lacks {
                out.push(format!("    | {line}"));
            }
        }

        out.push(
            "  The block differs from this binary's render. The text between the markers (stamp \
             line left out) against the render:"
                .to_string(),
        );
        out.push("--- CLAUDE.md, between the markers".to_string());
        out.push("+++ the render".to_string());
        out.extend(self.diff.iter().cloned());
        out.push(format!(
            "  Once the user agrees, replace everything from the BEGIN line through the END line \
             with the output of `{}` — update never writes CLAUDE.md. Then restart the Claude \
             Code session.",
            self.print_command()
        ));
    }

    /// The command that prints the replacement, with the mode it was
    /// rendered with when the user named it.
    fn print_command(&self) -> String {
        if self.mode_from_flag {
            format!(
                "update --print-block --episodic-mode {}",
                self.mode.as_str()
            )
        } else {
            "update --print-block".to_string()
        }
    }
}

fn cutover(out: &mut Vec<String>) {
    out.push(
        "  a legacy block (synced-at <sha>), written by the installer script from a repository \
         checkout. The cutover note:"
            .to_string(),
    );
    for line in claude_md::CUTOVER_NOTE.lines() {
        out.push(format!("    {line}"));
    }
}

/// The whole block `update --print-block` prints: markers, this binary's
/// stamp, the render. The reason it cannot be printed otherwise.
pub fn print_block(text: Option<&str>, r: &Render) -> Result<String, String> {
    let block_mode = text.and_then(|t| match locate(t) {
        Located::Found(block) => episodic_mode(&block.body),
        _ => None,
    });
    let mode = r.mode.or(block_mode).ok_or_else(|| {
        "no episodic mode to render with: CLAUDE.md has no managed block with a readable \
         `Episodic policy on this machine` line. Ask the user which mode, then pass \
         --episodic-mode <mode>"
            .to_string()
    })?;
    let rendered = r.render(mode).map_err(|names| {
        format!(
            "the embedded template names {}, which this binary cannot fill — there is no block \
             to print",
            names.join(", ")
        )
    })?;
    Ok(claude_md::block(&rendered, r.version))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_md::{BLOCK_BEGIN, BLOCK_END, CUTOVER_NOTE, HEADLINE};

    const EXE: &str = "/opt/sm/supercharged-memory";
    const OWN: &str = "0.2.0";

    /// A template shaped like the real one, naming only what the binary fills.
    const TEMPLATE: &str = "# Long-term memory — Turso vector DB (supercharged-memory)\n\
                            \n\
                            Run `{{EXE}} status` first; store with `{{EXE}} remember --table semantic …`.\n\
                            Search with `{{EXE}} recall \"<query>\"`.\n\
                            **Episodic policy on this machine — `{{EPISODIC_MODE}}`: {{EPISODIC_RULE}}**\n\
                            - \"backup\" → `{{EXE}} backup`.\n\
                            Statuses: `READY n` or `EMPTY`.\n";

    /// The same template one release earlier: the search line read otherwise.
    const OLD_TEMPLATE: &str = "# Long-term memory — Turso vector DB (supercharged-memory)\n\
                                \n\
                                Run `{{EXE}} status` first; store with `{{EXE}} remember --table semantic …`.\n\
                                Search memory with `{{EXE}} recall`.\n\
                                **Episodic policy on this machine — `{{EPISODIC_MODE}}`: {{EPISODIC_RULE}}**\n\
                                - \"backup\" → `{{EXE}} backup`.\n\
                                Statuses: `READY n` or `EMPTY`.\n";

    const LEGACY: &str = "# Mine\n\n\
        <!-- BEGIN agentic-memory (managed by install-claude-md.sh) -->\n\
        <!-- supercharged-memory: synced-at 0123abcd -->\n\
        # Long-term memory — Turso vector DB (supercharged-memory)\n\
        \n\
        Run them as `python3 \"/repo/scripts/<cmd>\"`; status is `python3 \"/repo/scripts/recall.py\" --status`.\n\
        **Episodic policy on this machine — `major-actions`: store one for every substantive action.**\n\
        - \"backup\" → `bash \"/repo/scripts/supercharged-memory-backup.sh\"`.\n\
        <!-- END agentic-memory -->\n";

    fn render_with(template: &str, exe: &str, mode: EpisodicMode) -> String {
        claude_md::render(
            template,
            &[
                ("EXE", exe),
                ("EPISODIC_MODE", mode.as_str()),
                ("EPISODIC_RULE", mode.rule()),
            ],
        )
        .unwrap()
    }

    /// CLAUDE.md with the user's own text and a block `init` wrote.
    fn installed(template: &str, exe: &str, mode: EpisodicMode, version: &str) -> String {
        format!(
            "# Mine\n\nAlways use pnpm.\n\n{}",
            claude_md::block(&render_with(template, exe, mode), version)
        )
    }

    fn r() -> Render<'static> {
        Render {
            exe: EXE,
            mode: None,
            template: TEMPLATE,
            version: OWN,
        }
    }

    fn compared(report: Report) -> Compared {
        match report {
            Report::Compared(c) => *c,
            other => panic!("not compared: {other:?}"),
        }
    }

    fn text(lines: &[String]) -> String {
        lines.join("\n")
    }

    // ---- the report fixtures ---------------------------------------------------

    #[test]
    fn an_identical_block_is_current() {
        let md = installed(TEMPLATE, EXE, EpisodicMode::MajorActions, OWN);
        let report = compare(Some(&md), &r());
        assert!(!report.actionable());
        let shown = text(&report.lines());
        assert!(shown.contains("instructions current"), "{shown}");
        let c = compared(report);
        assert!(c.diff.is_empty() && c.lacks.is_empty(), "{c:?}");
        assert_eq!(c.mode, EpisodicMode::MajorActions);
        assert!(!c.mode_from_flag);
        assert_eq!(c.block_exe.as_deref(), Some(EXE));
        assert_eq!(c.rendered_by.as_deref(), Some(OWN));
    }

    /// The stamp line is not compared: a block rendered by an older binary
    /// from the same template is current.
    #[test]
    fn the_stamp_line_is_ignored() {
        let md = installed(TEMPLATE, EXE, EpisodicMode::Manual, "0.1.0");
        let report = compare(Some(&md), &r());
        assert!(!report.actionable(), "{report:?}");
        assert!(compared(report).diff.is_empty());
    }

    #[test]
    fn a_block_from_an_older_template_shows_a_diff_and_what_to_replace() {
        let md = installed(OLD_TEMPLATE, EXE, EpisodicMode::MajorEvents, "0.1.0");
        let report = compare(Some(&md), &r());
        assert!(report.actionable());
        let shown = text(&report.lines());
        for want in [
            "-Search memory with `/opt/sm/supercharged-memory recall`.",
            "+Search with `/opt/sm/supercharged-memory recall \"<query>\"`.",
            "@@ ",
            "update --print-block",
            "BEGIN",
            "END",
        ] {
            assert!(shown.contains(want), "{want:?} not in:\n{shown}");
        }
        // The old line is text the render lacks; the block's older version
        // means it may be the old template's, not a hand-edit.
        let c = compared(report);
        assert_eq!(
            c.lacks,
            vec!["Search memory with `/opt/sm/supercharged-memory recall`.".to_string()]
        );
        assert!(shown.contains("0.1.0"), "{shown}");
    }

    /// The block names another binary: named on its own, with both paths,
    /// and not mistaken for hand-edits on every command line.
    #[test]
    fn a_moved_binary_is_a_finding_of_its_own() {
        let old = "/home/u/Downloads/supercharged-memory";
        let md = installed(TEMPLATE, old, EpisodicMode::MajorEvents, OWN);
        let report = compare(Some(&md), &r());
        assert!(report.actionable());
        let shown = text(&report.lines());
        assert!(shown.contains("binary path"), "{shown}");
        assert!(shown.contains(old) && shown.contains(EXE), "{shown}");
        assert!(shown.contains("Ask the user which"), "{shown}");
        let c = compared(report);
        assert_eq!(c.block_exe.as_deref(), Some(old));
        assert!(c.lacks.is_empty(), "{:?}", c.lacks);
        assert!(!c.diff.is_empty());
    }

    /// A line the user added inside the markers is listed on its own: the
    /// replacement would destroy it.
    #[test]
    fn a_hand_edit_inside_the_markers_is_a_finding_of_its_own() {
        let md = installed(TEMPLATE, EXE, EpisodicMode::MajorEvents, OWN).replace(
            "Statuses: `READY n` or `EMPTY`.\n",
            "Statuses: `READY n` or `EMPTY`.\nAlways answer in German.\n\n",
        );
        let report = compare(Some(&md), &r());
        assert!(report.actionable());
        let shown = text(&report.lines());
        assert!(shown.contains("| Always answer in German."), "{shown}");
        assert!(shown.contains("hand-edit"), "{shown}");
        assert!(!shown.contains("+Always answer in German."), "{shown}");
        assert!(shown.contains("-Always answer in German."), "{shown}");
        let c = compared(report);
        // Blank lines are in the diff, not in the finding.
        assert_eq!(c.lacks, vec!["Always answer in German.".to_string()]);
    }

    #[test]
    fn a_legacy_block_gets_the_cutover_note() {
        let report = compare(Some(LEGACY), &r());
        assert!(report.actionable());
        let shown = text(&report.lines());
        assert!(shown.contains("legacy"), "{shown}");
        for line in CUTOVER_NOTE.lines() {
            assert!(shown.contains(line), "{line:?} not in:\n{shown}");
        }
        // Every command line differs, so the lines are not listed one by one
        // as hand-edits; the user compares by hand.
        assert!(!shown.contains("| Run them as"), "{shown}");
        assert!(shown.contains("by hand"), "{shown}");
        let c = compared(report);
        assert!(c.legacy);
        assert_eq!(c.mode, EpisodicMode::MajorActions);
        assert_eq!(c.block_exe, None);
        assert!(!c.diff.is_empty());
    }

    #[test]
    fn no_block_points_at_init() {
        for md in [None, Some(""), Some("# Mine\n\nAlways use pnpm.\n")] {
            let report = compare(md, &r());
            assert_eq!(report, Report::NoBlock, "{md:?}");
            assert!(report.actionable());
            let shown = text(&report.lines());
            assert!(shown.contains("no managed block"), "{shown}");
            assert!(shown.contains("init"), "{shown}");
        }
    }

    #[test]
    fn a_headline_without_markers_is_an_unmanaged_block() {
        let md = format!("# Mine\n\n{HEADLINE}\n\nRun things.\n");
        let report = compare(Some(&md), &r());
        assert_eq!(report, Report::Unmanaged);
        assert!(report.actionable());
        let shown = text(&report.lines());
        assert!(shown.contains("unmanaged block, cannot diff"), "{shown}");
        assert!(shown.contains("Ask the user"), "{shown}");
    }

    /// Without a readable policy line the mode is asked for, never defaulted
    /// (the installer's lesson); `--episodic-mode` is how the answer comes back.
    #[test]
    fn an_unreadable_episodic_mode_is_asked_for_never_defaulted() {
        let md =
            installed(TEMPLATE, EXE, EpisodicMode::Manual, OWN).replace(MODE_ANCHOR, "Policy — `");
        let report = compare(Some(&md), &r());
        assert_eq!(report, Report::ModeUnreadable { legacy: false });
        assert!(report.actionable());
        let shown = text(&report.lines());
        for want in ["Ask the user", "--episodic-mode", "never"] {
            assert!(shown.contains(want), "{want:?} not in:\n{shown}");
        }
        for m in EpisodicMode::ALL {
            assert!(shown.contains(m.as_str()), "{shown}");
        }

        let answered = Render {
            mode: Some(EpisodicMode::Manual),
            ..r()
        };
        let c = compared(compare(Some(&md), &answered));
        assert_eq!(c.mode, EpisodicMode::Manual);
        assert!(c.mode_from_flag);
        assert_eq!(c.block_mode, None);
    }

    #[test]
    fn markers_that_do_not_delimit_one_block_are_ambiguous() {
        let one = claude_md::block(&render_with(TEMPLATE, EXE, EpisodicMode::Manual), OWN);
        let two = format!("{one}\n{one}");
        let unterminated = format!("# Mine\n{BLOCK_BEGIN}\n{HEADLINE}\n");
        let end_only = format!("# Mine\n{HEADLINE}\n{BLOCK_END}\n");
        for (md, why) in [(two, "two"), (unterminated, "END"), (end_only, "BEGIN")] {
            let report = compare(Some(&md), &r());
            match &report {
                Report::Ambiguous(w) => assert!(w.contains(why), "{why:?} not in {w:?}"),
                other => panic!("{why}: {other:?}"),
            }
            assert!(report.actionable());
            assert!(text(&report.lines()).contains("cannot be delimited"));
        }
    }

    /// A template naming placeholders the binary cannot fill is reported, not
    /// rendered -- and not something for the user to act on.
    #[test]
    fn an_unrenderable_template_is_not_compared() {
        let md = installed(TEMPLATE, EXE, EpisodicMode::Manual, OWN);
        let r = Render {
            template: "# H\n{{PY}} \"{{BASE_PATH}}/scripts/x\" {{EXE}}\n",
            ..r()
        };
        let report = compare(Some(&md), &r);
        assert_eq!(
            report,
            Report::Unrenderable {
                names: vec!["{{PY}}".into(), "{{BASE_PATH}}".into()],
                legacy: false
            }
        );
        assert!(!report.actionable());
        let shown = text(&report.lines());
        assert!(shown.contains("not compared"), "{shown}");
        assert!(shown.contains("{{BASE_PATH}}"), "{shown}");

        // A legacy block still needs the cutover, render or not.
        let legacy = compare(Some(LEGACY), &r);
        assert_eq!(
            legacy,
            Report::Unrenderable {
                names: vec!["{{PY}}".into(), "{{BASE_PATH}}".into()],
                legacy: true
            }
        );
        assert!(legacy.actionable());
        assert!(text(&legacy.lines()).contains(CUTOVER_NOTE.lines().next().unwrap()));
    }

    #[test]
    fn crlf_line_endings_do_not_make_a_difference() {
        let md = installed(TEMPLATE, EXE, EpisodicMode::MajorEvents, OWN).replace('\n', "\r\n");
        let report = compare(Some(&md), &r());
        assert!(!report.actionable(), "{report:?}");
        assert!(compared(report).diff.is_empty());
    }

    // ---- the pieces --------------------------------------------------------

    #[test]
    fn the_real_template_carries_the_policy_line_anchor() {
        assert!(
            claude_md::TEMPLATE.contains(&format!("{MODE_ANCHOR}{{{{EPISODIC_MODE}}}}`")),
            "the episodic policy line moved: update could no longer read a block's mode"
        );
    }

    #[test]
    fn the_episodic_mode_is_read_from_the_policy_line() {
        let line = |m: &str| format!("**{MODE_ANCHOR}{m}`: rule**");
        let manual = line("manual");
        let events = line("major-events");
        let bogus = line("sometimes");
        assert_eq!(episodic_mode(&[&manual]), Some(EpisodicMode::Manual));
        assert_eq!(
            episodic_mode(&["x", &events, "y", &events]),
            Some(EpisodicMode::MajorEvents)
        );
        assert_eq!(
            episodic_mode(&[&manual, &events]),
            None,
            "two modes disagree"
        );
        assert_eq!(episodic_mode(&[&bogus]), None);
        assert_eq!(episodic_mode(&["no policy here"]), None);
    }

    #[test]
    fn the_binary_path_is_the_most_common_command_word() {
        let body = [
            "Run `/opt/sm/supercharged-memory status` first.",
            "Then `/opt/sm/supercharged-memory recall \"x\"` and `/old/sm remember`.",
            "Paths alone are not commands: `/opt/sm/README.md`, nor `READY n`.",
        ];
        assert_eq!(
            binary_path(&body).as_deref(),
            Some("/opt/sm/supercharged-memory")
        );
        let quoted = ["`\"/home/u/My Tools/supercharged-memory\" status`"];
        assert_eq!(
            binary_path(&quoted).as_deref(),
            Some("\"/home/u/My Tools/supercharged-memory\"")
        );
        let windows = ["`C:/Users/me/bin/sm.exe recall --baseline`"];
        assert_eq!(
            binary_path(&windows).as_deref(),
            Some("C:/Users/me/bin/sm.exe")
        );
        let legacy = [
            "`python3 \"/repo/scripts/recall.py\" --status`",
            "`bash \"/repo/scripts/supercharged-memory-backup.sh\"`",
            "`/repo/instructions/SLEEP.md`",
        ];
        assert_eq!(binary_path(&legacy), None);
    }

    #[test]
    fn diff_is_unified_with_three_lines_of_context() {
        let old: Vec<String> = (1..=20).map(|i| i.to_string()).collect();
        let mut new = old.clone();
        new[1] = "2x".into();
        new[17] = "18x".into();
        let old: Vec<&str> = old.iter().map(String::as_str).collect();
        let new: Vec<&str> = new.iter().map(String::as_str).collect();
        assert_eq!(
            diff(&old, &new),
            vec![
                "@@ -1,5 +1,5 @@",
                " 1",
                "-2",
                "+2x",
                " 3",
                " 4",
                " 5",
                "@@ -15,6 +15,6 @@",
                " 15",
                " 16",
                " 17",
                "-18",
                "+18x",
                " 19",
                " 20",
            ]
        );
        // Two changes six lines apart share one hunk; an appended line too.
        let old = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"];
        let new = ["a", "b", "c", "d", "E", "f", "g", "h", "i", "j", "k"];
        assert_eq!(
            diff(&old, &new),
            vec![
                "@@ -2,9 +2,10 @@",
                " b",
                " c",
                " d",
                "-e",
                "+E",
                " f",
                " g",
                " h",
                " i",
                " j",
                "+k",
            ]
        );
        assert!(diff(&old, &old).is_empty());
        assert_eq!(diff(&[], &["x"]), vec!["@@ -0,0 +1 @@", "+x"]);
    }

    // ---- --print-block -------------------------------------------------------

    #[test]
    fn print_block_is_the_whole_block_this_binary_would_write() {
        let md = installed(OLD_TEMPLATE, "/old/sm", EpisodicMode::MajorActions, "0.1.0");
        let printed = print_block(Some(&md), &r()).unwrap();
        assert_eq!(
            printed,
            claude_md::block(&render_with(TEMPLATE, EXE, EpisodicMode::MajorActions), OWN)
        );
        // Once the block is replaced with it, the report says current.
        let replaced = format!("# Mine\n\nAlways use pnpm.\n\n{printed}");
        assert!(!compare(Some(&replaced), &r()).actionable());
        // From a legacy block, the legacy block's mode.
        assert_eq!(
            print_block(Some(LEGACY), &r()).unwrap(),
            claude_md::block(&render_with(TEMPLATE, EXE, EpisodicMode::MajorActions), OWN)
        );
    }

    #[test]
    fn print_block_needs_a_mode_and_a_renderable_template() {
        let unreadable =
            installed(TEMPLATE, EXE, EpisodicMode::Manual, OWN).replace(MODE_ANCHOR, "P `");
        for md in [None, Some(unreadable.as_str()), Some("# Mine\n")] {
            let why = print_block(md, &r()).unwrap_err();
            assert!(why.contains("--episodic-mode"), "{why}");
        }
        let named = Render {
            mode: Some(EpisodicMode::Manual),
            ..r()
        };
        assert_eq!(
            print_block(None, &named).unwrap(),
            claude_md::block(&render_with(TEMPLATE, EXE, EpisodicMode::Manual), OWN)
        );
        let unrenderable = Render {
            template: "{{PY}}",
            ..named
        };
        assert!(
            print_block(None, &unrenderable)
                .unwrap_err()
                .contains("{{PY}}")
        );
    }
}
