//! Schema migrations, as the store runs them: a probe that asks the database
//! whether one is applied, and the migration itself in one transaction.

use turso::transaction::{Transaction, TransactionBehavior};
use turso::{Connection, Value};

use super::{TursoStore, is_busy};
use crate::error::{Error, Result};

/// What running a migration did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Migrated {
    Applied,
    /// Its probe already held inside the transaction -- another run got
    /// there first -- so nothing was run.
    AlreadyApplied,
}

/// How a migration ended, inside the retry loop: only contention is retried.
enum Ended {
    Done(Migrated),
    /// A statement failed: its position, its text, the engine's message.
    Statement(usize, String, String),
    /// Every statement ran, and the probe still does not hold.
    ProbeFalse,
}

impl TursoStore {
    /// Whether `query` -- one integer, non-zero when applied -- holds.
    pub fn holds(&self, query: &str) -> Result<bool> {
        self.with_retry(|| pollster::block_on(probe(&self.conn, query)))
    }

    /// Run `statements` in one IMMEDIATE transaction, unless `probe` already
    /// holds inside it. Committed only when every statement succeeded and the
    /// probe holds afterwards; otherwise rolled back, and the error names the
    /// statement.
    pub fn migrate(&self, probe_query: &str, statements: &[&str]) -> Result<Migrated> {
        let ended = self.with_retry(|| {
            pollster::block_on(async {
                // IMMEDIATE: the write lock is taken before the probe, so a
                // second `update -y` waits here and then finds it applied.
                let tx =
                    Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate).await?;
                if probe(&self.conn, probe_query).await? {
                    tx.rollback().await?;
                    return Ok(Ended::Done(Migrated::AlreadyApplied));
                }
                for (i, sql) in statements.iter().enumerate() {
                    if let Err(e) = tx.execute(sql, ()).await {
                        // Contention is retried from the top; dropping the
                        // transaction rolls it back.
                        if is_busy(&e) {
                            return Err(e);
                        }
                        tx.rollback().await?;
                        return Ok(Ended::Statement(
                            i + 1,
                            sql.trim().to_string(),
                            e.to_string(),
                        ));
                    }
                }
                if !probe(&self.conn, probe_query).await? {
                    tx.rollback().await?;
                    return Ok(Ended::ProbeFalse);
                }
                tx.commit().await?;
                Ok(Ended::Done(Migrated::Applied))
            })
        })?;
        match ended {
            Ended::Done(m) => Ok(m),
            Ended::Statement(n, sql, why) => Err(Error::failed(format!(
                "statement {n} of {} failed: {why} — `{}`; rolled back, nothing of this migration \
                 was applied",
                statements.len(),
                first_line(&sql)
            ))),
            Ended::ProbeFalse => Err(Error::failed(
                "every statement ran, but its probe still does not hold; rolled back, nothing of \
                 this migration was applied",
            )),
        }
    }
}

/// The probe, on a connection that may be inside a transaction.
async fn probe(conn: &Connection, query: &str) -> turso::Result<bool> {
    let mut rows = conn.query(query, ()).await?;
    Ok(match rows.next().await? {
        Some(row) => !matches!(row.get_value(0)?, Value::Integer(0) | Value::Null),
        None => false,
    })
}

fn first_line(sql: &str) -> &str {
    sql.lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("--"))
        .unwrap_or(sql)
}

#[cfg(test)]
mod tests {
    use super::super::dump::tests::{batch, dump_text};
    use super::super::tests::scratch;
    use super::*;
    use crate::error::EXIT_ERROR;

    const HAS_A: &str = "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = 'a'";

    #[test]
    fn a_probe_reads_the_database_itself() {
        let cfg = scratch("migrate-holds");
        let store = TursoStore::open(&cfg).unwrap();
        assert!(!store.holds(HAS_A).unwrap());
        batch(&cfg, "CREATE TABLE a (x);");
        assert!(store.holds(HAS_A).unwrap());
        assert!(
            store
                .holds("SELECT count(*) FROM sqlite_schema WHERE name = 'semantic_memory'")
                .unwrap()
        );
        assert!(store.holds("SELECT nope FROM nowhere").is_err());
    }

    #[test]
    fn a_migration_runs_every_statement_and_commits() {
        let cfg = scratch("migrate-apply");
        let store = TursoStore::open(&cfg).unwrap();
        let got = store
            .migrate(
                HAS_A,
                &[
                    "CREATE TABLE a (x)",
                    "CREATE INDEX ia ON a (x)",
                    "INSERT INTO a VALUES (1)",
                ],
            )
            .unwrap();
        assert_eq!(got, Migrated::Applied);
        drop(store);
        // Seen from a connection of its own: committed, not just visible to
        // the one that wrote it.
        let store = TursoStore::open(&cfg).unwrap();
        assert!(store.holds(HAS_A).unwrap());
        assert!(
            store
                .holds("SELECT count(*) FROM sqlite_schema WHERE name = 'ia'")
                .unwrap()
        );
        assert_eq!(store.count_rows("a"), Ok(1));
    }

    /// The probe is asked again inside the transaction: a migration another
    /// run applied since the plan runs nothing -- here, a CREATE TABLE that
    /// would fail if it ran again.
    #[test]
    fn an_applied_migration_runs_nothing() {
        let cfg = scratch("migrate-already");
        let store = TursoStore::open(&cfg).unwrap();
        batch(&cfg, "CREATE TABLE a (x);");
        let before = dump_text(&cfg);
        assert_eq!(
            store.migrate(HAS_A, &["CREATE TABLE a (x)"]).unwrap(),
            Migrated::AlreadyApplied
        );
        assert_eq!(dump_text(&cfg), before);
    }

    /// A failing statement rolls back the statements before it: the database
    /// is as it was, and the error says which statement failed.
    #[test]
    fn a_failing_statement_rolls_back_the_whole_migration() {
        let cfg = scratch("migrate-fail");
        let store = TursoStore::open(&cfg).unwrap();
        let before = dump_text(&cfg);
        let err = store
            .migrate(
                HAS_A,
                &[
                    "CREATE TABLE a (x)",
                    "INSERT INTO no_such_table VALUES (1)",
                    "CREATE TABLE b (y)",
                ],
            )
            .unwrap_err();
        assert_eq!(err.exit_code(), EXIT_ERROR, "{err}");
        let msg = err.to_string();
        assert!(msg.contains("statement 2 of 3"), "{msg}");
        assert!(msg.contains("no_such_table"), "{msg}");
        assert!(msg.contains("rolled back"), "{msg}");
        assert!(!store.holds(HAS_A).unwrap());
        assert_eq!(dump_text(&cfg), before);
        // The connection is usable afterwards: no transaction was left open.
        store.migrate(HAS_A, &["CREATE TABLE a (x)"]).unwrap();
        assert!(store.holds(HAS_A).unwrap());
    }

    /// Statements that run but leave the probe false are a broken migration:
    /// rolled back, never committed half-true.
    #[test]
    fn a_migration_whose_probe_still_fails_is_rolled_back() {
        let cfg = scratch("migrate-probe-false");
        let store = TursoStore::open(&cfg).unwrap();
        let err = store
            .migrate(HAS_A, &["CREATE TABLE not_a (x)"])
            .unwrap_err();
        assert!(err.to_string().contains("probe"), "{err}");
        assert_eq!(store.count_rows("not_a"), Err("missing".to_string()));
    }

    /// A CHECK cannot be altered in place, so changing one is a table
    /// rebuild -- create, copy, drop, rename -- in the one transaction. The
    /// rows survive, the new CHECK holds, and a probe on the stored SQL sees it.
    #[test]
    fn a_table_rebuild_changes_a_check_and_keeps_the_rows() {
        let cfg = scratch("migrate-rebuild");
        batch(
            &cfg,
            "CREATE TABLE c (id INTEGER PRIMARY KEY, e TEXT CHECK (length(e) <= 3));
             INSERT INTO c (e) VALUES ('abc');
             INSERT INTO c (e) VALUES ('xyz');",
        );
        let probe = "SELECT count(*) FROM sqlite_schema WHERE name = 'c' AND sql LIKE '%<= 10%'";
        let store = TursoStore::open(&cfg).unwrap();
        assert!(!store.holds(probe).unwrap());
        assert_eq!(
            store
                .migrate(
                    probe,
                    &[
                        "CREATE TABLE c_new (id INTEGER PRIMARY KEY, e TEXT CHECK (length(e) <= 10))",
                        "INSERT INTO c_new (id, e) SELECT id, e FROM c",
                        "DROP TABLE c",
                        "ALTER TABLE c_new RENAME TO c",
                    ],
                )
                .unwrap(),
            Migrated::Applied
        );
        assert!(store.holds(probe).unwrap());
        assert_eq!(store.count_rows("c"), Ok(2));
        batch(&cfg, "INSERT INTO c (e) VALUES ('ten chars!');");
        assert_eq!(store.count_rows("c"), Ok(3));
        assert_eq!(store.count_rows("c_new"), Err("missing".to_string()));
    }
}
