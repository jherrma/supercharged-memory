//! The storage seam.
//!
//! The trait is deliberately MINIMAL: it holds exactly the operations a landed
//! command needs, and grows as stories 004-014 land. A trait designed against
//! imagined callers is how a backend interface ends up shaped like one database's
//! query language, which is the mistake issue #3 (MongoDB) has to avoid.
//!
//! Two invariants every implementation owes:
//!
//! - Multiprocess WAL on every open. This is what lets several Claude sessions
//!   share one database.
//! - Never auto-create a database. A missing file almost always means a wrong
//!   path, and creating an empty one strands the real database.

pub mod candidates;
pub mod turso;

use crate::error::Result;
use crate::recall::rank::WeightedToken;

/// Which memory table a search is scoped to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Table {
    Semantic,
    Episodic,
}

impl Table {
    pub fn as_str(&self) -> &'static str {
        match self {
            Table::Semantic => "semantic",
            Table::Episodic => "episodic",
        }
    }
}

/// Which rows a search may see: one table, optionally narrowed to a work item,
/// optionally filtered to what a coworker persona can see (rows tagged to nobody
/// are global, rows tagged to this coworker are theirs, everything else is not).
#[derive(Debug, Clone)]
pub struct Scope {
    pub table: Table,
    pub project: Option<String>,
    pub coworker: Option<i64>,
}

/// One ranked search, already reduced to numbers by `crate::recall::rank`.
pub struct SearchQuery<'a> {
    pub tokens: &'a [WeightedToken],
    /// The 0..1 normaliser for the keyword share.
    pub total: f64,
    /// Weight of the keyword layer against cosine distance.
    pub alpha: f64,
    pub limit: u32,
    /// `None` when Ollama is down: the search degrades to keyword-only.
    pub vector: Option<&'a [f32]>,
}

/// A single value, in the shapes a memory row can hold.
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
}

/// Rows plus their column names, which is what the output format needs. It is a
/// result set, not a query -- a MongoDB backend produces the same shape.
#[derive(Debug, Clone)]
pub struct ResultSet {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Cell>>,
}

pub trait Store {
    /// Every memory in the database: all semantic rows including superseded and
    /// retired ones, plus all episodic rows. This is the `n` in `READY n`, and it
    /// deliberately counts history rather than current truth -- it answers "is
    /// this the database I think it is", not "how much is live".
    fn total_memories(&self) -> Result<u64>;

    /// Current baseline rules: the must-always-apply memories loaded every
    /// session by name rather than by search.
    fn baseline(&self) -> Result<ResultSet>;

    /// The curated topic -> keywords index, newest first. Loaded every session so
    /// that "no memory found" is not mistaken for "nothing to look for".
    fn topics(&self) -> Result<ResultSet>;

    /// The coworker's id, or `None` if no persona has that name.
    fn resolve_coworker(&self, name: &str) -> Result<Option<i64>>;

    /// How many in-scope rows contain each LIKE pattern, and how many rows are in
    /// scope at all. Measured against exactly the rows the search will rank, so
    /// the IDF means something for this query.
    fn document_frequencies(&self, scope: &Scope, patterns: &[String]) -> Result<(Vec<u64>, u64)>;

    /// The ranked hits.
    fn search(&self, scope: &Scope, query: &SearchQuery) -> Result<ResultSet>;
}
