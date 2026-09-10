//! The MISSING failsafe: what else looks like a memory database, and where.
//!
//! A configured path that does not exist is treated as a WRONG PATH by default,
//! never as lost data -- the two are indistinguishable from the outside, and
//! guessing wrong is destructive in both directions. Creating a fresh database
//! strands the real one; restoring a backup over a live database loses everything
//! since that backup.
//!
//! So nothing here creates, restores or writes anything. It reports.

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::store::Store;
use crate::store::turso::TursoStore;

/// A database found somewhere other than the configured path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateDb {
    pub path: PathBuf,
    pub memories: u64,
}

#[derive(Debug, Default)]
pub struct Candidates {
    /// Richest first: the one with the most memories is the likeliest intent.
    pub dbs: Vec<CandidateDb>,
    /// Newest first.
    pub backups: Vec<PathBuf>,
}

impl Candidates {
    pub fn is_empty(&self) -> bool {
        self.dbs.is_empty() && self.backups.is_empty()
    }
}

/// Scan the search directories, non-recursively, for readable memory databases,
/// plus `BACKUP_DIR` for restorable dumps.
pub fn find(cfg: &Config) -> Candidates {
    let configured = canonical(&cfg.db_path);
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut dbs = Vec::new();

    for dir in &cfg.search_dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue; // unreadable or absent: not a finding, just nothing here
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("db")))
            .collect();
        paths.sort();

        for path in paths {
            let real = canonical(&path);
            if real == configured || seen.contains(&real) {
                continue;
            }
            seen.push(real.clone());
            if let Some(n) = count_memories(cfg, &path) {
                dbs.push(CandidateDb {
                    path: real,
                    memories: n,
                });
            }
        }
    }
    dbs.sort_by_key(|d| std::cmp::Reverse(d.memories));

    Candidates {
        dbs,
        backups: find_backups(&cfg.backup_dir),
    }
}

/// Dumps are matched on the project name, which couples the filename to the
/// project: a past rename made every existing dump invisible here, because the
/// glob stopped matching. Keep the two in step.
fn find_backups(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_ascii_lowercase();
            name.contains("supercharged-memory") && name.ends_with(".sql.gz")
        })
        .filter_map(|e| {
            let modified = e.metadata().and_then(|m| m.modified()).ok()?;
            Some((modified, e.path()))
        })
        .collect();
    found.sort_by_key(|f| std::cmp::Reverse(f.0));
    found.into_iter().map(|(_, p)| p).collect()
}

/// Row count for a candidate database, or `None` if it is not a readable memory
/// database. A file that cannot be opened, or has no memory tables, is simply not
/// a candidate -- it must never be reported as an empty one.
fn count_memories(cfg: &Config, path: &Path) -> Option<u64> {
    let probe = Config {
        db_path: path.to_path_buf(),
        ..cfg.clone()
    };
    TursoStore::open(&probe).ok()?.total_memories().ok()
}

fn canonical(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// The lines `--status` prints under MISSING, and what `--candidates` prints on
/// demand. The closing line is not decoration: it is the instruction that stops
/// an agent from "helpfully" creating a database.
pub fn missing_report(cfg: &Config) -> Vec<String> {
    let mut lines = vec![
        format!("configured path : {}", cfg.db_path.display()),
        format!("path came from  : {}", cfg.db_source_label()),
    ];
    let found = find(cfg);
    for db in &found.dbs {
        lines.push(format!(
            "CANDIDATE DB    : {} ({} memories)",
            db.path.display(),
            db.memories
        ));
    }
    for b in found.backups.iter().take(3) {
        lines.push(format!("CANDIDATE BACKUP: {}", b.display()));
    }
    if found.is_empty() {
        lines.push("no other database or backup found in the usual locations".into());
    }
    lines.push("DO NOT create or overwrite a database — ask the user first.".into());
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sm-cand-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cfg_with(dir: &Path, db: &str) -> Config {
        let env = crate::config::Env {
            vars: [
                (
                    crate::config::DB_ENV.to_string(),
                    dir.join(db).to_string_lossy().to_string(),
                ),
                ("BACKUP_DIR".to_string(), dir.to_string_lossy().to_string()),
            ]
            .into_iter()
            .collect(),
            home: dir.to_path_buf(),
            windows: false,
        };
        Config::from_env(&env).unwrap()
    }

    #[test]
    fn backups_are_newest_first_and_matched_on_the_project_name() {
        let dir = tmpdir("backups");
        for name in [
            "2026-01-01-supercharged-memory.sql.gz",
            "2026-02-01-supercharged-memory-weekly.sql.gz",
            "2026-03-01-something-else.sql.gz", // wrong project: not a candidate
            "notes.txt",
        ] {
            fs::write(dir.join(name), b"x").unwrap();
            // Distinct mtimes, in the order written.
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let found = find_backups(&dir);
        let names: Vec<String> = found
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(
            names,
            vec![
                "2026-02-01-supercharged-memory-weekly.sql.gz",
                "2026-01-01-supercharged-memory.sql.gz"
            ]
        );
    }

    #[test]
    fn an_unreadable_backup_dir_yields_nothing_rather_than_failing() {
        assert!(find_backups(Path::new("/definitely/not/here")).is_empty());
    }

    /// The report has to state the path, where it came from, and the refusal --
    /// an agent reading only "MISSING" is one step from creating a fresh database.
    #[test]
    fn the_missing_report_names_the_path_its_source_and_the_refusal() {
        let dir = tmpdir("report");
        let cfg = cfg_with(&dir, "nope.db");
        let lines = missing_report(&cfg).join("\n");
        assert!(lines.contains("nope.db"), "{lines}");
        assert!(lines.contains(crate::config::DB_ENV), "{lines}");
        assert!(lines.contains("DO NOT create or overwrite"), "{lines}");
        assert!(
            lines.contains("no other database or backup found"),
            "an empty search must say so explicitly: {lines}"
        );
    }

    #[test]
    fn a_file_that_is_not_a_memory_database_is_not_reported_as_an_empty_one() {
        let dir = tmpdir("notadb");
        fs::write(dir.join("garbage.db"), b"this is not sqlite").unwrap();
        let cfg = cfg_with(&dir, "configured.db");
        assert!(
            find(&cfg).dbs.is_empty(),
            "an unreadable .db must not be offered as a candidate"
        );
    }
}
