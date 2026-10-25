//! `restore`: a dump into a fresh database file, counted table by table
//! against the dump before the file takes its name.
//!
//! This replaces `scripts/restore.py`, which split the dump on `;` outside
//! quotes -- and so cut every trigger at the `;` inside its body -- and ran each
//! statement in its own tursodb process. Here the engine's own parser splits
//! the dump and one connection replays it (see `store::turso::restore`).
//!
//! The restore is built beside the target under a hidden name, counted, and
//! only then moved onto the target. A failed restore leaves nothing at the
//! target, and `--force` replaces an existing file only with a verified one.

use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;

use crate::config::{Config, DB_ENV};
use crate::error::{Error, Result};
use crate::store::turso::TursoStore;
use crate::store::turso::restore::{Failure, parse};

/// The files the engine keeps beside a database. A stale one next to a new
/// database would be read as part of it.
pub const SIDECARS: [&str; 3] = ["-wal", "-shm", "-tshm"];

/// How many rejected statements the report lists one by one.
const FAILURES_LISTED: usize = 5;

#[derive(Debug, Clone)]
pub struct Args {
    /// The dump to restore; `None` means the newest in BACKUP_DIR.
    pub dump: Option<PathBuf>,
    /// The database file to create.
    pub out: PathBuf,
    /// Replace an existing file at `out` once the restore is verified.
    pub force: bool,
}

/// One table: the rows the dump holds for it, and what the restore counted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Counted {
    pub table: String,
    pub in_dump: u64,
    /// The rows counted in the restored file, or why they could not be.
    pub restored: std::result::Result<u64, String>,
}

/// What a restore did, complete or not.
#[derive(Debug, Default)]
pub struct Report {
    pub dump: PathBuf,
    /// The dump was not named: it is the newest in BACKUP_DIR.
    pub newest: bool,
    pub target: PathBuf,
    /// Statements run.
    pub statements: usize,
    /// Statements left out because they write the engine's own bookkeeping.
    pub skipped: usize,
    pub tables: Vec<Counted>,
    pub failures: Vec<Failure>,
    /// Why the dump itself is incomplete: a damaged gzip, a statement cut
    /// off, a transaction never committed.
    pub problems: Vec<String>,
}

impl Report {
    /// Every statement ran, the dump was whole, and every table holds what
    /// the dump says it does.
    pub fn complete(&self) -> bool {
        self.failures.is_empty() && self.problems.is_empty() && self.mismatched() == 0
    }

    fn mismatched(&self) -> usize {
        self.tables
            .iter()
            .filter(|c| c.restored != Ok(c.in_dump))
            .count()
    }

    /// The report: the dump, the target, every table's two counts, every
    /// failure and problem, and the verdict when the restore is complete.
    pub fn lines(&self, cfg: &Config) -> Vec<String> {
        let rows: u64 = self.tables.iter().map(|c| c.in_dump).sum();
        let mut lines = vec![
            format!(
                "dump   : {}{}",
                self.dump.display(),
                if self.newest {
                    " (newest in BACKUP_DIR)"
                } else {
                    ""
                }
            ),
            format!("target : {}", self.target.display()),
            format!(
                "{} statements, {rows} rows across {} tables",
                self.statements,
                self.tables.len()
            ),
        ];
        if self.skipped > 0 {
            lines.push(format!(
                "skipped {} statements on tursodb-internal AUTOINCREMENT sequence tables \
                 (the engine recreates them with their tables)",
                self.skipped
            ));
        }

        let width = self
            .tables
            .iter()
            .map(|c| c.table.chars().count())
            .chain([20])
            .max()
            .unwrap_or(20);
        lines.push(String::new());
        lines.push(format!(
            "{:<width$}  {:>8}  {:>8}",
            "table", "in dump", "restored"
        ));
        for c in &self.tables {
            let restored = match &c.restored {
                Ok(n) => n.to_string(),
                Err(why) => why.clone(),
            };
            let mark = if c.restored == Ok(c.in_dump) {
                ""
            } else {
                "  <<< MISMATCH"
            };
            lines.push(format!(
                "{:<width$}  {:>8}  {:>8}{mark}",
                c.table, c.in_dump, restored
            ));
        }

        for f in self.failures.iter().take(FAILURES_LISTED) {
            lines.push(format!("FAILED statement {}: {}", f.statement, f.error));
            lines.push(format!("  {}", f.excerpt));
        }
        if self.failures.len() > FAILURES_LISTED {
            lines.push(format!(
                "… and {} more failed statements",
                self.failures.len() - FAILURES_LISTED
            ));
        }
        for p in &self.problems {
            lines.push(format!("problem: {p}"));
        }

        if self.complete() {
            lines.push(format!(
                "RESTORE OK — every row in the dump is present in {}",
                self.target.display()
            ));
            lines.push(in_use(cfg, &self.target));
        }
        lines
    }
}

/// Whether the restored file is the database the binary uses -- and if not,
/// what would make it so. Switching is the user's decision.
fn in_use(cfg: &Config, target: &Path) -> String {
    let configured = std::path::absolute(&cfg.db_path).unwrap_or_else(|_| cfg.db_path.clone());
    if configured == target {
        return format!(
            "This is the configured database ({}).",
            cfg.db_source_label()
        );
    }
    format!(
        "Not in use yet: the configured database is still {} ({}). To use the restored one, \
         set {DB_ENV}={} in {} (and re-point the turso MCP server, if one is registered) — \
         ask the user first.",
        cfg.db_path.display(),
        cfg.db_source_label(),
        target.display(),
        cfg.dotenv_path.display()
    )
}

/// Restore `args.dump` (or the newest dump) into `args.out`.
///
/// Refused, with nothing created: `out` is the configured database (even with
/// `--force`), `out` or one of its sidecars exists without `--force`, `out` is
/// a directory or its directory does not exist, no dump is found, or the dump
/// is not one whose row counts can be read. An incomplete restore is returned
/// as a report that is not `complete()`, and nothing is left at `out`.
pub fn restore(cfg: &Config, args: &Args) -> Result<Report> {
    let target = std::path::absolute(&args.out)
        .map_err(|e| Error::failed(format!("cannot resolve --out {}: {e}", args.out.display())))?;
    check_target(cfg, &target, args.force)?;
    let (dump_path, newest) = match &args.dump {
        Some(path) if present(path) => (path.clone(), false),
        Some(path) => {
            return Err(Error::refused(format!(
                "no dump at {} — nothing was restored",
                path.display()
            )));
        }
        None => (newest_dump(cfg)?, true),
    };
    let (text, damage) = read_dump(&dump_path)?;
    let dump = parse(&text)?;

    let scratch = Scratch::create(&target)?;
    let at_scratch = Config {
        db_path: scratch.0.clone(),
        ..cfg.clone()
    };
    let store = TursoStore::open(&at_scratch)?;
    let replayed = store.replay(&dump)?;
    store.checkpoint()?;
    drop(store);
    // Counted through a connection of its own: what is in the file, not what
    // the replaying connection believes.
    let store = TursoStore::open(&at_scratch)?;
    let tables = dump
        .tables
        .iter()
        .map(|(table, in_dump)| Counted {
            table: table.clone(),
            in_dump: *in_dump,
            restored: store.count_rows(table),
        })
        .collect();
    drop(store);

    let mut problems: Vec<String> = damage.into_iter().chain(dump.unreadable).collect();
    if replayed.uncommitted {
        problems.push(
            "the dump opened a transaction it never committed — it is cut off, so none of it \
             was kept"
                .to_string(),
        );
    }
    let report = Report {
        dump: dump_path,
        newest,
        target,
        statements: dump.statements.len(),
        skipped: dump.skipped,
        tables,
        failures: replayed.failures,
        problems,
    };
    if report.complete() {
        wal_is_empty(&scratch.0)?;
        scratch.persist(&report.target, args.force)?;
    }
    Ok(report)
}

/// Something is at this path: a file, a directory, or a symlink, even one
/// that leads nowhere.
fn present(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// A database's sidecar: its path with the suffix appended.
fn sidecar(db: &Path, suffix: &str) -> PathBuf {
    let mut name = OsString::from(db.as_os_str());
    name.push(suffix);
    PathBuf::from(name)
}

/// The target must be a file name in an existing directory, must not be the
/// database in use, and must be free -- with its sidecars -- unless `force`.
fn check_target(cfg: &Config, target: &Path, force: bool) -> Result<()> {
    if target.is_dir() || target.file_name().is_none() {
        return Err(Error::refused(format!(
            "{} is a directory — --out names the database file to create",
            target.display()
        )));
    }
    // The database in use, or one of its sidecars: its log holds committed
    // rows the main file does not have yet.
    let in_use = std::iter::once(cfg.db_path.clone())
        .chain(SIDECARS.iter().map(|s| sidecar(&cfg.db_path, s)))
        .filter_map(|p| fs::canonicalize(p).ok())
        .collect::<Vec<_>>();
    if fs::canonicalize(target).is_ok_and(|t| in_use.contains(&t)) {
        return Err(Error::refused(format!(
            "{} is the configured database or one of its files ({}) — a restore never \
             replaces the database in use, not even with --force. Restore to a new path, \
             then point {DB_ENV} at it.",
            target.display(),
            cfg.db_source_label()
        )));
    }
    // A path with a file name always has a parent.
    let dir = target.parent().unwrap_or(target);
    if !dir.is_dir() {
        return Err(Error::refused(format!(
            "the directory {} does not exist — nothing was created",
            dir.display()
        )));
    }
    if !force {
        let taken = std::iter::once(target.to_path_buf())
            .chain(SIDECARS.iter().map(|s| sidecar(target, s)))
            .find(|p| present(p));
        if let Some(p) = taken {
            return Err(Error::refused(format!(
                "{} already exists — restoring over a database loses everything written since \
                 the backup. Move it aside first, or pass --force to replace it once the \
                 restore is verified.",
                p.display()
            )));
        }
    }
    Ok(())
}

/// The newest dump in BACKUP_DIR, by the names the backup writes.
fn newest_dump(cfg: &Config) -> Result<PathBuf> {
    crate::store::candidates::find_backups(&cfg.backup_dir)
        .into_iter()
        .next()
        .ok_or_else(|| {
            Error::refused(format!(
                "no dump given and none found in BACKUP_DIR {} (looked for \
                 *supercharged-memory*.sql.gz) — pass --dump",
                cfg.backup_dir.display()
            ))
        })
}

/// The dump's text, gunzipped when it is a gzip, and why it may be cut off:
/// a gzip that ends early or fails its check, or text that stops in the
/// middle of a character. What could be read is kept, so the restore can
/// show what the cut-off dump holds.
fn read_dump(path: &Path) -> Result<(String, Option<String>)> {
    let bytes = fs::read(path)
        .map_err(|e| Error::failed(format!("cannot read the dump {}: {e}", path.display())))?;
    let (bytes, mut damage) = if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut sql = Vec::new();
        match GzDecoder::new(&bytes[..]).read_to_end(&mut sql) {
            Ok(_) => (sql, None),
            Err(e) => {
                let why = format!(
                    "the gzip is cut off or damaged ({e}); only the {} bytes of SQL before \
                     that point could be read",
                    sql.len()
                );
                (sql, Some(why))
            }
        }
    } else {
        (bytes, None)
    };
    match String::from_utf8(bytes) {
        Ok(text) => Ok((text, damage)),
        Err(e) if damage.is_some() || e.utf8_error().error_len().is_none() => {
            let valid = e.utf8_error().valid_up_to();
            damage.get_or_insert_with(|| {
                format!("the dump ends in the middle of a character at byte {valid}: it is cut off")
            });
            let mut bytes = e.into_bytes();
            bytes.truncate(valid);
            let text = String::from_utf8(bytes).expect("a valid UTF-8 prefix");
            Ok((text, damage))
        }
        Err(e) => Err(Error::refused(format!(
            "{} is not a SQL dump: it is not UTF-8 text (byte {}) — nothing was restored",
            path.display(),
            e.utf8_error().valid_up_to()
        ))),
    }
}

/// The restore being built, beside the target under a hidden name that
/// neither the backup scan nor the candidate search matches. Removed with its
/// sidecars when dropped; after a successful move only the sidecars are left
/// to remove.
struct Scratch(PathBuf);

impl Scratch {
    fn create(target: &Path) -> Result<Scratch> {
        let name = target.file_name().unwrap_or_default().to_string_lossy();
        let path = target.with_file_name(format!(".{name}.restore-{}", std::process::id()));
        // Sidecars of a killed run that had this process id, before the file
        // is claimed: a stale log beside it would be read as part of it.
        for s in SIDECARS {
            let _ = fs::remove_file(sidecar(&path, s));
        }
        fs::File::create_new(&path).map_err(|e| {
            Error::failed(format!(
                "cannot create {} to restore into: {e}",
                path.display()
            ))
        })?;
        Ok(Scratch(path))
    }

    /// Move the verified restore onto the target. With `force`, the target's
    /// own sidecars go first: left beside the new file, a stale log would be
    /// read as part of it. Without, the target must still be free.
    fn persist(self, target: &Path, force: bool) -> Result<()> {
        for p in SIDECARS.iter().map(|s| sidecar(target, s)) {
            if !present(&p) {
                continue;
            }
            if !force {
                return Err(Error::failed(format!(
                    "{} appeared while the restore ran — nothing was written",
                    p.display()
                )));
            }
            fs::remove_file(&p).map_err(|e| {
                Error::failed(format!(
                    "cannot remove the stale {}: {e} — nothing was written",
                    p.display()
                ))
            })?;
        }
        if !force && present(target) {
            return Err(Error::failed(format!(
                "{} appeared while the restore ran — nothing was written",
                target.display()
            )));
        }
        fs::rename(&self.0, target).map_err(|e| {
            Error::failed(format!(
                "cannot move the verified restore to {}: {e}",
                target.display()
            ))
        })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
        for s in SIDECARS {
            let _ = fs::remove_file(sidecar(&self.0, s));
        }
    }
}

/// The write-ahead log beside `db` holds nothing, so the main file alone is
/// the whole database and can be moved.
pub fn wal_is_empty(db: &Path) -> Result<()> {
    let wal = sidecar(db, "-wal");
    match fs::metadata(&wal) {
        Ok(m) if m.len() > 0 => Err(Error::failed(format!(
            "{} still holds {} bytes after the checkpoint, so the restored database is not in \
             its main file alone — it was not moved into place",
            wal.display(),
            m.len()
        ))),
        _ => Ok(()),
    }
}

/// Restore, print the report to `out`, and fail unless the restore is
/// complete.
pub fn run_to(cfg: &Config, args: &Args, out: &mut dyn Write) -> Result<()> {
    let report = restore(cfg, args)?;
    for line in report.lines(cfg) {
        writeln!(out, "{line}")
            .map_err(|e| Error::failed(format!("cannot print the report: {e}")))?;
    }
    if report.complete() {
        return Ok(());
    }
    Err(Error::failed(format!(
        "RESTORE INCOMPLETE — {} failed statements, {} of {} tables do not match the dump{}; \
         nothing was written to {}. Do not use a partial restore.",
        report.failures.len(),
        report.mismatched(),
        report.tables.len(),
        if report.problems.is_empty() {
            ""
        } else {
            ", and the dump itself is cut off or damaged"
        },
        report.target.display()
    )))
}

/// `restore`: the report on stdout; a refusal or an incomplete restore on
/// stderr, with a non-zero exit.
pub fn run(cfg: &Config, args: &Args) -> Result<()> {
    run_to(cfg, args, &mut std::io::stdout().lock())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Read;
    use std::time::{Duration, SystemTime};

    use chrono::NaiveDate;

    use super::*;
    use crate::config::DB_ENV;
    use crate::error::{EXIT_ERROR, EXIT_REFUSED};
    use crate::store::Cell;
    use crate::store::Table;
    use crate::store::turso::dump::tests::{batch, dump_text, fixture};
    use crate::store::turso::restore::tests::{
        TURSODB_DUMP, quarters, tenths, tursodb_dump_tables,
    };
    use crate::store::turso::tests::{distance, rows, scratch};

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

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(bytes).unwrap();
        enc.finish().unwrap()
    }

    /// A directory of its own beside the configured database, for the
    /// restored file -- so "nothing was created" is a listing that stays empty.
    fn target_dir(cfg: &Config) -> PathBuf {
        let dir = cfg.db_path.with_file_name("restored");
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_dump(cfg: &Config, name: &str, bytes: &[u8]) -> PathBuf {
        let path = cfg.db_path.with_file_name(name);
        fs::write(&path, bytes).unwrap();
        path
    }

    fn args(dump: Option<&Path>, out: &Path, force: bool) -> Args {
        Args {
            dump: dump.map(Path::to_path_buf),
            out: out.to_path_buf(),
            force,
        }
    }

    /// The same configuration, pointed at another database file.
    fn at(cfg: &Config, db: &Path) -> Config {
        let mut c = cfg.clone();
        c.db_path = db.to_path_buf();
        c
    }

    fn int(cfg: &Config, sql: &str) -> i64 {
        match rows(cfg, sql)[0][0] {
            Cell::Int(n) => n,
            ref other => panic!("{sql}: {other:?}"),
        }
    }

    fn text(cfg: &Config, sql: &str) -> String {
        match &rows(cfg, sql)[0][0] {
            Cell::Text(t) => t.clone(),
            other => panic!("{sql}: {other:?}"),
        }
    }

    /// Every table holds what the dump says, and nothing failed.
    fn assert_complete(report: &Report, want: &[(String, u64)]) {
        assert!(report.complete(), "{report:#?}");
        assert!(report.failures.is_empty(), "{report:#?}");
        assert!(report.problems.is_empty(), "{report:#?}");
        let got: Vec<(String, u64, std::result::Result<u64, String>)> = report
            .tables
            .iter()
            .map(|c| (c.table.clone(), c.in_dump, c.restored.clone()))
            .collect();
        let want: Vec<(String, u64, std::result::Result<u64, String>)> =
            want.iter().map(|(t, n)| (t.clone(), *n, Ok(*n))).collect();
        assert_eq!(got, want);
    }

    fn assert_refused(r: Result<Report>, needle: &str) {
        let err = r.expect_err("refused");
        assert_eq!(err.exit_code(), EXIT_REFUSED, "{err}");
        let msg = err.to_string();
        assert!(msg.starts_with("refused: "), "{msg}");
        assert!(msg.contains(needle), "{needle:?} not in {msg}");
    }

    /// Restore through `run_to`, expecting the incomplete verdict: exit 1,
    /// the report printed, the error naming the target. Returns the report.
    fn incomplete(cfg: &Config, a: &Args) -> String {
        let mut out = Vec::new();
        let err = run_to(cfg, a, &mut out).expect_err("incomplete");
        assert_eq!(err.exit_code(), EXIT_ERROR, "{err}");
        let msg = err.to_string();
        assert!(msg.starts_with("error: RESTORE INCOMPLETE"), "{msg}");
        assert!(msg.contains(&a.out.display().to_string()), "{msg}");
        String::from_utf8(out).unwrap()
    }

    // ---- the restore -------------------------------------------------------

    /// With no dump named, the newest dump in BACKUP_DIR is restored -- not an
    /// older one, not a partial file, not a dump of another project -- into a
    /// fresh file, every table counted. The restored database dumps back to
    /// the very dump it came from, and nothing else is left beside it.
    #[test]
    fn the_newest_dump_restores_into_a_fresh_file_with_every_count_matching() {
        let cfg = fixture("restore-newest");
        let older = crate::backup::create(&cfg, day("2026-10-27")).unwrap();
        batch(
            &cfg,
            "INSERT INTO episodic_memory (event_type, importance, memory_text) \
             VALUES ('note', 'routine', 'only in the newer dump');",
        );
        let newer = crate::backup::create(&cfg, day("2026-10-28")).unwrap();
        let now = SystemTime::now();
        fs::File::options()
            .write(true)
            .open(&older.path)
            .unwrap()
            .set_modified(now - Duration::from_secs(3600))
            .unwrap();
        for decoy in [".backup-1.partial", "2026-10-29-other-project.sql.gz"] {
            let f = fs::File::create(cfg.backup_dir.join(decoy)).unwrap();
            f.set_modified(now + Duration::from_secs(60)).unwrap();
        }

        let dir = target_dir(&cfg);
        let out = dir.join("new.db");
        let report = restore(&cfg, &args(None, &out, false)).unwrap();
        assert_eq!(report.dump, newer.path);
        assert!(report.newest);
        assert_eq!(report.target, out);
        assert_complete(&report, &newer.dumped.tables);
        assert_eq!(names(&dir), vec!["new.db"], "nothing beside it");

        let restored = at(&cfg, &out);
        assert_eq!(dump_text(&restored), gunzip(&newer.path));
        assert_eq!(
            int(
                &restored,
                "SELECT count(*) FROM episodic_memory WHERE memory_text = 'only in the newer dump'"
            ),
            1
        );
    }

    /// A named dump restores the same whether it is plain SQL or gzipped.
    #[test]
    fn a_named_dump_restores_the_same_plain_or_gzipped() {
        let cfg = fixture("restore-named");
        let sql = dump_text(&cfg);
        let want = parse_tables(&sql);
        for (name, bytes) in [
            ("plain.sql", sql.as_bytes().to_vec()),
            ("zipped.sql.gz", gzip(sql.as_bytes())),
        ] {
            let dump = write_dump(&cfg, name, &bytes);
            let out = target_dir(&cfg).join(format!("{name}.db"));
            let report = restore(&cfg, &args(Some(&dump), &out, false)).unwrap();
            assert_eq!(report.dump, dump);
            assert!(!report.newest);
            assert_complete(&report, &want);
            assert_eq!(dump_text(&at(&cfg, &out)), sql, "{name}");
        }
    }

    fn parse_tables(sql: &str) -> Vec<(String, u64)> {
        crate::store::turso::restore::parse(sql).unwrap().tables
    }

    /// The original bug: text with newlines, quotes, statement and comment
    /// syntax and non-ASCII characters comes back exactly as it was stored.
    #[test]
    fn newlines_and_quotes_in_memory_text_restore_intact() {
        let cfg = fixture("restore-tricky-text");
        let dump = write_dump(&cfg, "tricky.sql", dump_text(&cfg).as_bytes());
        let out = target_dir(&cfg).join("new.db");
        assert!(
            restore(&cfg, &args(Some(&dump), &out, false))
                .unwrap()
                .complete()
        );
        let restored = at(&cfg, &out);
        for table in ["semantic_memory", "episodic_memory", "instructions"] {
            let sql = format!("SELECT id, memory_text FROM {table} ORDER BY id");
            let original = rows(&cfg, &sql);
            assert!(original.len() >= 2, "{table}");
            assert_eq!(rows(&restored, &sql), original, "{table}");
        }
        assert!(
            rows(&restored, "SELECT memory_text FROM semantic_memory")
                .iter()
                .any(|r| r[0] == Cell::Text("line one\nline 'two' \"dq\" über; end".into()))
        );
    }

    /// The legacy format: a real `tursodb .dump`, internal sequence table and
    /// all, restores with every count matching, its text and vectors intact,
    /// its view, index and trigger in place, and its AUTOINCREMENT table still
    /// counting where it stopped.
    #[test]
    fn a_tursodb_dump_restores_with_matching_counts() {
        let cfg = scratch("restore-tursodb");
        let dump = write_dump(&cfg, "legacy.sql", TURSODB_DUMP.as_bytes());
        let out = target_dir(&cfg).join("new.db");
        let report = restore(&cfg, &args(Some(&dump), &out, false)).unwrap();
        assert_complete(&report, &tursodb_dump_tables());
        assert_eq!(report.skipped, 2);

        let r = at(&cfg, &out);
        assert_eq!(
            text(&r, "SELECT memory_text FROM semantic_memory WHERE id = 1"),
            "line one\nline 'two' \"dq\" über; end"
        );
        assert_eq!(
            text(&r, "SELECT memory_text FROM semantic_memory WHERE id = 3"),
            "-- not a comment; /* nor this */\nINSERT INTO \"semantic_memory\" VALUES(1);\n\
             CREATE TRIGGER x AFTER INSERT ON t BEGIN SELECT 1; END;\nCOMMIT;"
        );
        assert_eq!(
            text(&r, "SELECT memory_text FROM episodic_memory WHERE id = 1"),
            "an event\nover two lines\nand a tab\there"
        );
        assert!(distance(&r, Table::Semantic, 1, &tenths()) < 1e-6);
        assert!(distance(&r, Table::Episodic, 2, &quarters()) < 1e-6);
        let hex = text(&r, "SELECT hex(embedding) FROM instructions WHERE id = 2");
        assert_eq!(hex.len(), 1024 * 4 * 2);
        assert!(TURSODB_DUMP.contains(&format!("X'{}'", hex.to_lowercase())));
        assert_eq!(
            int(
                &r,
                "SELECT count(*) FROM sqlite_schema WHERE name IN \
                 ('current_semantic', 'idx_instructions_live', 'odd_touch')"
            ),
            3
        );
        batch(&r, "INSERT INTO odd (x) VALUES ('next');");
        assert_eq!(int(&r, "SELECT max(id) FROM odd"), 4);
    }

    /// Rows stored without an embedding come back with NULL in both columns,
    /// from either format: not an empty blob, not a zero vector.
    #[test]
    fn rows_without_an_embedding_come_back_null_in_both_formats() {
        let nulls = |c: &Config, table: &str| {
            int(
                c,
                &format!(
                    "SELECT count(*) FROM {table} WHERE embedding IS NULL AND embed_model IS NULL \
                     AND typeof(embedding) = 'null'"
                ),
            )
        };

        let cfg = fixture("restore-nulls-binary");
        let dump = write_dump(&cfg, "binary.sql", dump_text(&cfg).as_bytes());
        let out = target_dir(&cfg).join("binary.db");
        assert!(
            restore(&cfg, &args(Some(&dump), &out, false))
                .unwrap()
                .complete()
        );
        for table in ["semantic_memory", "episodic_memory", "instructions"] {
            assert!(nulls(&cfg, table) >= 1, "{table}: the fixture has one");
            assert_eq!(nulls(&at(&cfg, &out), table), nulls(&cfg, table), "{table}");
        }

        let dump = write_dump(&cfg, "legacy.sql", TURSODB_DUMP.as_bytes());
        let out = target_dir(&cfg).join("legacy.db");
        assert!(
            restore(&cfg, &args(Some(&dump), &out, false))
                .unwrap()
                .complete()
        );
        let r = at(&cfg, &out);
        assert_eq!(nulls(&r, "semantic_memory"), 2);
        assert_eq!(nulls(&r, "episodic_memory"), 1);
        assert_eq!(nulls(&r, "instructions"), 1);
    }

    /// No list of tables anywhere: one this command has never heard of is
    /// counted like the rest, and so is one the dump creates and leaves empty.
    #[test]
    fn a_table_never_heard_of_is_counted_like_the_rest() {
        let cfg = scratch("restore-unknown-table");
        let dump = write_dump(
            &cfg,
            "unknown.sql",
            b"PRAGMA foreign_keys=OFF;\nBEGIN TRANSACTION;\n\
              CREATE TABLE brand_new_table (a TEXT, b BLOB);\n\
              INSERT INTO \"brand_new_table\" VALUES('x\ny',X'00');\n\
              INSERT INTO \"brand_new_table\" VALUES('z',NULL);\n\
              INSERT INTO \"brand_new_table\" VALUES('w',NULL);\n\
              CREATE TABLE still_empty (z);\nCOMMIT;\n",
        );
        let out = target_dir(&cfg).join("new.db");
        let report = restore(&cfg, &args(Some(&dump), &out, false)).unwrap();
        assert_complete(
            &report,
            &[("brand_new_table".into(), 3), ("still_empty".into(), 0)],
        );
    }

    // ---- an incomplete restore -------------------------------------------

    /// A dump cut short -- inside a statement, at a statement boundary before
    /// its COMMIT, or as a gzip that ends early -- is never a partial
    /// success: it fails with every table's mismatch in the report, and
    /// leaves nothing at the target, not even a temporary file.
    #[test]
    fn a_truncated_dump_fails_with_a_per_table_mismatch_and_leaves_nothing() {
        let cfg = fixture("restore-truncated");
        let sql = dump_text(&cfg);
        let mid = sql.find("INSERT INTO \"episodic_memory\" VALUES(").unwrap() + 40;
        let no_commit = sql.strip_suffix("COMMIT;\n").unwrap();
        let zipped = gzip(sql.as_bytes());
        let cases: [(&str, Vec<u8>, &str); 3] = [
            ("mid-statement.sql", sql.as_bytes()[..mid].to_vec(), "byte"),
            (
                "no-commit.sql",
                no_commit.as_bytes().to_vec(),
                "never committed",
            ),
            ("cut.sql.gz", zipped[..zipped.len() / 2].to_vec(), "gzip"),
        ];
        for (name, bytes, why) in cases {
            let dump = write_dump(&cfg, name, &bytes);
            let dir = target_dir(&cfg);
            let out = dir.join("new.db");
            let printed = incomplete(&cfg, &args(Some(&dump), &out, false));
            let semantic = printed
                .lines()
                .find(|l| l.starts_with("semantic_memory "))
                .unwrap_or_else(|| panic!("{name}: no semantic_memory line in\n{printed}"));
            assert!(semantic.contains("<<< MISMATCH"), "{name}: {semantic}");
            assert!(printed.contains(why), "{name}: {why:?} not in\n{printed}");
            assert!(!printed.contains("RESTORE OK"), "{name}");
            assert!(names(&dir).is_empty(), "{name}: {:?}", names(&dir));
        }
    }

    /// A statement the engine rejects makes the restore incomplete: it is
    /// named in the report, its table mismatches, and nothing is left at the
    /// target.
    #[test]
    fn a_rejected_statement_makes_the_restore_incomplete_and_leaves_nothing() {
        let cfg = scratch("restore-rejected");
        let dump = write_dump(
            &cfg,
            "rejected.sql",
            b"BEGIN TRANSACTION;\nCREATE TABLE t (x CHECK (x < 10));\n\
              INSERT INTO t VALUES (1);\nINSERT INTO t VALUES (99);\nCOMMIT;\n",
        );
        let dir = target_dir(&cfg);
        let out = dir.join("new.db");
        let report = restore(&cfg, &args(Some(&dump), &out, false)).unwrap();
        assert!(!report.complete());
        assert_eq!(report.failures.len(), 1, "{report:#?}");
        assert_eq!(
            report.tables,
            vec![Counted {
                table: "t".into(),
                in_dump: 2,
                restored: Ok(1)
            }]
        );
        assert!(names(&dir).is_empty(), "{:?}", names(&dir));
        let printed = incomplete(&cfg, &args(Some(&dump), &out, false));
        assert!(
            printed.contains("FAILED statement 4")
                && printed.contains("INSERT INTO t VALUES (99);"),
            "{printed}"
        );
    }

    // ---- an existing target ----------------------------------------------

    /// Restoring over a file -- or next to a write-ahead log an earlier
    /// database left -- is refused without --force, and the file is left as
    /// it was.
    #[test]
    fn an_existing_file_is_refused_without_force() {
        let cfg = fixture("restore-existing");
        let dump = write_dump(&cfg, "good.sql", dump_text(&cfg).as_bytes());
        let dir = target_dir(&cfg);
        let out = dir.join("new.db");
        fs::write(&out, b"keep me").unwrap();
        assert_refused(
            restore(&cfg, &args(Some(&dump), &out, false)),
            "already exists",
        );
        assert_eq!(fs::read(&out).unwrap(), b"keep me");
        assert_eq!(names(&dir), vec!["new.db"]);

        fs::remove_file(&out).unwrap();
        fs::write(dir.join("new.db-wal"), b"stale log").unwrap();
        assert_refused(restore(&cfg, &args(Some(&dump), &out, false)), "new.db-wal");
        assert_eq!(names(&dir), vec!["new.db-wal"]);
    }

    /// --force replaces the file and its stale sidecars, but only with a
    /// restore that has been verified: a failed one leaves them untouched.
    #[test]
    fn force_replaces_an_existing_file_only_with_a_verified_restore() {
        let cfg = fixture("restore-force");
        let sql = dump_text(&cfg);
        let dir = target_dir(&cfg);
        let out = dir.join("new.db");
        let plant = || {
            fs::write(&out, b"keep me").unwrap();
            fs::write(dir.join("new.db-wal"), b"stale log").unwrap();
            fs::write(dir.join("new.db-tshm"), b"stale index").unwrap();
        };

        plant();
        let cut = write_dump(&cfg, "cut.sql", &sql.as_bytes()[..sql.len() / 2]);
        incomplete(&cfg, &args(Some(&cut), &out, true));
        assert_eq!(fs::read(&out).unwrap(), b"keep me");
        assert_eq!(fs::read(dir.join("new.db-wal")).unwrap(), b"stale log");
        assert_eq!(names(&dir), vec!["new.db", "new.db-tshm", "new.db-wal"]);

        let good = write_dump(&cfg, "good.sql", sql.as_bytes());
        let report = restore(&cfg, &args(Some(&good), &out, true)).unwrap();
        assert_complete(&report, &parse_tables(&sql));
        assert_eq!(names(&dir), vec!["new.db"], "the stale sidecars went too");
        assert_eq!(dump_text(&at(&cfg, &out)), sql);
    }

    // ---- refusals ----------------------------------------------------------

    /// The live database is never a target, not even with --force and not
    /// under another spelling of its path.
    #[test]
    fn the_configured_database_is_refused_even_with_force() {
        let cfg = fixture("restore-live");
        let dump = write_dump(&cfg, "good.sql", dump_text(&cfg).as_bytes());
        let before = fs::read(&cfg.db_path).unwrap();
        let dir = cfg.db_path.parent().unwrap().to_path_buf();
        let spelled = dir.join(".").join(cfg.db_path.file_name().unwrap());
        // Its write-ahead log is part of it: replaced, the database is lost.
        let wal = sidecar(&cfg.db_path, "-wal");
        assert!(wal.exists(), "the fixture's database has a log");
        let wal_before = fs::read(&wal).unwrap();
        let listing = names(&dir);
        for out in [cfg.db_path.clone(), spelled, wal.clone()] {
            for force in [false, true] {
                assert_refused(
                    restore(&cfg, &args(Some(&dump), &out, force)),
                    "configured database",
                );
            }
        }
        assert_eq!(fs::read(&cfg.db_path).unwrap(), before);
        assert_eq!(fs::read(&wal).unwrap(), wal_before);
        assert_eq!(names(&dir), listing);
    }

    /// No dump named and none in BACKUP_DIR: refused, and nothing created --
    /// a partial file, a dump of another project and a plain .sql do not
    /// count as backups.
    #[test]
    fn no_dump_found_is_refused_and_nothing_is_created() {
        let cfg = scratch("restore-no-dump");
        let dir = target_dir(&cfg);
        let out = dir.join("new.db");
        assert!(!cfg.backup_dir.exists());
        assert_refused(restore(&cfg, &args(None, &out, false)), "no dump");
        assert!(!cfg.backup_dir.exists(), "BACKUP_DIR was created");

        fs::create_dir_all(&cfg.backup_dir).unwrap();
        for decoy in [
            ".backup-1.partial",
            "2026-10-27-other-project.sql.gz",
            "2026-10-27-supercharged-memory.sql",
        ] {
            fs::write(cfg.backup_dir.join(decoy), b"x").unwrap();
        }
        assert_refused(
            restore(&cfg, &args(None, &out, false)),
            &cfg.backup_dir.display().to_string(),
        );
        assert!(names(&dir).is_empty());
    }

    #[test]
    fn a_dump_that_does_not_exist_is_refused() {
        let cfg = scratch("restore-dump-missing");
        let dir = target_dir(&cfg);
        let missing = cfg.db_path.with_file_name("nope.sql.gz");
        assert_refused(
            restore(&cfg, &args(Some(&missing), &dir.join("new.db"), false)),
            "nope.sql.gz",
        );
        assert!(names(&dir).is_empty());
    }

    /// A target whose directory does not exist is not created on a guess, and
    /// a directory is never a target.
    #[test]
    fn a_target_in_a_missing_directory_or_that_is_a_directory_is_refused() {
        let cfg = fixture("restore-bad-target");
        let dump = write_dump(&cfg, "good.sql", dump_text(&cfg).as_bytes());
        let missing = cfg.db_path.with_file_name("no-such-dir");
        assert_refused(
            restore(&cfg, &args(Some(&dump), &missing.join("new.db"), false)),
            "does not exist",
        );
        assert!(!missing.exists());

        let dir = target_dir(&cfg);
        for force in [false, true] {
            assert_refused(restore(&cfg, &args(Some(&dump), &dir, force)), "directory");
        }
        assert!(names(&dir).is_empty());
    }

    /// A file that is not a dump whose counts can be read -- not text, no
    /// table, a statement that cannot be counted -- is refused before
    /// anything is created.
    #[test]
    fn a_file_that_is_not_a_countable_dump_is_refused_and_nothing_is_created() {
        let cfg = scratch("restore-not-a-dump");
        let dir = target_dir(&cfg);
        let out = dir.join("new.db");
        for (name, bytes, needle) in [
            ("binary.sql", &[0xff, 0xfe, 0x00, 0x80][..], "not"),
            (
                "prose.sql",
                &b"Dear diary; today I made a backup."[..],
                "no table",
            ),
            (
                "select.sql",
                &b"CREATE TABLE t (x);\nINSERT INTO t SELECT 1;\n"[..],
                "INSERT INTO t SELECT 1",
            ),
        ] {
            let dump = write_dump(&cfg, name, bytes);
            assert_refused(restore(&cfg, &args(Some(&dump), &out, false)), needle);
            assert!(names(&dir).is_empty(), "{name}: {:?}", names(&dir));
        }
    }

    // ---- moving the restore into place ---------------------------------------

    /// The main file is moved alone, so its write-ahead log must hold nothing.
    #[test]
    fn a_database_whose_log_still_holds_data_is_not_moved() {
        let dir = scratch("restore-wal").db_path.with_file_name("wal");
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("x.db");
        fs::write(&db, b"").unwrap();
        assert!(wal_is_empty(&db).is_ok(), "no log at all");
        fs::write(dir.join("x.db-wal"), b"").unwrap();
        assert!(wal_is_empty(&db).is_ok(), "an empty log");
        fs::write(dir.join("x.db-wal"), b"frames").unwrap();
        let err = wal_is_empty(&db).unwrap_err();
        assert_eq!(err.exit_code(), EXIT_ERROR, "{err}");
        assert!(err.to_string().contains("x.db-wal"), "{err}");
    }

    // ---- what is printed ---------------------------------------------------

    /// The report: which dump (and that it was the newest), the target, the
    /// statement and row totals, the skipped internal statements, one line per
    /// table with both counts and a mark on each mismatch, every failure and
    /// problem -- and the verdict only when it is earned.
    #[test]
    fn the_report_names_the_dump_the_target_every_table_and_each_failure() {
        let cfg = scratch("restore-lines");
        let mut report = Report {
            dump: PathBuf::from("/b/2026-10-27-supercharged-memory.sql.gz"),
            newest: true,
            target: PathBuf::from("/x/new.db"),
            statements: 57,
            skipped: 2,
            tables: vec![
                Counted {
                    table: "semantic_memory".into(),
                    in_dump: 9,
                    restored: Ok(9),
                },
                Counted {
                    table: "instructions".into(),
                    in_dump: 2,
                    restored: Err("missing".into()),
                },
                Counted {
                    table: "odd".into(),
                    in_dump: 2,
                    restored: Ok(1),
                },
            ],
            failures: vec![Failure {
                statement: 12,
                error: "CHECK constraint failed".into(),
                excerpt: "INSERT INTO \"odd\" VALUES(3,'c');".into(),
            }],
            problems: vec!["the dump never committed its transaction".into()],
        };
        assert!(!report.complete());
        let text = report.lines(&cfg).join("\n");
        for want in [
            "/b/2026-10-27-supercharged-memory.sql.gz",
            "newest in BACKUP_DIR",
            "/x/new.db",
            "57 statements",
            "13 rows",
            "3 tables",
            "skipped 2",
            "FAILED statement 12: CHECK constraint failed",
            "INSERT INTO \"odd\" VALUES(3,'c');",
            "the dump never committed its transaction",
        ] {
            assert!(text.contains(want), "{want:?} not in:\n{text}");
        }
        let line = |t: &str| {
            text.lines()
                .find(|l| l.starts_with(&format!("{t} ")))
                .unwrap_or_else(|| panic!("no {t} line in:\n{text}"))
                .to_string()
        };
        assert!(!line("semantic_memory").contains("MISMATCH"));
        assert!(line("instructions").contains("missing"));
        assert!(line("instructions").contains("<<< MISMATCH"));
        assert!(line("odd").contains("<<< MISMATCH"));
        assert!(!text.contains("RESTORE OK"));

        report.tables[1].restored = Ok(2);
        report.tables[2].restored = Ok(2);
        report.failures.clear();
        report.problems.clear();
        assert!(report.complete());
        let text = report.lines(&cfg).join("\n");
        assert!(!text.contains("MISMATCH"), "{text}");
        assert!(text.contains("RESTORE OK"), "{text}");
        // Not in use until the .env says so, and that is the user's call.
        for want in [
            DB_ENV.to_string(),
            cfg.db_path.display().to_string(),
            cfg.dotenv_path.display().to_string(),
            "ask the user".to_string(),
        ] {
            assert!(text.contains(&want), "{want:?} not in:\n{text}");
        }
    }

    /// `run_to` prints the report and succeeds only on a complete restore.
    #[test]
    fn run_to_prints_the_report_and_the_verdict() {
        let cfg = fixture("restore-run-to");
        let dump = write_dump(&cfg, "good.sql", dump_text(&cfg).as_bytes());
        let out = target_dir(&cfg).join("new.db");
        let mut printed = Vec::new();
        run_to(&cfg, &args(Some(&dump), &out, false), &mut printed).unwrap();
        let printed = String::from_utf8(printed).unwrap();
        assert!(printed.contains("semantic_memory "), "{printed}");
        assert!(
            printed.contains(&format!(
                "RESTORE OK — every row in the dump is present in {}",
                out.display()
            )),
            "{printed}"
        );
    }
}
