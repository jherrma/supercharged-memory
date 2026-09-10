//! The Turso backend: the `turso` crate, in-process and statically linked.
//!
//! What used to be `memlib.exec_sql` -- forty lines deciding whether the tursodb
//! CLI had failed, by reading exit status, stderr and stdout, with a carve-out
//! because this corpus stores tursodb error strings as memory rows -- is now a
//! typed error from a driver. None of that heuristic survives, deliberately.

use std::time::Duration;

use turso::{Builder, Connection, Value};

use super::{Cell, ResultSet, Scope, SearchQuery, Store, Table};
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

    /// Run a query and collect it, with the column names the output format needs.
    fn query(&self, sql: &str, params: Vec<Value>) -> Result<ResultSet> {
        self.with_retry(|| {
            pollster::block_on(async {
                let mut rows = self.conn.query(sql, params.clone()).await?;
                let columns = rows.column_names();
                let mut out = Vec::new();
                while let Some(row) = rows.next().await? {
                    let mut cells = Vec::with_capacity(columns.len());
                    for i in 0..columns.len() {
                        cells.push(cell(row.get_value(i)?));
                    }
                    out.push(cells);
                }
                Ok(ResultSet { columns, rows: out })
            })
        })
    }
}

/// The predicate selecting the rows a scope may see, plus its bound parameters.
///
/// Semantic memory is revisable, so "current truth" is the rows that have not
/// been superseded and not been retired. Episodic memory is append-only: every
/// row is current.
fn base_predicate(scope: &Scope) -> (String, Vec<Value>) {
    let mut sql = match scope.table {
        Table::Semantic => "superseded_by IS NULL AND retired_at IS NULL".to_string(),
        Table::Episodic => "1=1".to_string(),
    };
    let mut params = Vec::new();
    if let Some(project) = &scope.project {
        sql.push_str(" AND project = ?");
        params.push(Value::Text(project.clone()));
    }
    if let Some(coworker) = scope.coworker {
        // No rows in memory_coworkers means global/visible-to-all; otherwise the
        // memory must be tagged to this coworker.
        sql.push_str(
            " AND (id NOT IN (SELECT memory_id FROM memory_coworkers WHERE memory_table=?) \
             OR id IN (SELECT memory_id FROM memory_coworkers WHERE memory_table=? AND coworker_id=?))",
        );
        params.push(Value::Text(scope.table.as_str().to_string()));
        params.push(Value::Text(scope.table.as_str().to_string()));
        params.push(Value::Integer(coworker));
    }
    (sql, params)
}

/// The IDF-weighted keyword share, as SQL plus its parameters.
///
/// Weights are rounded to six decimals before binding, matching the `%.6f` the
/// Python implementation formatted into the SQL text -- a seventh digit would
/// move scores in the last place, which is enough to reorder a tie.
fn kw_expression(query: &SearchQuery) -> (String, Vec<Value>) {
    if query.tokens.is_empty() {
        return ("0".to_string(), Vec::new());
    }
    let mut terms = Vec::new();
    let mut params = Vec::new();
    for t in query.tokens {
        terms.push("(CASE WHEN lower(memory_text) LIKE ? THEN ? ELSE 0 END)".to_string());
        params.push(Value::Text(crate::recall::rank::like_pattern(&t.token)));
        params.push(Value::Real(crate::recall::rank::round6(t.weight)));
    }
    params.push(Value::Real(query.total));
    (format!("(({}) / ?)", terms.join(" + ")), params)
}

/// The query vector, as a literal.
///
/// This is the one value not bound as a parameter: `vector32()` takes a literal,
/// and the digits are generated from f32s we produced -- there is no user text
/// anywhere near it. Rust's shortest round-trip formatting is used rather than
/// the Python client's `%.7g`, because `vector32` stores f32 and the shortest
/// round-trip of an f32 IS that f32.
fn vector_literal(v: &[f32]) -> String {
    let mut s = String::from("vector32('[");
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&x.to_string());
    }
    s.push_str("]')");
    s
}

fn cell(v: Value) -> Cell {
    match v {
        Value::Null => Cell::Null,
        Value::Integer(n) => Cell::Int(n),
        Value::Real(x) => Cell::Real(x),
        Value::Text(s) => Cell::Text(s),
        Value::Blob(b) => Cell::Text(format!("<{} byte blob>", b.len())),
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

    fn baseline(&self) -> Result<ResultSet> {
        self.query(
            "SELECT topic, memory_text FROM semantic_memory \
             WHERE category='baseline' AND superseded_by IS NULL AND retired_at IS NULL \
             ORDER BY created_at;",
            Vec::new(),
        )
    }

    fn topics(&self) -> Result<ResultSet> {
        self.query(
            "SELECT topic, keywords FROM topic_keywords ORDER BY updated_at DESC;",
            Vec::new(),
        )
    }

    fn resolve_coworker(&self, name: &str) -> Result<Option<i64>> {
        let rs = self.query(
            "SELECT id FROM coworkers WHERE name = ?;",
            vec![Value::Text(name.to_string())],
        )?;
        Ok(match rs.rows.first().and_then(|r| r.first()) {
            Some(Cell::Int(id)) => Some(*id),
            _ => None,
        })
    }

    fn document_frequencies(&self, scope: &Scope, patterns: &[String]) -> Result<(Vec<u64>, u64)> {
        let (base, base_params) = base_predicate(scope);
        if patterns.is_empty() {
            let n = self.scalar_i64(&format!(
                "SELECT COUNT(*) FROM {}_memory WHERE {base};",
                scope.table.as_str()
            ))?;
            return Ok((Vec::new(), n.max(0) as u64));
        }
        let sums: Vec<String> = patterns
            .iter()
            .map(|_| "SUM(CASE WHEN lower(memory_text) LIKE ? THEN 1 ELSE 0 END)".to_string())
            .collect();
        let mut params: Vec<Value> = patterns.iter().map(|p| Value::Text(p.clone())).collect();
        params.extend(base_params);
        let rs = self.query(
            &format!(
                "SELECT {}, COUNT(*) FROM {}_memory WHERE {base};",
                sums.join(", "),
                scope.table.as_str()
            ),
            params,
        )?;

        // SUM() over zero matching rows is NULL, which means df = 0 -- the first
        // search after a fresh setup hits this, because episodic_memory is empty
        // and --table defaults to both.
        let row = rs.rows.first().cloned().unwrap_or_default();
        let as_u64 = |c: Option<&Cell>| match c {
            Some(Cell::Int(n)) => (*n).max(0) as u64,
            _ => 0,
        };
        let dfs: Vec<u64> = (0..patterns.len()).map(|i| as_u64(row.get(i))).collect();
        Ok((dfs, as_u64(row.get(patterns.len()))))
    }

    fn search(&self, scope: &Scope, query: &SearchQuery) -> Result<ResultSet> {
        let (base, base_params) = base_predicate(scope);
        let (kw, kw_params) = kw_expression(query);
        let meta = match scope.table {
            Table::Semantic => "category",
            Table::Episodic => "event_type || '/' || importance",
        };

        let (sql, params) = match query.vector {
            Some(v) => {
                let vec_lit = vector_literal(v);
                // `embedding IS NOT NULL` is not optional: vector_distance_cos
                // raises "Invalid vector type" on a NULL, taking down the whole
                // query over one bad row.
                let sql = format!(
                    "SELECT round(vector_distance_cos(embedding,{vec_lit}),4) AS dist, \
                     round({kw},3) AS kw, \
                     round(vector_distance_cos(embedding,{vec_lit}) - ?*({kw}),4) AS score, \
                     created_at, {meta} AS meta, project, topic, memory_text \
                     FROM {}_memory WHERE {base} AND embedding IS NOT NULL \
                     ORDER BY score ASC LIMIT ?;",
                    scope.table.as_str()
                );
                let mut params = kw_params.clone();
                params.push(Value::Real(query.alpha));
                params.extend(kw_params.clone());
                params.extend(base_params);
                params.push(Value::Integer(query.limit as i64));
                (sql, params)
            }
            None => {
                let sql = format!(
                    "SELECT round({kw},3) AS kw, created_at, {meta} AS meta, project, topic, memory_text \
                     FROM {}_memory WHERE {base} AND ({kw}) > 0 \
                     ORDER BY kw DESC, created_at DESC LIMIT ?;",
                    scope.table.as_str()
                );
                let mut params = kw_params.clone();
                params.extend(base_params);
                params.extend(kw_params);
                params.push(Value::Integer(query.limit as i64));
                (sql, params)
            }
        };
        self.query(&sql, params)
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
