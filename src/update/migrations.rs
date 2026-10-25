//! What changed between releases, compiled into the binary: the migration notes
//! and the schema migrations, keyed by the version that introduced them.
//!
//! The source is `migrations/<version>/` in the repository: a `NOTE.md` (What
//! broke / How to resolve / Verification), plus an optional `<id>.sql` schema
//! migration beside it. The tables below embed them; a test asserts that every
//! directory is in them and nothing else is. The version is the key, so a note
//! can land in the same commit as the change it describes.

use turso_parser::ast::{Cmd, Stmt};
use turso_parser::parser::Parser;

use super::semver::Version;

/// One release's note, as written.
#[derive(Debug, Clone, Copy)]
pub struct Note {
    pub version: &'static str,
    pub text: &'static str,
}

/// A change to the database's schema, applied by `update -y` in one
/// transaction of its own.
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    /// The name it is reported by: the file name of its SQL.
    pub id: &'static str,
    /// The release that introduced it; its note is in the same directory.
    pub version: &'static str,
    /// One line for the plan.
    pub summary: &'static str,
    /// One or more statements, without transaction control: `update` wraps
    /// them in a transaction itself. A table rebuild (create, copy, drop,
    /// rename) is just more statements.
    pub sql: &'static str,
    pub probe: Probe,
}

/// Whether a migration is already in a database, asked of the database
/// itself. Not a counter: `tursodb .dump` does not carry `PRAGMA
/// user_version`, so a restored dump would read as never migrated. A probe
/// survives every dump and restore, and a `.env` pointing at another database.
#[derive(Debug, Clone, Copy)]
pub struct Probe {
    /// What holds once it is applied, for the plan.
    pub applied_when: &'static str,
    /// One integer: non-zero when applied.
    pub query: &'static str,
}

/// Every release's note, from `migrations/<version>/NOTE.md`.
pub const NOTES: &[Note] = &[Note {
    version: "0.1.0",
    text: include_str!("../../migrations/0.1.0/NOTE.md"),
}];

/// Every schema migration, from `migrations/<version>/<id>.sql`.
pub const MIGRATIONS: &[Migration] = &[Migration {
    id: "instructions",
    version: "0.1.0",
    summary: "creates the instructions table and its index",
    sql: include_str!("../../migrations/0.1.0/instructions.sql"),
    probe: Probe {
        applied_when: "table `instructions` exists",
        query: "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = 'instructions'",
    },
}];

/// A note's parts: its title, and the text after the title line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sections<'a> {
    pub title: &'a str,
    pub body: &'a str,
}

/// The sections every note must have, in this order.
pub const REQUIRED: [&str; 3] = ["## What broke", "## How to resolve", "## Verification"];

/// Check a note's shape: a `# ` title on its first line, then the three
/// required sections in order. A `commit:` line is refused: the version is
/// the key now, and a sha anchor left in would read as one to resolve.
pub fn parse_note(text: &str) -> Result<Sections<'_>, String> {
    if text.lines().any(|l| l.trim_start().starts_with("commit:")) {
        return Err("a `commit:` line: the version is the key, drop the sha anchor".into());
    }
    let (first, body) = text.split_once('\n').unwrap_or((text, ""));
    let Some(title) = first
        .strip_prefix("# ")
        .map(str::trim)
        .filter(|t| !t.is_empty())
    else {
        return Err("the first line must be the `# ` title".into());
    };

    // Headings outside fenced code blocks: a `## Verification` inside an
    // example is text.
    let mut fenced = false;
    let mut headings = Vec::new();
    for line in body.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
        } else if !fenced && line.starts_with("## ") {
            headings.push(line.trim_end());
        }
    }
    let mut at = Vec::new();
    for section in REQUIRED {
        match headings.iter().position(|h| *h == section) {
            Some(i) => at.push(i),
            None => return Err(format!("no `{section}` section")),
        }
    }
    if !at.windows(2).all(|w| w[0] < w[1]) {
        return Err(format!(
            "the sections are out of order; they go {}",
            REQUIRED.join(", ")
        ));
    }
    Ok(Sections { title, body })
}

/// The notes a machine has not seen: version above `installed`, up to and
/// including `own`, oldest first.
pub fn between<'a>(
    notes: &'a [Note],
    installed: &Version,
    own: &Version,
) -> Result<Vec<&'a Note>, String> {
    let mut picked = Vec::new();
    for note in notes {
        let version = Version::parse(note.version)?;
        if &version > installed && &version <= own {
            picked.push((version, note));
        }
    }
    picked.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(picked.into_iter().map(|(_, note)| note).collect())
}

/// A migration's SQL, split where the engine splits it. Transaction control
/// and VACUUM are refused: the statements run inside the transaction `update`
/// opens, and a COMMIT among them would end it half-way.
pub fn statements(sql: &str) -> Result<Vec<&str>, String> {
    let mut parser = Parser::new(sql.as_bytes());
    let mut out = Vec::new();
    loop {
        let start = parser.offset();
        let cmd = match parser.next_cmd() {
            Ok(Some(cmd)) => cmd,
            Ok(None) => break,
            Err(e) => return Err(format!("it does not parse from byte {start} on: {e}")),
        };
        let text = &sql[start..parser.offset()];
        let refused = match &cmd {
            Cmd::Stmt(Stmt::Begin { .. }) => Some("BEGIN"),
            Cmd::Stmt(Stmt::Commit { .. }) => Some("COMMIT"),
            Cmd::Stmt(Stmt::Rollback { .. }) => Some("ROLLBACK"),
            Cmd::Stmt(Stmt::Savepoint { .. }) => Some("SAVEPOINT"),
            Cmd::Stmt(Stmt::Release { .. }) => Some("RELEASE"),
            Cmd::Stmt(Stmt::Vacuum { .. }) => Some("VACUUM"),
            _ => None,
        };
        if let Some(word) = refused {
            return Err(format!(
                "{word} cannot run inside the migration's own transaction: `{}`",
                text.trim()
            ));
        }
        out.push(text);
    }
    if out.is_empty() {
        return Err("no statement to run".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::Path;

    use super::*;
    use crate::store::turso::TursoStore;
    use crate::store::turso::migrate::Migrated;
    use crate::store::turso::tests::scratch;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    const NOTE: &str = "# A title\n\n## What broke\n\nx\n\n## How to resolve\n\ny\n\n\
                        ## Verification\n\nz\n";

    // ---- the embedded tables -------------------------------------------------

    /// Every directory under `migrations/` is a note in the table, every SQL
    /// file in one is a migration in the table, with the same text -- and the
    /// tables hold nothing else.
    #[test]
    fn every_directory_under_migrations_is_in_the_tables_and_nothing_else() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
        let mut dirs = BTreeSet::new();
        let mut sql_files = BTreeSet::new();
        for entry in fs::read_dir(&root).unwrap() {
            let entry = entry.unwrap();
            assert!(entry.file_type().unwrap().is_dir(), "{:?}", entry.path());
            let version = entry.file_name().to_string_lossy().into_owned();
            for file in fs::read_dir(entry.path()).unwrap() {
                let name = file.unwrap().file_name().to_string_lossy().into_owned();
                let text = fs::read_to_string(entry.path().join(&name)).unwrap();
                if name == "NOTE.md" {
                    let note = NOTES
                        .iter()
                        .find(|n| n.version == version)
                        .unwrap_or_else(|| panic!("migrations/{version}/NOTE.md is not in NOTES"));
                    assert_eq!(note.text, text, "NOTES[{version}] embeds another file");
                } else if let Some(id) = name.strip_suffix(".sql") {
                    let m = MIGRATIONS
                        .iter()
                        .find(|m| m.id == id && m.version == version)
                        .unwrap_or_else(|| {
                            panic!("migrations/{version}/{name} is not in MIGRATIONS")
                        });
                    assert_eq!(m.sql, text, "MIGRATIONS[{id}] embeds another file");
                    sql_files.insert((version.clone(), id.to_string()));
                } else {
                    panic!("migrations/{version}/{name}: only NOTE.md and <id>.sql belong here");
                }
            }
            dirs.insert(version);
        }
        let notes: BTreeSet<String> = NOTES.iter().map(|n| n.version.to_string()).collect();
        assert_eq!(notes, dirs, "NOTES and the directories differ");
        assert_eq!(NOTES.len(), notes.len(), "a version is in NOTES twice");
        let migrations: BTreeSet<(String, String)> = MIGRATIONS
            .iter()
            .map(|m| (m.version.to_string(), m.id.to_string()))
            .collect();
        assert_eq!(migrations, sql_files, "MIGRATIONS and the SQL files differ");
        assert!(
            MIGRATIONS.iter().any(|m| m.id == "instructions"),
            "the first schema migration is the instructions table"
        );
    }

    /// The analog of UPDATE.md's MALFORMED: an unparsable version, a missing
    /// section, or a note above the release being built fails the build.
    #[test]
    fn every_embedded_note_parses_and_none_is_above_the_crate_version() {
        let crate_version = v(env!("CARGO_PKG_VERSION"));
        assert!(!NOTES.is_empty());
        for note in NOTES {
            let version = Version::parse(note.version)
                .unwrap_or_else(|e| panic!("NOTES[{}]: {e}", note.version));
            assert!(
                version <= crate_version,
                "the note for {version} is above the crate's own {crate_version}"
            );
            parse_note(note.text).unwrap_or_else(|e| panic!("NOTES[{}]: {e}", note.version));
        }
        let ids: BTreeSet<&str> = MIGRATIONS.iter().map(|m| m.id).collect();
        assert_eq!(ids.len(), MIGRATIONS.len(), "a migration id is used twice");
        for m in MIGRATIONS {
            assert!(
                NOTES.iter().any(|n| n.version == m.version),
                "{}: no note for {}",
                m.id,
                m.version
            );
            assert!(v(m.version) <= crate_version, "{}", m.id);
            assert!(!m.summary.is_empty() && !m.probe.applied_when.is_empty());
            let stmts = statements(m.sql).unwrap_or_else(|e| panic!("{}: {e}", m.id));
            assert!(!stmts.is_empty(), "{}", m.id);
        }
    }

    /// Run against a database built from today's schema, every migration whose
    /// probe does not hold yet makes it hold, and a second pass finds each one
    /// applied and runs nothing.
    #[test]
    fn the_embedded_migrations_bring_a_fresh_database_to_every_probe() {
        assert!(!MIGRATIONS.is_empty());
        let cfg = scratch("migrations-embedded");
        let store = TursoStore::open(&cfg).unwrap();
        for m in MIGRATIONS {
            if !store.holds(m.probe.query).unwrap() {
                let stmts = statements(m.sql).unwrap();
                assert_eq!(
                    store.migrate(m.probe.query, &stmts).unwrap(),
                    Migrated::Applied,
                    "{}",
                    m.id
                );
            }
            assert!(store.holds(m.probe.query).unwrap(), "{}", m.id);
        }
        for m in MIGRATIONS {
            let stmts = statements(m.sql).unwrap();
            assert_eq!(
                store.migrate(m.probe.query, &stmts).unwrap(),
                Migrated::AlreadyApplied,
                "{}",
                m.id
            );
        }
    }

    // ---- a note's shape ----------------------------------------------------

    #[test]
    fn a_note_has_a_title_and_the_three_sections_in_order() {
        let s = parse_note(NOTE).unwrap();
        assert_eq!(s.title, "A title");
        assert!(s.body.starts_with("\n## What broke"), "{:?}", s.body);
        assert!(s.body.ends_with("z\n"));

        for (broken, why) in [
            ("", "title"),
            (
                "## What broke\n## How to resolve\n## Verification\n",
                "title",
            ),
            ("# T\n## How to resolve\n## Verification\n", "## What broke"),
            ("# T\n## What broke\n## Verification\n", "## How to resolve"),
            ("# T\n## What broke\n## How to resolve\n", "## Verification"),
            (
                "# T\n## How to resolve\n## What broke\n## Verification\n",
                "order",
            ),
            (
                "commit: abc1234\n\n# T\n## What broke\n## How to resolve\n## Verification\n",
                "commit:",
            ),
        ] {
            let err = parse_note(broken).expect_err(broken);
            assert!(err.contains(why), "{why:?} not in {err:?} for {broken:?}");
        }
        // A heading inside a fenced block is text, not a section.
        let fenced = "# T\n## What broke\n```\n## Verification\n```\n## How to resolve\n";
        assert!(parse_note(fenced).unwrap_err().contains("## Verification"));
    }

    // ---- which notes a machine has not seen ------------------------------------

    fn fixture_notes() -> Vec<Note> {
        // Out of order on purpose: the order printed is the versions', not the
        // table's.
        ["0.3.0", "0.1.0", "0.2.0"]
            .into_iter()
            .map(|version| Note {
                version,
                text: NOTE,
            })
            .collect()
    }

    fn versions(notes: &[&Note]) -> Vec<&'static str> {
        notes.iter().map(|n| n.version).collect()
    }

    #[test]
    fn the_notes_above_installed_up_to_own_oldest_first() {
        let notes = fixture_notes();
        let cases: &[(&str, &str, &[&str])] = &[
            ("0.1.0", "0.3.0", &["0.2.0", "0.3.0"]),
            ("0.0.9", "0.3.0", &["0.1.0", "0.2.0", "0.3.0"]),
            ("0.1.0", "0.2.0", &["0.2.0"]),
            ("0.3.0", "0.3.0", &[]),
            ("0.2.0-rc.1", "0.3.0", &["0.2.0", "0.3.0"]),
            ("0.1.0+build", "0.3.0", &["0.2.0", "0.3.0"]),
        ];
        for (installed, own, want) in cases {
            let got = between(&notes, &v(installed), &v(own)).unwrap();
            assert_eq!(versions(&got), *want, "{installed} -> {own}");
        }
    }

    #[test]
    fn a_note_with_an_unparsable_version_is_an_error() {
        let notes = [Note {
            version: "0.2",
            text: NOTE,
        }];
        let err = between(&notes, &v("0.1.0"), &v("0.3.0")).unwrap_err();
        assert!(err.contains("\"0.2\""), "{err}");
    }

    // ---- a migration's statements --------------------------------------------

    #[test]
    fn statements_split_where_the_engine_does() {
        let sql = "-- a comment; with a semicolon\n\
                   CREATE TABLE a (x TEXT DEFAULT 'it''s; fine');\n\
                   CREATE INDEX ia ON a (x);\n";
        let got = statements(sql).unwrap();
        assert_eq!(got.len(), 2, "{got:?}");
        assert!(got[0].contains("CREATE TABLE a"), "{got:?}");
        assert!(got[1].trim().starts_with("CREATE INDEX ia"), "{got:?}");
    }

    #[test]
    fn transaction_control_and_vacuum_are_refused_in_a_migration() {
        for (sql, word) in [
            ("BEGIN; CREATE TABLE a (x);", "BEGIN"),
            ("CREATE TABLE a (x); COMMIT;", "COMMIT"),
            ("CREATE TABLE a (x); ROLLBACK;", "ROLLBACK"),
            ("SAVEPOINT s; CREATE TABLE a (x);", "SAVEPOINT"),
            ("RELEASE s;", "RELEASE"),
            ("VACUUM;", "VACUUM"),
        ] {
            let err = statements(sql).expect_err(sql);
            assert!(err.contains(word), "{word} not in {err:?}");
        }
        assert!(statements("").unwrap_err().contains("no statement"));
        assert!(
            statements("-- only a comment\n")
                .unwrap_err()
                .contains("no statement")
        );
        assert!(statements("CREATE TABLE (").is_err());
    }
}
