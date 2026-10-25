//! Reading a dump back: its statements, split by the engine's own parser; the
//! rows each table holds once it has run, read from the dump itself; and the
//! replay of those statements through one connection.
//!
//! Piping a dump into the tursodb shell splits it on lines, and `memory_text`
//! holds newlines: an arbitrary fraction of the rows lands, plus a misleading
//! "table … does not exist". The parser here is the one the engine runs, so a
//! statement ends exactly where the engine says it does -- never inside a
//! quoted value, a comment or a trigger body. Both dump formats, tursodb's
//! `.dump` and this binary's own, go through the same path.

use turso::Value;
use turso_parser::ast::{Cmd, CreateTableBody, InsertBody, Name, OneSelect, Stmt};
use turso_parser::parser::Parser;

use super::dump::ident;
use super::{TursoStore, map_err};
use crate::error::{Error, Result};

/// The engine's own objects: the sequence table tursodb's `.dump` writes for
/// each AUTOINCREMENT column, and refuses to run back ("Object name reserved
/// for internal use"). The engine recreates it with its parent table.
const INTERNAL: &str = "__turso_internal_";

/// Longest excerpt of a statement or an error in a report.
const EXCERPT: usize = 100;

/// A dump, read.
#[derive(Debug, Default)]
pub struct Dump<'a> {
    /// The statements to run, in order, exactly as the dump wrote them.
    pub statements: Vec<&'a str>,
    /// Every table the dump creates or writes, in order of first mention,
    /// with the rows it holds once the dump has run.
    pub tables: Vec<(String, u64)>,
    /// Statements left out because they write the engine's own bookkeeping,
    /// which it recreates by itself and refuses to have written.
    pub skipped: usize,
    /// Where the text stops being SQL, and why: a dump cut off mid-statement.
    pub unreadable: Option<String>,
}

/// Split a dump into statements and read off the rows it claims per table.
///
/// Refuses a dump that creates no table, and one with a statement whose
/// effect on a table's row count cannot be read from its text: the counts
/// are what the restore is verified against.
pub fn parse(sql: &str) -> Result<Dump<'_>> {
    let mut dump = Dump::default();
    let mut parser = Parser::new(sql.as_bytes());
    loop {
        // Each statement runs from where the last one ended, so together the
        // statements are the whole text, whitespace and all.
        let start = parser.offset();
        let cmd = match parser.next_cmd() {
            Ok(Some(cmd)) => cmd,
            Ok(None) => break,
            Err(e) => {
                dump.unreadable = Some(format!(
                    "the dump cannot be read from byte {start} on, where it is cut off \
                     or damaged: `{}` ({})",
                    excerpt(&sql[start..]),
                    excerpt(&e.to_string())
                ));
                break;
            }
        };
        let text = &sql[start..parser.offset()];
        match effect(&cmd) {
            Effect::Internal => {
                dump.skipped += 1;
                continue;
            }
            Effect::Uncountable => {
                return Err(Error::refused(format!(
                    "the dump's statement `{}` changes a table's rows in a way the dump's \
                     text cannot count, so a restore of it could not be verified — nothing \
                     was restored",
                    excerpt(text)
                )));
            }
            Effect::Creates(table) => {
                dump.rows(table);
            }
            Effect::Inserts(table, n) => *dump.rows(table) += n,
            Effect::Empties(table) => *dump.rows(table) = 0,
            Effect::Nothing => {}
        }
        dump.statements.push(text);
    }
    if dump.tables.is_empty() {
        return Err(Error::refused(match &dump.unreadable {
            Some(why) => format!("the dump creates no table — {why}"),
            None => "the dump creates no table, so it is not a database dump".to_string(),
        }));
    }
    Ok(dump)
}

impl Dump<'_> {
    /// The running row count of a table, registered on first mention. Table
    /// names are matched the way the engine matches them, ignoring ASCII case.
    fn rows(&mut self, table: String) -> &mut u64 {
        let i = match self
            .tables
            .iter()
            .position(|(t, _)| t.eq_ignore_ascii_case(&table))
        {
            Some(i) => i,
            None => {
                self.tables.push((table, 0));
                self.tables.len() - 1
            }
        };
        &mut self.tables[i].1
    }
}

/// What one statement does to the tables' row counts.
enum Effect {
    /// Neither creates a table nor changes how many rows one holds.
    Nothing,
    /// Writes the engine's own bookkeeping: left out.
    Internal,
    Creates(String),
    Inserts(String, u64),
    /// `DELETE FROM t` with no condition.
    Empties(String),
    /// Changes rows in a way its text does not tell: INSERT … SELECT, an
    /// INSERT that may replace, a conditional DELETE, a DROP.
    Uncountable,
}

fn effect(cmd: &Cmd) -> Effect {
    let Cmd::Stmt(stmt) = cmd else {
        return Effect::Nothing;
    };
    let internal = |name: &Name| name.as_str().starts_with(INTERNAL);
    let named = |name: &Name| name.as_str().to_string();
    match stmt {
        Stmt::CreateTable { tbl_name, body, .. } => {
            if internal(&tbl_name.name) {
                Effect::Internal
            } else if matches!(body, CreateTableBody::AsSelect(_)) {
                Effect::Uncountable
            } else {
                Effect::Creates(named(&tbl_name.name))
            }
        }
        Stmt::CreateIndex {
            idx_name, tbl_name, ..
        } if internal(&idx_name.name) || internal(tbl_name) => Effect::Internal,
        Stmt::Insert {
            tbl_name,
            or_conflict,
            body,
            ..
        } => {
            if internal(&tbl_name.name) {
                return Effect::Internal;
            }
            match (or_conflict, inserted_rows(body)) {
                (None, Some(n)) => Effect::Inserts(named(&tbl_name.name), n),
                _ => Effect::Uncountable,
            }
        }
        Stmt::Delete {
            tbl_name,
            where_clause,
            limit,
            ..
        } => {
            if internal(&tbl_name.name) {
                Effect::Internal
            } else if where_clause.is_none() && limit.is_none() {
                Effect::Empties(named(&tbl_name.name))
            } else {
                Effect::Uncountable
            }
        }
        Stmt::Update(update) if internal(&update.tbl_name.name) => Effect::Internal,
        Stmt::DropTable { tbl_name, .. } => {
            if internal(&tbl_name.name) {
                Effect::Internal
            } else {
                Effect::Uncountable
            }
        }
        _ => Effect::Nothing,
    }
}

/// The rows an INSERT adds, when its text says: a VALUES list, or DEFAULT
/// VALUES.
fn inserted_rows(body: &InsertBody) -> Option<u64> {
    match body {
        InsertBody::DefaultValues => Some(1),
        InsertBody::Select(select, None) if select.body.compounds.is_empty() => {
            match &select.body.select {
                OneSelect::Values(rows) => Some(rows.len() as u64),
                _ => None,
            }
        }
        InsertBody::Select(..) => None,
    }
}

/// The start of a statement or message, on one line.
fn excerpt(text: &str) -> String {
    let line = text.trim().lines().next().unwrap_or_default();
    match line.char_indices().nth(EXCERPT) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line.to_string(),
    }
}

/// What replaying a dump did.
#[derive(Debug, Default)]
pub struct Replayed {
    pub failures: Vec<Failure>,
    /// The dump opened a transaction it never committed -- it was cut off --
    /// and that transaction was rolled back.
    pub uncommitted: bool,
}

/// A statement the engine rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// Its position among the statements run, from 1.
    pub statement: usize,
    pub error: String,
    /// The start of the statement, on one line.
    pub excerpt: String,
}

impl TursoStore {
    /// Run every statement of the dump, in order, on this connection -- its
    /// own `BEGIN`/`COMMIT` included, so a dump that never reaches its
    /// `COMMIT` lands nothing. A rejected statement is recorded and the rest
    /// still run, so one report shows every failure.
    ///
    /// Nothing is retried: the file being restored into is new and private
    /// to this run, so there is no contention to wait out.
    pub fn replay(&self, dump: &Dump) -> Result<Replayed> {
        pollster::block_on(async {
            let mut replayed = Replayed::default();
            for (i, sql) in dump.statements.iter().enumerate() {
                if let Err(e) = self.conn.execute(*sql, ()).await {
                    replayed.failures.push(Failure {
                        statement: i + 1,
                        error: excerpt(&e.to_string()),
                        excerpt: excerpt(sql),
                    });
                }
            }
            if !self.conn.is_autocommit().map_err(map_err)? {
                self.conn.execute("ROLLBACK", ()).await.map_err(map_err)?;
                replayed.uncommitted = true;
            }
            Ok(replayed)
        })
    }

    /// Move everything in the write-ahead log into the main file, so the
    /// main file alone is the whole database.
    pub fn checkpoint(&self) -> Result<()> {
        pollster::block_on(async {
            let mut rows = self
                .conn
                .query("PRAGMA wal_checkpoint(TRUNCATE)", ())
                .await
                .map_err(map_err)?;
            // The first column is 1 when the checkpoint could not finish.
            match rows.next().await.map_err(map_err)? {
                Some(row) if row.get_value(0).map_err(map_err)? != Value::Integer(0) => Err(
                    Error::failed("the checkpoint of the restored database did not finish"),
                ),
                _ => Ok(()),
            }
        })
    }

    /// The rows a table holds, or why it could not be counted (`missing` when
    /// there is no such table).
    pub fn count_rows(&self, table: &str) -> std::result::Result<u64, String> {
        pollster::block_on(async {
            let mut rows = self
                .conn
                .query(format!("SELECT count(*) FROM {}", ident(table)), ())
                .await?;
            match rows.next().await? {
                Some(row) => row.get_value(0),
                None => Ok(Value::Null),
            }
        })
        .map_err(|e| {
            let msg = e.to_string();
            if msg.contains("no such table") {
                "missing".to_string()
            } else {
                excerpt(&msg)
            }
        })
        .and_then(|v| match v {
            Value::Integer(n) => Ok(n.max(0) as u64),
            other => Err(format!("counted as {other:?}")),
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::dump::tests::{dump_text, fixture};
    use super::super::tests::scratch;
    use super::*;
    use crate::config::Config;
    use crate::error::EXIT_REFUSED;

    /// A real `tursodb .dump` (Turso 0.7.2) of a database built from the
    /// schema plus: multi-line and quoted text, a row whose text mentions the
    /// internal sequence table, NULL and non-NULL embeddings in three tables,
    /// an `instructions`-shaped table with its own index, an AUTOINCREMENT
    /// table whose sequence is ahead of its rows, a view and a trigger. Each
    /// statement was passed to tursodb as one argument (it splits stdin on
    /// lines), then `tursodb <db> --experimental-multiprocess-wal .dump`.
    pub(crate) const TURSODB_DUMP: &str = include_str!("../../../tests/fixtures/tursodb-dump.sql");

    /// The rows each table of `TURSODB_DUMP` holds, in dump order.
    pub(crate) fn tursodb_dump_tables() -> Vec<(String, u64)> {
        [
            ("semantic_memory", 3),
            ("episodic_memory", 2),
            ("topic_keywords", 1),
            ("coworkers", 1),
            ("memory_coworkers", 1),
            ("appraisals", 0),
            ("eval_cases", 0),
            ("eval_runs", 0),
            ("instructions", 2),
            ("odd", 2),
            ("sqlite_sequence", 1),
        ]
        .iter()
        .map(|(t, n)| (t.to_string(), *n))
        .collect()
    }

    /// The vectors in `TURSODB_DUMP`: `((i % 10) + 1) / 10` for semantic row 1
    /// and instruction 2, `((i % 4) + 1) / 4` for episodic row 2. Every
    /// component is the nearest f32 to the decimal written there.
    pub(crate) fn tenths() -> Vec<f32> {
        (0..1024).map(|i| ((i % 10) + 1) as f32 / 10.0).collect()
    }

    pub(crate) fn quarters() -> Vec<f32> {
        (0..1024).map(|i| ((i % 4) + 1) as f32 / 4.0).collect()
    }

    /// A config pointing at an empty file, the way a restore starts.
    fn fresh(name: &str) -> Config {
        let mut cfg = scratch(name);
        cfg.db_path = cfg.db_path.with_file_name("fresh.db");
        std::fs::File::create_new(&cfg.db_path).unwrap();
        cfg
    }

    fn assert_refused(r: Result<Dump<'_>>, needle: &str) {
        let err = r.expect_err("refused");
        assert_eq!(err.exit_code(), EXIT_REFUSED, "{err}");
        let msg = err.to_string();
        assert!(msg.starts_with("refused: "), "{msg}");
        assert!(msg.contains(needle), "{needle:?} not in {msg}");
    }

    // ---- reading a dump ------------------------------------------------------

    /// The binary's own dump: the statements cover the text exactly, the
    /// trigger is one statement, no statement starts inside a value, and the
    /// rows per table are the ones the dump counted while writing them.
    #[test]
    fn a_binary_dump_splits_where_the_engine_does_and_counts_what_the_dump_wrote() {
        let cfg = fixture("restore-parse-binary");
        let mut text = Vec::new();
        let dumped = TursoStore::open(&cfg).unwrap().dump(&mut text).unwrap();
        let text = String::from_utf8(text).unwrap();

        let dump = parse(&text).unwrap();
        assert_eq!(dump.tables, dumped.tables);
        assert_eq!(dump.skipped, 0);
        assert_eq!(dump.unreadable, None);
        assert_eq!(
            dump.statements.concat(),
            text,
            "nothing lost, nothing twice"
        );
        assert!(
            dump.statements.iter().any(|s| s.trim()
                == "CREATE TRIGGER odd_values_touch AFTER UPDATE ON odd_values BEGIN SELECT 1; END;"),
            "{:?}",
            dump.statements.iter().filter(|s| s.contains("TRIGGER")).collect::<Vec<_>>()
        );
        for s in &dump.statements {
            let s = s.trim_start();
            assert!(
                ["PRAGMA", "BEGIN", "CREATE", "INSERT", "DELETE", "COMMIT"]
                    .iter()
                    .any(|verb| s.starts_with(verb)),
                "a statement starts inside a value: {s:?}"
            );
        }
    }

    /// tursodb's dump: the same reading, with the statements on the internal
    /// sequence table left out -- anchored to the object's name, so the row
    /// whose TEXT mentions that name is kept.
    #[test]
    fn a_tursodb_dump_is_read_with_its_internal_statements_left_out() {
        let dump = parse(TURSODB_DUMP).unwrap();
        assert_eq!(dump.tables, tursodb_dump_tables());
        assert_eq!(dump.skipped, 2);
        assert_eq!(dump.unreadable, None);
        for s in &dump.statements {
            for verb in ["CREATE TABLE", "INSERT INTO"] {
                assert!(
                    !s.trim_start()
                        .starts_with(&format!("{verb} \"__turso_internal")),
                    "{s}"
                );
            }
        }
        assert!(
            dump.statements
                .iter()
                .any(|s| s
                    .contains("mentions __turso_internal_seq___turso_internal_autoincrement_odd")),
            "a row that only mentions the name was dropped"
        );
        assert!(dump.statements.iter().any(
            |s| s.trim() == "CREATE TRIGGER odd_touch AFTER UPDATE ON odd BEGIN SELECT 1; END;"
        ));
    }

    /// Rows are counted from what each statement does -- a multi-row VALUES,
    /// DEFAULT VALUES, a DELETE of the whole table -- never from lines, and
    /// never from INSERT syntax inside a value.
    #[test]
    fn rows_are_counted_from_the_statements_not_from_the_lines() {
        let sql = "CREATE TABLE t (x);\n\
                   INSERT INTO t VALUES (1),(2),(3);\n\
                   INSERT INTO \"t\" VALUES ('a\nINSERT INTO t VALUES (9);\nb');\n\
                   INSERT INTO t DEFAULT VALUES;\n\
                   CREATE TABLE \"we\"\"ird\" (y);\n\
                   INSERT INTO \"we\"\"ird\" VALUES ('-- ; /* */');\n\
                   CREATE TABLE gone (z);\n\
                   INSERT INTO gone VALUES (1);\n\
                   DELETE FROM gone;\n\
                   DELETE FROM sqlite_sequence;\n\
                   INSERT INTO sqlite_sequence(name,seq) VALUES('t',5);\n";
        let dump = parse(sql).unwrap();
        assert_eq!(
            dump.tables,
            vec![
                ("t".to_string(), 5),
                ("we\"ird".to_string(), 1),
                ("gone".to_string(), 0),
                ("sqlite_sequence".to_string(), 1),
            ]
        );
        assert_eq!(dump.statements.len(), 11);
    }

    /// A dump cut off inside a statement -- inside a quoted value or outside
    /// one -- is read up to the last whole statement, and says where it stops.
    #[test]
    fn a_dump_cut_off_mid_statement_says_where_it_stops() {
        let at = TURSODB_DUMP
            .find("INSERT INTO \"episodic_memory\" VALUES(1,")
            .unwrap();
        for cut in [
            TURSODB_DUMP.find("over two lines").unwrap(),
            at + "INSERT INTO \"episodic_memory\" VALUES(1,".len(),
        ] {
            let dump = parse(&TURSODB_DUMP[..cut]).unwrap();
            let why = dump.unreadable.clone().expect("cut off");
            assert!(why.contains(&format!("byte {at}")), "{why}");
            assert!(why.contains("INSERT INTO \"episodic_memory\""), "{why}");
            assert_eq!(dump.statements.concat(), &TURSODB_DUMP[..at]);
            assert_eq!(
                dump.tables,
                vec![
                    ("semantic_memory".to_string(), 3),
                    ("episodic_memory".to_string(), 0)
                ]
            );
        }
    }

    /// The restore is verified against the dump's own counts, so a statement
    /// whose effect on a row count cannot be read from its text is refused.
    #[test]
    fn a_statement_whose_rows_cannot_be_counted_is_refused() {
        for stmt in [
            "INSERT INTO t SELECT * FROM t",
            "INSERT OR REPLACE INTO t VALUES (1)",
            "REPLACE INTO t VALUES (1)",
            "DELETE FROM t WHERE x = 1",
            "CREATE TABLE u AS SELECT * FROM t",
            "DROP TABLE t",
        ] {
            let sql = format!("CREATE TABLE t (x);\nINSERT INTO t VALUES (1);\n{stmt};\n");
            assert_refused(parse(&sql), stmt);
        }
    }

    /// No table, nothing to verify: an empty file, a dump of nothing, and a
    /// text that is not SQL at all are not database dumps.
    #[test]
    fn a_dump_that_creates_no_table_is_refused() {
        for sql in [
            "",
            "PRAGMA foreign_keys=OFF;\nBEGIN TRANSACTION;\nCOMMIT;\n",
            "this is not a dump; it is prose.",
        ] {
            assert_refused(parse(sql), "no table");
        }
    }

    // ---- replaying it ------------------------------------------------------

    /// Every statement runs; a rejected one is recorded with its position and
    /// its start, and the statements after it still run.
    #[test]
    fn replay_runs_every_statement_and_records_each_failure() {
        let cfg = fresh("restore-replay-failure");
        let sql = "BEGIN TRANSACTION;\n\
                   CREATE TABLE t (x CHECK (x < 10));\n\
                   INSERT INTO t VALUES (1);\n\
                   INSERT INTO t VALUES (99);\n\
                   INSERT INTO t VALUES (2);\n\
                   COMMIT;\n";
        let dump = parse(sql).unwrap();
        let store = TursoStore::open(&cfg).unwrap();
        let replayed = store.replay(&dump).unwrap();
        assert!(!replayed.uncommitted);
        assert_eq!(replayed.failures.len(), 1, "{replayed:?}");
        assert_eq!(replayed.failures[0].statement, 4);
        assert_eq!(replayed.failures[0].excerpt, "INSERT INTO t VALUES (99);");
        assert!(
            replayed.failures[0].error.to_lowercase().contains("check"),
            "{replayed:?}"
        );
        assert_eq!(store.count_rows("t"), Ok(2));
    }

    /// A dump cut off before its COMMIT lands nothing: the transaction it
    /// opened is rolled back, and its tables do not exist.
    #[test]
    fn a_transaction_the_dump_never_commits_is_rolled_back() {
        let cfg = fresh("restore-replay-uncommitted");
        let dump = parse(
            "PRAGMA foreign_keys=OFF;\nBEGIN TRANSACTION;\nCREATE TABLE t (x);\nINSERT INTO t VALUES (1);\n",
        )
        .unwrap();
        let store = TursoStore::open(&cfg).unwrap();
        let replayed = store.replay(&dump).unwrap();
        assert!(replayed.uncommitted);
        assert!(replayed.failures.is_empty(), "{replayed:?}");
        assert_eq!(store.count_rows("t"), Err("missing".to_string()));
        drop(store);
        assert_eq!(
            TursoStore::open(&cfg).unwrap().count_rows("t"),
            Err("missing".to_string())
        );
    }

    /// After the replay and a checkpoint, the main file alone is the whole
    /// database: copied away from its sidecars, it holds every row.
    #[test]
    fn after_a_checkpoint_the_main_file_alone_holds_every_row() {
        let source = fixture("restore-checkpoint-source");
        let text = dump_text(&source);
        let cfg = fresh("restore-checkpoint");
        let dump = parse(&text).unwrap();
        let store = TursoStore::open(&cfg).unwrap();
        let replayed = store.replay(&dump).unwrap();
        assert!(replayed.failures.is_empty(), "{replayed:?}");
        store.checkpoint().unwrap();
        drop(store);

        let wal = cfg.db_path.with_file_name("fresh.db-wal");
        assert_eq!(std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0), 0);
        let mut alone = cfg.clone();
        alone.db_path = cfg.db_path.with_file_name("alone.db");
        std::fs::copy(&cfg.db_path, &alone.db_path).unwrap();
        let store = TursoStore::open(&alone).unwrap();
        assert!(
            dump.tables
                .iter()
                .any(|(t, n)| t == "semantic_memory" && *n > 0),
            "{:?}",
            dump.tables
        );
        for (table, rows) in &dump.tables {
            assert_eq!(store.count_rows(table), Ok(*rows), "{table}");
        }
    }

    #[test]
    fn count_rows_says_missing_for_a_table_that_does_not_exist() {
        let cfg = scratch("restore-count");
        let store = TursoStore::open(&cfg).unwrap();
        assert_eq!(store.count_rows("semantic_memory"), Ok(0));
        assert_eq!(
            store.count_rows("no_such_table"),
            Err("missing".to_string())
        );
        assert_eq!(store.count_rows("we\"ird"), Err("missing".to_string()));
    }
}
