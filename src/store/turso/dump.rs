//! A native SQL dump, in the shape `tursodb .dump` writes, so whatever restores
//! tursodb's dumps restores these too.
//!
//! `.dump` is a command of the tursodb shell, not of the driver, so it is
//! reimplemented here: every object enumerated from `sqlite_schema`, rows as
//! literals that keep both their value and their type, all of it read inside
//! one transaction.

use std::io::Write;

use turso::Value;

use super::{TursoStore, map_err};
use crate::error::{Error, Result};

/// What a dump wrote: the rows of each table, in dump order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Dumped {
    pub tables: Vec<(String, u64)>,
}

impl Dumped {
    pub fn rows(&self) -> u64 {
        self.tables.iter().map(|(_, n)| n).sum()
    }
}

/// One entry of `sqlite_schema`.
struct Object {
    kind: String,
    name: String,
    table: String,
    sql: Option<String>,
}

impl Object {
    /// The engine's own bookkeeping, which a restore recreates by itself.
    /// tursodb dumps the sequence table behind an AUTOINCREMENT column and then
    /// refuses to run its own output ("Object name reserved for internal use").
    /// `sqlite_sequence` is not in here: its rows are dumped on their own, so a
    /// restored AUTOINCREMENT table does not hand out an id used before.
    fn internal(&self) -> bool {
        self.name.starts_with("sqlite_")
            || self.name.starts_with("__turso_internal_")
            || self.table.starts_with("__turso_internal_")
    }
}

const SEQUENCE: &str = "sqlite_sequence";

impl TursoStore {
    /// Write the whole database to `out` as SQL, from one read snapshot.
    ///
    /// A reader under multiprocess WAL: writers are not blocked while it runs,
    /// and what it writes is the database as it stood when the dump began. Each
    /// table's rows are re-counted inside that snapshot against the rows
    /// written, and a mismatch fails the dump. Nothing is retried: contention
    /// does not block a reader here, and any other failure would only repeat.
    pub fn dump(&self, out: &mut dyn Write) -> Result<Dumped> {
        pollster::block_on(async {
            // Deferred: the first read below takes the snapshot, and it holds
            // until the transaction ends. Nothing was written through it, so it
            // is rolled back rather than committed.
            let tx = self.conn.unchecked_transaction().await.map_err(map_err)?;
            let dumped = self.dump_snapshot(out).await;
            let ended = tx.rollback().await.map_err(map_err);
            let dumped = dumped?;
            ended?;
            Ok(dumped)
        })
    }

    async fn dump_snapshot(&self, out: &mut dyn Write) -> Result<Dumped> {
        let schema = self.schema_objects().await?;
        if !schema.iter().any(|o| o.kind == "table" && !o.internal()) {
            return Err(Error::failed(
                "the database holds no tables, so it is not a memory database — nothing was dumped",
            ));
        }

        write(out, "PRAGMA foreign_keys=OFF;\nBEGIN TRANSACTION;\n")?;
        let mut dumped = Dumped::default();
        for o in schema.iter().filter(|o| o.kind == "table" && !o.internal()) {
            if let Some(sql) = &o.sql {
                statement(out, sql)?;
            }
            dumped.tables.push(self.dump_rows(&o.name, out).await?);
        }
        if schema
            .iter()
            .any(|o| o.kind == "table" && o.name == SEQUENCE)
        {
            write(out, "DELETE FROM sqlite_sequence;\n")?;
            dumped.tables.push(self.dump_rows(SEQUENCE, out).await?);
        }
        // Indexes, views and triggers after every table and its rows, in the
        // order they were created, so each one finds what it refers to.
        for o in schema.iter().filter(|o| o.kind != "table" && !o.internal()) {
            if let Some(sql) = &o.sql {
                statement(out, sql)?;
            }
        }
        write(out, "COMMIT;\n")?;
        Ok(dumped)
    }

    async fn schema_objects(&self) -> Result<Vec<Object>> {
        let mut rows = self
            .conn
            .query("SELECT type, name, tbl_name, sql FROM sqlite_schema", ())
            .await
            .map_err(map_err)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(map_err)? {
            let text = |i: usize| -> Result<Option<String>> {
                Ok(match row.get_value(i).map_err(map_err)? {
                    Value::Text(s) => Some(s),
                    _ => None,
                })
            };
            out.push(Object {
                kind: text(0)?.unwrap_or_default(),
                name: text(1)?.unwrap_or_default(),
                table: text(2)?.unwrap_or_default(),
                sql: text(3)?,
            });
        }
        Ok(out)
    }

    /// One INSERT per row, then the re-count against the same snapshot.
    async fn dump_rows(&self, table: &str, out: &mut dyn Write) -> Result<(String, u64)> {
        let mut rows = self
            .conn
            .query(format!("SELECT * FROM {}", ident(table)), ())
            .await
            .map_err(map_err)?;
        let columns = rows.column_names().len();
        let prefix = format!("INSERT INTO {} VALUES(", ident(table));
        let mut written = 0u64;
        let mut line = String::new();
        while let Some(row) = rows.next().await.map_err(map_err)? {
            line.clear();
            line.push_str(&prefix);
            for i in 0..columns {
                if i > 0 {
                    line.push(',');
                }
                line.push_str(&literal(&row.get_value(i).map_err(map_err)?));
            }
            line.push_str(");\n");
            write(out, &line)?;
            written += 1;
        }

        let mut rows = self
            .conn
            .query(format!("SELECT count(*) FROM {}", ident(table)), ())
            .await
            .map_err(map_err)?;
        let counted = match rows.next().await.map_err(map_err)? {
            Some(row) => match row.get_value(0).map_err(map_err)? {
                Value::Integer(n) => n.max(0) as u64,
                other => {
                    return Err(Error::failed(format!(
                        "counting {table} returned {other:?}"
                    )));
                }
            },
            None => 0,
        };
        check_count(table, written, counted)?;
        Ok((table.to_string(), written))
    }
}

fn write(out: &mut dyn Write, text: &str) -> Result<()> {
    out.write_all(text.as_bytes())
        .map_err(|e| Error::failed(format!("writing the dump failed: {e}")))
}

/// A schema object's SQL as one statement. The schema stores it without the
/// terminating semicolon.
fn statement(out: &mut dyn Write, sql: &str) -> Result<()> {
    write(out, sql.trim_end().trim_end_matches(';'))?;
    write(out, ";\n")
}

/// A value as an SQL literal that reads back as the same value of the same
/// type: a REAL keeps its decimal point or exponent (`1.0`, never `1`), text
/// is quoted with only `'` doubled -- newlines stay as they are -- and a blob,
/// an embedding included, is hex. NULL stays NULL, never an empty blob.
fn literal(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::Integer(n) => n.to_string(),
        // The engine stores no NaN; it would be a NULL.
        Value::Real(x) if x.is_nan() => "NULL".to_string(),
        // What sqlite3's own dump writes: a literal that overflows to infinity.
        Value::Real(x) if x.is_infinite() => if *x > 0.0 { "1e999" } else { "-1e999" }.to_string(),
        // Debug is the shortest text that parses back to the same f64, and
        // always carries a `.` or an exponent.
        Value::Real(x) => format!("{x:?}"),
        Value::Text(s) => format!("'{}'", s.replace('\'', "''")),
        Value::Blob(b) => {
            const HEX: &[u8; 16] = b"0123456789abcdef";
            let mut s = String::with_capacity(b.len() * 2 + 3);
            s.push_str("X'");
            for byte in b {
                s.push(HEX[(byte >> 4) as usize] as char);
                s.push(HEX[(byte & 0x0f) as usize] as char);
            }
            s.push('\'');
            s
        }
    }
}

fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The rows written must be the rows the snapshot holds.
fn check_count(table: &str, written: u64, counted: u64) -> Result<()> {
    if written == counted {
        return Ok(());
    }
    Err(Error::failed(format!(
        "the dump of {table} has {written} rows but the table holds {counted} — the dump is incomplete"
    )))
}

#[cfg(test)]
pub(crate) mod tests {
    use turso::Builder;

    use super::super::tests::{put, scratch};
    use super::super::vector_literal;
    use super::*;
    use crate::config::Config;
    use crate::embed::fake::Fake;
    use crate::error::EXIT_ERROR;
    use crate::store::{Store, Table};

    /// Run several statements against a database, outside the dump.
    pub(crate) fn batch(cfg: &Config, sql: &str) {
        let store = TursoStore::open(cfg).unwrap();
        pollster::block_on(store.conn.execute_batch(sql)).unwrap();
    }

    /// Restore a dump into a fresh file beside the configured database, the way
    /// a driver can: the whole text in one batch, parsed by the engine rather
    /// than split on lines. Returns a config pointing at the restored file.
    pub(crate) fn restore(cfg: &Config, sql: &str, name: &str) -> Config {
        let mut restored = cfg.clone();
        restored.db_path = cfg.db_path.with_file_name(format!("{name}.db"));
        let p = restored.db_path.to_string_lossy().to_string();
        assert!(!restored.db_path.exists(), "restore needs a fresh file");
        pollster::block_on(async {
            let db = Builder::new_local(&p)
                .experimental_multiprocess_wal(true)
                .build()
                .await
                .unwrap();
            db.connect().unwrap().execute_batch(sql).await.unwrap();
        });
        restored
    }

    /// Dump a database to text.
    pub(crate) fn dump_text(cfg: &Config) -> String {
        let mut out = Vec::new();
        TursoStore::open(cfg).unwrap().dump(&mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    /// Every row of a table, typed, in scan order.
    fn table_rows(cfg: &Config, table: &str) -> Vec<Vec<Value>> {
        let store = TursoStore::open(cfg).unwrap();
        pollster::block_on(async {
            let mut rows = store
                .conn
                .query(format!("SELECT * FROM \"{table}\""), ())
                .await
                .unwrap();
            let n = rows.column_names().len();
            let mut out = Vec::new();
            while let Some(row) = rows.next().await.unwrap() {
                out.push((0..n).map(|i| row.get_value(i).unwrap()).collect());
            }
            out
        })
    }

    /// The schema as the database records it: type, name, table, SQL.
    fn schema(cfg: &Config) -> Vec<Vec<Value>> {
        let store = TursoStore::open(cfg).unwrap();
        pollster::block_on(async {
            let mut rows = store
                .conn
                .query(
                    "SELECT type, name, tbl_name, sql FROM sqlite_schema ORDER BY type, name",
                    (),
                )
                .await
                .unwrap();
            let mut out = Vec::new();
            while let Some(row) = rows.next().await.unwrap() {
                out.push((0..4).map(|i| row.get_value(i).unwrap()).collect());
            }
            out
        })
    }

    fn tables(cfg: &Config) -> Vec<String> {
        schema(cfg)
            .into_iter()
            .filter(|r| r[0] == Value::Text("table".into()))
            .map(|r| match &r[1] {
                Value::Text(t) => t.clone(),
                other => panic!("{other:?}"),
            })
            .collect()
    }

    /// Text that has broken dumps and restores before: newlines (the original
    /// bug -- the CLI split its input on lines), quotes, statement and comment
    /// syntax inside a value, the internal table's name inside a value, and
    /// non-ASCII text.
    const TRICKY: &[&str] = &[
        "line one\nline 'two' \"dq\" über; end",
        "CRLF\r\nand a lone CR\rand a tab\there",
        "it''s doubled already '' and a backslash \\ and a literal \\n",
        "-- not a comment; /* nor this */ INSERT INTO \"semantic_memory\" VALUES(1);",
        "mentions __turso_internal_seq___turso_internal_autoincrement_x on purpose",
        "emoji 🧠, CJK 記憶, ß and ÄÖÜ",
        "ends with a semicolon;",
        "'",
    ];

    /// Every shape a dump has to carry: the memory tables with and without
    /// embeddings, a table the dump has never heard of (shaped like the coming
    /// `instructions` table) with an index of its own, an AUTOINCREMENT table
    /// whose sequence is ahead of its rows, values whose type a careless literal
    /// would change, a view and a trigger.
    pub(crate) fn fixture(name: &str) -> Config {
        let cfg = scratch(name);
        for text in TRICKY {
            put(&cfg, Table::Semantic, text, true);
        }
        put(&cfg, Table::Semantic, "stored without a vector", false);
        put(&cfg, Table::Episodic, "an event\nover two lines", false);
        put(&cfg, Table::Episodic, "an embedded event", true);
        batch(
            &cfg,
            &format!(
                "INSERT INTO topic_keywords (topic, keywords, updated_at) \
                     VALUES ('backup', 'dump, gzip', '2026-01-01 00:00:00');
                 INSERT INTO coworkers (id, name, expertise, personality) \
                     VALUES (1, 'reviewer', 'rust', 'terse');
                 INSERT INTO memory_coworkers VALUES ('semantic', 1, 1);
                 INSERT INTO appraisals (coworker_id, trust_level, memory_text) \
                     VALUES (1, 'trusted', 'fine work');
                 INSERT INTO eval_cases (id, class, memory_table, query, expect_ids, expect_stamps) \
                     VALUES ('x01', 'exact-id', 'semantic', 'what''s up', '1,2', 'a,b');
                 INSERT INTO eval_runs (alpha, embed_model, n_cases, r1, r5, mrr) \
                     VALUES (0.15, 'bge-m3', 32, 0.1, 1.0, 0.3333333333333333);
                 CREATE TABLE instructions (id INTEGER PRIMARY KEY, \
                     created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, \
                     memory_text TEXT NOT NULL, embed_model TEXT, \
                     embedding F32_BLOB(1024), retired_at TEXT);
                 CREATE INDEX idx_instructions_live ON instructions (retired_at);
                 INSERT INTO instructions (memory_text, embed_model, embedding) \
                     VALUES ('always\nrun the tests; then ''lint''', 'bge-m3', {v});
                 INSERT INTO instructions (memory_text) VALUES ('no vector here');
                 CREATE TABLE odd_values (id INTEGER PRIMARY KEY AUTOINCREMENT, anything, r REAL, b BLOB);
                 INSERT INTO odd_values (anything, r, b) VALUES (9223372036854775807, 1.0, X'');
                 INSERT INTO odd_values (anything, r, b) VALUES (-9223372036854775807 - 1, 1e-50, X'00ff10');
                 INSERT INTO odd_values (anything, r, b) VALUES ('007', -2.5e90, NULL);
                 INSERT INTO odd_values (anything, r, b) VALUES (1.0, 0.1, X'27');
                 INSERT INTO odd_values (anything, r, b) VALUES (X'0a3b27', 1e308 * 10, 'text in a blob column');
                 INSERT INTO odd_values (anything) VALUES ('deleted, so the sequence is ahead');
                 DELETE FROM odd_values WHERE id = 6;
                 CREATE VIEW current_semantic AS \
                     SELECT id, topic FROM semantic_memory WHERE superseded_by IS NULL;
                 CREATE TRIGGER odd_values_touch AFTER UPDATE ON odd_values BEGIN SELECT 1; END;",
                v = vector_literal(&Fake::vector("an instruction")),
            ),
        );
        cfg
    }

    // ---- the round trip ----------------------------------------------------

    /// Restored, every table holds the same typed values in the same order, and
    /// dumping the restored database gives back the dump byte for byte.
    #[test]
    fn a_dump_restores_to_the_same_rows_and_dumps_back_byte_for_byte() {
        let cfg = fixture("dump-round-trip");
        let first = dump_text(&cfg);
        let restored = restore(&cfg, &first, "restored");

        assert_eq!(tables(&restored), tables(&cfg));
        for table in tables(&cfg) {
            assert_eq!(
                table_rows(&restored, &table),
                table_rows(&cfg, &table),
                "{table}"
            );
        }
        assert_eq!(schema(&restored), schema(&cfg));
        assert_eq!(dump_text(&restored), first);
    }

    /// A row stored without an embedding comes back with NULL in both columns:
    /// not an empty blob, not a zero vector.
    #[test]
    fn rows_without_an_embedding_restore_with_null_not_an_empty_blob() {
        let cfg = fixture("dump-null-embeddings");
        let restored = restore(&cfg, &dump_text(&cfg), "restored");
        let nulls = |c: &Config, table: &str| {
            table_rows(c, table)
                .iter()
                .map(|r| r.iter().map(|v| *v == Value::Null).collect::<Vec<_>>())
                .collect::<Vec<_>>()
        };
        for table in ["semantic_memory", "episodic_memory", "instructions"] {
            assert_eq!(nulls(&restored, table), nulls(&cfg, table), "{table}");
        }
        let unembedded = |c: &Config| {
            super::super::tests::rows(
                c,
                "SELECT count(*) FROM semantic_memory \
                 WHERE embedding IS NULL AND embed_model IS NULL",
            )
        };
        assert_eq!(unembedded(&restored), unembedded(&cfg));
        assert_eq!(
            unembedded(&cfg),
            vec![vec![crate::store::Cell::Int(1)]],
            "the fixture has one"
        );
        // An embedded row's vector survives exactly.
        assert!(
            super::super::tests::distance(&restored, Table::Semantic, 1, &Fake::vector(TRICKY[0]))
                < 1e-6
        );
    }

    /// Enumerated from the schema, so a table the dump has never heard of is
    /// carried with its index; the engine's own objects are left out.
    #[test]
    fn every_table_index_view_and_trigger_is_dumped_and_internal_ones_are_not() {
        let cfg = fixture("dump-enumerated");
        let text = dump_text(&cfg);
        for want in [
            "CREATE TABLE instructions",
            "CREATE INDEX idx_instructions_live",
            "INSERT INTO \"instructions\" VALUES(",
            "CREATE VIEW current_semantic",
            "CREATE TRIGGER odd_values_touch",
            "INSERT INTO \"odd_values\" VALUES(",
            "DELETE FROM sqlite_sequence;",
        ] {
            assert!(
                text.lines().any(|l| l.starts_with(want)),
                "no line starting {want:?}"
            );
        }
        // tursodb emits the AUTOINCREMENT sequence table and then refuses its
        // own output ("Object name reserved for internal use"). Only a row's
        // TEXT may mention the name.
        for line in text.lines() {
            for verb in ["CREATE TABLE", "INSERT INTO", "CREATE INDEX"] {
                assert!(
                    !line.starts_with(&format!("{verb} \"__turso_internal"))
                        && !line.starts_with(&format!("{verb} __turso_internal")),
                    "{line}"
                );
            }
        }
        assert!(text.contains("__turso_internal_seq___turso_internal_autoincrement_x"));
        // Indexes the engine creates for a UNIQUE or PRIMARY KEY have no SQL.
        assert!(!text.contains("sqlite_autoindex"));
        assert!(text.starts_with("PRAGMA foreign_keys=OFF;\nBEGIN TRANSACTION;\n"));
        assert!(text.ends_with("COMMIT;\n"));
    }

    /// The sequence of an AUTOINCREMENT table survives, so a restored database
    /// does not hand out an id that was used before.
    #[test]
    fn an_autoincrement_table_keeps_counting_after_a_restore() {
        let cfg = fixture("dump-sequence");
        let restored = restore(&cfg, &dump_text(&cfg), "restored");
        batch(
            &restored,
            "INSERT INTO odd_values (anything) VALUES ('next');",
        );
        assert_eq!(
            super::super::tests::rows(&restored, "SELECT max(id) FROM odd_values"),
            vec![vec![crate::store::Cell::Int(7)]]
        );
    }

    #[test]
    fn the_dump_reports_the_rows_of_every_table_in_dump_order() {
        let cfg = fixture("dump-counts");
        let dumped = TursoStore::open(&cfg)
            .unwrap()
            .dump(&mut Vec::new())
            .unwrap();
        let get = |t: &str| {
            dumped
                .tables
                .iter()
                .find(|(n, _)| n == t)
                .map(|(_, c)| *c)
                .unwrap_or_else(|| panic!("{t} not reported: {dumped:?}"))
        };
        assert_eq!(get("semantic_memory"), TRICKY.len() as u64 + 1);
        assert_eq!(get("episodic_memory"), 2);
        assert_eq!(get("instructions"), 2);
        assert_eq!(get("odd_values"), 5);
        assert_eq!(get("eval_cases"), 1);
        assert_eq!(get("sqlite_sequence"), 1);
        assert_eq!(dumped.tables[0].0, "semantic_memory");
        assert!(
            dumped.tables.iter().all(|(t, _)| !t.starts_with("__turso")),
            "{dumped:?}"
        );
        assert_eq!(
            dumped.rows(),
            dumped.tables.iter().map(|(_, c)| c).sum::<u64>()
        );
    }

    /// A database with no rows dumps its schema, and that restores to a
    /// database with the same schema and no rows. The shell script refused
    /// exactly this case, because it looked for an INSERT.
    #[test]
    fn an_empty_database_dumps_its_schema_and_restores_empty() {
        let cfg = scratch("dump-empty");
        let text = dump_text(&cfg);
        assert!(!text.contains("INSERT INTO"), "{text}");
        assert!(text.contains("CREATE TABLE semantic_memory"), "{text}");
        let restored = restore(&cfg, &text, "restored");
        assert_eq!(schema(&restored), schema(&cfg));
        assert_eq!(
            TursoStore::open(&restored)
                .unwrap()
                .total_memories()
                .unwrap(),
            0
        );
        assert_eq!(dump_text(&restored), text);
    }

    // ---- one snapshot, never blocking a writer ------------------------------

    /// A sink that runs `during` once, the first time `trigger` appears in what
    /// the dump has written -- that is, while the dump's read is open.
    struct Interrupting<F: FnMut()> {
        buf: Vec<u8>,
        trigger: &'static str,
        during: Option<F>,
    }

    impl<F: FnMut()> Write for Interrupting<F> {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.buf.extend_from_slice(b);
            if self.during.is_some() && String::from_utf8_lossy(&self.buf).contains(self.trigger) {
                (self.during.take().unwrap())();
            }
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn insert_without_retry(cfg: &Config, sql: &str) -> std::result::Result<u64, turso::Error> {
        let other = TursoStore::open(cfg).unwrap();
        pollster::block_on(other.conn.execute(sql, ()))
    }

    /// Another process writes while the dump is half way through: the write
    /// succeeds on its first attempt -- no retry, no busy -- and the dump is
    /// the database as it was when it started, its re-count included.
    #[test]
    fn a_concurrent_writer_is_not_blocked_and_the_dump_is_one_point_in_time() {
        let cfg = fixture("dump-concurrent");
        let writer_cfg = cfg.clone();
        let mut results = Vec::new();
        let mut sink = Interrupting {
            buf: Vec::new(),
            trigger: "INSERT INTO \"semantic_memory\" VALUES(",
            during: Some(|| {
                let c = writer_cfg.clone();
                // A thread of its own, as another process would be.
                let handle = std::thread::spawn(move || {
                    [
                        insert_without_retry(
                            &c,
                            "INSERT INTO episodic_memory (event_type, importance, memory_text) \
                             VALUES ('note', 'routine', 'written mid-dump, episodic')",
                        ),
                        insert_without_retry(
                            &c,
                            "INSERT INTO semantic_memory (category, memory_text) \
                             VALUES ('reference', 'written mid-dump, semantic')",
                        ),
                    ]
                });
                results.extend(handle.join().unwrap());
            }),
        };
        let dumped = TursoStore::open(&cfg).unwrap().dump(&mut sink).unwrap();
        let text = String::from_utf8(sink.buf).unwrap();

        assert_eq!(results.len(), 2, "the writer ran during the dump");
        for r in &results {
            assert!(matches!(r, Ok(1)), "the writer was blocked: {r:?}");
        }
        assert!(!text.contains("written mid-dump"), "not one snapshot");
        assert_eq!(
            dumped.tables[0],
            ("semantic_memory".into(), TRICKY.len() as u64 + 1)
        );
        // The writes are there for the next reader.
        assert_eq!(
            super::super::tests::rows(
                &cfg,
                "SELECT count(*) FROM semantic_memory WHERE memory_text LIKE 'written mid-dump%'"
            ),
            vec![vec![crate::store::Cell::Int(1)]]
        );
        assert!(dump_text(&cfg).contains("written mid-dump, episodic"));
    }

    /// A writer holding its transaction open does not stop the dump either,
    /// and its uncommitted row is not in it.
    #[test]
    fn an_open_write_transaction_elsewhere_does_not_stop_the_dump() {
        let cfg = fixture("dump-open-writer");
        let other = TursoStore::open(&cfg).unwrap();
        pollster::block_on(async {
            other.conn.execute("BEGIN IMMEDIATE", ()).await.unwrap();
            other
                .conn
                .execute(
                    "INSERT INTO semantic_memory (category, memory_text) \
                     VALUES ('reference', 'not committed yet')",
                    (),
                )
                .await
                .unwrap();
        });
        let mut out = Vec::new();
        let dumped = TursoStore::open(&cfg).unwrap().dump(&mut out);
        assert!(dumped.is_ok(), "{dumped:?}");
        assert!(
            !String::from_utf8(out)
                .unwrap()
                .contains("not committed yet")
        );
        pollster::block_on(other.conn.execute("COMMIT", ())).unwrap();
    }

    /// Turso 0.7.2 parses a float literal beyond roughly 1e-75 and 1e100 to a
    /// neighbouring double -- measured on 200,000 random doubles. Inside that
    /// range, where every REAL this schema stores lies, the round trip is
    /// exact (the fixture above); outside it, a restore lands at most one unit
    /// in the last place away. The same parser reads tursodb's own dumps.
    #[test]
    fn a_real_beyond_the_parsers_exact_range_restores_within_one_ulp() {
        let cfg = scratch("dump-extreme-reals");
        batch(&cfg, "CREATE TABLE reals (r REAL);");
        let extreme = [1e-300, -2.1452846540129615e-305, 1.7375623958024647e125];
        let store = TursoStore::open(&cfg).unwrap();
        for x in extreme {
            // Bound, not written as text, so the stored value is exactly x.
            pollster::block_on(
                store
                    .conn
                    .execute("INSERT INTO reals VALUES (?)", vec![Value::Real(x)]),
            )
            .unwrap();
        }
        let restored = restore(&cfg, &dump_text(&cfg), "restored");
        let back: Vec<Value> = table_rows(&restored, "reals")
            .into_iter()
            .map(|mut r| r.remove(0))
            .collect();
        for (x, got) in extreme.iter().zip(back) {
            let Value::Real(y) = got else {
                panic!("{x}: {got:?}")
            };
            assert!(
                x.to_bits().abs_diff(y.to_bits()) <= 1,
                "{x:?} came back as {y:?}"
            );
        }
    }

    // ---- literals and the re-count -----------------------------------------

    #[test]
    fn literals_keep_the_value_and_its_type() {
        let cases: &[(Value, &str)] = &[
            (Value::Null, "NULL"),
            (Value::Integer(42), "42"),
            (Value::Integer(-7), "-7"),
            (Value::Real(1.0), "1.0"),
            (Value::Real(0.1), "0.1"),
            (Value::Real(-2.5e-300), "-2.5e-300"),
            (Value::Real(1e300), "1e300"),
            (Value::Real(f64::INFINITY), "1e999"),
            (Value::Real(f64::NEG_INFINITY), "-1e999"),
            (Value::Real(f64::NAN), "NULL"),
            (Value::Text("it's".into()), "'it''s'"),
            (Value::Text("a\nb\r\n\"c\"".into()), "'a\nb\r\n\"c\"'"),
            (Value::Text("über".into()), "'über'"),
            (Value::Text(String::new()), "''"),
            (Value::Blob(Vec::new()), "X''"),
            (Value::Blob(vec![0x00, 0xff, 0x10, 0xab]), "X'00ff10ab'"),
        ];
        for (v, want) in cases {
            assert_eq!(literal(v), *want, "{v:?}");
        }
    }

    #[test]
    fn identifiers_are_double_quoted_with_quotes_doubled() {
        assert_eq!(ident("semantic_memory"), "\"semantic_memory\"");
        assert_eq!(ident("we\"ird"), "\"we\"\"ird\"");
    }

    /// The rows written must be the rows the snapshot holds. A mismatch fails
    /// the backup; it is never patched over.
    #[test]
    fn a_re_count_that_disagrees_fails_the_dump() {
        assert!(check_count("semantic_memory", 3, 3).is_ok());
        let err = check_count("semantic_memory", 2, 3).unwrap_err();
        assert_eq!(err.exit_code(), EXIT_ERROR);
        let msg = err.to_string();
        assert!(
            msg.contains("semantic_memory") && msg.contains('2') && msg.contains('3'),
            "{msg}"
        );
    }

    #[test]
    fn the_database_file_is_only_read() {
        let cfg = fixture("dump-reader");
        let before = std::fs::read(&cfg.db_path).unwrap();
        let mtime = std::fs::metadata(&cfg.db_path).unwrap().modified().unwrap();
        dump_text(&cfg);
        assert_eq!(std::fs::read(&cfg.db_path).unwrap(), before);
        assert_eq!(
            std::fs::metadata(&cfg.db_path).unwrap().modified().unwrap(),
            mtime
        );
    }
}
