//! `backup`: a gzipped SQL dump of the database into BACKUP_DIR, verified before
//! it replaces anything, with daily and weekly retention.
//!
//! This replaces `supercharged-memory-backup.sh`, which re-implemented the
//! config rules in shell and drifted from them, shelled out to `tursodb .dump`,
//! and refused an empty database because it validated a dump by grepping for
//! an INSERT. Here the dump is native (see `store::turso::dump`), its rows are
//! re-counted against the snapshot they came from, and the gzip on disk is
//! decompressed to its end before it takes a dump's name.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{Datelike, NaiveDate, Weekday};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::store::turso::TursoStore;
use crate::store::turso::dump::Dumped;

/// The two kinds of dump in BACKUP_DIR. The suffixes are disjoint, so each
/// tier's retention only ever sees its own files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Daily,
    Weekly,
}

impl Tier {
    pub fn suffix(self) -> &'static str {
        match self {
            Tier::Daily => "-supercharged-memory-native.sql.gz",
            Tier::Weekly => "-supercharged-memory-native-weekly.sql.gz",
        }
    }

    /// How many of the tier's newest files retention keeps.
    pub fn keep(self) -> usize {
        match self {
            Tier::Daily => 3,
            Tier::Weekly => 4,
        }
    }
}

/// `YYYY-MM-DD-supercharged-memory-native.sql.gz`, and the weekly variant.
/// The `-native` marker keeps these names apart from the Python script's
/// (`<date>-supercharged-memory.sql.gz`), which share BACKUP_DIR on a migrated
/// machine: neither tool replaces or prunes the other's dumps. The candidate
/// scan finds dumps by the project name and `.sql.gz`, which both match.
pub fn file_name(date: NaiveDate, tier: Tier) -> String {
    format!("{}{}", date.format("%Y-%m-%d"), tier.suffix())
}

/// One directory entry, as retention sees it.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: OsString,
    pub modified: SystemTime,
}

/// What the shell's glob `"$DEST"/*<suffix>` matched: the suffix, and no
/// leading dot.
fn in_tier(name: &OsStr, tier: Tier) -> bool {
    let name = name.to_string_lossy();
    name.ends_with(tier.suffix()) && !name.starts_with('.')
}

/// The entries retention removes, as the shell script chose them:
/// `ls -1t | tail -n +<keep+1>` -- newest modification time first, a tie in
/// name order (`ls` in the C locale), everything past the tier's count.
///
/// One deliberate difference: `protect`, the dump just written, counts as the
/// newest whatever its timestamp, so files dated in the future cannot rotate
/// out the backup that was just taken.
pub fn to_prune(entries: &[Entry], tier: Tier, protect: Option<&OsStr>) -> Vec<OsString> {
    let mut matched: Vec<&Entry> = entries.iter().filter(|e| in_tier(&e.name, tier)).collect();
    matched.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| a.name.cmp(&b.name))
    });
    if let Some(i) = protect.and_then(|p| matched.iter().position(|e| e.name == p)) {
        let just_written = matched.remove(i);
        matched.insert(0, just_written);
    }
    matched
        .into_iter()
        .skip(tier.keep())
        .map(|e| e.name.clone())
        .collect()
}

/// What retention removed, and what it could not.
#[derive(Debug, Default)]
pub struct Pruned {
    pub removed: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

/// Apply one tier's retention to a directory. Housekeeping: whatever cannot be
/// listed or removed becomes a warning, never a failure of the backup it
/// follows. Directories are never candidates.
pub fn prune(dir: &Path, tier: Tier, protect: Option<&OsStr>) -> Pruned {
    let mut pruned = Pruned::default();
    let listing = match fs::read_dir(dir) {
        Ok(listing) => listing,
        Err(e) => {
            pruned
                .warnings
                .push(format!("could not list {} to prune it: {e}", dir.display()));
            return pruned;
        }
    };
    let mut entries = Vec::new();
    for entry in listing {
        let read = entry.and_then(|e| {
            let meta = e.metadata()?;
            Ok((e.file_name(), meta.is_dir(), meta.modified()?))
        });
        match read {
            Ok((_, true, _)) => {}
            Ok((name, false, modified)) => entries.push(Entry { name, modified }),
            Err(e) => pruned.warnings.push(format!(
                "could not read an entry of {} to prune it: {e}",
                dir.display()
            )),
        }
    }
    for name in to_prune(&entries, tier, protect) {
        let path = dir.join(name);
        match fs::remove_file(&path) {
            Ok(()) => pruned.removed.push(path),
            Err(e) => pruned
                .warnings
                .push(format!("could not prune {}: {e}", path.display())),
        }
    }
    pruned
}

/// A finished backup.
#[derive(Debug)]
pub struct Done {
    /// The day's dump, absolute.
    pub path: PathBuf,
    /// The weekly copy, written on Mondays.
    pub weekly: Option<PathBuf>,
    pub dumped: Dumped,
    /// The size of the SQL inside the gzip.
    pub sql_bytes: u64,
    pub pruned: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

impl Done {
    /// The diagnostics, for stderr. The path is named here too, for a run
    /// whose stdout goes nowhere.
    pub fn lines(&self, cfg: &Config) -> Vec<String> {
        let tables: Vec<String> = self
            .dumped
            .tables
            .iter()
            .map(|(t, n)| format!("{t} {n}"))
            .collect();
        let mut lines = vec![
            format!(
                "backup: {} -> {}",
                cfg.db_path.display(),
                self.path.display()
            ),
            format!(
                "backup: {} rows in {} tables from one read snapshot, re-counted: {}",
                self.dumped.rows(),
                self.dumped.tables.len(),
                tables.join(", ")
            ),
            format!(
                "backup: gzip verified, {} bytes of SQL inside",
                self.sql_bytes
            ),
        ];
        if let Some(weekly) = &self.weekly {
            lines.push(format!("backup: weekly copy {}", weekly.display()));
        }
        for p in &self.pruned {
            lines.push(format!("backup: pruned {}", p.display()));
        }
        for w in &self.warnings {
            lines.push(format!("warning: {w}"));
        }
        lines
    }
}

/// Take a backup dated `today` (local time): dump, verify, move into place,
/// add the weekly copy on a Monday, then prune.
///
/// Nothing in BACKUP_DIR is replaced until the new dump is verified, and a
/// failure removes its partial file. Without a database, or with a file that
/// is not one, it refuses before opening or creating anything.
pub fn create(cfg: &Config, today: NaiveDate) -> Result<Done> {
    refuse_unless_database(&cfg.db_path)?;
    let store = TursoStore::open(cfg)?;
    let dir = std::path::absolute(&cfg.backup_dir).map_err(|e| {
        Error::failed(format!(
            "cannot resolve BACKUP_DIR {}: {e}",
            cfg.backup_dir.display()
        ))
    })?;
    fs::create_dir_all(&dir)
        .map_err(|e| Error::failed(format!("cannot create BACKUP_DIR {}: {e}", dir.display())))?;

    let daily = Partial(dir.join(format!(".backup-{}.partial", std::process::id())));
    let (dumped, sql_bytes) = write_dump(&store, &daily.0)?;
    verify(&daily.0, sql_bytes)?;
    let weekly = if today.weekday() == Weekday::Mon {
        let weekly = Partial(dir.join(format!(".backup-{}-weekly.partial", std::process::id())));
        fs::copy(&daily.0, &weekly.0).map_err(|e| {
            Error::failed(format!(
                "cannot write the weekly copy in {}: {e}",
                dir.display()
            ))
        })?;
        verify(&weekly.0, sql_bytes)?;
        Some(weekly)
    } else {
        None
    };

    let path = dir.join(file_name(today, Tier::Daily));
    daily.persist(&path)?;
    let weekly = match weekly {
        Some(partial) => {
            let to = dir.join(file_name(today, Tier::Weekly));
            partial.persist(&to)?;
            Some(to)
        }
        None => None,
    };

    let mut pruned = prune(&dir, Tier::Daily, path.file_name());
    let weekly_pruned = prune(
        &dir,
        Tier::Weekly,
        weekly.as_deref().and_then(Path::file_name),
    );
    pruned.removed.extend(weekly_pruned.removed);
    pruned.warnings.extend(weekly_pruned.warnings);

    Ok(Done {
        path,
        weekly,
        dumped,
        sql_bytes,
        pruned: pruned.removed,
        warnings: pruned.warnings,
    })
}

/// A missing database is a wrong path, never something to create. An empty
/// file -- what a stale process re-creating a renamed path leaves behind -- or
/// a foreign one is judged by its header and never opened, since opening it
/// would create files beside it. A dump of either would enter the rotation and
/// push the real dumps out.
fn refuse_unless_database(db: &Path) -> Result<()> {
    if !db.exists() {
        return Err(Error::refused(format!(
            "no database at {} — nothing to back up",
            db.display()
        )));
    }
    match crate::store::turso::not_a_database(db) {
        Ok(None) => Ok(()),
        Ok(Some(why)) => Err(Error::refused(format!(
            "nothing to back up: {} is {why}",
            db.display()
        ))),
        Err(e) => Err(Error::failed(format!("cannot read {}: {e}", db.display()))),
    }
}

/// A file being written beside the dumps, under a name neither retention nor
/// the candidate scan matches. Removed when dropped, unless it was moved into
/// place first.
struct Partial(PathBuf);

impl Partial {
    fn persist(self, to: &Path) -> Result<()> {
        fs::rename(&self.0, to).map_err(|e| {
            Error::failed(format!(
                "cannot move the verified dump to {}: {e}",
                to.display()
            ))
        })
    }
}

impl Drop for Partial {
    fn drop(&mut self) {
        // After a successful persist there is nothing left at this path.
        let _ = fs::remove_file(&self.0);
    }
}

/// Counts the bytes written through it: the length the gzip on disk must
/// decompress to.
struct Counted<W> {
    inner: W,
    bytes: u64,
}

impl<W: Write> Write for Counted<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.bytes += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Dump into a gzip at `path`, synced to disk. Returns what was dumped and the
/// size of the SQL.
fn write_dump(store: &TursoStore, path: &Path) -> Result<(Dumped, u64)> {
    let io = |e: std::io::Error| Error::failed(format!("cannot write {}: {e}", path.display()));
    let file = fs::File::create(path).map_err(io)?;
    let mut sql = Counted {
        inner: GzEncoder::new(BufWriter::new(file), Compression::default()),
        bytes: 0,
    };
    let dumped = store.dump(&mut sql)?;
    let file = sql
        .inner
        .finish()
        .and_then(|w| w.into_inner().map_err(|e| e.into_error()))
        .map_err(io)?;
    file.sync_all().map_err(io)?;
    Ok((dumped, sql.bytes))
}

/// Decompress the file to its end -- which checks the gzip trailer's CRC and
/// length -- and compare the result with the size of the SQL written.
fn verify(path: &Path, sql_bytes: u64) -> Result<()> {
    let fail = |why: String| {
        Error::failed(format!(
            "the dump written to {} failed verification: {why}",
            path.display()
        ))
    };
    let file = fs::File::open(path).map_err(|e| fail(e.to_string()))?;
    let n = std::io::copy(
        &mut GzDecoder::new(BufReader::new(file)),
        &mut std::io::sink(),
    )
    .map_err(|e| fail(e.to_string()))?;
    if n != sql_bytes {
        return Err(fail(format!(
            "it holds {n} bytes of SQL where {sql_bytes} were written"
        )));
    }
    Ok(())
}

/// Back up, then print the diagnostics to `err` and the path to `out`. Nothing
/// reaches `out` unless the dump is written, verified and in place.
pub fn run_to(
    cfg: &Config,
    today: NaiveDate,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<()> {
    let done = create(cfg, today)?;
    for line in done.lines(cfg) {
        let _ = writeln!(err, "{line}");
    }
    writeln!(out, "{}", done.path.display())
        .map_err(|e| Error::failed(format!("cannot print the dump's path: {e}")))
}

/// `backup`: stdout is the dump's path and nothing else; every other line goes
/// to stderr.
pub fn run(cfg: &Config) -> Result<()> {
    run_to(
        cfg,
        chrono::Local::now().date_naive(),
        &mut std::io::stdout().lock(),
        &mut std::io::stderr().lock(),
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Read;
    use std::path::Path;
    use std::time::{Duration, Instant, UNIX_EPOCH};

    use super::*;
    use crate::config::{DB_ENV, DotEnv, Env};
    use crate::error::{EXIT_ERROR, EXIT_REFUSED};
    use crate::store::Store;
    use crate::store::turso::TursoStore;
    use crate::store::turso::dump::tests::{batch, fixture, restore};
    use crate::store::turso::tests::scratch;

    const MONDAY: &str = "2026-10-26";
    const TUESDAY: &str = "2026-10-27";

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    /// The names in a directory, sorted; nothing when it does not exist.
    fn names(dir: &Path) -> Vec<String> {
        let mut out: Vec<String> = fs::read_dir(dir)
            .map(|rd| {
                rd.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        out.sort();
        out
    }

    fn gunzip(path: &Path) -> String {
        let mut text = String::new();
        flate2::read::GzDecoder::new(fs::File::open(path).unwrap())
            .read_to_string(&mut text)
            .unwrap();
        text
    }

    fn counts(cfg: &Config, tables: &[(String, u64)]) -> Vec<(String, u64)> {
        tables
            .iter()
            .map(|(t, _)| {
                let n = match &crate::store::turso::tests::rows(
                    cfg,
                    &format!("SELECT count(*) FROM \"{t}\""),
                )[0][0]
                {
                    crate::store::Cell::Int(n) => *n as u64,
                    other => panic!("{other:?}"),
                };
                (t.clone(), n)
            })
            .collect()
    }

    /// A config for the same database and home, resolved from these variables:
    /// the way the binary resolves them, not by setting fields.
    fn resolved(base: &Config, vars: &[(&str, &str)]) -> Config {
        let mut all = vec![
            (
                DB_ENV.to_string(),
                base.db_path.to_string_lossy().to_string(),
            ),
            ("OLLAMA_URL".to_string(), "http://127.0.0.1:9".to_string()),
        ];
        all.extend(vars.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        Config::from_env(&Env {
            vars: all.into_iter().collect(),
            home: base.home.clone(),
            windows: false,
            dotenv: DotEnv::absent(PathBuf::from(crate::config::tests::EXE)),
        })
        .unwrap()
    }

    // ---- names and retention -----------------------------------------------

    #[test]
    fn file_names_cannot_collide_with_the_python_script() {
        assert_eq!(
            file_name(day(MONDAY), Tier::Daily),
            "2026-10-26-supercharged-memory-native.sql.gz"
        );
        assert_eq!(
            file_name(day(MONDAY), Tier::Weekly),
            "2026-10-26-supercharged-memory-native-weekly.sql.gz"
        );
        assert_eq!(
            file_name(day("2026-01-05"), Tier::Daily),
            "2026-01-05-supercharged-memory-native.sql.gz"
        );
        for tier in [Tier::Daily, Tier::Weekly] {
            let native = file_name(day(MONDAY), tier);
            assert!(native.ends_with(".sql.gz"), "{native}");
            let python = match tier {
                Tier::Daily => "2026-10-26-supercharged-memory.sql.gz",
                Tier::Weekly => "2026-10-26-supercharged-memory-weekly.sql.gz",
            };
            assert_ne!(native, python);
        }
    }

    /// Retention only ever sees the dumps this binary wrote: the Python
    /// script's dumps, however many, are neither pruned nor counted against
    /// the native keep limits.
    #[test]
    fn retention_leaves_the_python_scripts_dumps_alone() {
        let mut fixtures = Vec::new();
        let mut names = Vec::new();
        for i in 1..=9 {
            names.push(format!("2026-10-0{i}-supercharged-memory.sql.gz"));
            names.push(format!("2026-09-0{i}-supercharged-memory-weekly.sql.gz"));
        }
        for (i, n) in names.iter().enumerate() {
            fixtures.push((n.as_str(), at(i as u64)));
        }
        let e = entries(&fixtures);
        assert!(to_prune(&e, Tier::Daily, None).is_empty());
        assert!(to_prune(&e, Tier::Weekly, None).is_empty());
    }

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_800_000_000 + secs)
    }

    /// A directory of dated dumps whose modification times do not follow their
    /// names, with a tie, an empty prefix, and the names the globs never match.
    fn dated_fixtures() -> Vec<(&'static str, SystemTime)> {
        vec![
            ("2026-10-20-supercharged-memory-native.sql.gz", at(1)),
            ("2026-10-21-supercharged-memory-native.sql.gz", at(2)),
            ("2026-10-22-supercharged-memory-native.sql.gz", at(3)),
            ("2026-10-23-supercharged-memory-native.sql.gz", at(3)),
            ("2026-01-01-supercharged-memory-native.sql.gz", at(9)),
            ("-supercharged-memory-native.sql.gz", at(0)),
            (".2026-10-19-supercharged-memory-native.sql.gz", at(0)),
            ("2026-10-18-SUPERCHARGED-MEMORY-NATIVE.SQL.GZ", at(0)),
            ("2026-10-24-supercharged-memory.sql", at(0)),
            ("2026-10-24-supercharged-memory-native.sql.gz.bak", at(0)),
            ("notes.txt", at(0)),
            (".backup-1234.partial", at(0)),
            ("2026-09-07-supercharged-memory-native-weekly.sql.gz", at(1)),
            ("2026-09-14-supercharged-memory-native-weekly.sql.gz", at(2)),
            ("2026-09-21-supercharged-memory-native-weekly.sql.gz", at(3)),
            ("2026-09-28-supercharged-memory-native-weekly.sql.gz", at(4)),
            ("2026-10-05-supercharged-memory-native-weekly.sql.gz", at(5)),
            ("2026-10-12-supercharged-memory-native-weekly.sql.gz", at(6)),
        ]
    }

    fn entries(fixtures: &[(&str, SystemTime)]) -> Vec<Entry> {
        fixtures
            .iter()
            .map(|(n, t)| Entry {
                name: OsString::from(n),
                modified: *t,
            })
            .collect()
    }

    fn strings(names: Vec<OsString>) -> Vec<String> {
        names
            .into_iter()
            .map(|n| n.to_string_lossy().into_owned())
            .collect()
    }

    /// Newest three daily and newest four weekly by modification time, a tie
    /// broken by name; only names the shell globs match are candidates.
    #[test]
    fn retention_keeps_the_newest_three_daily_and_four_weekly_by_mtime() {
        let e = entries(&dated_fixtures());
        assert_eq!(
            strings(to_prune(&e, Tier::Daily, None)),
            vec![
                "2026-10-21-supercharged-memory-native.sql.gz",
                "2026-10-20-supercharged-memory-native.sql.gz",
                "-supercharged-memory-native.sql.gz",
            ]
        );
        assert_eq!(
            strings(to_prune(&e, Tier::Weekly, None)),
            vec![
                "2026-09-14-supercharged-memory-native-weekly.sql.gz",
                "2026-09-07-supercharged-memory-native-weekly.sql.gz",
            ]
        );
    }

    /// The Python script and this module share BACKUP_DIR without touching
    /// each other's dumps: on one directory holding both name families, the
    /// script's own prune lines (run by bash) remove only Python-named files and
    /// this module's retention removes only native-named ones.
    #[cfg(unix)]
    #[test]
    fn retention_and_the_shell_script_each_prune_only_their_own_dumps() {
        let script = fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/scripts/supercharged-memory-backup.sh"
        ))
        .unwrap();
        let prune_lines: Vec<&str> = script
            .lines()
            .filter(|l| l.starts_with("ls -1t \"$DEST\""))
            .collect();
        assert_eq!(prune_lines.len(), 2, "{prune_lines:?}");

        let native = dated_fixtures();
        let python: Vec<(String, SystemTime)> = native
            .iter()
            .map(|(n, t)| {
                (
                    n.replace("-supercharged-memory-native", "-supercharged-memory"),
                    *t,
                )
            })
            .collect();
        let base = scratch("backup-parity").db_path.with_file_name("parity");
        let dir = base.join("mixed");
        fs::create_dir_all(&dir).unwrap();
        for (name, t) in native
            .iter()
            .map(|(n, t)| (n.to_string(), *t))
            .chain(python.iter().cloned())
        {
            let f = fs::File::create(dir.join(name)).unwrap();
            f.set_modified(t).unwrap();
        }
        let before = names(&dir);

        prune(&dir, Tier::Daily, None);
        prune(&dir, Tier::Weekly, None);
        let after_ours = names(&dir);
        let removed_by_us: Vec<&String> =
            before.iter().filter(|n| !after_ours.contains(n)).collect();
        assert!(!removed_by_us.is_empty());
        assert!(
            removed_by_us.iter().all(|n| n.contains("-native")),
            "{removed_by_us:?}"
        );

        let status = std::process::Command::new("bash")
            .arg("-c")
            .arg(prune_lines.join("\n"))
            .env("DEST", &dir)
            .env("LC_ALL", "C")
            .status()
            .unwrap();
        assert!(status.success());
        let after_both = names(&dir);
        let removed_by_script: Vec<&String> = after_ours
            .iter()
            .filter(|n| !after_both.contains(n))
            .collect();
        assert!(!removed_by_script.is_empty());
        assert!(
            removed_by_script.iter().all(|n| !n.contains("-native")),
            "{removed_by_script:?}"
        );
    }

    /// A dump with an older modification time than three others -- a clock
    /// that went back, or files from the future -- is still kept: it is the
    /// one just written.
    #[test]
    fn the_dump_just_written_is_never_pruned() {
        let e = entries(&[
            ("2026-10-27-supercharged-memory-native.sql.gz", at(1)),
            ("2027-01-01-supercharged-memory-native.sql.gz", at(10)),
            ("2027-01-02-supercharged-memory-native.sql.gz", at(11)),
            ("2027-01-03-supercharged-memory-native.sql.gz", at(12)),
        ]);
        assert_eq!(
            strings(to_prune(
                &e,
                Tier::Daily,
                Some(OsStr::new("2026-10-27-supercharged-memory-native.sql.gz"))
            )),
            vec!["2027-01-01-supercharged-memory-native.sql.gz"]
        );
    }

    /// Retention is housekeeping: a dump it cannot remove is reported, and the
    /// backup it follows still stands.
    #[cfg(unix)]
    #[test]
    fn a_dump_that_cannot_be_pruned_is_a_warning() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("backup-prune-denied")
            .db_path
            .with_file_name("backups");
        fs::create_dir_all(&dir).unwrap();
        for i in 1..=5 {
            let f = fs::File::create(
                dir.join(format!("2026-10-2{i}-supercharged-memory-native.sql.gz")),
            )
            .unwrap();
            f.set_modified(at(i)).unwrap();
        }
        let before = names(&dir);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
        let pruned = prune(&dir, Tier::Daily, None);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        if pruned.warnings.is_empty() && names(&dir) != before {
            eprintln!("a read-only directory does not stop removal here (root?): not exercised");
            return;
        }
        assert!(pruned.removed.is_empty(), "{pruned:?}");
        assert_eq!(pruned.warnings.len(), 2, "{pruned:?}");
        assert!(pruned.warnings[0].contains("2026-10-22"), "{pruned:?}");
        assert_eq!(names(&dir), before);
    }

    // ---- the backup --------------------------------------------------------

    /// The dump lands under its day's name, alone (no partial file beside it),
    /// and restores to the same row counts as the database it came from.
    #[test]
    fn a_backup_writes_a_dump_that_restores_with_matching_counts() {
        let cfg = fixture("backup-restores");
        let done = create(&cfg, day(TUESDAY)).unwrap();
        assert_eq!(
            done.path,
            cfg.backup_dir
                .join("2026-10-27-supercharged-memory-native.sql.gz")
        );
        assert!(done.path.is_absolute());
        assert_eq!(done.weekly, None);
        assert_eq!(
            names(&cfg.backup_dir),
            vec!["2026-10-27-supercharged-memory-native.sql.gz"]
        );
        let sql = gunzip(&done.path);
        assert_eq!(done.sql_bytes, sql.len() as u64);
        assert!(done.dumped.tables.iter().any(|(t, _)| t == "instructions"));

        let restored = restore(&cfg, &sql, "restored");
        assert_eq!(counts(&restored, &done.dumped.tables), done.dumped.tables);
        assert_eq!(counts(&cfg, &done.dumped.tables), done.dumped.tables);
    }

    #[test]
    fn monday_adds_the_weekly_copy_and_other_days_do_not() {
        let cfg = fixture("backup-weekly");
        let done = create(&cfg, day(MONDAY)).unwrap();
        let weekly = done.weekly.clone().expect("a weekly copy on Monday");
        assert_eq!(
            weekly,
            cfg.backup_dir
                .join("2026-10-26-supercharged-memory-native-weekly.sql.gz")
        );
        assert_eq!(fs::read(&weekly).unwrap(), fs::read(&done.path).unwrap());

        let done = create(&cfg, day(TUESDAY)).unwrap();
        assert_eq!(done.weekly, None);
        assert_eq!(
            names(&cfg.backup_dir),
            vec![
                "2026-10-26-supercharged-memory-native-weekly.sql.gz",
                "2026-10-26-supercharged-memory-native.sql.gz",
                "2026-10-27-supercharged-memory-native.sql.gz",
            ]
        );
    }

    /// The shell script failed here after 25 seconds of retries, because a
    /// dump without an INSERT looked broken to it.
    #[test]
    fn an_empty_database_backs_up_and_restores_empty() {
        let cfg = scratch("backup-empty");
        let done = create(&cfg, day(TUESDAY)).unwrap();
        assert_eq!(done.dumped.rows(), 0);
        let sql = gunzip(&done.path);
        assert!(!sql.contains("INSERT INTO"), "{sql}");
        let restored = restore(&cfg, &sql, "restored");
        assert_eq!(
            TursoStore::open(&restored)
                .unwrap()
                .total_memories()
                .unwrap(),
            0
        );
        assert_eq!(counts(&restored, &done.dumped.tables), done.dumped.tables);
    }

    /// A second run on the same day replaces that day's dump with the newer
    /// state of the database.
    #[test]
    fn a_rerun_on_the_same_day_replaces_that_days_dump() {
        let cfg = fixture("backup-rerun");
        create(&cfg, day(TUESDAY)).unwrap();
        batch(
            &cfg,
            "INSERT INTO episodic_memory (event_type, importance, memory_text) \
             VALUES ('note', 'routine', 'after the first backup');",
        );
        let done = create(&cfg, day(TUESDAY)).unwrap();
        assert!(gunzip(&done.path).contains("after the first backup"));
        assert_eq!(
            names(&cfg.backup_dir),
            vec!["2026-10-27-supercharged-memory-native.sql.gz"]
        );
    }

    #[test]
    fn the_backup_only_reads_the_database_file() {
        let cfg = fixture("backup-reader");
        let bytes = fs::read(&cfg.db_path).unwrap();
        let mtime = fs::metadata(&cfg.db_path).unwrap().modified().unwrap();
        create(&cfg, day(TUESDAY)).unwrap();
        assert_eq!(fs::read(&cfg.db_path).unwrap(), bytes);
        assert_eq!(
            fs::metadata(&cfg.db_path).unwrap().modified().unwrap(),
            mtime
        );
    }

    // ---- refusals and failures ---------------------------------------------

    fn assert_refused(r: Result<Done>, needle: &str) {
        let err = r.expect_err("refused");
        assert_eq!(err.exit_code(), EXIT_REFUSED, "{err}");
        let msg = err.to_string();
        assert!(msg.starts_with("refused: "), "{msg}");
        assert!(msg.contains(needle), "{msg}");
    }

    /// No database, nothing to back up -- and nothing created on the way: not
    /// the database, not the backup directory.
    #[test]
    fn a_missing_database_is_refused_and_nothing_is_created() {
        let mut cfg = scratch("backup-missing");
        cfg.db_path = cfg.db_path.with_file_name("not-there.db");
        assert_refused(create(&cfg, day(TUESDAY)), "not-there.db");
        assert!(!cfg.db_path.exists());
        assert!(!cfg.backup_dir.exists());
    }

    /// An empty file is what a stale process re-creating a renamed path leaves
    /// behind. Backing it up would put a dump of nothing into the rotation and,
    /// three days later, have pushed out every real one. It is judged by its
    /// header, never opened: opening it would create files beside it.
    #[test]
    fn an_empty_or_foreign_file_is_refused_without_being_opened() {
        for (name, content, needle) in [
            ("empty", &b""[..], "empty file"),
            (
                "foreign",
                &b"this is not sqlite at all"[..],
                "not an SQLite database",
            ),
        ] {
            let mut cfg = scratch(&format!("backup-{name}-file"));
            cfg.db_path = cfg.db_path.with_file_name(format!("{name}.db"));
            fs::write(&cfg.db_path, content).unwrap();
            let dir = cfg.db_path.parent().unwrap().to_path_buf();
            let before = names(&dir);
            assert_refused(create(&cfg, day(TUESDAY)), needle);
            assert_eq!(names(&dir), before, "{name}: something was created");
            assert_eq!(fs::read(&cfg.db_path).unwrap(), content);
            assert!(!cfg.backup_dir.exists());
        }
    }

    /// A database with no tables at all is not a memory database, and a dump
    /// of it would rotate the real dumps out. It fails, and the dump already
    /// on disk for that day is left exactly as it was: nothing is replaced
    /// before the new dump has been verified.
    #[test]
    fn a_database_without_tables_fails_and_replaces_nothing() {
        let mut cfg = scratch("backup-no-tables");
        cfg.db_path = cfg.db_path.with_file_name("tableless.db");
        let p = cfg.db_path.to_string_lossy().to_string();
        pollster::block_on(async {
            let db = turso::Builder::new_local(&p)
                .experimental_multiprocess_wal(true)
                .build()
                .await
                .unwrap();
            db.connect()
                .unwrap()
                .execute_batch("CREATE TABLE t (x); DROP TABLE t;")
                .await
                .unwrap();
        });
        fs::create_dir_all(&cfg.backup_dir).unwrap();
        let previous = cfg
            .backup_dir
            .join("2026-10-27-supercharged-memory-native.sql.gz");
        fs::write(&previous, b"yesterday's good dump").unwrap();

        let err = create(&cfg, day(TUESDAY)).unwrap_err();
        assert_eq!(err.exit_code(), EXIT_ERROR, "{err}");
        assert!(err.to_string().contains("no tables"), "{err}");
        assert_eq!(fs::read(&previous).unwrap(), b"yesterday's good dump");
        assert_eq!(
            names(&cfg.backup_dir),
            vec!["2026-10-27-supercharged-memory-native.sql.gz"],
            "no partial file left behind"
        );
    }

    /// `TURSO_VFS` reaches the backup through the resolver every command
    /// shares. The matrix is the one that caught the shell script's drift; a
    /// value that can never work fails at once, without a retry.
    #[test]
    fn turso_vfs_reaches_the_backup_through_the_shared_resolver() {
        let base = fixture("backup-vfs");
        let works: &[Option<&str>] = &[
            None,
            Some(""),
            Some("   "),
            Some("NoNe"),
            Some(" none "),
            Some("\tNONE\n"),
        ];
        for raw in works {
            let vars: Vec<(&str, &str)> = raw.iter().map(|v| ("TURSO_VFS", *v)).collect();
            let cfg = resolved(&base, &vars);
            assert_eq!(cfg.vfs, None, "{raw:?}");
            let done = create(&cfg, day(TUESDAY));
            assert!(done.is_ok(), "TURSO_VFS={raw:?}: {done:?}");
        }

        let dir = base.backup_dir.clone();
        fs::remove_dir_all(&dir).unwrap();
        for (raw, needle) in [("bogus", "bogus"), (" Bogus ", "Bogus")] {
            let cfg = resolved(&base, &[("TURSO_VFS", raw)]);
            let started = Instant::now();
            let err = create(&cfg, day(TUESDAY)).unwrap_err();
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "a dead failure was retried: {:?}",
                started.elapsed()
            );
            assert_eq!(err.exit_code(), EXIT_ERROR, "{err}");
            assert!(err.to_string().contains(needle), "{err}");
            assert!(names(&dir).is_empty(), "{raw:?}: {:?}", names(&dir));
        }

        // The in-memory backend opens nothing on disk: an empty dump of it
        // would look like a backup and be none. Turso refuses multiprocess WAL
        // on it, so it fails at the open.
        let cfg = resolved(&base, &[("TURSO_VFS", "memory")]);
        let err = create(&cfg, day(TUESDAY)).unwrap_err();
        assert_eq!(err.exit_code(), EXIT_ERROR, "{err}");
        assert!(names(&dir).is_empty(), "{:?}", names(&dir));
    }

    /// The gzip on disk is decompressed to the end -- its CRC checked -- and
    /// must hold exactly the SQL that was written.
    #[test]
    fn verify_rejects_a_corrupt_truncated_or_short_gzip() {
        let dir = scratch("backup-verify").db_path.with_file_name("gz");
        fs::create_dir_all(&dir).unwrap();
        let sql = "BEGIN TRANSACTION;\nCOMMIT;\n".repeat(50);
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(sql.as_bytes()).unwrap();
        let good = enc.finish().unwrap();
        let n = sql.len() as u64;

        let path = dir.join("good.gz");
        fs::write(&path, &good).unwrap();
        assert!(verify(&path, n).is_ok());

        let mut bad: Vec<(&str, Vec<u8>, u64)> = vec![
            ("short of what was written", good.clone(), n + 1),
            ("truncated", good[..good.len() - 4].to_vec(), n),
            ("not gzip", b"plain text".to_vec(), n),
        ];
        let mut flipped = good.clone();
        let mid = flipped.len() / 2;
        flipped[mid] ^= 0xff;
        bad.push(("corrupted", flipped, n));
        for (what, bytes, expect) in bad {
            let p = dir.join(format!("{}.gz", what.replace(' ', "-")));
            fs::write(&p, &bytes).unwrap();
            let err = verify(&p, expect).expect_err(what);
            assert_eq!(err.exit_code(), EXIT_ERROR, "{what}: {err}");
        }
    }

    // ---- what is printed where ----------------------------------------------

    /// stdout is the path, one line, absolute, and nothing else; the rest is on
    /// stderr. On failure stdout stays empty, so `dump=$(… backup)` can never
    /// capture a diagnostic as a file name.
    #[test]
    fn stdout_is_the_path_alone_and_empty_on_failure() {
        let cfg = fixture("backup-stdout");
        let (mut out, mut err) = (Vec::new(), Vec::new());
        run_to(&cfg, day(MONDAY), &mut out, &mut err).unwrap();
        let path = cfg
            .backup_dir
            .join("2026-10-26-supercharged-memory-native.sql.gz");
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!("{}\n", path.display())
        );
        let err = String::from_utf8(err).unwrap();
        assert!(err.contains("semantic_memory"), "{err}");
        assert!(err.contains("weekly"), "{err}");

        let mut missing = cfg.clone();
        missing.db_path = cfg.db_path.with_file_name("gone.db");
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert!(run_to(&missing, day(TUESDAY), &mut out, &mut err).is_err());
        assert!(out.is_empty(), "{}", String::from_utf8_lossy(&out));
    }

    #[test]
    fn the_diagnostics_name_the_tables_the_copy_and_each_pruned_file() {
        let cfg = scratch("backup-lines");
        let done = Done {
            path: PathBuf::from("/b/2026-10-26-supercharged-memory-native.sql.gz"),
            weekly: Some(PathBuf::from(
                "/b/2026-10-26-supercharged-memory-native-weekly.sql.gz",
            )),
            dumped: Dumped {
                tables: vec![
                    ("semantic_memory".into(), 12),
                    ("episodic_memory".into(), 3),
                ],
            },
            sql_bytes: 4096,
            pruned: vec![PathBuf::from(
                "/b/2026-10-20-supercharged-memory-native.sql.gz",
            )],
            warnings: vec!["could not remove /b/x: denied".into()],
        };
        let text = done.lines(&cfg).join("\n");
        for want in [
            cfg.db_path.display().to_string(),
            // Named on stderr too, for a run whose stdout goes nowhere.
            "/b/2026-10-26-supercharged-memory-native.sql.gz".into(),
            "15 rows".into(),
            "semantic_memory 12".into(),
            "episodic_memory 3".into(),
            "/b/2026-10-26-supercharged-memory-native-weekly.sql.gz".into(),
            "pruned /b/2026-10-20-supercharged-memory-native.sql.gz".into(),
            "warning: could not remove /b/x: denied".into(),
        ] {
            assert!(text.contains(&want), "{want:?} not in:\n{text}");
        }
    }
}
