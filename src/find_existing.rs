//! `find-existing-memory`: report file-based Claude memory that is not in this
//! system yet.
//!
//! Read-only and dependency-free on purpose: no database and no Ollama, so the
//! setup flow can run it before either is proven to work. The report is the
//! reference's JSON (`scripts/find-existing-memory.py`), field for field.
//!
//! Scope is the user's own memory under the Claude config dir: CLAUDE.md outside
//! this system's managed block, `memory/`, and every `projects/<slug>/memory/`,
//! the last two recursively (they nest: `team/` is shared memory). Every count and
//! total of memory to IMPORT covers `kind: memory` only:
//!
//! - `memory`: one fact, the unit a semantic row is written from
//! - `index`: a MEMORY.md pointer list; importing it stores a table of contents
//! - `claude`: CLAUDE.md outside the managed block, i.e. instructions
//! - `empty`: nothing left after trimming; `remember` refuses an empty text
//!
//! Nothing skipped is dropped silently. Whatever is not counted lands in
//! `over_max_text`, `empty`, `unreadable` or `excluded_dirs`, because a skip that
//! appears in no field reads as "you have no memory there". `nothing_to_do` -- no
//! memory file AND nothing unreadable or excluded -- is the one field to gate on.
//!
//! The walk follows symlinked directories, which is how memory parked elsewhere
//! is found, and guards the two ways that goes wrong: a link to an ancestor of
//! the memory root (`memory/loop -> ..` once dragged the whole config dir in) is
//! excluded, and a directory reached twice -- a loop, or a second path to one
//! already walked, across every root of one scan -- is walked once and reported.

use std::collections::{BTreeSet, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

// The managed block's markers: the block is this system's instructions, not
// the user's memory, whichever writer put it there.
use crate::claude_md::{BLOCK_BEGIN_PREFIX, BLOCK_END};
use crate::config::{Config, MAX_TEXT};
use crate::error::Result;
use crate::json::{self, Json};

/// Room left for the `Keywords:` line. `remember` appends `--keywords` INTO the
/// text and the 2000-char CHECK counts the result, so a file that fits the cap
/// on its own is refused once its keyword list is on the end (measured
/// 2026-09-09: a 1948-char file plus eleven keywords came to 2096). The probe
/// cannot know keywords nobody has written yet, so it flags against this stated
/// reserve -- the same 1800 chars of prose the migration runbook asks for.
pub const KEYWORD_RESERVE: usize = 200;
pub const EFFECTIVE_MAX_TEXT: usize = MAX_TEXT - KEYWORD_RESERVE;

/// `.md` only: Claude Code's file-based memory writes nothing else (its memory
/// permission gate is a `.md` suffix test). Matched case-insensitively -- a
/// `*.md` glob is case-sensitive on Linux and hid `notes.MD`.
const MEMORY_SUFFIX: &str = ".md";
/// The pointer list Claude Code keeps beside the facts.
const INDEX_NAME: &str = "MEMORY.md";
/// Subdirectories of a memory ROOT that are not memory: `agents/` holds subagent
/// definitions, `.git` a work tree. Matched at the root only -- a topic directory
/// deeper down that shares the name is memory -- and reported when they hit.
/// `team/` is deliberately absent: it is shared memory. Sorted: report order.
const SKIP_DIRS: [&str; 2] = [".git", "agents"];

const REPEAT: &str = "symlink loop or repeated directory -- walked once, not again";
const NOT_REGULAR: &str = "not a regular file (broken symlink, or a directory named *.md)";
const PROTECTED: &str = "protected subdirectory of a memory root, not memory";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Memory,
    Index,
    Claude,
    Empty,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Memory => "memory",
            Kind::Index => "index",
            Kind::Claude => "claude",
            Kind::Empty => "empty",
        }
    }
}

/// `global` applies everywhere; `project` only when the working directory
/// matches, so its facts are narrower and that has to survive the move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Global,
    Project,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Global => "global",
            Scope::Project => "project",
        }
    }
}

/// One file found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    pub kind: Kind,
    /// Characters after trimming, line endings counted once.
    pub chars: usize,
    pub scope: Scope,
    /// The `projects/<slug>` directory name, verbatim: Claude Code's mangled
    /// working directory, NOT a work-item id -- never a `remember --project`.
    pub project_dir: Option<String>,
}

/// Something that could not be read or walked, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub path: PathBuf,
    pub reason: String,
}

/// A directory left out on purpose, reported all the same.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Excluded {
    pub path: PathBuf,
    /// How many `.md` files it holds, so a topic directory that happens to be
    /// named `agents` shows its content; `None` when that is unknown.
    pub n_md: Option<usize>,
    pub reason: String,
}

/// Everything one scan found, in the reference's order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scan {
    pub config_dir: PathBuf,
    pub files: Vec<Entry>,
    pub sources: Vec<String>,
    pub unreadable: Vec<Skipped>,
    pub excluded_dirs: Vec<Excluded>,
}

/// Whether a file name ends in `.md`, in any case. A suffix test, like Claude
/// Code's own memory gate, so a bare `.md` counts too -- `Path::extension` would
/// miss that one.
pub fn is_memory_name(name: &OsStr) -> bool {
    let name = name.as_encoded_bytes();
    name.len() >= MEMORY_SUFFIX.len()
        && name[name.len() - MEMORY_SUFFIX.len()..].eq_ignore_ascii_case(MEMORY_SUFFIX.as_bytes())
}

/// A memory directory file's kind, from its name and its trimmed length.
pub fn classify(path: &Path, chars: usize) -> Kind {
    if path
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case(INDEX_NAME))
    {
        Kind::Index
    } else if chars == 0 {
        Kind::Empty
    } else {
        Kind::Memory
    }
}

/// Why a file of `chars` characters cannot become one memory as it stands, in
/// the words a worker needs to act on it; `None` when it fits the budget.
pub fn over_budget(chars: usize) -> Option<String> {
    if chars <= EFFECTIVE_MAX_TEXT {
        return None;
    }
    Some(if chars > MAX_TEXT {
        format!(
            "{chars} chars, over the {MAX_TEXT}-char hard cap (schema CHECK): split it or \
             condense it, never truncate"
        )
    } else {
        format!(
            "{chars} chars, over the {EFFECTIVE_MAX_TEXT}-char effective budget ({MAX_TEXT} cap \
             minus {KEYWORD_RESERVE} chars reserved for the 'Keywords:' line remember appends \
             into the same field). It fits the cap on its own and is refused once keywords are \
             added, so treat it like an over-cap file: split or condense"
        )
    })
}

/// Characters after trimming -- the length `remember` would store. Characters,
/// not bytes: the schema's CHECK counts characters. Trimmed the way `remember`
/// trims, which leaves the ASCII separators U+001C..=U+001F that the reference's
/// `str.strip` also removed.
pub fn char_count(text: &str) -> usize {
    text.trim().chars().count()
}

/// `\r\n` and a lone `\r` become `\n`. The reference read in text mode, so a
/// CRLF file counted one character per line ending, not two.
fn normalise_newlines(text: String) -> String {
    if !text.contains('\r') {
        return text;
    }
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// CLAUDE.md with every managed block removed: from the BEGIN prefix through
/// the next END marker, or to the end of the file when none follows.
pub fn outside_managed_block(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(BLOCK_BEGIN_PREFIX) {
        out.push_str(&rest[..start]);
        let block = &rest[start + BLOCK_BEGIN_PREFIX.len()..];
        rest = block
            .find(BLOCK_END)
            .map_or("", |end| &block[end + BLOCK_END.len()..]);
    }
    out.push_str(rest);
    out
}

/// A file's text as the reference read it: UTF-8 with undecodable bytes
/// replaced, line endings normalised. The error is the reason `unreadable` gives.
fn read_text(path: &Path) -> std::result::Result<String, String> {
    let bytes = fs::read(path).map_err(|e| strerror(&e))?;
    Ok(normalise_newlines(
        String::from_utf8_lossy(&bytes).into_owned(),
    ))
}

/// The OS's wording for an error (`Permission denied`), without the
/// ` (os error 13)` Rust appends -- what the reference reported.
fn strerror(e: &io::Error) -> String {
    let text = e.to_string();
    if let Some(code) = e.raw_os_error()
        && let Some(bare) = text.strip_suffix(&format!(" (os error {code})"))
    {
        return bare.to_string();
    }
    text
}

/// What sits at a path the scan expects, symlinks followed.
enum Found {
    Dir,
    File,
    /// Nothing there, or something that cannot hold memory: a FIFO, or a file
    /// where a directory on the way should be. Not a skip.
    Nothing,
    /// Something is there that cannot be reached: a symlink that does not
    /// resolve, or a directory on the way that cannot be searched.
    Unreachable(String),
}

fn probe(path: &Path) -> Found {
    match fs::metadata(path) {
        Ok(meta) if meta.is_dir() => Found::Dir,
        Ok(meta) if meta.is_file() => Found::File,
        Ok(_) => Found::Nothing,
        Err(e) => match fs::symlink_metadata(path) {
            Ok(_) => Found::Unreachable(format!("symlink that does not resolve: {}", strerror(&e))),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                Found::Nothing
            }
            Err(e) => Found::Unreachable(format!("could not be checked: {}", strerror(&e))),
        },
    }
}

/// A directory's identity, to catch a second path to it.
#[cfg(unix)]
type DirId = (u64, u64);

#[cfg(unix)]
fn dir_id(_dir: &Path, meta: &fs::Metadata) -> DirId {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

/// Without a portable inode number, the resolved path stands in for one.
#[cfg(not(unix))]
type DirId = PathBuf;

#[cfg(not(unix))]
fn dir_id(dir: &Path, _meta: &fs::Metadata) -> DirId {
    fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf())
}

/// What walking one memory root found.
#[derive(Default)]
struct Walk {
    /// Memory-named paths, sorted; not yet checked to be readable files.
    files: Vec<PathBuf>,
    /// Feeds `unreadable`.
    issues: Vec<Skipped>,
    /// Feeds `excluded_dirs`.
    excluded: Vec<Excluded>,
}

impl Walk {
    fn issue(path: &Path, reason: String) -> Walk {
        Walk {
            issues: vec![Skipped {
                path: path.to_path_buf(),
                reason,
            }],
            ..Walk::default()
        }
    }

    fn append(&mut self, other: Walk) {
        self.files.extend(other.files);
        self.issues.extend(other.issues);
        self.excluded.extend(other.excluded);
    }
}

/// One memory root, if there is one.
fn walk_root(base: &Path, seen: &mut HashSet<DirId>) -> Walk {
    match probe(base) {
        Found::Dir => walk(base, seen),
        Found::Unreachable(reason) => Walk::issue(base, reason),
        Found::File | Found::Nothing => Walk::default(),
    }
}

/// Every `projects/<slug>/memory`, in slug order. A `projects/` that cannot be
/// listed is reported: it may hold every project's memory.
fn walk_projects(projects: &Path, seen: &mut HashSet<DirId>) -> Walk {
    match probe(projects) {
        Found::Dir => {}
        Found::Unreachable(reason) => return Walk::issue(projects, reason),
        Found::File | Found::Nothing => return Walk::default(),
    }
    let slugs = fs::read_dir(projects).and_then(|entries| {
        entries
            .map(|e| e.map(|e| e.file_name()))
            .collect::<io::Result<Vec<OsString>>>()
    });
    let mut slugs = match slugs {
        Ok(slugs) => slugs,
        Err(e) => {
            return Walk::issue(
                projects,
                format!("directory could not be listed: {}", strerror(&e)),
            );
        }
    };
    slugs.sort();
    let mut all = Walk::default();
    for slug in slugs {
        all.append(walk_root(&projects.join(slug).join("memory"), seen));
    }
    all
}

/// A directory's entries, split into directories (symlinks followed) and the
/// rest. An error on the directory or on any one entry fails the whole listing,
/// so a half-read directory is reported rather than passed off as complete.
fn list(dir: &Path) -> io::Result<(Vec<OsString>, Vec<OsString>)> {
    let (mut dirs, mut rest) = (Vec::new(), Vec::new());
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let is_dir = match entry.file_type() {
            Ok(t) if t.is_symlink() => fs::metadata(entry.path()).is_ok_and(|m| m.is_dir()),
            Ok(t) => t.is_dir(),
            Err(_) => false,
        };
        if is_dir {
            dirs.push(entry.file_name());
        } else {
            rest.push(entry.file_name());
        }
    }
    Ok((dirs, rest))
}

/// Walk one memory root depth-first in name order, following symlinked
/// directories. `seen` is shared by every root of one scan, so a directory
/// reachable from two roots is reported as a repeat instead of having its files
/// counted twice under two scopes.
fn walk(base: &Path, seen: &mut HashSet<DirId>) -> Walk {
    let mut w = Walk::default();
    let base_real = fs::canonicalize(base).unwrap_or_else(|_| base.to_path_buf());
    let mut stack = vec![base.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let (mut dirs, rest) = match list(&dir) {
            Ok(listing) => listing,
            Err(e) => {
                w.issues.push(Skipped {
                    reason: format!("directory could not be listed: {}", strerror(&e)),
                    path: dir,
                });
                continue;
            }
        };
        let meta = match fs::metadata(&dir) {
            Ok(meta) => meta,
            Err(e) => {
                w.issues.push(Skipped {
                    reason: format!("directory could not be stat'ed: {}", strerror(&e)),
                    path: dir,
                });
                continue;
            }
        };
        if let Ok(real) = fs::canonicalize(&dir)
            && base_real.ancestors().skip(1).any(|a| a == real)
        {
            w.excluded.push(Excluded {
                reason: format!(
                    "symlink to {}, an ancestor of the memory root -- not walked",
                    real.display()
                ),
                path: dir,
                n_md: None,
            });
            continue;
        }
        if !seen.insert(dir_id(&dir, &meta)) {
            w.issues.push(Skipped {
                path: dir,
                reason: REPEAT.into(),
            });
            continue;
        }
        if dir == base {
            for name in SKIP_DIRS {
                if let Some(i) = dirs.iter().position(|d| d == name) {
                    let path = dir.join(dirs.remove(i));
                    w.excluded.push(Excluded {
                        n_md: count_md(&path),
                        path,
                        reason: PROTECTED.into(),
                    });
                }
            }
        }
        dirs.sort();
        w.files.extend(
            rest.iter()
                .filter(|name| is_memory_name(name))
                .map(|name| dir.join(name)),
        );
        stack.extend(dirs.iter().rev().map(|d| dir.join(d)));
    }
    w.files.sort();
    w
}

/// How many memory-named regular files an excluded directory holds, or `None`
/// when part of it cannot be listed: unknown, not zero. Symlinked directories
/// inside it are not followed.
fn count_md(dir: &Path) -> Option<usize> {
    let mut n = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d).ok()? {
            let entry = entry.ok()?;
            let path = entry.path();
            if is_memory_name(&entry.file_name()) && fs::metadata(&path).is_ok_and(|m| m.is_file())
            {
                n += 1;
            }
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(path);
            }
        }
    }
    Some(n)
}

/// Scan one Claude config dir. Never fails: whatever cannot be read is reported.
pub fn scan(config_dir: &Path) -> Scan {
    let mut s = Scan {
        config_dir: config_dir.to_path_buf(),
        files: Vec::new(),
        sources: Vec::new(),
        unreadable: Vec::new(),
        excluded_dirs: Vec::new(),
    };

    let claude_md = config_dir.join("CLAUDE.md");
    let claude = match probe(&claude_md) {
        Found::File => read_text(&claude_md),
        Found::Unreachable(reason) => Err(reason),
        Found::Dir | Found::Nothing => Ok(String::new()),
    };
    match claude.map(|text| char_count(&outside_managed_block(&text))) {
        Ok(0) => {}
        Ok(chars) => {
            s.files.push(Entry {
                path: claude_md,
                kind: Kind::Claude,
                chars,
                scope: Scope::Global,
                project_dir: None,
            });
            s.sources
                .push("CLAUDE.md (outside the managed block)".into());
        }
        Err(reason) => s.unreadable.push(Skipped {
            path: claude_md,
            reason,
        }),
    }

    let mut seen = HashSet::new();
    let global_dir = config_dir.join("memory");
    let global = walk_root(&global_dir, &mut seen);
    let projects = config_dir.join("projects");
    let project = walk_projects(&projects, &mut seen);
    s.unreadable.extend(global.issues);
    s.unreadable.extend(project.issues);
    s.excluded_dirs.extend(global.excluded);
    s.excluded_dirs.extend(project.excluded);

    let groups = [
        (
            "global memory file",
            &global_dir,
            global.files,
            Scope::Global,
        ),
        (
            "project memory file",
            &projects,
            project.files,
            Scope::Project,
        ),
    ];
    for (label, base, files, scope) in groups {
        let mut n = 0;
        for path in files {
            if !fs::metadata(&path).is_ok_and(|m| m.is_file()) {
                s.unreadable.push(Skipped {
                    path,
                    reason: NOT_REGULAR.into(),
                });
                continue;
            }
            let chars = match read_text(&path) {
                Ok(text) => char_count(&text),
                Err(reason) => {
                    s.unreadable.push(Skipped { path, reason });
                    continue;
                }
            };
            let kind = classify(&path, chars);
            let project_dir = match scope {
                Scope::Global => None,
                Scope::Project => path
                    .strip_prefix(&projects)
                    .ok()
                    .and_then(|rel| rel.iter().next())
                    .map(|slug| slug.to_string_lossy().into_owned()),
            };
            if kind == Kind::Memory {
                n += 1;
            }
            s.files.push(Entry {
                path,
                kind,
                chars,
                scope,
                project_dir,
            });
        }
        // Memory files only, so this line cannot disagree with `n_memory_files`.
        if n > 0 {
            let plural = if n == 1 { "" } else { "s" };
            s.sources
                .push(format!("{n} {label}{plural} under {}", base.display()));
        }
    }
    s
}

fn path_json(path: &Path) -> Json {
    path.to_string_lossy().into_owned().into()
}

impl Entry {
    fn json(&self) -> Json {
        Json::object([
            ("path", path_json(&self.path)),
            ("kind", self.kind.as_str().into()),
            ("chars", self.chars.into()),
            // Against the effective budget, not the cap: what the writer caps
            // is the file PLUS the keywords it has yet to be given.
            ("over_max_text", (self.chars > EFFECTIVE_MAX_TEXT).into()),
            ("scope", self.scope.as_str().into()),
            ("project_dir", self.project_dir.clone().into()),
        ])
    }
}

impl Scan {
    fn memories(&self) -> impl Iterator<Item = &Entry> {
        self.files.iter().filter(|f| f.kind == Kind::Memory)
    }

    fn count(&self, kind: Kind) -> usize {
        self.files.iter().filter(|f| f.kind == kind).count()
    }

    /// The gate: nothing to import AND nothing skipped. A config dir holding one
    /// mode-000 memory file has no memory files to count, and reading only the
    /// count would tell the user there is nothing to migrate. `empty` is
    /// deliberately not part of it: an empty file has nothing to import at all.
    pub fn nothing_to_do(&self) -> bool {
        self.memories().next().is_none()
            && self.unreadable.is_empty()
            && self.excluded_dirs.is_empty()
    }

    /// `--quiet`'s one word, from the same gate as the report.
    pub fn verdict(&self) -> &'static str {
        if self.nothing_to_do() {
            "nothing"
        } else {
            "found"
        }
    }

    /// The reference's report, key for key and in its order.
    pub fn report(&self) -> Json {
        let memories: Vec<&Entry> = self.memories().collect();
        let in_scope = |scope| memories.iter().filter(|f| f.scope == scope).count();
        let projects: BTreeSet<&str> = memories
            .iter()
            .filter_map(|f| f.project_dir.as_deref())
            .collect();
        let over: Vec<Json> = memories
            .iter()
            .filter_map(|f| {
                over_budget(f.chars).map(|reason| {
                    Json::object([
                        ("path", path_json(&f.path)),
                        ("chars", f.chars.into()),
                        ("reason", reason.into()),
                    ])
                })
            })
            .collect();
        let empty: Vec<Json> = self
            .files
            .iter()
            .filter(|f| f.kind == Kind::Empty)
            .map(|f| path_json(&f.path))
            .collect();
        let unreadable: Vec<Json> = self
            .unreadable
            .iter()
            .map(|u| {
                Json::object([
                    ("path", path_json(&u.path)),
                    ("reason", u.reason.as_str().into()),
                ])
            })
            .collect();
        let excluded: Vec<Json> = self
            .excluded_dirs
            .iter()
            .map(|e| {
                Json::object([
                    ("path", path_json(&e.path)),
                    ("n_md", e.n_md.into()),
                    ("reason", e.reason.as_str().into()),
                ])
            })
            .collect();
        Json::object([
            ("config_dir", path_json(&self.config_dir)),
            ("nothing_to_do", self.nothing_to_do().into()),
            ("n_memory_files", memories.len().into()),
            ("n_index_files", self.count(Kind::Index).into()),
            ("n_claude_files", self.count(Kind::Claude).into()),
            ("n_empty_files", self.count(Kind::Empty).into()),
            ("n_global", in_scope(Scope::Global).into()),
            ("n_project", in_scope(Scope::Project).into()),
            ("projects", projects.into_iter().collect::<Vec<_>>().into()),
            (
                "total_chars",
                memories.iter().map(|f| f.chars).sum::<usize>().into(),
            ),
            ("over_max_text", Json::Array(over)),
            ("empty", Json::Array(empty)),
            ("unreadable", Json::Array(unreadable)),
            ("excluded_dirs", Json::Array(excluded)),
            ("sources", self.sources.clone().into()),
            (
                "files",
                Json::Array(self.files.iter().map(Entry::json).collect()),
            ),
            ("note", note().into()),
        ])
    }
}

fn note() -> String {
    format!(
        "counts and totals of memory to import cover `kind: memory` only. An index file is a \
         pointer list and a `claude` entry is instructions -- read both for context, import \
         neither as a fact. A memory file is flagged in `over_max_text` above \
         {EFFECTIVE_MAX_TEXT} chars, not {MAX_TEXT}: remember appends --keywords into the same \
         field the {MAX_TEXT}-char CHECK counts, so {KEYWORD_RESERVE} chars are reserved for \
         them and each entry's `reason` says which limit it hit. Split or condense such a file, \
         never truncate it. Nothing is skipped silently: report `unreadable` (unreadable file \
         or directory, broken symlink, symlink loop), `excluded_dirs` (protected subdirectory \
         of a memory root) and `empty` (nothing to import -- the writer refuses an empty \
         --text) as gaps. `nothing_to_do` is true only when there is nothing to import AND \
         nothing was skipped; do not derive that verdict from `n_memory_files` alone."
    )
}

/// `find-existing-memory`: print the report, or with `quiet` only the verdict.
/// Exits 0 either way: the verdict is the output, not a failure.
pub fn run(cfg: &Config, quiet: bool) -> Result<()> {
    let scan = scan(&cfg.claude_dir);
    if quiet {
        println!("{}", scan.verdict());
    } else {
        println!("{}", json::dumps(&scan.report()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- pure classification and arithmetic --------------------------------

    #[test]
    fn the_suffix_match_is_case_insensitive_and_md_only() {
        for yes in [
            "notes.md",
            "notes.MD",
            "other.Md",
            "x.mD",
            "MEMORY.md",
            ".md",
            "größe.md",
        ] {
            assert!(is_memory_name(OsStr::new(yes)), "{yes}");
        }
        for no in [
            "notes.markdown",
            "notes.md.bak",
            "notesmd",
            "md",
            "notes.txt",
            "",
        ] {
            assert!(!is_memory_name(OsStr::new(no)), "{no}");
        }
    }

    #[test]
    fn an_index_is_any_memory_md_whatever_its_case_or_size() {
        for name in ["MEMORY.md", "memory.md", "Memory.MD", "team/MEMORY.md"] {
            assert_eq!(classify(Path::new(name), 120), Kind::Index, "{name}");
            assert_eq!(classify(Path::new(name), 0), Kind::Index, "{name}");
        }
        assert_eq!(classify(Path::new("/c/memory/fact.md"), 30), Kind::Memory);
        // The directory's name is not the file's: only the last component counts.
        assert_eq!(
            classify(Path::new("/c/MEMORY.md/fact.md"), 30),
            Kind::Memory
        );
    }

    /// The writer refuses an empty text, so an empty file is never offered as a
    /// memory of zero characters.
    #[test]
    fn an_empty_file_is_its_own_kind() {
        assert_eq!(classify(Path::new("/c/memory/empty.md"), 0), Kind::Empty);
        assert_eq!(classify(Path::new("/c/memory/one.md"), 1), Kind::Memory);
    }

    #[test]
    fn the_budget_reserves_room_for_the_keywords_line() {
        assert_eq!(EFFECTIVE_MAX_TEXT, 1800);
        assert_eq!(over_budget(0), None);
        assert_eq!(over_budget(1800), None);
        assert_eq!(
            over_budget(1900).as_deref(),
            Some(
                "1900 chars, over the 1800-char effective budget (2000 cap minus 200 chars \
                 reserved for the 'Keywords:' line remember appends into the same field). It \
                 fits the cap on its own and is refused once keywords are added, so treat it \
                 like an over-cap file: split or condense"
            )
        );
        for effective in [1801, 2000] {
            let why = over_budget(effective).unwrap();
            assert!(why.contains("effective budget"), "{effective}: {why}");
        }
        assert_eq!(
            over_budget(2100).as_deref(),
            Some(
                "2100 chars, over the 2000-char hard cap (schema CHECK): split it or condense \
                 it, never truncate"
            )
        );
        assert!(over_budget(2001).unwrap().contains("hard cap"));
    }

    /// Characters, not bytes: the schema's CHECK counts characters, and this
    /// corpus holds German.
    #[test]
    fn chars_are_counted_after_trimming_in_characters_not_bytes() {
        assert_eq!(char_count("  Größe\n"), 5);
        assert_eq!(char_count("äöü ß"), 5);
        assert_eq!(char_count(" \t\r\n "), 0);
        assert_eq!(char_count(""), 0);
        assert_eq!(char_count("\u{feff}x"), 2, "a BOM is a character");
    }

    /// The reference read files in text mode, which turns `\r\n` and a lone `\r`
    /// into `\n` -- one character per line ending, not two.
    #[test]
    fn line_endings_are_read_the_way_python_reads_them() {
        assert_eq!(
            normalise_newlines("a\r\nb\rc\nd\r\n".into()),
            "a\nb\nc\nd\n"
        );
        assert_eq!(
            char_count(&normalise_newlines(
                "line one\r\nline two\r\nline three\rend\r\n".into()
            )),
            32
        );
    }

    const LEGACY: &str = "<!-- BEGIN agentic-memory (managed by install-claude-md.sh) -->";
    const INIT: &str = "<!-- BEGIN agentic-memory (managed by supercharged-memory) -->";

    #[test]
    fn the_managed_block_is_not_user_content() {
        for begin in [LEGACY, INIT] {
            let block = format!("{begin}\n# Long-term memory\nmanaged\n{BLOCK_END}");
            assert_eq!(outside_managed_block(&block).trim(), "", "{begin}");
            assert_eq!(
                outside_managed_block(&format!("# Mine\n\n{block}\n\nTail.\n")),
                "# Mine\n\n\n\nTail.\n",
                "{begin}"
            );
        }
        assert_eq!(
            outside_managed_block("Always use pnpm.\n"),
            "Always use pnpm.\n"
        );
        // An unterminated block runs to the end of the file.
        assert_eq!(
            outside_managed_block(&format!("Mine.\n{LEGACY}\nmanaged, no end")),
            "Mine.\n"
        );
        // Every block goes, not just the first.
        assert_eq!(
            outside_managed_block(&format!("a{LEGACY}x{BLOCK_END}b{INIT}y{BLOCK_END}c")),
            "abc"
        );
        // An END with no BEGIN before it is user text.
        assert_eq!(
            outside_managed_block(&format!("a{BLOCK_END}b")),
            format!("a{BLOCK_END}b")
        );
    }

    // ---- the walk, over real trees ------------------------------------------

    #[cfg(unix)]
    mod tree {
        use super::*;
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        /// A config dir at `<tmp>/<name>/claude`, with room beside it for
        /// directories outside the tree. Removed on drop, mode-000 entries included.
        pub struct Tree {
            pub top: PathBuf,
            pub root: PathBuf,
        }

        impl Tree {
            pub fn new(name: &str) -> Tree {
                let top =
                    std::env::temp_dir().join(format!("sm-existing-{name}-{}", std::process::id()));
                remove(&top);
                let root = top.join("claude");
                fs::create_dir_all(&root).unwrap();
                // Canonical, so a resolved symlink target compares equal to it.
                let root = fs::canonicalize(&root).unwrap();
                let top = root.parent().unwrap().to_path_buf();
                Tree { top, root }
            }

            pub fn path(&self, rel: &str) -> PathBuf {
                self.root.join(rel)
            }

            pub fn file(&self, rel: &str, text: &str) -> PathBuf {
                let p = self.path(rel);
                fs::create_dir_all(p.parent().unwrap()).unwrap();
                fs::write(&p, text).unwrap();
                p
            }

            pub fn link(&self, rel: &str, target: &str) -> PathBuf {
                let p = self.path(rel);
                fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::os::unix::fs::symlink(target, &p).unwrap();
                p
            }

            /// Mode 000. False when the process can read it anyway (root), in
            /// which case the caller cannot test what it meant to.
            pub fn lock(&self, rel: &str) -> bool {
                let p = self.path(rel);
                fs::set_permissions(&p, fs::Permissions::from_mode(0o000)).unwrap();
                let denied = if p.is_dir() {
                    fs::read_dir(&p).is_err()
                } else {
                    fs::read(&p).is_err()
                };
                if !denied {
                    eprintln!("mode 000 does not deny access here (root?): not exercised");
                }
                denied
            }

            pub fn scan(&self) -> Scan {
                scan(&self.root)
            }

            /// A path in a report, relative to the config dir.
            pub fn rel(&self, p: &Path) -> String {
                p.strip_prefix(&self.root)
                    .unwrap_or(p)
                    .to_string_lossy()
                    .into_owned()
            }
        }

        impl Drop for Tree {
            fn drop(&mut self) {
                remove(&self.top);
            }
        }

        fn unlock(p: &Path) {
            let Ok(meta) = fs::symlink_metadata(p) else {
                return;
            };
            if meta.file_type().is_symlink() {
                return;
            }
            let mode = if meta.is_dir() { 0o755 } else { 0o644 };
            let _ = fs::set_permissions(p, fs::Permissions::from_mode(mode));
            if meta.is_dir()
                && let Ok(entries) = fs::read_dir(p)
            {
                for e in entries.flatten() {
                    unlock(&e.path());
                }
            }
        }

        fn remove(p: &Path) {
            unlock(p);
            let _ = fs::remove_dir_all(p);
        }
    }

    #[cfg(unix)]
    use tree::Tree;

    #[cfg(unix)]
    fn kinds(t: &Tree, s: &Scan) -> Vec<(String, Kind)> {
        s.files.iter().map(|f| (t.rel(&f.path), f.kind)).collect()
    }

    #[cfg(unix)]
    fn unreadable(t: &Tree, s: &Scan) -> Vec<(String, String)> {
        s.unreadable
            .iter()
            .map(|u| (t.rel(&u.path), u.reason.clone()))
            .collect()
    }

    #[cfg(unix)]
    fn excluded(t: &Tree, s: &Scan) -> Vec<(String, Option<usize>, String)> {
        s.excluded_dirs
            .iter()
            .map(|e| (t.rel(&e.path), e.n_md, e.reason.clone()))
            .collect()
    }

    #[cfg(unix)]
    fn memories(s: &Scan) -> usize {
        s.files.iter().filter(|f| f.kind == Kind::Memory).count()
    }

    #[cfg(unix)]
    const REPEAT: &str = "symlink loop or repeated directory -- walked once, not again";
    #[cfg(unix)]
    const NOT_REGULAR: &str = "not a regular file (broken symlink, or a directory named *.md)";
    #[cfg(unix)]
    const PROTECTED: &str = "protected subdirectory of a memory root, not memory";

    #[cfg(unix)]
    #[test]
    fn a_missing_config_dir_has_nothing_to_do() {
        let t = Tree::new("missing");
        let s = scan(&t.path("nowhere"));
        assert_eq!(s.config_dir, t.path("nowhere"));
        assert!(s.files.is_empty() && s.unreadable.is_empty() && s.excluded_dirs.is_empty());
        assert!(s.nothing_to_do());
        assert_eq!(s.verdict(), "nothing");
    }

    /// The trap the reference was rebuilt around: a directory that cannot be
    /// listed must show up, or "nothing found" reads as "nothing there".
    #[cfg(unix)]
    #[test]
    fn an_unlistable_directory_is_reported_with_its_reason() {
        let t = Tree::new("unlistable");
        t.file("memory/noaccess/inside.md", "hidden\n");
        if !t.lock("memory/noaccess") {
            return;
        }
        let s = t.scan();
        assert_eq!(
            unreadable(&t, &s),
            vec![(
                "memory/noaccess".to_string(),
                "directory could not be listed: Permission denied".to_string()
            )]
        );
        assert_eq!(memories(&s), 0);
        assert!(!s.nothing_to_do(), "a skip is not nothing");
        assert_eq!(s.verdict(), "found");
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_file_is_reported_with_its_reason() {
        let t = Tree::new("unreadable-file");
        t.file("memory/locked.md", "secret\n");
        if !t.lock("memory/locked.md") {
            return;
        }
        let s = t.scan();
        assert_eq!(
            unreadable(&t, &s),
            vec![(
                "memory/locked.md".to_string(),
                "Permission denied".to_string()
            )]
        );
        assert!(s.files.is_empty());
        assert!(!s.nothing_to_do());
    }

    /// `memory/loop -> ..` once dragged the whole config dir in, CLAUDE.md and
    /// every project's memory included.
    #[cfg(unix)]
    #[test]
    fn a_symlink_to_an_ancestor_is_excluded_and_not_walked() {
        let t = Tree::new("ancestor");
        t.file("CLAUDE.md", "Always use pnpm.\n");
        t.file("memory/fact.md", "a fact\n");
        t.file("projects/p/memory/pfact.md", "a project fact\n");
        t.link("memory/loop", "..");
        t.link("memory/up", "../..");
        let s = t.scan();
        assert_eq!(
            excluded(&t, &s),
            vec![
                (
                    "memory/loop".to_string(),
                    None,
                    format!(
                        "symlink to {}, an ancestor of the memory root -- not walked",
                        t.root.display()
                    )
                ),
                (
                    "memory/up".to_string(),
                    None,
                    format!(
                        "symlink to {}, an ancestor of the memory root -- not walked",
                        t.top.display()
                    )
                ),
            ]
        );
        assert_eq!(
            kinds(&t, &s),
            vec![
                ("CLAUDE.md".to_string(), Kind::Claude),
                ("memory/fact.md".to_string(), Kind::Memory),
                ("projects/p/memory/pfact.md".to_string(), Kind::Memory),
            ]
        );
        assert!(!s.nothing_to_do(), "an exclusion is reported, not dropped");
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_reached_twice_is_walked_once() {
        let t = Tree::new("loop");
        t.file("memory/fact.md", "a fact\n");
        t.link("memory/a/back", "..");
        t.link("projects/p/memory/global", "../../../memory");
        let s = t.scan();
        assert_eq!(
            unreadable(&t, &s),
            vec![
                ("memory/a/back".to_string(), REPEAT.to_string()),
                ("projects/p/memory/global".to_string(), REPEAT.to_string()),
            ]
        );
        assert_eq!(
            kinds(&t, &s),
            vec![("memory/fact.md".to_string(), Kind::Memory)],
            "one file, under one name"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_broken_symlink_and_a_looping_file_symlink_are_unreadable() {
        let t = Tree::new("broken");
        t.link("memory/broken.md", "/nonexistent/target.md");
        t.link("memory/selfloop.md", "selfloop.md");
        // Not named like memory: a dangling link elsewhere is nobody's file.
        t.link("memory/dangling.txt", "/nonexistent");
        let s = t.scan();
        assert_eq!(
            unreadable(&t, &s),
            vec![
                ("memory/broken.md".to_string(), NOT_REGULAR.to_string()),
                ("memory/selfloop.md".to_string(), NOT_REGULAR.to_string()),
            ]
        );
        assert!(!s.nothing_to_do());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_directory_is_followed_and_reported_under_the_link() {
        let t = Tree::new("linkdir");
        fs_write(&t.top.join("elsewhere/real.md"), "outside fact\n");
        t.link("memory/linkdir", t.top.join("elsewhere").to_str().unwrap());
        let s = t.scan();
        assert_eq!(
            kinds(&t, &s),
            vec![("memory/linkdir/real.md".to_string(), Kind::Memory)]
        );
    }

    #[cfg(unix)]
    fn fs_write(p: &Path, text: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn the_suffix_is_matched_case_insensitively_in_the_walk() {
        let t = Tree::new("suffix");
        t.file("memory/notes.MD", "upper\n");
        t.file("memory/other.Md", "mixed\n");
        t.file("memory/readme.txt", "not memory\n");
        let s = t.scan();
        assert_eq!(
            kinds(&t, &s),
            vec![
                ("memory/notes.MD".to_string(), Kind::Memory),
                ("memory/other.Md".to_string(), Kind::Memory),
            ]
        );
    }

    /// `agents/` and `.git` are not memory at a memory ROOT. A topic directory
    /// deeper down that merely shares the name is memory like any other.
    #[cfg(unix)]
    #[test]
    fn protected_directories_are_excluded_at_the_root_only() {
        let t = Tree::new("protected");
        t.file("memory/agents/one.md", "subagent\n");
        t.file("memory/agents/sub/two.md", "subagent\n");
        t.file("memory/agents/notes.txt", "x\n");
        t.file("memory/.git/HEAD.md", "git\n");
        t.file("memory/topic/agents/kept.md", "a topic\n");
        t.file("projects/p/memory/agents/three.md", "subagent\n");
        let s = t.scan();
        assert_eq!(
            excluded(&t, &s),
            vec![
                ("memory/.git".to_string(), Some(1), PROTECTED.to_string()),
                ("memory/agents".to_string(), Some(2), PROTECTED.to_string()),
                (
                    "projects/p/memory/agents".to_string(),
                    Some(1),
                    PROTECTED.to_string()
                ),
            ]
        );
        assert_eq!(
            kinds(&t, &s),
            vec![("memory/topic/agents/kept.md".to_string(), Kind::Memory)]
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_uncountable_protected_directory_counts_as_unknown() {
        let t = Tree::new("uncountable");
        t.file("memory/agents/one.md", "subagent\n");
        t.file("memory/agents/sub/two.md", "subagent\n");
        if !t.lock("memory/agents/sub") {
            return;
        }
        let s = t.scan();
        assert_eq!(
            excluded(&t, &s),
            vec![("memory/agents".to_string(), None, PROTECTED.to_string())]
        );
    }

    #[cfg(unix)]
    #[test]
    fn project_memory_carries_its_slug_at_any_depth() {
        let t = Tree::new("slug");
        t.file("projects/-home-u-proj/memory/pfact.md", "project fact\n");
        t.file(
            "projects/-home-u-proj/memory/team/nested/pdeep.md",
            "nested\n",
        );
        t.file("projects/-home-u-other/session.jsonl", "{}\n");
        t.file("memory/fact.md", "global fact\n");
        let s = t.scan();
        let got: Vec<(String, Scope, Option<String>)> = s
            .files
            .iter()
            .map(|f| (t.rel(&f.path), f.scope, f.project_dir.clone()))
            .collect();
        let proj = Some("-home-u-proj".to_string());
        assert_eq!(
            got,
            vec![
                ("memory/fact.md".to_string(), Scope::Global, None),
                (
                    "projects/-home-u-proj/memory/pfact.md".to_string(),
                    Scope::Project,
                    proj.clone()
                ),
                (
                    "projects/-home-u-proj/memory/team/nested/pdeep.md".to_string(),
                    Scope::Project,
                    proj
                ),
            ]
        );
    }

    /// Every count of memory to import covers `kind: memory` only: an index is a
    /// pointer list and a CLAUDE.md body is instructions.
    #[cfg(unix)]
    #[test]
    fn counts_and_sources_cover_memory_files_only() {
        let t = Tree::new("counts");
        t.file("CLAUDE.md", "Always use pnpm.\n");
        t.file("memory/MEMORY.md", "- [fact](fact.md)\n");
        t.file("memory/fact.md", "a fact\n");
        t.file("memory/empty.md", "  \n\t\n");
        t.file("projects/p/memory/MEMORY.md", "- [x](x.md)\n");
        let s = t.scan();
        assert_eq!(
            kinds(&t, &s),
            vec![
                ("CLAUDE.md".to_string(), Kind::Claude),
                ("memory/MEMORY.md".to_string(), Kind::Index),
                ("memory/empty.md".to_string(), Kind::Empty),
                ("memory/fact.md".to_string(), Kind::Memory),
                ("projects/p/memory/MEMORY.md".to_string(), Kind::Index),
            ]
        );
        assert_eq!(
            s.sources,
            vec![
                "CLAUDE.md (outside the managed block)".to_string(),
                format!("1 global memory file under {}", t.path("memory").display()),
            ],
            "no project line: the project holds an index and no memory"
        );
        let r = json::dumps(&s.report());
        for want in [
            "\"n_memory_files\": 1,",
            "\"n_index_files\": 2,",
            "\"n_claude_files\": 1,",
            "\"n_empty_files\": 1,",
            "\"n_global\": 1,",
            "\"n_project\": 0,",
            "\"projects\": [],",
            "\"total_chars\": 6,",
        ] {
            assert!(r.contains(want), "{want} in\n{r}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn nothing_to_do_needs_no_memory_and_no_skip() {
        // A CLAUDE.md, an index and an empty file are worth a mention, not a
        // migration.
        let t = Tree::new("nothing");
        t.file("CLAUDE.md", "Always use pnpm.\n");
        t.file("memory/MEMORY.md", "- [x](x.md)\n");
        t.file("memory/empty.md", "\n");
        let s = t.scan();
        assert!(s.nothing_to_do());
        assert_eq!(s.verdict(), "nothing");

        t.file("memory/fact.md", "a fact\n");
        assert!(!t.scan().nothing_to_do());
        assert_eq!(t.scan().verdict(), "found");
    }

    #[cfg(unix)]
    #[test]
    fn claude_md_reports_only_the_text_outside_the_block() {
        let t = Tree::new("claude-md");
        for begin in [LEGACY, INIT] {
            t.file(
                "CLAUDE.md",
                &format!("{begin}\nmanaged rules\n{BLOCK_END}\n"),
            );
            let s = t.scan();
            assert!(s.files.is_empty(), "{begin}: {:?}", s.files);
            assert!(s.sources.is_empty(), "{begin}");

            t.file(
                "CLAUDE.md",
                &format!("# Mine\r\nAlways use pnpm.\r\n{begin}\nmanaged\n{BLOCK_END}\nTail.\n"),
            );
            let s = t.scan();
            assert_eq!(s.files.len(), 1, "{begin}");
            let f = &s.files[0];
            assert_eq!(
                (f.kind, f.scope, f.project_dir.clone()),
                (Kind::Claude, Scope::Global, None)
            );
            assert_eq!(
                f.chars,
                "# Mine\nAlways use pnpm.\n\nTail.".chars().count(),
                "{begin}"
            );
        }
    }

    /// Flagged against the effective budget per file, but only memory files are
    /// listed as files to split.
    #[cfg(unix)]
    #[test]
    fn over_max_text_lists_memory_files_only() {
        let t = Tree::new("over");
        t.file("CLAUDE.md", &"c".repeat(2500));
        t.file("memory/budget.md", &"x".repeat(1801));
        t.file("memory/fits.md", &"x".repeat(1800));
        t.file("memory/cap.md", &"y".repeat(2001));
        let s = t.scan();
        let r = json::dumps(&s.report());
        let listed = r.split("\"over_max_text\": [").nth(1).unwrap();
        let listed = listed.split("\n ],").next().unwrap();
        assert!(listed.contains("memory/budget.md"), "{listed}");
        assert!(listed.contains("memory/cap.md"), "{listed}");
        assert!(!listed.contains("fits.md"), "{listed}");
        assert!(!listed.contains("CLAUDE.md"), "{listed}");
        // The per-file flag still says it about the CLAUDE.md entry.
        assert!(
            r.contains(&format!(
                "\"path\": \"{}\",\n   \"kind\": \"claude\",\n   \"chars\": 2500,\n   \"over_max_text\": true,",
                t.path("CLAUDE.md").display()
            )),
            "{r}"
        );
    }

    // ---- skips the reference dropped without a trace --------------------------

    #[cfg(unix)]
    #[test]
    fn an_unlistable_projects_directory_is_reported() {
        let t = Tree::new("projects-locked");
        t.file("projects/p/memory/pfact.md", "a project fact\n");
        if !t.lock("projects") {
            return;
        }
        let s = t.scan();
        assert_eq!(
            unreadable(&t, &s),
            vec![(
                "projects".to_string(),
                "directory could not be listed: Permission denied".to_string()
            )]
        );
        assert!(!s.nothing_to_do());
    }

    #[cfg(unix)]
    #[test]
    fn an_unsearchable_project_directory_is_reported() {
        let t = Tree::new("project-locked");
        t.file("projects/p/memory/pfact.md", "a project fact\n");
        t.file("projects/q/memory/qfact.md", "another\n");
        if !t.lock("projects/p") {
            return;
        }
        let s = t.scan();
        assert_eq!(
            unreadable(&t, &s),
            vec![(
                "projects/p/memory".to_string(),
                "could not be checked: Permission denied".to_string()
            )]
        );
        assert_eq!(
            kinds(&t, &s),
            vec![("projects/q/memory/qfact.md".to_string(), Kind::Memory)]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_memory_directory_symlink_is_reported() {
        let t = Tree::new("dangling-root");
        t.link("memory", "/nonexistent/memory");
        t.link("projects/p/memory", "/nonexistent/pmemory");
        let s = t.scan();
        let why = "symlink that does not resolve: No such file or directory".to_string();
        assert_eq!(
            unreadable(&t, &s),
            vec![
                ("memory".to_string(), why.clone()),
                ("projects/p/memory".to_string(), why)
            ]
        );
        assert!(!s.nothing_to_do());
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_claude_md_symlink_is_reported() {
        let t = Tree::new("dangling-claude");
        t.link("CLAUDE.md", "/nonexistent/CLAUDE.md");
        let s = t.scan();
        assert_eq!(
            unreadable(&t, &s),
            vec![(
                "CLAUDE.md".to_string(),
                "symlink that does not resolve: No such file or directory".to_string()
            )]
        );
    }

    #[cfg(unix)]
    #[test]
    fn plain_files_where_directories_belong_are_not_skips() {
        // Nothing can hide under a file: these are absences, not gaps.
        let t = Tree::new("files-not-dirs");
        t.file("memory", "a file named memory\n");
        t.file("projects/p", "a file where a project dir would be\n");
        let s = t.scan();
        assert!(s.unreadable.is_empty(), "{:?}", s.unreadable);
        assert!(s.nothing_to_do());
    }

    // ---- the report ------------------------------------------------------------

    #[cfg(unix)]
    #[test]
    fn the_report_keeps_the_reference_key_order() {
        let t = Tree::new("order");
        t.file("memory/fact.md", "a fact\n");
        t.link("memory/broken.md", "/nonexistent");
        t.file("memory/agents/a.md", "x\n");
        let r = json::dumps(&t.scan().report());
        let keys: Vec<&str> = r
            .lines()
            .filter(|l| l.starts_with(" \"") && !l.starts_with("  "))
            .map(|l| l.trim_start().split('"').nth(1).unwrap())
            .collect();
        assert_eq!(
            keys,
            vec![
                "config_dir",
                "nothing_to_do",
                "n_memory_files",
                "n_index_files",
                "n_claude_files",
                "n_empty_files",
                "n_global",
                "n_project",
                "projects",
                "total_chars",
                "over_max_text",
                "empty",
                "unreadable",
                "excluded_dirs",
                "sources",
                "files",
                "note",
            ]
        );
        for entry in [
            "{\n   \"path\": ",
            "\",\n   \"kind\": \"memory\",\n   \"chars\": 6,\n   \"over_max_text\": false,\n   \"scope\": \"global\",\n   \"project_dir\": null\n  }",
            "\",\n   \"reason\": \"not a regular file",
            "\",\n   \"n_md\": 1,\n   \"reason\": \"protected",
        ] {
            assert!(r.contains(entry), "{entry:?} in\n{r}");
        }
        assert!(r.starts_with("{\n \"config_dir\": "), "{r}");
        assert!(r.ends_with("\n}"), "{r}");
    }
}
