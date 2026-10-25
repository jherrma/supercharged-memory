//! The Turso backend: the `turso` crate, in-process and statically linked.
//!
//! What used to be `memlib.exec_sql` -- forty lines deciding whether the tursodb
//! CLI had failed, by reading exit status, stderr and stdout, with a carve-out
//! because this corpus stores tursodb error strings as memory rows -- is now a
//! typed error from a driver. None of that heuristic survives, deliberately.

use std::time::Duration;

use turso::{Builder, Connection, Value};

use super::{Cell, Embedded, NewMemory, ResultSet, Scope, SearchQuery, Store, Table};
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
}

/// The active rows of a table that have no embedding.
fn unembedded(table: Table) -> Scope {
    Scope {
        table,
        project: None,
        coworker: None,
        embedded: Embedded::Without,
    }
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
