//! The `.env` beside the binary: the machine's own configuration.
//!
//! It replaces the `env` block in `~/.claude/settings.json`, which only ever
//! reached Claude Code's own tool processes -- a scheduled backup never saw it. A
//! file next to the one artifact the user installed is read by every process that
//! runs that artifact.
//!
//! The format is deliberately small and hand-parsed (every dependency ends up
//! inside the binary): `KEY=VALUE` lines, `#` comments, blank lines, optional
//! double quotes around a value, CRLF tolerated. No `export`, no `${}` expansion,
//! no backslash escapes (Windows paths are full of backslashes), no multi-line
//! values. Anything outside that is a REFUSAL naming the file and the line, never a
//! skipped line: a skipped database-path line silently falls back to the default
//! database, which is exactly the MISSING failure mode.

use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};

use super::ConfigError;

/// The file name, beside the binary.
pub const FILE_NAME: &str = ".env";

/// One `KEY=VALUE` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub key: String,
    pub value: String,
    /// 1-based, for messages.
    pub line: usize,
    /// Where the value sits in its line as written, quotes included, so the
    /// writer can replace exactly that and keep the rest of the line -- a
    /// trailing comment above all -- byte-for-byte.
    span: Range<usize>,
}

/// Why a line is not in the accepted format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxError {
    /// 1-based.
    pub line: usize,
    pub why: String,
}

/// Parse a whole file. The first malformed line or duplicate key ends it: a
/// partial result would be a config that silently differs from the file.
pub fn parse(text: &str) -> Result<Vec<Entry>, SyntaxError> {
    let mut entries: Vec<Entry> = Vec::new();
    for (i, (content, _)) in lines(text).enumerate() {
        let line = i + 1;
        // A byte-order mark is an editor's artefact, not part of the first key.
        let start = if i == 0 && content.starts_with('\u{feff}') {
            '\u{feff}'.len_utf8()
        } else {
            0
        };
        let Some((key, value, span)) =
            parse_line(content, start).map_err(|why| SyntaxError { line, why })?
        else {
            continue;
        };
        if let Some(first) = entries.iter().find(|e| e.key == key) {
            return Err(SyntaxError {
                line,
                why: format!("duplicate key {key} (first set on line {})", first.line),
            });
        }
        entries.push(Entry {
            key: key.to_string(),
            value: value.to_string(),
            line,
            span,
        });
    }
    Ok(entries)
}

/// Split into (content, line ending) pairs. The ending is `\n`, `\r\n`, or empty
/// for an unterminated last line; the writer puts it back exactly.
fn lines(text: &str) -> impl Iterator<Item = (&str, &str)> {
    text.split_inclusive('\n').map(|l| {
        let content = match l.strip_suffix('\n') {
            Some(c) => c.strip_suffix('\r').unwrap_or(c),
            None => l,
        };
        (content, &l[content.len()..])
    })
}

/// A parsed `KEY=VALUE` line: the key, the value, and the value's byte range
/// within the line.
type Assignment<'a> = (&'a str, &'a str, Range<usize>);

/// One line: `None` for a blank line or a comment.
fn parse_line(content: &str, start: usize) -> Result<Option<Assignment<'_>>, String> {
    let body = &content[start..];
    let trimmed = body.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return Ok(None);
    }
    let Some(eq) = trimmed.find('=') else {
        return Err(if is_export(trimmed) {
            EXPORT.into()
        } else {
            "expected KEY=VALUE".into()
        });
    };
    let key = trimmed[..eq].trim_end();
    if is_export(key) {
        return Err(EXPORT.into());
    }
    if key.is_empty() {
        return Err("missing key before `=`".into());
    }
    if !valid_key(key) {
        return Err(format!(
            "`{key}` is not a valid key (letters, digits and `_`, not starting with a digit)"
        ));
    }

    let after_eq = start + (body.len() - trimmed.len()) + eq + 1;
    let rest = &content[after_eq..];
    let value = rest.trim_start();
    let skipped = rest.len() - value.len();
    let at = after_eq + skipped;

    match value.chars().next() {
        // `KEY=` and `KEY=   # comment` are both an empty value. The span sits
        // right after `=`, so a writer filling it in keeps the comment.
        None => Ok(Some((key, "", after_eq..after_eq))),
        Some('#') if skipped > 0 => Ok(Some((key, "", after_eq..after_eq))),
        Some('"') => {
            let inner = &value[1..];
            let Some(close) = inner.find('"') else {
                return Err("unterminated double quote".into());
            };
            let end = at + 1 + close + 1;
            if !is_comment_or_nothing(&content[end..]) {
                return Err("unexpected text after the closing quote".into());
            }
            Ok(Some((key, &inner[..close], at..end)))
        }
        Some('\'') => Err("single quotes are not supported; use double quotes".into()),
        Some(_) => {
            let len = value.find(char::is_whitespace).unwrap_or(value.len());
            let token = &value[..len];
            if token.contains(['"', '\'']) {
                return Err(
                    "a quote inside an unquoted value; wrap the whole value in double quotes"
                        .into(),
                );
            }
            if token.contains('#') {
                return Err(
                    "`#` in an unquoted value is ambiguous; wrap the value in double quotes".into(),
                );
            }
            if !is_comment_or_nothing(&value[len..]) {
                return Err("unquoted value contains whitespace; wrap it in double quotes".into());
            }
            Ok(Some((key, token, at..at + len)))
        }
    }
}

const EXPORT: &str = "`export` is not supported; write KEY=VALUE";

fn is_export(s: &str) -> bool {
    s == "export" || s.starts_with("export ") || s.starts_with("export\t")
}

fn is_comment_or_nothing(s: &str) -> bool {
    let s = s.trim_start();
    s.is_empty() || s.starts_with('#')
}

fn valid_key(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Where the `.env` for this binary lives: beside the canonical binary, so a
/// binary linked into `~/.local/bin` finds the file next to the real one.
/// Returns the canonical binary path and the `.env` path.
pub fn location(exe: &Path) -> io::Result<(PathBuf, PathBuf)> {
    let exe = exe.canonicalize()?;
    let dir = exe
        .parent()
        .ok_or_else(|| io::Error::other("the binary has no parent directory"))?;
    let path = dir.join(FILE_NAME);
    Ok((exe, path))
}

/// Read and parse the file at `path`: `None` when there is none, a refusal when
/// it cannot be read or parsed.
pub fn read(path: &Path) -> Result<Option<Vec<Entry>>, ConfigError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(ConfigError::Unreadable {
                path: path.to_path_buf(),
                why: e.to_string(),
            });
        }
    };
    let Ok(text) = String::from_utf8(bytes) else {
        return Err(ConfigError::Unreadable {
            path: path.to_path_buf(),
            why: "not valid UTF-8; save it as UTF-8".into(),
        });
    };
    parse(&text).map(Some).map_err(|e| ConfigError::Malformed {
        path: path.to_path_buf(),
        line: e.line,
        why: e.why,
    })
}

/// What was found beside the binary.
#[derive(Debug, Clone)]
pub struct DotEnv {
    /// The binary the file belongs to, symlinks resolved.
    pub exe: PathBuf,
    /// `<directory of exe>/.env`, whether or not it exists.
    pub path: PathBuf,
    /// The parsed entries, or `None` when there is no file.
    pub entries: Option<Vec<Entry>>,
}

impl DotEnv {
    /// Locate, read and parse the `.env` beside `exe`. No file is not an error --
    /// the built-in defaults are a working install. A file that exists but cannot
    /// be read or parsed is refused.
    pub fn beside(exe: &Path) -> Result<DotEnv, ConfigError> {
        let (exe, path) = location(exe).map_err(|e| ConfigError::Unlocated {
            why: format!("{}: {e}", exe.display()),
        })?;
        let entries = read(&path)?;
        Ok(DotEnv { exe, path, entries })
    }

    /// No file at all, for a binary at `exe` (taken as already canonical).
    #[cfg(test)]
    pub fn absent(exe: PathBuf) -> DotEnv {
        let path = exe.with_file_name(FILE_NAME);
        DotEnv {
            exe,
            path,
            entries: None,
        }
    }

    pub fn present(&self) -> bool {
        self.entries.is_some()
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries()
            .iter()
            .find(|e| e.key == key)
            .map(|e| e.value.as_str())
    }

    pub fn entries(&self) -> &[Entry] {
        self.entries.as_deref().unwrap_or(&[])
    }
}

// ---------------------------------------------------------------------------
// The writer, shared by first-run setup and by `update`.

/// What to do with a key that already holds a different value.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnConflict {
    /// Leave it and report it. The default for every caller: a value the user
    /// set is theirs.
    Keep,
    /// Replace it. Only `update` bumping the recorded version does this.
    Overwrite,
}

/// A key whose existing value was left alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub key: String,
    pub existing: String,
    pub wanted: String,
}

/// The result of merging updates into a file's text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Merged {
    pub text: String,
    pub added: Vec<String>,
    pub changed: Vec<String>,
    pub kept: Vec<Conflict>,
    /// Whether the file on disk was replaced. `false` when nothing differed.
    pub wrote: bool,
}

/// Merge `updates` into `original`, preserving every other byte. Pure: `path`
/// only names the file in a refusal.
///
/// A file that does not parse is refused, never rewritten: rewriting would
/// either drop the line the parser could not read or bless it.
pub fn merge(
    path: &Path,
    original: &str,
    updates: &[(&str, &str)],
    on_conflict: OnConflict,
) -> Result<Merged, ConfigError> {
    let entries = parse(original).map_err(|e| ConfigError::Malformed {
        path: path.to_path_buf(),
        line: e.line,
        why: e.why,
    })?;
    let unwritable = |why: String| ConfigError::Unwritable {
        path: path.to_path_buf(),
        why,
    };

    // Validate every update before touching anything: all or nothing.
    let mut formatted = Vec::with_capacity(updates.len());
    for (i, (key, value)) in updates.iter().enumerate() {
        if !valid_key(key) {
            return Err(unwritable(format!("`{key}` is not a valid key")));
        }
        if updates[..i].iter().any(|(k, _)| k == key) {
            return Err(unwritable(format!("{key} is given twice")));
        }
        formatted.push(format_value(value).map_err(|why| unwritable(format!("{key}: {why}")))?);
    }

    let mut lines: Vec<(String, &str)> = lines(original)
        .map(|(content, eol)| (content.to_string(), eol))
        .collect();
    let mut merged = Merged::default();
    let mut appended: Vec<String> = Vec::new();
    for ((key, value), written) in updates.iter().zip(formatted) {
        match entries.iter().find(|e| e.key == *key) {
            Some(e) if e.value == *value => {}
            Some(e) if on_conflict == OnConflict::Keep => merged.kept.push(Conflict {
                key: key.to_string(),
                existing: e.value.clone(),
                wanted: value.to_string(),
            }),
            Some(e) => {
                lines[e.line - 1].0.replace_range(e.span.clone(), &written);
                merged.changed.push(key.to_string());
            }
            None => {
                appended.push(format!("{key}={written}"));
                merged.added.push(key.to_string());
            }
        }
    }

    let mut text: String = lines
        .iter()
        .flat_map(|(c, eol)| [c.as_str(), eol])
        .collect();
    if !appended.is_empty() {
        // Continue the convention of the last terminated line: in a file whose
        // endings are mixed, that is the one an editor would carry on with.
        let eol = lines
            .iter()
            .rev()
            .map(|(_, eol)| *eol)
            .find(|eol| !eol.is_empty())
            .unwrap_or("\n");
        if !text.is_empty() && !text.ends_with('\n') {
            text.push_str(eol);
        }
        for line in appended {
            text.push_str(&line);
            text.push_str(eol);
        }
    }
    merged.text = text;
    Ok(merged)
}

/// Read `path` (absent counts as empty), merge `updates`, and replace the file
/// atomically if anything changed. An unchanged file is not rewritten at all, so
/// a repeated run leaves its bytes and its mtime alone.
pub fn set_keys(
    path: &Path,
    updates: &[(&str, &str)],
    on_conflict: OnConflict,
) -> Result<Merged, ConfigError> {
    let original = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(ConfigError::Unreadable {
                path: path.to_path_buf(),
                why: e.to_string(),
            });
        }
    };
    let mut merged = merge(path, &original, updates, on_conflict)?;
    if merged.text != original {
        write_atomic_with(path, merged.text.as_bytes(), |_| Ok(())).map_err(|e| {
            ConfigError::Io {
                path: path.to_path_buf(),
                why: e.to_string(),
            }
        })?;
        merged.wrote = true;
    }
    Ok(merged)
}

/// A value as it must be written to parse back unchanged: bare when it can be,
/// double-quoted otherwise. The format has no escapes, so a `"` or a line break
/// cannot be written at all -- refused rather than mangled.
pub fn format_value(value: &str) -> Result<String, String> {
    if value.contains('"') {
        return Err("a value containing `\"` cannot be written; the format has no escapes".into());
    }
    if value.contains(['\n', '\r']) {
        return Err("a value cannot span lines".into());
    }
    if value.contains(|c: char| c.is_whitespace() || c == '#' || c == '\'') {
        Ok(format!("\"{value}\""))
    } else {
        Ok(value.to_string())
    }
}

/// Replace `path` with `bytes` via a temporary file in the same directory and a
/// rename, so a reader sees either the old file or the new one, never half.
/// `before_rename` is the seam a test uses to stand in for a crash.
fn write_atomic_with(
    path: &Path,
    bytes: &[u8],
    before_rename: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<()> {
    use std::io::Write;

    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| FILE_NAME.into());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let tmp = dir.join(format!("{name}.tmp.{}.{nanos}", std::process::id()));

    // create_new: never truncate a file some other writer is still filling.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    let result = (|| {
        file.write_all(bytes)?;
        if let Ok(meta) = std::fs::metadata(path) {
            file.set_permissions(meta.permissions())?;
        }
        file.sync_all()?;
        drop(file);
        before_rename(&tmp)?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return result;
    }
    // Make the rename itself durable. Directories cannot be opened on Windows,
    // where the rename is already durable once it returns.
    #[cfg(unix)]
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::error::{EXIT_REFUSED, Error};
    use std::fs;

    pub(crate) fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sm-dotenv-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn values(text: &str) -> Vec<(String, String)> {
        parse(text)
            .unwrap_or_else(|e| panic!("{text:?} should parse: {e:?}"))
            .into_iter()
            .map(|e| (e.key, e.value))
            .collect()
    }

    fn kv(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    // ---- the parser --------------------------------------------------------

    /// (case name, file text, the key/value pairs it must yield)
    type Accepted = (
        &'static str,
        &'static str,
        &'static [(&'static str, &'static str)],
    );

    #[test]
    fn accepted_lines() {
        let cases: &[Accepted] = &[
            (
                "comments and blank lines",
                "# written by init\n\nA=1\n   # indented comment\n\t\nB=2\n",
                &[("A", "1"), ("B", "2")],
            ),
            (
                "quoted value with spaces",
                "BACKUP_DIR=\"/home/u/My Backups\"\n",
                &[("BACKUP_DIR", "/home/u/My Backups")],
            ),
            ("# inside quotes", "K=\"a # b\"\n", &[("K", "a # b")]),
            (
                "trailing comments",
                "A=v # note\nB=\"w x\"   # note\n",
                &[("A", "v"), ("B", "w x")],
            ),
            (
                "CRLF",
                "A=1\r\nB=\"x y\"\r\n# c\r\n\r\n",
                &[("A", "1"), ("B", "x y")],
            ),
            ("no trailing newline", "A=1", &[("A", "1")]),
            ("whitespace around =", "A = 1\n", &[("A", "1")]),
            (
                "empty value",
                "A=\nB=   # nothing\n",
                &[("A", ""), ("B", "")],
            ),
            ("empty quoted value", "A=\"\"\n", &[("A", "")]),
            (
                "= inside a value",
                "OLLAMA_URL=http://box:11434/?a=b\n",
                &[("OLLAMA_URL", "http://box:11434/?a=b")],
            ),
            (
                "backslashes are literal",
                "P=\"C:\\Users\\me\\mem.db\"\nQ=C:\\x\\y\n",
                &[("P", "C:\\Users\\me\\mem.db"), ("Q", "C:\\x\\y")],
            ),
            ("a UTF-8 BOM", "\u{feff}A=1\n", &[("A", "1")]),
            (
                "umlauts",
                "P=/home/jürgen/mem.db\n",
                &[("P", "/home/jürgen/mem.db")],
            ),
        ];
        for (name, text, want) in cases {
            assert_eq!(values(text), kv(want), "{name}");
        }
    }

    #[test]
    fn entries_carry_their_line_numbers() {
        let got = parse("# c\n\nA=1\nB=2\n").unwrap();
        assert_eq!(got.iter().map(|e| e.line).collect::<Vec<_>>(), vec![3, 4]);
    }

    /// Every malformed shape, with the line it must be reported on and a phrase
    /// the message must contain.
    const REFUSED: &[(&str, &str, usize, &str)] = &[
        ("export", "A=1\nexport X=1\n", 2, "export"),
        ("missing =", "JUSTAKEY\n", 1, "KEY=VALUE"),
        ("unterminated quote", "A=\"/home/u/x\n", 1, "unterminated"),
        ("duplicate key", "A=1\n# c\nA=2\n", 3, "duplicate"),
        ("single quotes", "A='x y'\n", 1, "single quotes"),
        ("unquoted whitespace", "A=two words\n", 1, "double quotes"),
        ("unquoted #", "A=a#b\n", 1, "#"),
        (
            "text after the closing quote",
            "A=\"a\"b\n",
            1,
            "closing quote",
        ),
        ("quote inside an unquoted value", "A=a\"b\"\n", 1, "quote"),
        ("key starting with a digit", "1A=x\n", 1, "not a valid key"),
        ("key with a dash", "MY-KEY=x\n", 1, "not a valid key"),
        ("missing key", "=x\n", 1, "key"),
    ];

    #[test]
    fn refused_lines_name_the_line_and_the_problem() {
        for (name, text, line, phrase) in REFUSED {
            let err = parse(text).expect_err(name);
            assert_eq!(err.line, *line, "{name}: {err:?}");
            assert!(err.why.contains(phrase), "{name}: {:?}", err.why);
        }
    }

    #[test]
    fn a_duplicate_names_the_first_occurrence_too() {
        let err = parse("A=1\nA=2\n").unwrap_err();
        assert!(err.why.contains("line 1"), "{}", err.why);
    }

    /// Each refusal, read from a real file: exit 2, the `refused:` prefix, and the
    /// file and line named, so the agent can point the user at the exact spot.
    #[test]
    fn every_malformed_file_is_a_refusal_naming_file_and_line() {
        for (i, (name, text, line, _)) in REFUSED.iter().enumerate() {
            let dir = tmpdir(&format!("refuse-{i}"));
            let exe = dir.join("supercharged-memory");
            fs::write(&exe, b"").unwrap();
            fs::write(dir.join(FILE_NAME), text).unwrap();
            let err: Error = DotEnv::beside(&exe).expect_err(name).into();
            let msg = err.to_string();
            assert_eq!(err.exit_code(), EXIT_REFUSED, "{name}");
            assert!(msg.starts_with("refused: "), "{name}: {msg}");
            assert!(msg.contains(".env"), "{name}: {msg}");
            assert!(msg.contains(&format!("line {line}")), "{name}: {msg}");
        }
    }

    // ---- locating and loading ----------------------------------------------

    #[test]
    fn no_file_is_not_an_error() {
        let dir = tmpdir("absent");
        let exe = dir.join("supercharged-memory");
        fs::write(&exe, b"").unwrap();
        let d = DotEnv::beside(&exe).unwrap();
        assert!(!d.present());
        assert_eq!(d.path, dir.canonicalize().unwrap().join(FILE_NAME));
        assert_eq!(d.get("A"), None);
    }

    #[test]
    fn a_present_file_is_read() {
        let dir = tmpdir("present");
        let exe = dir.join("supercharged-memory");
        fs::write(&exe, b"").unwrap();
        fs::write(dir.join(FILE_NAME), "A=1\nB=\"x y\"\n").unwrap();
        let d = DotEnv::beside(&exe).unwrap();
        assert!(d.present());
        assert_eq!(d.get("A"), Some("1"));
        assert_eq!(d.get("B"), Some("x y"));
        assert_eq!(d.get("C"), None);
    }

    #[test]
    fn an_unreadable_file_is_refused_not_skipped() {
        let dir = tmpdir("unreadable");
        let exe = dir.join("supercharged-memory");
        fs::write(&exe, b"").unwrap();
        fs::create_dir(dir.join(FILE_NAME)).unwrap(); // a directory cannot be read as text
        let err: Error = DotEnv::beside(&exe).unwrap_err().into();
        assert_eq!(err.exit_code(), EXIT_REFUSED);
        assert!(err.to_string().starts_with("refused: "), "{err}");
        assert!(err.to_string().contains(".env"), "{err}");
    }

    #[test]
    fn a_file_that_is_not_utf8_is_refused() {
        let dir = tmpdir("latin1");
        let exe = dir.join("supercharged-memory");
        fs::write(&exe, b"").unwrap();
        fs::write(dir.join(FILE_NAME), b"P=/home/j\xfcrgen\n").unwrap();
        let err: Error = DotEnv::beside(&exe).unwrap_err().into();
        assert_eq!(err.exit_code(), EXIT_REFUSED);
        assert!(err.to_string().starts_with("refused: "), "{err}");
    }

    /// A binary linked into `~/.local/bin` must find the `.env` beside the real
    /// file, not one that happens to sit beside the link.
    #[cfg(unix)]
    #[test]
    fn through_a_symlink_the_file_beside_the_target_is_read() {
        let dir = tmpdir("symlink");
        let real = dir.join("real");
        let link = dir.join("link");
        fs::create_dir_all(&real).unwrap();
        fs::create_dir_all(&link).unwrap();
        fs::write(real.join("supercharged-memory"), b"").unwrap();
        std::os::unix::fs::symlink(
            real.join("supercharged-memory"),
            link.join("supercharged-memory"),
        )
        .unwrap();
        fs::write(real.join(FILE_NAME), "A=real\n").unwrap();
        fs::write(link.join(FILE_NAME), "A=decoy\n").unwrap();

        let (exe, path) = location(&link.join("supercharged-memory")).unwrap();
        let real = real.canonicalize().unwrap();
        assert_eq!(exe, real.join("supercharged-memory"));
        assert_eq!(path, real.join(FILE_NAME));

        let d = DotEnv::beside(&link.join("supercharged-memory")).unwrap();
        assert_eq!(d.get("A"), Some("real"));
    }

    // ---- the writer --------------------------------------------------------

    const ORIGINAL: &str = "# machine config -- hand edits welcome\r\n\
                            \n\
                            SUPERCHARGED_MEMORY_TURSO_PATH=/data/mem.db   # the live one\n\
                            FUTURE_KEY=\"from a newer binary\"\n\
                            \t# indented note\n\
                            SUPERCHARGED_MEMORY_VERSION=0.1.0\n";

    #[test]
    fn unrelated_lines_and_comments_survive_byte_for_byte() {
        let m = merge(
            Path::new("/x/.env"),
            ORIGINAL,
            &[
                ("SUPERCHARGED_MEMORY_VERSION", "0.2.0"),
                ("BACKUP_DIR", "/home/u/My Backups"),
            ],
            OnConflict::Overwrite,
        )
        .unwrap();
        let want = "# machine config -- hand edits welcome\r\n\
                    \n\
                    SUPERCHARGED_MEMORY_TURSO_PATH=/data/mem.db   # the live one\n\
                    FUTURE_KEY=\"from a newer binary\"\n\
                    \t# indented note\n\
                    SUPERCHARGED_MEMORY_VERSION=0.2.0\n\
                    BACKUP_DIR=\"/home/u/My Backups\"\n";
        assert_eq!(m.text, want);
        assert_eq!(m.changed, vec!["SUPERCHARGED_MEMORY_VERSION".to_string()]);
        assert_eq!(m.added, vec!["BACKUP_DIR".to_string()]);
        assert!(m.kept.is_empty());
    }

    #[test]
    fn a_replaced_value_keeps_its_trailing_comment() {
        let m = merge(
            Path::new("/x/.env"),
            ORIGINAL,
            &[("SUPERCHARGED_MEMORY_TURSO_PATH", "/new place/mem.db")],
            OnConflict::Overwrite,
        )
        .unwrap();
        assert!(
            m.text.contains(
                "SUPERCHARGED_MEMORY_TURSO_PATH=\"/new place/mem.db\"   # the live one\n"
            ),
            "{}",
            m.text
        );
    }

    #[test]
    fn a_differing_value_is_never_clobbered_unless_asked() {
        let m = merge(
            Path::new("/x/.env"),
            ORIGINAL,
            &[("SUPERCHARGED_MEMORY_TURSO_PATH", "/other/mem.db")],
            OnConflict::Keep,
        )
        .unwrap();
        assert_eq!(m.text, ORIGINAL);
        assert_eq!(
            m.kept,
            vec![Conflict {
                key: "SUPERCHARGED_MEMORY_TURSO_PATH".into(),
                existing: "/data/mem.db".into(),
                wanted: "/other/mem.db".into(),
            }]
        );
        assert!(m.changed.is_empty() && m.added.is_empty());
    }

    #[test]
    fn an_equal_value_is_neither_changed_nor_a_conflict() {
        let m = merge(
            Path::new("/x/.env"),
            ORIGINAL,
            &[("SUPERCHARGED_MEMORY_TURSO_PATH", "/data/mem.db")],
            OnConflict::Keep,
        )
        .unwrap();
        assert_eq!(m.text, ORIGINAL);
        assert!(m.changed.is_empty() && m.added.is_empty() && m.kept.is_empty());
    }

    #[test]
    fn a_new_file_gets_one_line_per_key() {
        let m = merge(
            Path::new("/x/.env"),
            "",
            &[("A", "1"), ("B", "x y")],
            OnConflict::Keep,
        )
        .unwrap();
        assert_eq!(m.text, "A=1\nB=\"x y\"\n");
    }

    #[test]
    fn appending_terminates_an_unterminated_last_line() {
        let m = merge(Path::new("/x/.env"), "A=1", &[("B", "2")], OnConflict::Keep).unwrap();
        assert_eq!(m.text, "A=1\nB=2\n");
    }

    #[test]
    fn appending_to_a_crlf_file_uses_crlf() {
        let m = merge(
            Path::new("/x/.env"),
            "A=1\r\n",
            &[("B", "2")],
            OnConflict::Keep,
        )
        .unwrap();
        assert_eq!(m.text, "A=1\r\nB=2\r\n");
    }

    #[test]
    fn a_malformed_file_is_refused_rather_than_rewritten() {
        let err: Error = merge(
            Path::new("/x/.env"),
            "export A=1\n",
            &[("B", "2")],
            OnConflict::Keep,
        )
        .unwrap_err()
        .into();
        assert_eq!(err.exit_code(), EXIT_REFUSED);
        assert!(err.to_string().starts_with("refused: "), "{err}");
        assert!(err.to_string().contains("line 1"), "{err}");
    }

    #[test]
    fn a_value_the_format_cannot_hold_is_refused() {
        for (key, value) in [
            ("A", "has \"quotes\""),
            ("A", "two\nlines"),
            ("BAD-KEY", "x"),
        ] {
            let err: Error = merge(Path::new("/x/.env"), "", &[(key, value)], OnConflict::Keep)
                .unwrap_err()
                .into();
            assert_eq!(err.exit_code(), EXIT_REFUSED, "{key}={value:?}");
            assert!(err.to_string().starts_with("refused: "), "{err}");
        }
    }

    #[test]
    fn written_values_parse_back_unchanged() {
        for v in [
            "",
            "plain",
            "/home/u/My Backups",
            "a # b",
            "it's",
            "C:\\Users\\me\\mem.db",
            "  padded  ",
            "http://box:11434/?a=b",
            "#leading",
        ] {
            let line = format!("K={}\n", format_value(v).unwrap());
            assert_eq!(values(&line), kv(&[("K", v)]), "{v:?} written as {line:?}");
        }
        assert!(format_value("a\"b").is_err());
        assert!(format_value("a\rb").is_err());
    }

    #[test]
    fn set_keys_writes_the_file_and_is_idempotent() {
        let dir = tmpdir("set-keys");
        let path = dir.join(FILE_NAME);
        let first = set_keys(&path, &[("A", "1")], OnConflict::Keep).unwrap();
        assert!(first.wrote);
        assert_eq!(fs::read_to_string(&path).unwrap(), "A=1\n");

        let second = set_keys(&path, &[("A", "1")], OnConflict::Keep).unwrap();
        assert!(
            !second.wrote,
            "nothing differed, so nothing may be rewritten"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "A=1\n");

        // No temporary file is left behind.
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n != FILE_NAME)
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    /// The crash window is after the temporary file is complete and before the
    /// rename. Whatever happens there, the old file must still be whole.
    #[test]
    fn a_crash_between_temp_write_and_rename_leaves_the_old_file_intact() {
        let dir = tmpdir("crash");
        let path = dir.join(FILE_NAME);
        fs::write(&path, ORIGINAL).unwrap();

        let mut seen: Option<(PathBuf, Vec<u8>)> = None;
        let result = write_atomic_with(&path, b"A=new\n", |tmp| {
            seen = Some((tmp.to_path_buf(), fs::read(tmp)?));
            Err(io::Error::other("simulated crash"))
        });

        assert!(result.is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), ORIGINAL);
        let (tmp, content) = seen.expect("the temporary file was written first");
        assert_eq!(
            content, b"A=new\n",
            "the temp file held the complete new text"
        );
        assert_eq!(
            tmp.parent(),
            path.parent(),
            "same directory, so the rename is atomic"
        );
        assert_ne!(tmp, path);
    }
}
