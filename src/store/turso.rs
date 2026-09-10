//! The Turso backend: the `turso` crate, in-process and statically linked.
//!
//! What used to be `memlib.exec_sql` -- forty lines deciding whether the tursodb
//! CLI had failed, by reading exit status, stderr and stdout, with a carve-out
//! because this corpus stores tursodb error strings as memory rows -- is now a
//! typed error from a driver. None of that heuristic survives, deliberately.

use std::time::Duration;

use turso::{Builder, Connection, Value};

use super::Store;
use crate::config::Config;
use crate::error::{Error, Result};

/// Contention backoff. Matches the Python client's six attempts with a rising
/// delay, but keyed on the driver's own busy error rather than on grepping output
/// for `database is (busy|locked)` -- a phrase this corpus legitimately stores in
/// row bodies, which is why the text match had to be so careful.
const MAX_ATTEMPTS: u32 = 6;
const BACKOFF_STEP: Duration = Duration::from_millis(300);

pub struct TursoStore {
    conn: Connection,
}

impl TursoStore {
    /// Open the configured database. Refuses to create one: a missing file almost
    /// always means a wrong path, and an empty new database strands the real one.
    pub fn open(cfg: &Config) -> Result<Self> {
        if !cfg.db_exists() {
            return Err(Error::refused(format!(
                "no database at {} — refusing to create one",
                cfg.db_path.display()
            )));
        }
        let path = cfg.db_path.to_string_lossy().to_string();
        let vfs = cfg.vfs.clone();

        let conn = pollster::block_on(async move {
            let mut builder = Builder::new_local(&path)
                // THE invariant: without it Turso takes an exclusive lock and a
                // second Claude session is refused outright.
                .experimental_multiprocess_wal(true);
            if let Some(vfs) = vfs {
                builder = builder.with_io(vfs);
            }
            let db = builder.build().await?;
            db.connect()
        })
        .map_err(map_err)?;

        Ok(TursoStore { conn })
    }

    /// Run a query returning a single integer.
    fn scalar_i64(&self, sql: &str) -> Result<i64> {
        let value = self.with_retry(|| {
            pollster::block_on(async {
                let mut rows = self.conn.query(sql, ()).await?;
                match rows.next().await? {
                    Some(row) => row.get_value(0).map(Some),
                    None => Ok(None),
                }
            })
        })?;
        match value {
            Some(Value::Integer(n)) => Ok(n),
            Some(other) => Err(Error::failed(format!(
                "expected an integer from `{sql}`, got {other:?}"
            ))),
            None => Err(Error::failed(format!("`{sql}` returned no rows"))),
        }
    }

    /// Retry only genuine contention, and only that. Every other failure is
    /// returned on the first attempt: the old client burned 25 seconds retrying a
    /// bad `--vfs` that could never succeed.
    fn with_retry<T>(
        &self,
        mut op: impl FnMut() -> std::result::Result<T, turso::Error>,
    ) -> Result<T> {
        let mut last = None;
        for attempt in 0..MAX_ATTEMPTS {
            match op() {
                Ok(v) => return Ok(v),
                Err(e) if is_busy(&e) => {
                    std::thread::sleep(BACKOFF_STEP * (attempt + 1));
                    last = Some(e);
                }
                Err(e) => return Err(map_err(e)),
            }
        }
        Err(Error::failed(format!(
            "database busy after {MAX_ATTEMPTS} attempts: {}",
            last.map(|e| e.to_string()).unwrap_or_default()
        )))
    }
}

impl Store for TursoStore {
    fn total_memories(&self) -> Result<u64> {
        let n = self.scalar_i64(
            "SELECT (SELECT count(*) FROM semantic_memory) \
             + (SELECT count(*) FROM episodic_memory);",
        )?;
        Ok(n.max(0) as u64)
    }
}

/// Contention, as the driver reports it. `Busy` is the typed case; the string
/// probe is a backstop for a message that has not been given a variant yet, and
/// is scoped to the driver's own error text -- never to row content.
fn is_busy(e: &turso::Error) -> bool {
    if matches!(e, turso::Error::Busy(_) | turso::Error::BusySnapshot(_)) {
        return true;
    }
    let msg = e.to_string().to_ascii_lowercase();
    msg.contains("database is busy") || msg.contains("database is locked")
}

fn map_err(e: turso::Error) -> Error {
    Error::failed(e.to_string())
}
