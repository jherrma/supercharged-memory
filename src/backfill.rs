//! `backfill`: import a directory of files, one memory per file.
//!
//! The write path is `remember`'s, not a copy of it: the embedding (and the one
//! model per database rule that goes with it), the near-duplicate guards and the
//! wording of a row stored without a vector all come from `remember`. What is
//! specific to a bulk import is deciding, file by file, what is the file's own
//! problem -- too long, empty, a duplicate -- and gets reported and skipped, and
//! what is the run's -- a different embedding model in the table, an unusable
//! category -- and refuses before a single row is written.
//!
//! Nothing is dropped silently: every file the glob matched ends as a `stored`
//! line or a `SKIP` line with its reason, and the closing counts add up.

use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use crate::config::{Config, MAX_TEXT};
use crate::embed::{Embedder, Ollama};
use crate::error::{Error, Result};
use crate::remember::{self, DUP_DIST, Duplicate, validate};
use crate::render;
use crate::store::{self, NewMemory, Store, Table};

/// Where a file is too large to be worth reading at all: 2000 characters never
/// take this many bytes, whatever the encoding.
const READ_LIMIT: u64 = 1 << 20;

/// What the rows are stored as.
const SOURCE: &str = "backfill";

#[derive(Debug, Clone)]
pub struct Args {
    pub dir: PathBuf,
    pub glob: String,
    pub table: Table,
    pub category: String,
    pub project: Option<String>,
    pub model: String,
}

/// What one run did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Summary {
    pub stored: usize,
    pub skipped: usize,
    /// Of the stored rows, how many have no embedding.
    pub without_embedding: usize,
}

pub fn run(cfg: &Config, args: &Args) -> Result<()> {
    let embedder = Ollama::new(cfg);
    import(
        cfg,
        args,
        &embedder,
        &mut io::stdout().lock(),
        &mut io::stderr().lock(),
    )
    .map(|_| ())
}

/// A line to the output; a closed pipe is a failure, not something to ignore.
fn say(out: &mut dyn Write, line: impl AsRef<str>) -> Result<()> {
    writeln!(out, "{}", line.as_ref())
        .map_err(|e| Error::failed(format!("cannot write output: {e}")))
}

/// Import every file under `args.dir` the glob matches.
///
/// Progress goes to `out` as it happens, so a run that dies midway has still
/// said what it stored; the one warning a degraded run owes goes to `err`, once.
pub fn import(
    cfg: &Config,
    args: &Args,
    embedder: &dyn Embedder,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<Summary> {
    check(args)?;
    let dir = expand(&args.dir, &cfg.home);
    if !dir.is_dir() {
        return Err(Error::refused(format!(
            "not a directory: {}",
            dir.display()
        )));
    }
    let items = discover(&dir, &args.glob);
    if !items.iter().any(|i| matches!(i, Item::File(_))) {
        let mut msg = format!("no files matching {} under {}", args.glob, dir.display());
        for item in &items {
            if let Item::Problem(path, why) = item {
                msg.push_str(&format!("\n  {}: {why}", shown(&dir, path)));
            }
        }
        return Err(Error::refused(msg));
    }
    let store = store::open(cfg)?;

    let mut summary = Summary::default();
    let mut why_without: Option<String> = None;
    let mut warned = false;
    for item in &items {
        let (path, outcome) = match item {
            Item::Problem(path, why) => (path, Outcome::Skip(why.clone())),
            Item::File(path) => (path, one_file(cfg, &store, embedder, args, path)?),
        };
        let name = shown(&dir, path);
        match outcome {
            Outcome::Skip(why) => {
                say(out, format!("SKIP {name}: {why}"))?;
                summary.skipped += 1;
            }
            Outcome::Stored { id, missing } => {
                let line = remember::stored(cfg, id, format!("stored {name}"), missing.clone());
                say(out, &line.line)?;
                if let Some(why) = missing {
                    summary.without_embedding += 1;
                    why_without.get_or_insert(why);
                }
                if let Some(warning) = line.warning
                    && !warned
                {
                    warned = true;
                    writeln!(err, "{warning}")
                        .map_err(|e| Error::failed(format!("cannot write output: {e}")))?;
                }
                summary.stored += 1;
            }
        }
    }
    say(
        out,
        format!(
            "backfilled {} file(s); skipped {}",
            summary.stored, summary.skipped
        ),
    )?;
    if let Some(why) = why_without {
        say(
            out,
            format!(
                "{} stored WITHOUT embedding — {why}; run embed --missing once Ollama is available",
                summary.without_embedding
            ),
        )?;
    }
    Ok(summary)
}

/// What the run's own arguments can get wrong, before anything is read or written.
fn check(args: &Args) -> Result<()> {
    if args.table == Table::Instructions {
        return Err(Error::refused(
            "an instruction is not a memory: store it with `instruct`.",
        ));
    }
    if args.table == Table::Semantic {
        let category = validate::one_of("--category", &args.category, &validate::CATEGORIES)?;
        if category == "baseline" {
            return Err(Error::refused(
                "'baseline' memories load every session and need the user's explicit \
                 confirmation: store one with `remember --confirm-baseline`, not in bulk.",
            ));
        }
    }
    validate::meta_fits(
        "--project",
        &validate::blank_to_none(args.project.as_deref()),
    )?;
    validate::meta_fits("--model", &validate::blank_to_none(Some(&args.model)))?;
    if args.glob.is_empty() || args.glob.contains(['/', '\\', '[', ']', '{', '}']) {
        return Err(Error::refused(format!(
            "--glob '{}' is not a file-name pattern: only * and ? are understood, matched \
             against the file's name at any depth.",
            args.glob
        )));
    }
    Ok(())
}

/// A leading `~` is the home directory, as the shell would have it.
fn expand(dir: &Path, home: &Path) -> PathBuf {
    match dir.to_str() {
        Some("~") => home.to_path_buf(),
        Some(s) if s.starts_with("~/") || s.starts_with("~\\") => home.join(&s[2..]),
        _ => dir.to_path_buf(),
    }
}

/// A path as the report names it: relative to the imported directory, with `/`
/// whatever the platform.
fn shown(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    if rel.as_os_str().is_empty() {
        return ".".to_string();
    }
    rel.components()
        .map(|c| match c {
            Component::Normal(part) => part.to_string_lossy().into_owned(),
            other => other.as_os_str().to_string_lossy().into_owned(),
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// A file to import, or something on the way to one that could not be read.
#[derive(Debug)]
enum Item {
    File(PathBuf),
    Problem(PathBuf, String),
}

impl Item {
    fn path(&self) -> &Path {
        match self {
            Item::File(p) | Item::Problem(p, _) => p,
        }
    }
}

/// Every matching file under `dir` plus whatever could not be listed, in path
/// order. Symlinked directories are not entered (a link to an ancestor would
/// never end); a symlinked file is read like the file it points at.
fn discover(dir: &Path, glob: &str) -> Vec<Item> {
    let mut items = Vec::new();
    walk(dir, glob, &mut items);
    items.sort_by(|a, b| a.path().cmp(b.path()));
    items
}

fn walk(dir: &Path, glob: &str, items: &mut Vec<Item>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            items.push(Item::Problem(dir.to_path_buf(), cannot_read(&e)));
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                items.push(Item::Problem(dir.to_path_buf(), cannot_read(&e)));
                continue;
            }
        };
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            items.push(Item::Problem(path, "cannot be inspected".to_string()));
            continue;
        };
        let matches = || glob_matches(glob, &entry.file_name().to_string_lossy());
        if kind.is_dir() {
            walk(&path, glob, items);
        } else if kind.is_symlink() {
            match fs::metadata(&path) {
                Ok(meta) if meta.is_file() && matches() => items.push(Item::File(path)),
                Ok(_) => {}
                Err(_) if matches() => items.push(Item::Problem(
                    path,
                    "symlink that does not resolve".to_string(),
                )),
                Err(_) => {}
            }
        } else if kind.is_file() && matches() {
            items.push(Item::File(path));
        }
    }
}

/// `cannot be read (Permission denied)`: the OS's words, without the Rust
/// suffix that names its error number.
fn cannot_read(e: &io::Error) -> String {
    let text = e.to_string();
    let text = text.split(" (os error").next().unwrap_or(&text);
    format!("cannot be read ({text})")
}

/// Whether `pattern` matches the whole of `name`, ignoring case. `*` is any run
/// of characters, `?` exactly one; nothing else is special.
fn glob_matches(pattern: &str, name: &str) -> bool {
    let fold = |s: &str| s.chars().flat_map(char::to_lowercase).collect::<Vec<_>>();
    let (p, n) = (fold(pattern), fold(name));
    let (mut pi, mut ni) = (0, 0);
    // Where the last `*` was, and how much of the name it has absorbed so far.
    let mut star: Option<(usize, usize)> = None;
    while ni < n.len() {
        if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ni));
            pi += 1;
        } else if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if let Some((sp, sn)) = star {
            pi = sp + 1;
            ni = sn + 1;
            star = Some((sp, sn + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// The words of a file name, three characters or more each, lowercased: the
/// searchable part of a topic nobody wrote keywords for.
fn keywords_of(stem: &str) -> Option<String> {
    let lower = stem.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| w.chars().count() >= 3)
        .collect();
    (!words.is_empty()).then(|| words.join(", "))
}

/// What became of one file.
enum Outcome {
    Stored { id: i64, missing: Option<String> },
    Skip(String),
}

/// A refusal that belongs to the file, in the words of the report.
fn reason(e: &Error) -> String {
    match e {
        Error::Refused(why) | Error::Failed(why) => why.clone(),
    }
}

/// Read, check and store one file. `Err` is the run's problem (another
/// embedding model in the table, a database that cannot be written); anything
/// wrong with the file itself is a `Skip`.
fn one_file(
    cfg: &Config,
    store: &impl Store,
    embedder: &dyn Embedder,
    args: &Args,
    path: &Path,
) -> Result<Outcome> {
    let skip = |why: String| Ok(Outcome::Skip(why));
    match fs::metadata(path) {
        Ok(meta) if meta.len() > READ_LIMIT => {
            return skip(format!(
                "{} bytes, far over the {MAX_TEXT} char cap — split it manually",
                meta.len()
            ));
        }
        Ok(_) => {}
        Err(e) => return skip(cannot_read(&e)),
    }
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) => return skip(cannot_read(&e)),
    };
    // A lossy decode would store, and embed, replacement characters with
    // nothing to show for it afterwards.
    let Ok(raw) = String::from_utf8(bytes) else {
        return skip("not valid UTF-8 — convert it first".to_string());
    };
    // Line endings counted once, as a text-mode read would.
    let text = raw.replace("\r\n", "\n").replace('\r', "\n");
    let text = text.trim();
    if text.is_empty() {
        return skip("empty — nothing to store".to_string());
    }
    let chars = text.chars().count();
    if chars > MAX_TEXT {
        return skip(format!(
            "{chars} chars > {MAX_TEXT} cap — split it manually"
        ));
    }

    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let topic = Some(stem.clone());
    if let Err(e) = validate::meta_fits("--topic", &topic) {
        return skip(reason(&e));
    }
    let memory_text = match validate::assemble_text(text, keywords_of(&stem).as_deref()) {
        Ok(t) => t,
        Err(e) => return skip(reason(&e)),
    };

    let (vector, missing) = remember::embedding(cfg, store, embedder, &memory_text, &[args.table])?;

    // Semantic memory only, as in `remember`: events legitimately recur.
    if args.table == Table::Semantic {
        let found = remember::duplicate(
            store,
            Table::Semantic,
            &[],
            None,
            vector.as_deref(),
            &memory_text,
        )?;
        match found {
            Some(Duplicate::Near { id, dist }) => {
                return skip(format!(
                    "near-duplicate of id {id} (cosine {} < {}) — use remember --supersedes {id} \
                     to replace it",
                    render::format_real(dist),
                    render::format_real(DUP_DIST)
                ));
            }
            Some(Duplicate::Identical { id }) => {
                return skip(format!(
                    "identical memory already exists (id {id}; same text, ignoring case, \
                     whitespace and keywords) — use remember --supersedes {id} to replace it"
                ));
            }
            None => {}
        }
    }

    let semantic = args.table == Table::Semantic;
    let id = store.insert_memory(&NewMemory {
        table: args.table,
        project: validate::blank_to_none(args.project.as_deref()),
        topic,
        category: semantic.then(|| args.category.clone()),
        event_type: (!semantic).then(|| "note".to_string()),
        importance: (!semantic).then(|| "routine".to_string()),
        source: Some(SOURCE.to_string()),
        model: validate::blank_to_none(Some(&args.model)),
        embed_model: vector.as_ref().map(|_| cfg.embed_model.clone()),
        memory_text,
        file_reference: None,
        created_at: None,
        embedding: vector,
        supersedes: Vec::new(),
        retires: None,
        coworkers: Vec::new(),
    })?;
    Ok(Outcome::Stored { id, missing })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;
    use crate::embed::Unavailable;
    use crate::embed::fake::Fake;
    use crate::error::EXIT_REFUSED;
    use crate::store::Cell;
    use crate::store::turso::tests::{put, rows, scratch, set_vector};

    const NO_EMBEDDINGS: &str = "WITHOUT embedding — Ollama unreachable at http://127.0.0.1:9; \
                                 run embed --missing once Ollama is available";

    fn down() -> Fake {
        Fake::down(Unavailable::Unreachable {
            url: "http://127.0.0.1:9".into(),
        })
    }

    /// Files under `<home>/<name>`, parents created.
    fn tree(cfg: &Config, name: &str, files: &[(&str, &str)]) -> PathBuf {
        let root = cfg.home.join(name);
        for (rel, text) in files {
            let path = root.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn args(dir: &Path) -> Args {
        Args {
            dir: dir.to_path_buf(),
            glob: "*.md".into(),
            table: Table::Semantic,
            category: "reference".into(),
            project: None,
            model: "unknown".into(),
        }
    }

    struct Ran {
        result: Result<Summary>,
        out: String,
        err: String,
    }

    impl Ran {
        fn summary(&self) -> &Summary {
            self.result
                .as_ref()
                .unwrap_or_else(|e| panic!("{e}\nstdout:\n{}\nstderr:\n{}", self.out, self.err))
        }

        fn lines(&self) -> Vec<&str> {
            self.out.lines().collect()
        }
    }

    fn go(cfg: &Config, args: &Args, embedder: &dyn Embedder) -> Ran {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let result = import(cfg, args, embedder, &mut out, &mut err);
        Ran {
            result,
            out: String::from_utf8(out).unwrap(),
            err: String::from_utf8(err).unwrap(),
        }
    }

    fn refused(ran: Ran) -> String {
        let e = ran.result.expect_err("must be refused");
        assert_eq!(e.exit_code(), EXIT_REFUSED, "{e}");
        let msg = e.to_string();
        assert!(msg.starts_with("refused: "), "{msg}");
        assert_eq!(ran.out, "", "a refusal prints no progress");
        msg
    }

    fn count(cfg: &Config, table: &str) -> i64 {
        match rows(cfg, &format!("SELECT count(*) FROM {table};"))[0][0] {
            Cell::Int(n) => n,
            ref other => panic!("{other:?}"),
        }
    }

    fn text(s: &str) -> Cell {
        Cell::Text(s.into())
    }

    // ---- the glob ----------------------------------------------------------

    #[test]
    fn a_glob_matches_the_file_name_without_regard_to_case() {
        for (pattern, name, want) in [
            ("*.md", "notes.md", true),
            ("*.md", "notes.MD", true),
            ("*.md", "Notes.Md", true),
            ("*.MD", "notes.md", true),
            ("*.md", "notes.mdx", false),
            ("*.md", "notes.md.bak", false),
            ("*.md", "md", false),
            ("*.md", ".md", true),
            ("*", "anything", true),
            ("a?c.txt", "abc.txt", true),
            ("a?c.txt", "ac.txt", false),
            ("a*c*.txt", "a-long-c-name.txt", true),
            ("a*c*.txt", "a-long-name.txt", false),
            ("ärger*", "ÄRGER.md", true),
            ("exact.md", "exact.md", true),
            ("exact.md", "exactly.md", false),
        ] {
            assert_eq!(glob_matches(pattern, name), want, "{pattern} vs {name}");
        }
    }

    #[test]
    fn the_keywords_are_the_words_of_the_file_name() {
        for (stem, want) in [
            ("Deploy-Notes_v2", Some("deploy, notes_v2")),
            ("Deploy Notes", Some("deploy, notes")),
            ("übergröße-tip", Some("übergröße, tip")),
            ("an-os x", None),
            ("ab", None),
            ("", None),
        ] {
            assert_eq!(keywords_of(stem).as_deref(), want, "{stem:?}");
        }
    }

    // ---- what a file becomes -----------------------------------------------

    #[test]
    fn every_matching_file_becomes_one_row_with_its_source_and_keywords() {
        let cfg = scratch("bf-rows");
        let dir = tree(
            &cfg,
            "notes",
            &[
                ("deploy-notes.md", "  deploys need a tag\n\n"),
                ("sub/wal flag.md", "turso wants the wal flag"),
                ("ignored.txt", "not markdown"),
            ],
        );
        let ran = go(&cfg, &args(&dir), &Fake::up());
        assert_eq!(
            ran.summary(),
            &Summary {
                stored: 2,
                skipped: 0,
                without_embedding: 0
            }
        );
        assert_eq!(
            ran.lines(),
            [
                "stored deploy-notes.md",
                "stored sub/wal flag.md",
                "backfilled 2 file(s); skipped 0"
            ]
        );
        assert_eq!(ran.err, "");
        assert_eq!(
            rows(
                &cfg,
                "SELECT topic, category, source, model, project, file_reference, memory_text, \
                 embed_model, embedding IS NULL FROM semantic_memory ORDER BY id;"
            ),
            vec![
                vec![
                    text("deploy-notes"),
                    text("reference"),
                    text("backfill"),
                    text("unknown"),
                    Cell::Null,
                    Cell::Null,
                    text("deploys need a tag\n\nKeywords: deploy, notes"),
                    text("bge-m3"),
                    Cell::Int(0),
                ],
                vec![
                    text("wal flag"),
                    text("reference"),
                    text("backfill"),
                    text("unknown"),
                    Cell::Null,
                    Cell::Null,
                    text("turso wants the wal flag\n\nKeywords: wal, flag"),
                    text("bge-m3"),
                    Cell::Int(0),
                ],
            ]
        );
    }

    #[test]
    fn the_flags_decide_category_project_model_and_table() {
        let cfg = scratch("bf-flags");
        let dir = tree(&cfg, "notes", &[("a note.md", "one fact")]);
        let mut a = args(&dir);
        a.category = "project".into();
        a.project = Some("869abc".into());
        a.model = "claude-test".into();
        go(&cfg, &a, &Fake::up()).summary();
        assert_eq!(
            rows(
                &cfg,
                "SELECT category, project, model FROM semantic_memory;"
            ),
            vec![vec![text("project"), text("869abc"), text("claude-test")]]
        );

        a.table = Table::Episodic;
        let dir = tree(&cfg, "events", &[("an event.md", "it happened")]);
        a.dir = dir;
        go(&cfg, &a, &Fake::up()).summary();
        assert_eq!(
            rows(
                &cfg,
                "SELECT event_type, importance, source, topic, project FROM episodic_memory;"
            ),
            vec![vec![
                text("note"),
                text("routine"),
                text("backfill"),
                text("an event"),
                text("869abc")
            ]]
        );
    }

    #[test]
    fn a_suffix_in_another_case_is_found_and_a_custom_glob_is_honoured() {
        let cfg = scratch("bf-case");
        let dir = tree(
            &cfg,
            "notes",
            &[
                ("upper.MD", "shouted"),
                ("mixed.Md", "mixed"),
                ("plain.md", "plain"),
                ("other.txt", "text file"),
            ],
        );
        let ran = go(&cfg, &args(&dir), &Fake::up());
        assert_eq!(ran.summary().stored, 3, "{}", ran.out);
        assert!(ran.out.contains("stored upper.MD\n"), "{}", ran.out);
        assert_eq!(
            rows(&cfg, "SELECT topic FROM semantic_memory ORDER BY topic;")
                .iter()
                .map(|r| r[0].clone())
                .collect::<Vec<_>>(),
            vec![text("mixed"), text("plain"), text("upper")]
        );

        let mut a = args(&dir);
        a.glob = "*.TXT".into();
        assert_eq!(go(&cfg, &a, &Fake::up()).summary().stored, 1);
    }

    #[test]
    fn a_leading_tilde_is_the_home_directory() {
        let cfg = scratch("bf-tilde");
        tree(&cfg, "notes", &[("a.md", "one fact")]);
        let a = args(Path::new("~/notes"));
        assert_eq!(go(&cfg, &a, &Fake::up()).summary().stored, 1);
    }

    #[test]
    fn text_is_stored_trimmed_and_character_exact() {
        let cfg = scratch("bf-unicode");
        let dir = tree(
            &cfg,
            "notes",
            &[("größe.md", "\n  Übergrößen\r\n— „ja“\r\n")],
        );
        go(&cfg, &args(&dir), &Fake::up()).summary();
        assert_eq!(
            rows(&cfg, "SELECT topic, memory_text FROM semantic_memory;"),
            vec![vec![
                text("größe"),
                text("Übergrößen\n— „ja“\n\nKeywords: größe")
            ]]
        );
    }

    // ---- what is reported and skipped --------------------------------------

    #[test]
    fn a_file_over_the_cap_is_reported_and_skipped_and_the_run_goes_on() {
        let cfg = scratch("bf-cap");
        let big = "x".repeat(2001);
        let dir = tree(
            &cfg,
            "notes",
            &[
                ("a-big.md", &big),
                ("b-small.md", "fits"),
                ("c-big.md", &big),
            ],
        );
        let ran = go(&cfg, &args(&dir), &Fake::up());
        assert_eq!(
            ran.lines(),
            [
                "SKIP a-big.md: 2001 chars > 2000 cap — split it manually",
                "stored b-small.md",
                "SKIP c-big.md: 2001 chars > 2000 cap — split it manually",
                "backfilled 1 file(s); skipped 2"
            ]
        );
        assert_eq!(count(&cfg, "semantic_memory"), 1);
    }

    #[test]
    fn a_file_that_fits_only_without_its_keywords_is_skipped_and_says_why() {
        let cfg = scratch("bf-cap-keywords");
        let dir = tree(
            &cfg,
            "notes",
            &[("deploy.md", &"x".repeat(1995)), ("ok.md", "fits")],
        );
        let ran = go(&cfg, &args(&dir), &Fake::up());
        assert_eq!(ran.summary().skipped, 1);
        assert_eq!(ran.summary().stored, 1);
        let line = ran.lines()[0];
        assert!(line.starts_with("SKIP deploy.md: memory is "), "{line}");
        assert!(line.contains("hard cap 2000"), "{line}");
        assert!(line.contains("keywords"), "{line}");
        assert_eq!(count(&cfg, "semantic_memory"), 1);
    }

    #[test]
    fn an_empty_or_whitespace_file_is_reported_not_dropped() {
        let cfg = scratch("bf-empty");
        let dir = tree(
            &cfg,
            "notes",
            &[
                ("a-empty.md", ""),
                ("b-blank.md", " \n\t\n"),
                ("c.md", "text"),
            ],
        );
        let ran = go(&cfg, &args(&dir), &Fake::up());
        assert_eq!(
            ran.lines(),
            [
                "SKIP a-empty.md: empty — nothing to store",
                "SKIP b-blank.md: empty — nothing to store",
                "stored c.md",
                "backfilled 1 file(s); skipped 2"
            ]
        );
    }

    #[test]
    fn a_file_that_is_not_utf8_is_reported_not_stored_as_mojibake() {
        let cfg = scratch("bf-utf8");
        let dir = tree(&cfg, "notes", &[("ok.md", "fine")]);
        fs::write(dir.join("a-latin1.md"), b"caf\xe9 au lait").unwrap();
        let ran = go(&cfg, &args(&dir), &Fake::up());
        assert_eq!(
            ran.lines()[0],
            "SKIP a-latin1.md: not valid UTF-8 — convert it first"
        );
        assert_eq!(ran.summary().stored, 1);
        assert_eq!(count(&cfg, "semantic_memory"), 1);
    }

    #[test]
    fn a_topic_over_the_metadata_cap_skips_that_file_only() {
        let cfg = scratch("bf-topic");
        let long = format!("{}.md", "n".repeat(129));
        let dir = tree(&cfg, "notes", &[(&long, "text"), ("ok.md", "fits")]);
        let ran = go(&cfg, &args(&dir), &Fake::up());
        assert_eq!(ran.summary().skipped, 1);
        assert_eq!(ran.summary().stored, 1);
        let line = ran.lines()[0];
        assert!(line.starts_with("SKIP "), "{line}");
        assert!(
            line.contains("--topic is 129 chars; hard cap 128"),
            "{line}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_file_or_directory_is_reported_and_the_rest_imported() {
        use std::os::unix::fs::PermissionsExt;
        let cfg = scratch("bf-unreadable");
        let dir = tree(
            &cfg,
            "notes",
            &[
                ("locked/inside.md", "hidden"),
                ("secret.md", "secret"),
                ("ok.md", "fits"),
            ],
        );
        fs::set_permissions(dir.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
        fs::set_permissions(dir.join("secret.md"), fs::Permissions::from_mode(0o000)).unwrap();
        // Root reads anything; then there is nothing to show.
        if fs::read_dir(dir.join("locked")).is_ok() {
            return;
        }
        let ran = go(&cfg, &args(&dir), &Fake::up());
        fs::set_permissions(dir.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(ran.summary().stored, 1, "{}", ran.out);
        assert_eq!(ran.summary().skipped, 2, "{}", ran.out);
        assert!(
            ran.out
                .contains("SKIP locked: cannot be read (Permission denied)"),
            "{}",
            ran.out
        );
        assert!(
            ran.out
                .contains("SKIP secret.md: cannot be read (Permission denied)"),
            "{}",
            ran.out
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_directory_is_not_followed_and_a_symlinked_file_is() {
        use std::os::unix::fs::symlink;
        let cfg = scratch("bf-symlinks");
        let dir = tree(&cfg, "notes", &[("real.md", "the real one")]);
        symlink(&dir, dir.join("loop")).unwrap();
        symlink(dir.join("real.md"), dir.join("alias.md")).unwrap();
        symlink(dir.join("gone.md"), dir.join("dangling.md")).unwrap();
        let ran = go(&cfg, &args(&dir), &Fake::up());
        // The alias holds the same text as the real file: it is stored first
        // (sorted order), the real file is then its duplicate.
        assert_eq!(ran.summary().stored, 1, "{}", ran.out);
        assert!(
            ran.out
                .contains("SKIP dangling.md: symlink that does not resolve"),
            "{}",
            ran.out
        );
        assert!(!ran.out.contains("loop"), "{}", ran.out);
    }

    // ---- the guards it shares with remember --------------------------------

    #[test]
    fn running_it_twice_meets_the_near_duplicate_guard_instead_of_doubling_the_corpus() {
        let cfg = scratch("bf-rerun");
        let dir = tree(
            &cfg,
            "notes",
            &[("a.md", "first fact"), ("b.md", "second fact")],
        );
        go(&cfg, &args(&dir), &Fake::up()).summary();
        assert_eq!(count(&cfg, "semantic_memory"), 2);

        let ran = go(&cfg, &args(&dir), &Fake::up());
        assert_eq!(ran.summary().stored, 0);
        assert_eq!(ran.summary().skipped, 2);
        assert_eq!(count(&cfg, "semantic_memory"), 2);
        let lines = ran.lines();
        assert!(
            lines[0].starts_with("SKIP a.md: near-duplicate of id 1 (cosine "),
            "{}",
            lines[0]
        );
        assert!(lines[0].contains("remember --supersedes 1"), "{}", lines[0]);
        assert!(
            lines[1].starts_with("SKIP b.md: near-duplicate of id 2 "),
            "{}",
            lines[1]
        );
        assert_eq!(lines[2], "backfilled 0 file(s); skipped 2");
    }

    #[test]
    fn two_files_with_the_same_text_in_one_run_store_one() {
        let cfg = scratch("bf-twins");
        let dir = tree(
            &cfg,
            "notes",
            &[
                ("a-one.md", "The same fact."),
                ("b-two.md", "the   same\nfact."),
            ],
        );
        let ran = go(&cfg, &args(&dir), &Fake::up());
        assert_eq!(ran.summary().stored, 1);
        assert_eq!(ran.summary().skipped, 1);
        assert!(
            ran.out.contains(
                "SKIP b-two.md: identical memory already exists (id 1; same text, ignoring \
                 case, whitespace and keywords) — use remember --supersedes 1 to replace it"
            ),
            "{}",
            ran.out
        );
    }

    #[test]
    fn episodic_rows_have_no_duplicate_guard_as_with_remember() {
        let cfg = scratch("bf-episodic-twice");
        let dir = tree(&cfg, "notes", &[("an event.md", "it happened")]);
        let mut a = args(&dir);
        a.table = Table::Episodic;
        go(&cfg, &a, &Fake::up()).summary();
        go(&cfg, &a, &Fake::up()).summary();
        assert_eq!(count(&cfg, "episodic_memory"), 2);
    }

    #[test]
    fn another_embedding_model_in_the_table_refuses_the_whole_run() {
        let cfg = scratch("bf-model");
        let id = put(&cfg, Table::Semantic, "an old row", true);
        set_vector(&cfg, Table::Semantic, id, &Fake::vector("x"), "other-model");
        let dir = tree(&cfg, "notes", &[("a.md", "one"), ("b.md", "two")]);
        let msg = refused(go(&cfg, &args(&dir), &Fake::up()));
        assert!(msg.contains("different model than 'bge-m3'"), "{msg}");
        assert_eq!(count(&cfg, "semantic_memory"), 1);
    }

    // ---- without embeddings ------------------------------------------------

    #[test]
    fn without_an_embedder_every_file_is_stored_and_each_line_says_so() {
        let cfg = scratch("bf-no-embedder");
        let dir = tree(
            &cfg,
            "notes",
            &[("a.md", "first fact"), ("b.md", "second fact")],
        );
        let ran = go(&cfg, &args(&dir), &down());
        assert_eq!(
            ran.summary(),
            &Summary {
                stored: 2,
                skipped: 0,
                without_embedding: 2
            }
        );
        assert_eq!(
            ran.lines(),
            [
                format!("stored a.md {NO_EMBEDDINGS}").as_str(),
                format!("stored b.md {NO_EMBEDDINGS}").as_str(),
                "backfilled 2 file(s); skipped 0",
                format!("2 stored {NO_EMBEDDINGS}").as_str(),
            ]
        );
        // EMBEDDINGS=on promised vectors: said once, not once per file.
        assert_eq!(ran.err.matches("warning: EMBEDDINGS=on").count(), 1);
        assert_eq!(
            rows(
                &cfg,
                "SELECT embedding IS NULL, embed_model FROM semantic_memory ORDER BY id;"
            ),
            vec![
                vec![Cell::Int(1), Cell::Null],
                vec![Cell::Int(1), Cell::Null]
            ]
        );
    }

    #[test]
    fn without_embeddings_a_rerun_meets_the_text_guard() {
        let cfg = scratch("bf-no-embedder-rerun");
        let dir = tree(&cfg, "notes", &[("a.md", "first fact")]);
        go(&cfg, &args(&dir), &down()).summary();
        let ran = go(&cfg, &args(&dir), &down());
        assert_eq!(ran.summary().stored, 0);
        assert_eq!(ran.summary().skipped, 1);
        assert!(
            ran.out
                .contains("SKIP a.md: identical memory already exists (id 1;"),
            "{}",
            ran.out
        );
        assert_eq!(count(&cfg, "semantic_memory"), 1);
    }

    #[test]
    fn embeddings_off_never_contacts_the_embedder_and_does_not_warn() {
        let mut cfg = scratch("bf-off");
        cfg.embeddings = false;
        let dir = tree(&cfg, "notes", &[("a.md", "first fact")]);
        let fake = Fake::up();
        let ran = go(&cfg, &args(&dir), &fake);
        assert_eq!(fake.calls.get(), 0);
        assert_eq!(ran.summary().without_embedding, 1);
        assert!(
            ran.out
                .contains("stored a.md WITHOUT embedding — EMBEDDINGS=off"),
            "{}",
            ran.out
        );
        assert!(
            ran.out
                .lines()
                .last()
                .unwrap()
                .starts_with("1 stored WITHOUT embedding — EMBEDDINGS=off"),
            "{}",
            ran.out
        );
        assert_eq!(ran.err, "");
    }

    #[test]
    fn a_run_that_loses_its_embedder_midway_says_so_for_the_rows_after_it() {
        struct Flaky {
            inner: Fake,
        }
        impl Embedder for Flaky {
            fn probe(&self) -> std::result::Result<(), Unavailable> {
                // Up for the first file, gone from the second on.
                if self.inner.calls.get() >= 2 {
                    return Err(Unavailable::Unreachable {
                        url: "http://127.0.0.1:9".into(),
                    });
                }
                self.inner.probe()
            }
            fn embed(&self, text: &str) -> Result<Vec<f32>> {
                self.inner.embed(text)
            }
        }
        let cfg = scratch("bf-flaky");
        let dir = tree(
            &cfg,
            "notes",
            &[("a.md", "first fact"), ("b.md", "second fact")],
        );
        let ran = go(&cfg, &args(&dir), &Flaky { inner: Fake::up() });
        assert_eq!(ran.summary().stored, 2);
        assert_eq!(ran.summary().without_embedding, 1);
        assert_eq!(ran.lines()[0], "stored a.md");
        assert_eq!(ran.lines()[1], format!("stored b.md {NO_EMBEDDINGS}"));
    }

    // ---- refusals: nothing is written, nothing is printed -------------------

    #[test]
    fn a_missing_database_is_refused_and_none_is_created() {
        let mut cfg = scratch("bf-no-db");
        cfg.db_path = cfg.db_path.with_file_name("absent.db");
        let dir = tree(&cfg, "notes", &[("a.md", "one")]);
        let msg = refused(go(&cfg, &args(&dir), &Fake::up()));
        assert!(msg.contains("memory DB missing"), "{msg}");
        assert!(!cfg.db_path.exists());
    }

    #[test]
    fn a_path_that_is_not_a_directory_is_refused() {
        let cfg = scratch("bf-not-dir");
        let dir = tree(&cfg, "notes", &[("a.md", "one")]);
        let msg = refused(go(&cfg, &args(&dir.join("a.md")), &Fake::up()));
        assert!(msg.contains("not a directory"), "{msg}");
        let msg = refused(go(&cfg, &args(&dir.join("nope")), &Fake::up()));
        assert!(msg.contains("not a directory"), "{msg}");
        assert_eq!(count(&cfg, "semantic_memory"), 0);
    }

    #[test]
    fn a_glob_that_matches_nothing_is_refused_and_names_it() {
        let cfg = scratch("bf-no-match");
        let dir = tree(&cfg, "notes", &[("a.txt", "one")]);
        let msg = refused(go(&cfg, &args(&dir), &Fake::up()));
        assert!(msg.contains("no files matching *.md"), "{msg}");
        assert!(msg.contains(&dir.display().to_string()), "{msg}");
    }

    #[test]
    fn a_glob_that_names_a_path_or_a_class_is_refused() {
        let cfg = scratch("bf-bad-glob");
        let dir = tree(&cfg, "notes", &[("a.md", "one")]);
        for glob in ["sub/*.md", "*\\*.md", "[ab].md", "{a,b}.md", ""] {
            let mut a = args(&dir);
            a.glob = glob.into();
            let msg = refused(go(&cfg, &a, &Fake::up()));
            assert!(msg.contains("--glob"), "{glob:?}: {msg}");
        }
        assert_eq!(count(&cfg, "semantic_memory"), 0);
    }

    #[test]
    fn a_category_the_schema_does_not_know_is_refused_before_any_row() {
        let cfg = scratch("bf-category");
        let dir = tree(&cfg, "notes", &[("a.md", "one")]);
        let mut a = args(&dir);
        a.category = "gotcha".into();
        let msg = refused(go(&cfg, &a, &Fake::up()));
        assert!(msg.contains("--category 'gotcha'"), "{msg}");
        assert!(msg.contains("reference"), "{msg}");
        assert_eq!(count(&cfg, "semantic_memory"), 0);
    }

    #[test]
    fn baseline_is_never_stored_by_a_bulk_import() {
        let cfg = scratch("bf-baseline");
        let dir = tree(&cfg, "notes", &[("a.md", "one")]);
        let mut a = args(&dir);
        a.category = "baseline".into();
        let msg = refused(go(&cfg, &a, &Fake::up()));
        assert!(msg.contains("'baseline'"), "{msg}");
        assert!(msg.contains("remember"), "{msg}");
        assert_eq!(count(&cfg, "semantic_memory"), 0);
    }

    #[test]
    fn an_over_long_project_or_model_is_refused_before_any_row() {
        let cfg = scratch("bf-meta");
        let dir = tree(&cfg, "notes", &[("a.md", "one")]);
        let mut a = args(&dir);
        a.project = Some("p".repeat(129));
        assert!(refused(go(&cfg, &a, &Fake::up())).contains("--project is 129 chars"));
        let mut a = args(&dir);
        a.model = "m".repeat(129);
        assert!(refused(go(&cfg, &a, &Fake::up())).contains("--model is 129 chars"));
        assert_eq!(count(&cfg, "semantic_memory"), 0);
    }

    #[test]
    fn the_instructions_table_is_not_a_backfill_target() {
        let cfg = scratch("bf-instructions");
        let dir = tree(&cfg, "notes", &[("a.md", "one")]);
        let mut a = args(&dir);
        a.table = Table::Instructions;
        let msg = refused(go(&cfg, &a, &Fake::up()));
        assert!(msg.contains("instruct"), "{msg}");
    }
}
