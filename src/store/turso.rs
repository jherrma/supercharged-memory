//! The Turso backend: the `turso` crate, in-process and statically linked.
//!
//! What used to be `memlib.exec_sql` -- forty lines deciding whether the tursodb
//! CLI had failed, by reading exit status, stderr and stdout, with a carve-out
//! because this corpus stores tursodb error strings as memory rows -- is now a
//! typed error from a driver. None of that heuristic survives, deliberately.

pub mod dump;
pub mod migrate;
pub mod restore;

use std::io::Read;
use std::path::Path;
use std::time::Duration;

use turso::{Builder, Connection, Value};

use super::{
    ActiveRow, Cell, Embedded, NewMemory, Purged, ResultSet, Scope, SearchQuery, Store, Table,
};
use crate::config::Config;
use crate::error::{Error, Result};

/// Contention backoff. Matches the Python client's six attempts with a rising
/// delay, but keyed on the driver's own busy error rather than on grepping output
/// for `database is (busy|locked)` -- a phrase this corpus legitimately stores in
/// row bodies, which is why the text match had to be so careful.
const MAX_ATTEMPTS: u32 = 6;
const BACKOFF_STEP: Duration = Duration::from_millis(300);

/// The schema a new database is created from, compiled in from the repo's
/// `schema.sql`.
pub const SCHEMA: &str = include_str!("../../schema.sql");

/// Why a file cannot be a database, judged by its header alone, or `None` when
/// it can be. An empty or foreign file is never opened: opening an empty one
/// creates WAL files beside it.
pub fn not_a_database(path: &Path) -> std::io::Result<Option<&'static str>> {
    let mut head = Vec::new();
    std::fs::File::open(path)?.take(16).read_to_end(&mut head)?;
    Ok(if head.is_empty() {
        Some("an empty file — a stale process re-creating a renamed path leaves one")
    } else if head != b"SQLite format 3\0" {
        Some("not an SQLite database")
    } else {
        None
    })
}

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

    /// Create the memory tables from the embedded schema, through this same
    /// connection: the database `init` creates is opened exactly the way every
    /// other command opens it.
    pub fn create_schema(&self) -> Result<()> {
        self.with_retry(|| pollster::block_on(self.conn.execute_batch(SCHEMA)))
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
    match scope.embedded {
        Embedded::Any => {}
        Embedded::With => sql.push_str(" AND embedding IS NOT NULL"),
        Embedded::Without => sql.push_str(" AND embedding IS NULL"),
    }
    (sql, params)
}

/// Current semantic rows visible to any of these coworkers (empty = every row),
/// as SQL plus its parameters: what the near-duplicate guards compare against.
/// A memory a coworker cannot reach is not a duplicate of one they can.
fn current_semantic_visible_to(coworkers: &[i64]) -> (String, Vec<Value>) {
    let mut sql = "superseded_by IS NULL AND retired_at IS NULL".to_string();
    if coworkers.is_empty() {
        return (sql, Vec::new());
    }
    let placeholders = vec!["?"; coworkers.len()].join(",");
    sql.push_str(&format!(
        " AND (id NOT IN (SELECT memory_id FROM memory_coworkers WHERE memory_table='semantic') \
         OR id IN (SELECT memory_id FROM memory_coworkers WHERE memory_table='semantic' \
         AND coworker_id IN ({placeholders})))"
    ));
    (sql, coworkers.iter().map(|c| Value::Integer(*c)).collect())
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

    fn rows_with_other_embed_model(&self, table: Table, embed_model: &str) -> Result<u64> {
        let rs = self.query(
            &format!(
                "SELECT count(*) FROM {}_memory \
                 WHERE embed_model IS NOT NULL AND embed_model <> ?;",
                table.as_str()
            ),
            vec![Value::Text(embed_model.to_string())],
        )?;
        Ok(match rs.rows.first().and_then(|r| r.first()) {
            Some(Cell::Int(n)) => (*n).max(0) as u64,
            _ => 0,
        })
    }

    fn current_semantic_ids(&self, ids: &[i64]) -> Result<Vec<i64>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; ids.len()].join(",");
        let rs = self.query(
            &format!(
                "SELECT id FROM semantic_memory WHERE id IN ({placeholders}) \
                 AND superseded_by IS NULL AND retired_at IS NULL;"
            ),
            ids.iter().map(|i| Value::Integer(*i)).collect(),
        )?;
        Ok(rs
            .rows
            .iter()
            .filter_map(|r| match r.first() {
                Some(Cell::Int(n)) => Some(*n),
                _ => None,
            })
            .collect())
    }

    fn nearest_semantic(&self, coworkers: &[i64], vector: &[f32]) -> Result<Option<f64>> {
        let lit = vector_literal(vector);
        let (visible, params) = current_semantic_visible_to(coworkers);
        // `embedding IS NOT NULL` is not optional: vector_distance_cos raises
        // "Invalid vector type" on a NULL embedding, so one such row would make
        // every insert fail, not merely skew the comparison.
        let rs = self.query(
            &format!(
                "SELECT round(vector_distance_cos(embedding,{lit}),4) FROM semantic_memory \
                 WHERE {visible} AND embedding IS NOT NULL ORDER BY 1 LIMIT 1;"
            ),
            params,
        )?;
        Ok(match rs.rows.first().and_then(|r| r.first()) {
            Some(Cell::Real(d)) => Some(*d),
            Some(Cell::Int(n)) => Some(*n as f64),
            _ => None,
        })
    }

    fn current_semantic_texts(&self, coworkers: &[i64]) -> Result<Vec<(i64, String)>> {
        let (visible, params) = current_semantic_visible_to(coworkers);
        let rs = self.query(
            &format!("SELECT id, memory_text FROM semantic_memory WHERE {visible} ORDER BY id;"),
            params,
        )?;
        Ok(rs
            .rows
            .into_iter()
            .filter_map(|r| match (r.first(), r.get(1)) {
                (Some(Cell::Int(id)), Some(Cell::Text(t))) => Some((*id, t.clone())),
                _ => None,
            })
            .collect())
    }

    fn insert_memory(&self, m: &NewMemory) -> Result<i64> {
        let (insert_sql, insert_params) = insert_statement(m);
        let table = m.table.as_str().to_string();

        self.with_retry(|| {
            pollster::block_on(async {
                // Dropping the transaction without committing leaves it dangling,
                // and the driver rolls it back on the connection's next use -- so
                // an error anywhere below, including a retry, discards the row,
                // the supersede and the tags together.
                let tx = self.conn.unchecked_transaction().await?;
                tx.execute(&insert_sql, insert_params.clone()).await?;

                // Read the id ONCE, here, straight after the memory insert. Bug
                // B1 in the issue #3 review came from asking for
                // last_insert_rowid() again AFTER the memory_coworkers insert,
                // which answers with the join row's id.
                let id = self.conn.last_insert_rowid();

                if !m.supersedes.is_empty() {
                    // One UPDATE covers every superseded id, so an N->1 merge
                    // cannot half-apply.
                    let placeholders = vec!["?"; m.supersedes.len()].join(",");
                    let mut params = vec![Value::Integer(id)];
                    params.extend(m.supersedes.iter().map(|i| Value::Integer(*i)));
                    tx.execute(
                        &format!(
                            "UPDATE semantic_memory SET superseded_by=?, \
                             updated_at=datetime('now') WHERE id IN ({placeholders});"
                        ),
                        params,
                    )
                    .await?;
                }

                for cid in &m.coworkers {
                    tx.execute(
                        "INSERT INTO memory_coworkers (memory_table, memory_id, coworker_id) \
                         VALUES (?, ?, ?);",
                        vec![
                            Value::Text(table.clone()),
                            Value::Integer(id),
                            Value::Integer(*cid),
                        ],
                    )
                    .await?;
                }

                tx.commit().await?;
                Ok(id)
            })
        })
    }

    fn unembedded_rows(&self, table: Table, after: i64, limit: u32) -> Result<Vec<(i64, String)>> {
        let (active, mut params) = base_predicate(&unembedded(table));
        params.push(Value::Integer(after));
        params.push(Value::Integer(i64::from(limit)));
        let rs = self.query(
            &format!(
                "SELECT id, memory_text FROM {}_memory WHERE {active} AND id > ? \
                 ORDER BY id LIMIT ?;",
                table.as_str()
            ),
            params,
        )?;
        Ok(rs
            .rows
            .into_iter()
            .filter_map(|r| match (r.first(), r.get(1)) {
                (Some(Cell::Int(id)), Some(Cell::Text(t))) => Some((*id, t.clone())),
                _ => None,
            })
            .collect())
    }

    fn fill_embeddings(
        &self,
        table: Table,
        embed_model: &str,
        rows: &[(i64, Vec<f32>)],
    ) -> Result<Vec<i64>> {
        // Same predicate as the selection, so a row superseded, retired or
        // embedded since then is left exactly as it is. `updated_at` is not
        // touched: a vector filled in later is not a revision of the memory.
        let (eligible, eligible_params) = base_predicate(&unembedded(table));
        let updates: Vec<(i64, String)> = rows
            .iter()
            .map(|(id, v)| {
                (
                    *id,
                    format!(
                        "UPDATE {}_memory SET embedding = {}, embed_model = ? \
                         WHERE id = ? AND {eligible};",
                        table.as_str(),
                        vector_literal(v)
                    ),
                )
            })
            .collect();

        self.with_retry(|| {
            pollster::block_on(async {
                // One transaction per batch: dropped without a commit, it is
                // rolled back on the connection's next use, so a batch lands
                // whole or not at all.
                let tx = self.conn.unchecked_transaction().await?;
                let mut written = Vec::new();
                for (id, sql) in &updates {
                    let mut params =
                        vec![Value::Text(embed_model.to_string()), Value::Integer(*id)];
                    params.extend(eligible_params.clone());
                    if tx.execute(sql, params).await? > 0 {
                        written.push(*id);
                    }
                }
                tx.commit().await?;
                Ok(written)
            })
        })
    }

    fn semantic_within(&self, id: i64, vector: &[f32], below: f64) -> Result<Vec<(i64, f64)>> {
        let lit = vector_literal(vector);
        let (current, mut params) = current_semantic_visible_to(&[]);
        params.push(Value::Integer(id));
        params.push(Value::Real(below));
        let rs = self.query(
            &format!(
                "SELECT id, round(vector_distance_cos(embedding,{lit}),4) AS dist \
                 FROM semantic_memory WHERE {current} AND id <> ? AND embedding IS NOT NULL \
                 AND vector_distance_cos(embedding,{lit}) < ? ORDER BY dist, id;"
            ),
            params,
        )?;
        Ok(rs
            .rows
            .iter()
            .filter_map(|r| match (r.first(), r.get(1)) {
                (Some(Cell::Int(id)), Some(Cell::Real(d))) => Some((*id, *d)),
                (Some(Cell::Int(id)), Some(Cell::Int(d))) => Some((*id, *d as f64)),
                _ => None,
            })
            .collect())
    }

    fn active_rows(&self, table: Table) -> Result<Vec<ActiveRow>> {
        let (active, params) = base_predicate(&every_row(table));
        // The age is the engine's own date arithmetic, as the reference's SQL
        // computed it, so a date it cannot read comes back NULL, not guessed.
        let rs = self.query(
            &format!(
                "SELECT id, created_at, julianday('now') - julianday(created_at), topic, \
                 memory_text, embedding IS NOT NULL FROM {}_memory WHERE {active} \
                 ORDER BY created_at, id;",
                table.as_str()
            ),
            params,
        )?;
        rs.rows
            .into_iter()
            .map(|r| match r.as_slice() {
                [
                    Cell::Int(id),
                    Cell::Text(created_at),
                    age,
                    topic,
                    Cell::Text(text),
                    Cell::Int(embedded),
                ] => Ok(ActiveRow {
                    id: *id,
                    created_at: created_at.clone(),
                    age_days: match age {
                        Cell::Real(d) => Some(*d),
                        Cell::Int(d) => Some(*d as f64),
                        _ => None,
                    },
                    topic: match topic {
                        Cell::Text(t) => Some(t.clone()),
                        _ => None,
                    },
                    memory_text: text.clone(),
                    embedded: *embedded != 0,
                }),
                other => Err(Error::failed(format!(
                    "unexpected row shape in {}_memory: {other:?}",
                    table.as_str()
                ))),
            })
            .collect()
    }

    fn close_pairs(&self, table: Table, below: f64) -> Result<Vec<(i64, i64)>> {
        // `embedding IS NOT NULL` on both sides is not optional:
        // vector_distance_cos raises on a NULL and takes the query down.
        let active = |alias: &str| match table {
            Table::Semantic => format!(
                "{alias}.embedding IS NOT NULL AND {alias}.superseded_by IS NULL \
                 AND {alias}.retired_at IS NULL"
            ),
            Table::Episodic => format!("{alias}.embedding IS NOT NULL"),
        };
        let rs = self.query(
            &format!(
                "SELECT a.id, b.id FROM {t}_memory a JOIN {t}_memory b ON a.id < b.id \
                 WHERE {} AND {} AND vector_distance_cos(a.embedding, b.embedding) < ? \
                 ORDER BY a.id, b.id;",
                active("a"),
                active("b"),
                t = table.as_str()
            ),
            vec![Value::Real(below)],
        )?;
        Ok(rs
            .rows
            .iter()
            .filter_map(|r| match (r.first(), r.get(1)) {
                (Some(Cell::Int(a)), Some(Cell::Int(b))) => Some((*a, *b)),
                _ => None,
            })
            .collect())
    }

    fn existing_ids(&self, table: Table, ids: &[i64]) -> Result<Vec<i64>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let rs = self.query(
            &format!(
                "SELECT id FROM {}_memory WHERE id IN ({}) ORDER BY id;",
                table.as_str(),
                placeholders(ids)
            ),
            int_params(ids),
        )?;
        Ok(ints(&rs))
    }

    fn mark_processed(&self, ids: &[i64]) -> Result<u64> {
        if ids.is_empty() {
            return Ok(0);
        }
        let sql = format!(
            "UPDATE episodic_memory SET processed_at = datetime('now') WHERE id IN ({});",
            placeholders(ids)
        );
        self.with_retry(|| pollster::block_on(self.conn.execute(&sql, int_params(ids))))
    }

    fn retire_semantic(&self, id: i64) -> Result<bool> {
        // The guard is in the UPDATE itself, so a row superseded or retired
        // by another process since the caller looked is left alone.
        let changed = self.with_retry(|| {
            pollster::block_on(self.conn.execute(
                "UPDATE semantic_memory SET retired_at = datetime('now'), \
                 updated_at = datetime('now') \
                 WHERE id = ? AND superseded_by IS NULL AND retired_at IS NULL;",
                vec![Value::Integer(id)],
            ))
        })?;
        Ok(changed == 1)
    }

    fn replace_topics(&self, topics: &[(String, String)]) -> Result<()> {
        self.with_retry(|| {
            pollster::block_on(async {
                // Dropped without a commit, the transaction is rolled back: the
                // index is replaced whole or not at all.
                let tx = self.conn.unchecked_transaction().await?;
                tx.execute("DELETE FROM topic_keywords;", ()).await?;
                for (topic, keywords) in topics {
                    tx.execute(
                        "INSERT INTO topic_keywords (topic, keywords, updated_at) \
                         VALUES (?, ?, datetime('now'));",
                        vec![Value::Text(topic.clone()), Value::Text(keywords.clone())],
                    )
                    .await?;
                }
                tx.commit().await
            })
        })
    }

    fn stranded_by(&self, ids: &[i64]) -> Result<Vec<i64>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let list = placeholders(ids);
        let mut params = int_params(ids);
        params.extend(int_params(ids));
        let rs = self.query(
            &format!(
                "SELECT id FROM semantic_memory WHERE superseded_by IN ({list}) \
                 AND id NOT IN ({list}) ORDER BY id;"
            ),
            params,
        )?;
        Ok(ints(&rs))
    }

    fn purge_semantic(&self, ids: &[i64]) -> Result<Purged> {
        let list = placeholders(ids);
        let outcome = self.with_retry(|| {
            pollster::block_on(async {
                let tx = self.conn.unchecked_transaction().await?;
                let coworker_tags = tx
                    .execute(
                        &format!(
                            "DELETE FROM memory_coworkers WHERE memory_table = 'semantic' \
                             AND memory_id IN ({list});"
                        ),
                        int_params(ids),
                    )
                    .await?;
                // The same predicate the caller's checks used, inside the
                // transaction: only rows no longer current go.
                let rows = tx
                    .execute(
                        &format!(
                            "DELETE FROM semantic_memory WHERE id IN ({list}) \
                             AND (superseded_by IS NOT NULL OR retired_at IS NOT NULL);"
                        ),
                        int_params(ids),
                    )
                    .await?;
                let stranded = {
                    let mut left = tx
                        .query(
                            &format!(
                                "SELECT count(*) FROM semantic_memory \
                                 WHERE superseded_by IN ({list});"
                            ),
                            int_params(ids),
                        )
                        .await?;
                    match left.next().await? {
                        Some(row) => row.get_value(0)?,
                        None => Value::Integer(0),
                    }
                };
                if rows != ids.len() as u64 || stranded != Value::Integer(0) {
                    tx.rollback().await?;
                    return Ok(None);
                }
                tx.commit().await?;
                Ok(Some(Purged {
                    rows,
                    coworker_tags,
                }))
            })
        })?;
        outcome.ok_or_else(|| {
            Error::refused(
                "the rows changed under the purge (one is current, missing, or still \
                 pointed at) — nothing was deleted",
            )
        })
    }
}

/// The active rows of a table that have no embedding.
fn unembedded(table: Table) -> Scope {
    Scope {
        embedded: Embedded::Without,
        ..every_row(table)
    }
}

/// Every active row of a table.
fn every_row(table: Table) -> Scope {
    Scope {
        table,
        project: None,
        coworker: None,
        embedded: Embedded::Any,
    }
}

/// `?,?,?` for an `IN (...)` over these ids.
fn placeholders(ids: &[i64]) -> String {
    vec!["?"; ids.len()].join(",")
}

fn int_params(ids: &[i64]) -> Vec<Value> {
    ids.iter().map(|i| Value::Integer(*i)).collect()
}

/// The first column of each row, where it is an integer.
fn ints(rs: &ResultSet) -> Vec<i64> {
    rs.rows
        .iter()
        .filter_map(|r| match r.first() {
            Some(Cell::Int(n)) => Some(*n),
            _ => None,
        })
        .collect()
}

/// The INSERT and its bound parameters.
///
/// Only the columns actually given are named, so the schema's defaults still
/// apply -- omitting `created_at` is how a row asks for CURRENT_TIMESTAMP, and
/// binding NULL instead would violate its NOT NULL.
fn insert_statement(m: &NewMemory) -> (String, Vec<Value>) {
    let mut cols: Vec<&str> = Vec::new();
    let mut params: Vec<Value> = Vec::new();
    let mut push = |col: &'static str, v: &Option<String>| {
        cols.push(col);
        params.push(match v {
            Some(s) => Value::Text(s.clone()),
            None => Value::Null,
        });
    };
    push("project", &m.project);
    push("topic", &m.topic);
    push("source", &m.source);
    push("model", &m.model);
    push("embed_model", &m.embed_model);
    push("memory_text", &Some(m.memory_text.clone()));
    push("file_reference", &m.file_reference);
    match m.table {
        Table::Semantic => push("category", &m.category),
        Table::Episodic => {
            push("event_type", &m.event_type);
            push("importance", &m.importance);
        }
    }
    if m.created_at.is_some() {
        push("created_at", &m.created_at);
        // Semantic rows carry an updated_at too; an imported memory whose
        // created_at is backdated but whose updated_at is today looks revised.
        if m.table == Table::Semantic {
            push("updated_at", &m.created_at);
        }
    }

    // A row stored without an embedding has NULL in both columns, so it
    // neither ranks in a vector search nor counts towards the one-model check.
    let embedding = match &m.embedding {
        Some(v) => vector_literal(v),
        None => "NULL".to_string(),
    };
    let placeholders = vec!["?"; cols.len()].join(", ");
    let sql = format!(
        "INSERT INTO {}_memory ({}, embedding) VALUES ({placeholders}, {embedding});",
        m.table.as_str(),
        cols.join(", "),
    );
    (sql, params)
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

#[cfg(test)]
pub(crate) mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::config::{DB_ENV, DotEnv, Env};

    /// A config pointing at a fresh database built from the real schema, in a
    /// directory of its own. Ollama is pointed at a port that refuses at once,
    /// in case anything reaches past an injected embedder.
    pub(crate) fn scratch(name: &str) -> Config {
        let dir = std::env::temp_dir().join(format!("sm-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("memory.db");
        let p = path.to_string_lossy().to_string();
        pollster::block_on(async {
            let db = Builder::new_local(&p)
                .experimental_multiprocess_wal(true)
                .build()
                .await
                .unwrap();
            db.connect().unwrap().execute_batch(SCHEMA).await.unwrap();
        });
        Config::from_env(&Env {
            vars: [
                (DB_ENV.to_string(), p),
                ("OLLAMA_URL".to_string(), "http://127.0.0.1:9".to_string()),
            ]
            .into_iter()
            .collect(),
            home: dir.clone(),
            windows: false,
            dotenv: DotEnv::absent(PathBuf::from(crate::config::tests::EXE)),
        })
        .unwrap()
    }

    /// Run one statement against a scratch database, outside the store, so a
    /// test can read back exactly what was written.
    pub(crate) fn rows(cfg: &Config, sql: &str) -> Vec<Vec<Cell>> {
        TursoStore::open(cfg)
            .unwrap()
            .query(sql, Vec::new())
            .unwrap()
            .rows
    }

    /// Insert one row directly, with a deterministic vector or none, bypassing
    /// the writer so a test of the readers does not depend on it.
    pub(crate) fn put(cfg: &Config, table: Table, text: &str, embedded: bool) -> i64 {
        let (cols, vals) = match table {
            Table::Semantic => ("category", "'reference'"),
            Table::Episodic => ("event_type, importance", "'note', 'routine'"),
        };
        let (model, embedding) = if embedded {
            (
                "'bge-m3'".to_string(),
                vector_literal(&crate::embed::fake::Fake::vector(text)),
            )
        } else {
            ("NULL".to_string(), "NULL".to_string())
        };
        rows(
            cfg,
            &format!(
                "INSERT INTO {t}_memory ({cols}, created_at, memory_text, embed_model, embedding) \
                 VALUES ({vals}, '2026-01-01 00:00:00', '{text}', {model}, {embedding});",
                t = table.as_str(),
                text = text.replace('\'', "''"),
            ),
        );
        match rows(
            cfg,
            &format!("SELECT max(id) FROM {}_memory;", table.as_str()),
        )[0][0]
        {
            Cell::Int(id) => id,
            ref other => panic!("{other:?}"),
        }
    }

    /// Cosine distance between a row's stored embedding and `v`.
    pub(crate) fn distance(cfg: &Config, table: Table, id: i64, v: &[f32]) -> f64 {
        match rows(
            cfg,
            &format!(
                "SELECT vector_distance_cos(embedding, {}) FROM {}_memory WHERE id = {id};",
                vector_literal(v),
                table.as_str()
            ),
        )[0][0]
        {
            Cell::Real(d) => d,
            Cell::Int(n) => n as f64,
            ref other => panic!("{other:?}"),
        }
    }

    /// Give a row a vector and a model directly, as another writer would.
    pub(crate) fn set_vector(cfg: &Config, table: Table, id: i64, v: &[f32], model: &str) {
        rows(
            cfg,
            &format!(
                "UPDATE {}_memory SET embed_model = '{model}', embedding = {} WHERE id = {id};",
                table.as_str(),
                vector_literal(v)
            ),
        );
    }

    #[test]
    fn the_scratch_database_has_the_memory_tables() {
        let cfg = scratch("fixture");
        assert_eq!(TursoStore::open(&cfg).unwrap().total_memories().unwrap(), 0);
    }

    /// What `init` does: an empty file created exclusively, opened through the
    /// one opener, given the schema.
    #[test]
    fn create_schema_turns_an_empty_file_into_a_memory_database() {
        let mut cfg = scratch("create-schema");
        cfg.db_path = cfg.db_path.with_file_name("fresh.db");
        std::fs::File::create_new(&cfg.db_path).unwrap();
        let store = TursoStore::open(&cfg).unwrap();
        store.create_schema().unwrap();
        assert_eq!(store.total_memories().unwrap(), 0);
        // Idempotent, like the schema itself.
        store.create_schema().unwrap();
        assert_eq!(TursoStore::open(&cfg).unwrap().total_memories().unwrap(), 0);
    }

    // ---- filling in missing embeddings ------------------------------------

    use crate::embed::fake::Fake;

    fn ids(rows: &[(i64, String)]) -> Vec<i64> {
        rows.iter().map(|(id, _)| *id).collect()
    }

    /// Only ACTIVE rows without a vector: a superseded or retired semantic row
    /// is never recalled, so it is never embedded either. Episodic rows are
    /// all active.
    #[test]
    fn unembedded_rows_are_the_active_rows_without_a_vector_by_id() {
        let cfg = scratch("unembedded-rows");
        let a = put(&cfg, Table::Semantic, "blind a", false);
        put(&cfg, Table::Semantic, "embedded", true);
        let superseded = put(&cfg, Table::Semantic, "blind, superseded", false);
        let retired = put(&cfg, Table::Semantic, "blind, retired", false);
        let b = put(&cfg, Table::Semantic, "blind b", false);
        let c = put(&cfg, Table::Semantic, "blind c", false);
        rows(
            &cfg,
            &format!("UPDATE semantic_memory SET superseded_by = {a} WHERE id = {superseded};"),
        );
        rows(
            &cfg,
            &format!("UPDATE semantic_memory SET retired_at = '2026-01-02' WHERE id = {retired};"),
        );
        let e = put(&cfg, Table::Episodic, "blind event", false);
        put(&cfg, Table::Episodic, "embedded event", true);

        let store = TursoStore::open(&cfg).unwrap();
        assert_eq!(
            ids(&store.unembedded_rows(Table::Semantic, 0, 10).unwrap()),
            vec![a, b, c]
        );
        // The cursor and the limit page through them in id order.
        assert_eq!(
            ids(&store.unembedded_rows(Table::Semantic, a, 1).unwrap()),
            vec![b]
        );
        assert!(
            store
                .unembedded_rows(Table::Semantic, c, 10)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store.unembedded_rows(Table::Semantic, 0, 1).unwrap(),
            vec![(a, "blind a".to_string())]
        );
        assert_eq!(
            ids(&store.unembedded_rows(Table::Episodic, 0, 10).unwrap()),
            vec![e]
        );
    }

    /// The write lands only where the row is still active and still without a
    /// vector: one embedded by another process meanwhile keeps its vector and
    /// model, and one superseded meanwhile stays as it is.
    #[test]
    fn fill_embeddings_writes_only_rows_still_eligible() {
        let cfg = scratch("fill-embeddings");
        let a = put(&cfg, Table::Semantic, "blind a", false);
        let meanwhile = put(&cfg, Table::Semantic, "embedded meanwhile", false);
        let superseded = put(&cfg, Table::Semantic, "superseded meanwhile", false);
        let e = put(&cfg, Table::Episodic, "blind event", false);
        let updated_at = rows(
            &cfg,
            &format!("SELECT updated_at FROM semantic_memory WHERE id = {a};"),
        );

        // Open first, then let "another process" change rows under it.
        let store = TursoStore::open(&cfg).unwrap();
        set_vector(
            &cfg,
            Table::Semantic,
            meanwhile,
            &Fake::vector("elsewhere"),
            "bge-m3",
        );
        rows(
            &cfg,
            &format!("UPDATE semantic_memory SET superseded_by = {a} WHERE id = {superseded};"),
        );

        let written = store
            .fill_embeddings(
                Table::Semantic,
                "bge-m3",
                &[
                    (a, Fake::vector("blind a")),
                    (meanwhile, Fake::vector("embedded meanwhile")),
                    (superseded, Fake::vector("superseded meanwhile")),
                ],
            )
            .unwrap();
        assert_eq!(written, vec![a]);
        assert!(distance(&cfg, Table::Semantic, a, &Fake::vector("blind a")) < 1e-6);
        assert!(
            distance(&cfg, Table::Semantic, meanwhile, &Fake::vector("elsewhere")) < 1e-6,
            "never overwritten"
        );
        assert_eq!(
            rows(
                &cfg,
                "SELECT id, embedding IS NULL, embed_model FROM semantic_memory ORDER BY id;"
            ),
            vec![
                vec![Cell::Int(a), Cell::Int(0), Cell::Text("bge-m3".into())],
                vec![
                    Cell::Int(meanwhile),
                    Cell::Int(0),
                    Cell::Text("bge-m3".into())
                ],
                vec![Cell::Int(superseded), Cell::Int(1), Cell::Null],
            ]
        );
        // Filling in a vector is not a revision of the memory.
        assert_eq!(
            rows(
                &cfg,
                &format!("SELECT updated_at FROM semantic_memory WHERE id = {a};")
            ),
            updated_at
        );

        // Episodic rows have no supersede columns; the same write works there.
        let written = store
            .fill_embeddings(
                Table::Episodic,
                "bge-m3",
                &[(e, Fake::vector("blind event"))],
            )
            .unwrap();
        assert_eq!(written, vec![e]);
        assert!(distance(&cfg, Table::Episodic, e, &Fake::vector("blind event")) < 1e-6);
    }

    /// A unit vector at `deg` degrees in the plane of dimensions `2*plane` and
    /// `2*plane+1`, zero elsewhere: two of them in one plane lie exactly
    /// `1 - cos(angle between)` apart, and any two planes are orthogonal.
    pub(crate) fn angle(plane: usize, deg: f64) -> Vec<f32> {
        let mut v = vec![0.0f32; crate::config::DIM];
        v[2 * plane] = deg.to_radians().cos() as f32;
        v[2 * plane + 1] = deg.to_radians().sin() as f32;
        v
    }

    /// A row with exactly this vector.
    pub(crate) fn put_vector(cfg: &Config, table: Table, text: &str, v: &[f32]) -> i64 {
        let id = put(cfg, table, text, false);
        set_vector(cfg, table, id, v, "bge-m3");
        id
    }

    fn supersede(cfg: &Config, old: i64, by: i64) {
        rows(
            cfg,
            &format!("UPDATE semantic_memory SET superseded_by = {by} WHERE id = {old};"),
        );
    }

    fn retire_directly(cfg: &Config, id: i64) {
        rows(
            cfg,
            &format!("UPDATE semantic_memory SET retired_at = '2026-01-02' WHERE id = {id};"),
        );
    }

    // ---- what a sleep pass reads and writes -------------------------------

    #[test]
    fn active_rows_are_current_semantic_or_every_episodic_row_oldest_first() {
        let cfg = scratch("active-rows");
        let late = put(&cfg, Table::Semantic, "late", true);
        let early = put(&cfg, Table::Semantic, "early", false);
        let tie = put(&cfg, Table::Semantic, "same time as early", false);
        let old = put(&cfg, Table::Semantic, "superseded", true);
        let gone = put(&cfg, Table::Semantic, "retired", true);
        supersede(&cfg, old, late);
        retire_directly(&cfg, gone);
        rows(
            &cfg,
            &format!(
                "UPDATE semantic_memory SET created_at = datetime('now', '-10 days'), \
                 topic = 'a topic' WHERE id = {late};"
            ),
        );
        rows(
            &cfg,
            &format!(
                "UPDATE semantic_memory SET created_at = datetime('now', '-40 days') \
                 WHERE id IN ({early}, {tie});"
            ),
        );
        let e = put(&cfg, Table::Episodic, "an event", false);

        let store = TursoStore::open(&cfg).unwrap();
        let got = store.active_rows(Table::Semantic).unwrap();
        let ids: Vec<i64> = got.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![early, tie, late], "{got:?}");
        let late_row = &got[2];
        assert_eq!(late_row.topic.as_deref(), Some("a topic"));
        assert_eq!(late_row.memory_text, "late");
        assert!(late_row.embedded);
        assert!(!got[0].embedded);
        assert_eq!(got[0].topic, None);
        let age = late_row.age_days.unwrap();
        assert!((age - 10.0).abs() < 0.01, "{age}");
        assert!((got[0].age_days.unwrap() - 40.0).abs() < 0.01);
        assert_eq!(late_row.created_at.len(), "2026-01-01 00:00:00".len());

        let episodic = store.active_rows(Table::Episodic).unwrap();
        assert_eq!(episodic.len(), 1);
        assert_eq!(episodic[0].id, e);
        assert_eq!(episodic[0].created_at, "2026-01-01 00:00:00");
    }

    /// A date the engine cannot read has no age, rather than a wrong one.
    #[test]
    fn an_unreadable_created_at_has_no_age() {
        let cfg = scratch("active-rows-bad-date");
        let id = put(&cfg, Table::Semantic, "odd date", false);
        rows(
            &cfg,
            &format!("UPDATE semantic_memory SET created_at = 'last spring' WHERE id = {id};"),
        );
        let got = TursoStore::open(&cfg)
            .unwrap()
            .active_rows(Table::Semantic)
            .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].age_days, None);
        assert_eq!(got[0].created_at, "last spring");
    }

    #[test]
    fn close_pairs_are_active_embedded_rows_below_the_threshold_once_each() {
        let cfg = scratch("close-pairs");
        // 0 and 20 degrees: 1 - cos 20 = 0.0603. 0 and 40: 0.234.
        let a = put_vector(&cfg, Table::Semantic, "a", &angle(0, 0.0));
        let b = put_vector(&cfg, Table::Semantic, "b", &angle(0, 20.0));
        let c = put_vector(&cfg, Table::Semantic, "c", &angle(0, 40.0));
        let far = put_vector(&cfg, Table::Semantic, "other plane", &angle(1, 0.0));
        let old = put_vector(&cfg, Table::Semantic, "superseded twin", &angle(0, 0.0));
        let gone = put_vector(&cfg, Table::Semantic, "retired twin", &angle(0, 0.0));
        put(&cfg, Table::Semantic, "no vector", false);
        supersede(&cfg, old, a);
        retire_directly(&cfg, gone);
        let e1 = put_vector(&cfg, Table::Episodic, "e1", &angle(0, 0.0));
        let e2 = put_vector(&cfg, Table::Episodic, "e2", &angle(0, 1.0));
        let _ = far;

        let store = TursoStore::open(&cfg).unwrap();
        assert_eq!(
            store.close_pairs(Table::Semantic, 0.10).unwrap(),
            vec![(a, b), (b, c)]
        );
        assert_eq!(
            store.close_pairs(Table::Semantic, 0.30).unwrap(),
            vec![(a, b), (a, c), (b, c)]
        );
        assert!(store.close_pairs(Table::Semantic, 0.05).unwrap().is_empty());
        assert_eq!(
            store.close_pairs(Table::Episodic, 0.10).unwrap(),
            vec![(e1, e2)]
        );
    }

    #[test]
    fn existing_ids_lists_rows_of_that_table_current_or_not() {
        let cfg = scratch("existing-ids");
        let s = put(&cfg, Table::Semantic, "s", false);
        let old = put(&cfg, Table::Semantic, "old", false);
        supersede(&cfg, old, s);
        let e = put(&cfg, Table::Episodic, "e", false);
        let store = TursoStore::open(&cfg).unwrap();
        let mut got = store.existing_ids(Table::Semantic, &[s, old, 99]).unwrap();
        got.sort();
        assert_eq!(got, vec![s, old]);
        assert_eq!(
            store.existing_ids(Table::Episodic, &[e, 99]).unwrap(),
            vec![e]
        );
        assert!(store.existing_ids(Table::Episodic, &[]).unwrap().is_empty());
    }

    #[test]
    fn mark_processed_stamps_only_the_given_episodic_rows() {
        let cfg = scratch("mark-processed");
        let a = put(&cfg, Table::Episodic, "a", false);
        let b = put(&cfg, Table::Episodic, "b", false);
        let c = put(&cfg, Table::Episodic, "c", false);
        let store = TursoStore::open(&cfg).unwrap();
        assert_eq!(store.mark_processed(&[a, c]).unwrap(), 2);
        assert_eq!(
            rows(
                &cfg,
                "SELECT id, processed_at IS NOT NULL FROM episodic_memory ORDER BY id;"
            ),
            vec![
                vec![Cell::Int(a), Cell::Int(1)],
                vec![Cell::Int(b), Cell::Int(0)],
                vec![Cell::Int(c), Cell::Int(1)],
            ]
        );
    }

    #[test]
    fn retire_semantic_sets_retired_at_on_a_current_row_and_deletes_nothing() {
        let cfg = scratch("retire");
        let a = put(&cfg, Table::Semantic, "a", false);
        let old = put(&cfg, Table::Semantic, "old", false);
        supersede(&cfg, old, a);
        rows(
            &cfg,
            "UPDATE semantic_memory SET updated_at = '2026-01-01 00:00:00';",
        );
        let store = TursoStore::open(&cfg).unwrap();

        assert!(store.retire_semantic(a).unwrap());
        assert_eq!(
            rows(
                &cfg,
                &format!(
                    "SELECT retired_at >= datetime('now', '-1 minute'), \
                     updated_at >= datetime('now', '-1 minute') \
                     FROM semantic_memory WHERE id = {a};"
                )
            ),
            vec![vec![Cell::Int(1), Cell::Int(1)]]
        );
        assert!(!store.retire_semantic(a).unwrap(), "already retired");
        assert!(!store.retire_semantic(old).unwrap(), "superseded");
        assert!(!store.retire_semantic(99).unwrap(), "unknown");
        assert_eq!(
            rows(&cfg, "SELECT count(*) FROM semantic_memory;"),
            vec![vec![Cell::Int(2)]]
        );
        assert_eq!(
            rows(
                &cfg,
                &format!("SELECT retired_at FROM semantic_memory WHERE id = {old};")
            ),
            vec![vec![Cell::Null]]
        );
    }

    #[test]
    fn replace_topics_is_a_full_replace() {
        let cfg = scratch("replace-topics");
        rows(
            &cfg,
            "INSERT INTO topic_keywords (topic, keywords) VALUES ('old', 'x'), ('kept?', 'y');",
        );
        let store = TursoStore::open(&cfg).unwrap();
        store
            .replace_topics(&[
                ("kept?".into(), "new keywords".into()),
                ("fresh".into(), "Grüße, ümlauts".into()),
            ])
            .unwrap();
        assert_eq!(
            rows(
                &cfg,
                "SELECT topic, keywords, updated_at >= datetime('now', '-1 minute') \
                 FROM topic_keywords ORDER BY topic;"
            ),
            vec![
                vec![
                    Cell::Text("fresh".into()),
                    Cell::Text("Grüße, ümlauts".into()),
                    Cell::Int(1)
                ],
                vec![
                    Cell::Text("kept?".into()),
                    Cell::Text("new keywords".into()),
                    Cell::Int(1)
                ],
            ]
        );
        store.replace_topics(&[]).unwrap();
        assert_eq!(
            rows(&cfg, "SELECT count(*) FROM topic_keywords;"),
            vec![vec![Cell::Int(0)]]
        );
    }

    #[test]
    fn stranded_by_lists_rows_outside_the_set_that_one_inside_supersedes() {
        let cfg = scratch("stranded-by");
        let v1 = put(&cfg, Table::Semantic, "v1", false);
        let v2 = put(&cfg, Table::Semantic, "v2", false);
        let v3 = put(&cfg, Table::Semantic, "v3", false);
        supersede(&cfg, v1, v2);
        supersede(&cfg, v2, v3);
        let store = TursoStore::open(&cfg).unwrap();
        assert_eq!(store.stranded_by(&[v2]).unwrap(), vec![v1]);
        assert!(store.stranded_by(&[v1, v2]).unwrap().is_empty());
        assert!(store.stranded_by(&[v1]).unwrap().is_empty());
    }

    fn tag(cfg: &Config, table: &str, memory_id: i64, coworker: i64) {
        rows(
            cfg,
            &format!(
                "INSERT INTO memory_coworkers (memory_table, memory_id, coworker_id) \
                 VALUES ('{table}', {memory_id}, {coworker});"
            ),
        );
    }

    #[test]
    fn purge_deletes_the_rows_and_their_semantic_tags_and_nothing_else() {
        let cfg = scratch("purge-semantic");
        rows(
            &cfg,
            "INSERT INTO coworkers (name, expertise, personality) VALUES ('jeff', 'x', 'y');",
        );
        let keep = put(&cfg, Table::Semantic, "current", false);
        let old = put(&cfg, Table::Semantic, "superseded", false);
        let gone = put(&cfg, Table::Semantic, "retired", false);
        supersede(&cfg, old, keep);
        retire_directly(&cfg, gone);
        // An episodic row with the purged id, and its tag: neither is touched.
        put(&cfg, Table::Episodic, "e1", false);
        put(&cfg, Table::Episodic, "e2", false);
        tag(&cfg, "semantic", old, 1);
        tag(&cfg, "semantic", keep, 1);
        tag(&cfg, "episodic", old, 1);
        rows(
            &cfg,
            &format!(
                "INSERT INTO eval_cases (id, class, memory_table, query, expect_ids, \
                 expect_stamps) VALUES ('x01', 'exact-id', 'semantic', 'q', '{old}', \
                 '2026-01-01 00:00:00');"
            ),
        );

        let store = TursoStore::open(&cfg).unwrap();
        assert_eq!(
            store.purge_semantic(&[old, gone]).unwrap(),
            Purged {
                rows: 2,
                coworker_tags: 1
            }
        );
        assert_eq!(
            rows(&cfg, "SELECT id FROM semantic_memory ORDER BY id;"),
            vec![vec![Cell::Int(keep)]]
        );
        assert_eq!(
            rows(
                &cfg,
                "SELECT memory_table, memory_id FROM memory_coworkers ORDER BY memory_table;"
            ),
            vec![
                vec![Cell::Text("episodic".into()), Cell::Int(old)],
                vec![Cell::Text("semantic".into()), Cell::Int(keep)],
            ]
        );
        assert_eq!(
            rows(&cfg, "SELECT count(*) FROM episodic_memory;"),
            vec![vec![Cell::Int(2)]]
        );
        assert_eq!(
            rows(&cfg, "SELECT expect_ids FROM eval_cases;"),
            vec![vec![Cell::Text(old.to_string())]],
            "a dangling eval target is inert; --validate is what finds it"
        );
    }

    /// The transaction re-checks what the caller checked: a current row or a
    /// set that would strand a survivor deletes nothing at all.
    #[test]
    fn purge_refuses_a_current_row_or_a_stranding_set_and_deletes_nothing() {
        let cfg = scratch("purge-guarded");
        let v1 = put(&cfg, Table::Semantic, "v1", false);
        let v2 = put(&cfg, Table::Semantic, "v2", false);
        let v3 = put(&cfg, Table::Semantic, "v3", false);
        supersede(&cfg, v1, v2);
        supersede(&cfg, v2, v3);
        let store = TursoStore::open(&cfg).unwrap();
        for ids in [vec![v1, v3], vec![v2], vec![v1, 99]] {
            let err = store.purge_semantic(&ids).unwrap_err();
            assert_eq!(
                err.exit_code(),
                crate::error::EXIT_REFUSED,
                "{ids:?}: {err}"
            );
            assert_eq!(
                rows(&cfg, "SELECT count(*) FROM semantic_memory;"),
                vec![vec![Cell::Int(3)]],
                "{ids:?}"
            );
        }
        assert_eq!(store.purge_semantic(&[v1, v2]).unwrap().rows, 2);
    }

    #[test]
    fn semantic_within_lists_close_current_rows_but_never_the_row_itself() {
        let cfg = scratch("semantic-within");
        let me = put(&cfg, Table::Semantic, "twin", true);
        let twin = put(&cfg, Table::Semantic, "twin", true);
        put(&cfg, Table::Semantic, "far away", true);
        let old = put(&cfg, Table::Semantic, "twin", true);
        put(&cfg, Table::Semantic, "twin", false);
        rows(
            &cfg,
            &format!("UPDATE semantic_memory SET superseded_by = {twin} WHERE id = {old};"),
        );

        let store = TursoStore::open(&cfg).unwrap();
        let near = store
            .semantic_within(me, &Fake::vector("twin"), 0.10)
            .unwrap();
        assert_eq!(near.len(), 1, "{near:?}");
        assert_eq!(near[0].0, twin);
        assert!(near[0].1 < 1e-4, "{near:?}");
    }
}
