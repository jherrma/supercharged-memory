//! `--purge`: the one place in this system that deletes a memory.
//!
//! Deep sleep's purge gate only. It removes superseded or retired semantic rows
//! the user picked, and nothing else, so it refuses on every way that could go
//! wrong: no ids, an id that is not a semantic row, a current row, a row whose
//! deletion would strand one that points at it, no `--confirm-purge`, and no
//! backup newer than the database -- the dump is the only undo. Episodic memory
//! is never purged, and the `instructions` table is never touched.

use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use flate2::read::GzDecoder;

use super::parse_ids;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::store::{self, Store, Table, candidates};

/// Hard-delete the listed rows and print what was deleted and which dump
/// covers them.
pub fn purge(cfg: &Config, ids_csv: &str, confirm: bool) -> Result<String> {
    let ids = parse_ids(ids_csv)?;
    if !confirm {
        return Err(Error::refused(format!(
            "--purge hard-DELETEs {} memory row(s) — irreversible except from a backup. \
             Re-run with --confirm-purge once the user has picked the ids.",
            ids.len()
        )));
    }
    // Opening first is safe: an open changes neither the main file's time nor
    // the -wal's, which is all the gate compares.
    let store = store::open(cfg)?;
    let dump = backup_gate(cfg)?;

    let found = store.existing_ids(Table::Semantic, &ids)?;
    let missing: Vec<i64> = ids
        .iter()
        .copied()
        .filter(|id| !found.contains(id))
        .collect();
    if !missing.is_empty() {
        return Err(Error::refused(format!(
            "no semantic row with id {}.",
            list(&missing)
        )));
    }
    let mut current = store.current_semantic_ids(&ids)?;
    current.sort_unstable();
    if !current.is_empty() {
        return Err(Error::refused(format!(
            "id {} is CURRENT truth (neither superseded nor retired). --purge only removes \
             rows already replaced or retired; retire it first if it really is obsolete.",
            list(&current)
        )));
    }
    // A surviving row pointing at a purged one would be left with a dangling
    // superseded_by, breaking the revision chain it documents.
    let stranded = store.stranded_by(&ids)?;
    if !stranded.is_empty() {
        return Err(Error::refused(format!(
            "row(s) {} are superseded BY a row in this purge list, so deleting it would \
             strand them. Purge the whole chain together, or leave these targets in place.",
            list(&stranded)
        )));
    }

    let purged = store.purge_semantic(&ids)?;
    Ok(format!(
        "purged {} semantic row(s): {}\ncleared {} memory_coworkers row(s)\nbackup verified: {}",
        purged.rows,
        list(&ids),
        purged.coworker_tags,
        dump.display()
    ))
}

fn list(ids: &[i64]) -> String {
    ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
}

/// When the database last took a write, as far as its files tell: the main
/// file, or the `-wal` when it holds anything newer.
///
/// A write through the driver lands in the `-wal` and leaves the main file's
/// time alone until a checkpoint, so the main file alone would let a write made
/// after the backup slip past. An empty `-wal` holds no write -- a `tursodb`
/// reader truncates it on exit and bumps its time -- and `-tshm` is touched by
/// every opener, so neither counts.
fn last_written(db: &Path) -> std::io::Result<SystemTime> {
    let main = fs::metadata(db)?.modified()?;
    let mut wal = db.as_os_str().to_owned();
    wal.push("-wal");
    match fs::metadata(&wal) {
        Ok(m) if m.len() > 0 => Ok(main.max(m.modified()?)),
        Ok(_) => Ok(main),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(main),
        Err(e) => Err(e),
    }
}

/// The newest dump in BACKUP_DIR, if it is newer than the database's last
/// write and reads back whole; otherwise a refusal saying what to run.
fn backup_gate(cfg: &Config) -> Result<PathBuf> {
    let Some(dump) = candidates::find_backups(&cfg.backup_dir).into_iter().next() else {
        return Err(Error::refused(format!(
            "no backup found in {}. Run backup first — a purge has no other undo.",
            cfg.backup_dir.display()
        )));
    };
    let unreadable = |what: &Path, e: std::io::Error| {
        Error::failed(format!("cannot read the time of {}: {e}", what.display()))
    };
    let dumped = fs::metadata(&dump)
        .and_then(|m| m.modified())
        .map_err(|e| unreadable(&dump, e))?;
    let written = last_written(&cfg.db_path).map_err(|e| unreadable(&cfg.db_path, e))?;
    // Equal times cannot say which came first, so they do not pass.
    if dumped <= written {
        return Err(Error::refused(format!(
            "newest backup ({}) is not newer than the database's last write, so it does not \
             contain everything you are about to delete. Run backup first.\n  (Any write ages \
             the database past the dump — including a previous --purge. Pass ALL selected ids \
             in ONE --purge call rather than one call per id.)",
            dump.file_name().unwrap_or_default().to_string_lossy()
        )));
    }
    // It is the only undo, so it must decompress to its end: the gzip trailer's
    // CRC and length are checked there.
    fs::File::open(&dump)
        .and_then(|f| std::io::copy(&mut GzDecoder::new(BufReader::new(f)), &mut std::io::sink()))
        .map_err(|e| {
            Error::refused(format!(
                "newest backup ({}) cannot be read back: {e}. Run backup again — a purge has \
                 no other undo.",
                dump.display()
            ))
        })?;
    Ok(dump)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::Duration;

    use super::*;
    use crate::config::{DB_ENV, DotEnv, Env};
    use crate::error::EXIT_REFUSED;
    use crate::sleep::{Mode, execute};
    use crate::store::turso::tests::{put, rows, scratch};
    use crate::store::{Cell, Table};

    fn run(cfg: &Config, ids: &str, confirm: bool) -> Result<String> {
        execute(
            cfg,
            &Mode::Purge {
                ids: ids.into(),
                confirm,
            },
            &mut std::io::empty(),
        )
    }

    fn refusal(r: Result<String>) -> String {
        let err = r.expect_err("expected a refusal");
        assert_eq!(err.exit_code(), EXIT_REFUSED, "{err}");
        let msg = err.to_string();
        assert!(msg.starts_with("refused: "), "{msg}");
        msg
    }

    /// File times on Linux tick in jiffies unless a file's time was just read;
    /// a pause keeps "before" and "after" from landing on the same tick.
    fn settle() {
        std::thread::sleep(Duration::from_millis(30));
    }

    fn backup(cfg: &Config) -> PathBuf {
        settle();
        let path =
            crate::backup::create(cfg, chrono::NaiveDate::from_ymd_opt(2026, 10, 27).unwrap())
                .unwrap()
                .path;
        settle();
        path
    }

    fn set_mtime(path: &Path, t: SystemTime) {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    fn mtime(path: &Path) -> SystemTime {
        fs::metadata(path).unwrap().modified().unwrap()
    }

    fn wal(cfg: &Config) -> PathBuf {
        let mut name = cfg.db_path.file_name().unwrap().to_os_string();
        name.push("-wal");
        cfg.db_path.with_file_name(name)
    }

    fn semantic_ids(cfg: &Config) -> Vec<Vec<Cell>> {
        rows(cfg, "SELECT id FROM semantic_memory ORDER BY id;")
    }

    /// Current row 1, row 2 superseded by 1, row 3 retired, row 4 superseded
    /// by 2 (a chain: 4 -> 2 -> 1). Episodic rows 1..3 share the ids.
    fn corpus(cfg: &Config) {
        for t in ["current", "superseded", "retired", "older still"] {
            put(cfg, Table::Semantic, t, false);
        }
        // One statement per call: `rows` runs the first statement only.
        rows(
            cfg,
            "UPDATE semantic_memory SET superseded_by = 1 WHERE id = 2;",
        );
        rows(
            cfg,
            "UPDATE semantic_memory SET retired_at = '2026-02-02' WHERE id = 3;",
        );
        rows(
            cfg,
            "UPDATE semantic_memory SET superseded_by = 2 WHERE id = 4;",
        );
        for t in ["e1", "e2", "e3"] {
            put(cfg, Table::Episodic, t, false);
        }
    }

    // ---- the refusals -------------------------------------------------------

    #[test]
    fn no_ids_is_refused() {
        let cfg = scratch("purge-no-ids");
        assert_eq!(refusal(run(&cfg, " , ", true)), "refused: no ids given.");
        assert_eq!(
            refusal(run(&cfg, "2,all", true)),
            "refused: not an id: \"all\" — ids are comma-separated integers."
        );
    }

    /// Checked before the database is even opened: without the
    /// acknowledgement, nothing is read, created or deleted.
    #[test]
    fn without_confirm_purge_it_is_refused_before_anything_is_opened() {
        let mut cfg = scratch("purge-no-confirm");
        cfg.db_path = cfg.db_path.with_file_name("not-there.db");
        assert_eq!(
            refusal(run(&cfg, "2,3,2", false)),
            "refused: --purge hard-DELETEs 2 memory row(s) — irreversible except from a \
             backup. Re-run with --confirm-purge once the user has picked the ids."
        );
        assert!(!cfg.db_path.exists());
    }

    #[test]
    fn without_a_backup_it_is_refused() {
        let cfg = scratch("purge-no-backup");
        corpus(&cfg);
        let msg = refusal(run(&cfg, "2", true));
        assert_eq!(
            msg,
            format!(
                "refused: no backup found in {}. Run backup first — a purge has no other undo.",
                cfg.backup_dir.display()
            )
        );
        assert_eq!(semantic_ids(&cfg).len(), 4);
    }

    #[test]
    fn a_backup_older_than_the_database_is_refused() {
        let cfg = scratch("purge-old-backup");
        corpus(&cfg);
        let dump = backup(&cfg);
        set_mtime(&dump, mtime(&cfg.db_path) - Duration::from_secs(60));
        let msg = refusal(run(&cfg, "2", true));
        assert!(msg.contains("is not newer than the database"), "{msg}");
        assert!(msg.contains("Run backup first"), "{msg}");
        assert_eq!(semantic_ids(&cfg).len(), 4);
    }

    /// Equal file times cannot tell which came first, so they do not pass.
    #[test]
    fn a_backup_from_the_same_instant_is_refused() {
        let cfg = scratch("purge-same-instant");
        corpus(&cfg);
        let dump = backup(&cfg);
        let t = mtime(&dump);
        set_mtime(&cfg.db_path, t);
        set_mtime(&wal(&cfg), t);
        refusal(run(&cfg, "2", true));
    }

    /// The case the reference's main-file check missed: a write through the
    /// driver lands in the -wal and leaves the main file's time alone.
    #[test]
    fn a_write_after_the_backup_is_refused_though_only_the_wal_changed() {
        let cfg = scratch("purge-wal-write");
        corpus(&cfg);
        backup(&cfg);
        let main_before = mtime(&cfg.db_path);
        put(&cfg, Table::Semantic, "written after the backup", false);
        assert_eq!(
            mtime(&cfg.db_path),
            main_before,
            "the main file is untouched"
        );
        let msg = refusal(run(&cfg, "2", true));
        assert!(msg.contains("is not newer than the database"), "{msg}");
        assert_eq!(semantic_ids(&cfg).len(), 5);
    }

    /// A purge is itself a write, so a second call in one pass is refused
    /// until the backup is taken again.
    #[test]
    fn a_second_purge_in_one_pass_is_refused() {
        let cfg = scratch("purge-twice");
        corpus(&cfg);
        backup(&cfg);
        run(&cfg, "3", true).unwrap();
        settle();
        let msg = refusal(run(&cfg, "2,4", true));
        assert!(
            msg.contains("Pass ALL selected ids in ONE --purge call"),
            "{msg}"
        );
        assert_eq!(semantic_ids(&cfg).len(), 3);
        backup(&cfg);
        run(&cfg, "2,4", true).unwrap();
        assert_eq!(semantic_ids(&cfg), vec![vec![Cell::Int(1)]]);
    }

    #[test]
    fn a_dump_that_does_not_read_back_is_refused() {
        let cfg = scratch("purge-corrupt-dump");
        corpus(&cfg);
        settle();
        fs::create_dir_all(&cfg.backup_dir).unwrap();
        fs::write(
            cfg.backup_dir.join("2026-10-27-supercharged-memory.sql.gz"),
            b"not gzip at all",
        )
        .unwrap();
        let msg = refusal(run(&cfg, "2", true));
        assert!(msg.contains("cannot be read back"), "{msg}");
        assert_eq!(semantic_ids(&cfg).len(), 4);
    }

    /// Episodic rows 1..3 exist, but a purge only ever means semantic rows.
    #[test]
    fn an_id_that_is_no_semantic_row_is_refused() {
        let cfg = scratch("purge-unknown");
        corpus(&cfg);
        put(&cfg, Table::Episodic, "e4", false);
        put(&cfg, Table::Episodic, "e5", false);
        backup(&cfg);
        assert_eq!(
            refusal(run(&cfg, "3,5,99", true)),
            "refused: no semantic row with id 5,99."
        );
        assert_eq!(semantic_ids(&cfg).len(), 4);
        assert_eq!(
            rows(&cfg, "SELECT count(*) FROM episodic_memory;"),
            vec![vec![Cell::Int(5)]]
        );
    }

    #[test]
    fn a_current_row_is_refused() {
        let cfg = scratch("purge-current");
        corpus(&cfg);
        backup(&cfg);
        assert_eq!(
            refusal(run(&cfg, "1,3", true)),
            "refused: id 1 is CURRENT truth (neither superseded nor retired). --purge only \
             removes rows already replaced or retired; retire it first if it really is obsolete."
        );
        assert_eq!(semantic_ids(&cfg).len(), 4);
    }

    #[test]
    fn a_purge_that_would_strand_a_row_is_refused() {
        let cfg = scratch("purge-strand");
        corpus(&cfg);
        backup(&cfg);
        assert_eq!(
            refusal(run(&cfg, "2", true)),
            "refused: row(s) 4 are superseded BY a row in this purge list, so deleting it would \
             strand them. Purge the whole chain together, or leave these targets in place."
        );
        assert_eq!(semantic_ids(&cfg).len(), 4);
    }

    // ---- what a purge does --------------------------------------------------

    #[test]
    fn a_purge_deletes_the_rows_and_their_tags_and_names_the_dump() {
        let cfg = scratch("purge-ok");
        corpus(&cfg);
        rows(
            &cfg,
            "INSERT INTO coworkers (name, expertise, personality) VALUES ('jeff', 'x', 'y');",
        );
        rows(
            &cfg,
            "INSERT INTO memory_coworkers (memory_table, memory_id, coworker_id) VALUES \
             ('semantic', 2, 1), ('semantic', 1, 1), ('episodic', 2, 1);",
        );
        rows(
            &cfg,
            "INSERT INTO eval_cases (id, class, memory_table, query, expect_ids, expect_stamps) \
             VALUES ('x01', 'exact-id', 'semantic', 'q', '2,4', 'a,b');",
        );
        let dump = backup(&cfg);
        let out = run(&cfg, "4,2,3", true).unwrap();
        assert_eq!(
            out,
            format!(
                "purged 3 semantic row(s): 2,3,4\ncleared 1 memory_coworkers row(s)\n\
                 backup verified: {}",
                dump.display()
            )
        );
        assert_eq!(semantic_ids(&cfg), vec![vec![Cell::Int(1)]]);
        assert_eq!(
            rows(
                &cfg,
                "SELECT memory_table, memory_id FROM memory_coworkers ORDER BY memory_table;"
            ),
            vec![
                vec![Cell::Text("episodic".into()), Cell::Int(2)],
                vec![Cell::Text("semantic".into()), Cell::Int(1)],
            ]
        );
        assert_eq!(
            rows(&cfg, "SELECT count(*) FROM episodic_memory;"),
            vec![vec![Cell::Int(3)]],
            "episodic memory is never purged"
        );
        assert_eq!(
            rows(&cfg, "SELECT expect_ids, expect_stamps FROM eval_cases;"),
            vec![vec![Cell::Text("2,4".into()), Cell::Text("a,b".into())]],
            "eval cases are deliberately unlinked; --validate finds the dangling ids"
        );
    }

    // ---- the gate -----------------------------------------------------------

    #[test]
    fn the_last_write_is_the_main_file_or_a_non_empty_wal() {
        let dir = crate::config::dotenv::tests::tmpdir("purge-last-written");
        let db = dir.join("m.db");
        let wal = dir.join("m.db-wal");
        let tshm = dir.join("m.db-tshm");
        fs::write(&db, b"x").unwrap();
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let later = t0 + Duration::from_secs(60);
        set_mtime(&db, t0);
        assert_eq!(last_written(&db).unwrap(), t0, "no -wal at all");

        // A reader's open bumps -tshm; that is no write.
        fs::write(&tshm, b"x").unwrap();
        set_mtime(&tshm, later);
        assert_eq!(last_written(&db).unwrap(), t0);

        // A tursodb reader re-truncates an empty -wal; that is no write either.
        fs::write(&wal, b"").unwrap();
        set_mtime(&wal, later);
        assert_eq!(last_written(&db).unwrap(), t0);

        // Frames in the -wal are writes the main file has not seen yet.
        fs::write(&wal, b"frames").unwrap();
        set_mtime(&wal, later);
        assert_eq!(last_written(&db).unwrap(), later);

        // A checkpoint moves them into the main file.
        set_mtime(&db, later + Duration::from_secs(1));
        assert_eq!(last_written(&db).unwrap(), later + Duration::from_secs(1));
    }

    /// The gate looks where configuration resolved BACKUP_DIR -- the real
    /// environment, the .env, or the built-in default -- and nowhere else.
    #[test]
    fn the_gate_finds_dumps_where_config_resolved_backup_dir() {
        let base = scratch("purge-gate-layers");
        put(&base, Table::Semantic, "a row", false);
        let root = base.db_path.parent().unwrap().to_path_buf();
        let made = backup(&Config {
            backup_dir: root.join("made"),
            ..base.clone()
        });
        let home = root.join("home");
        let db = base.db_path.to_string_lossy().to_string();
        let env_dir = root.join("env-backups");
        let dot_dir = root.join("dot-backups");
        let default_dir = home.join(".local/share/supercharged-memory/backups");

        let layered = |vars: Vec<(&str, String)>, dotenv: DotEnv| {
            Config::from_env(&Env {
                vars: vars
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v))
                    .chain([(DB_ENV.to_string(), db.clone())])
                    .collect(),
                home: home.clone(),
                windows: false,
                dotenv,
            })
            .unwrap()
        };
        let absent = || DotEnv::absent(PathBuf::from(crate::config::tests::EXE));
        let from_env = layered(
            vec![("BACKUP_DIR", env_dir.to_string_lossy().to_string())],
            absent(),
        );
        let from_dotenv = layered(
            vec![],
            crate::config::tests::dotenv(&format!("BACKUP_DIR={}\n", dot_dir.display())),
        );
        let from_default = layered(vec![], absent());

        let name = made.file_name().unwrap();
        for (cfg, dir) in [
            (&from_env, &env_dir),
            (&from_dotenv, &dot_dir),
            (&from_default, &default_dir),
        ] {
            fs::create_dir_all(dir).unwrap();
            fs::copy(&made, dir.join(name)).unwrap();
            assert_eq!(
                backup_gate(cfg).unwrap(),
                dir.join(name),
                "{}",
                dir.display()
            );
            fs::remove_file(dir.join(name)).unwrap();
            let msg = refusal(backup_gate(cfg).map(|p| p.display().to_string()));
            assert!(
                msg.contains(&dir.display().to_string()),
                "a dump in another layer's directory is not found: {msg}"
            );
        }
    }
}
