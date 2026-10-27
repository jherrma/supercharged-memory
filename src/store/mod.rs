//! The storage seam.
//!
//! The trait is deliberately MINIMAL: it holds exactly the operations a landed
//! command needs, and grows as commands land. A trait designed against
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

use crate::config::Config;
use crate::error::{Error, Result};
use crate::recall::rank::WeightedToken;

/// A table that stores rows with text and an embedding: the two memory tables,
/// and the instructions -- directives, not memories, but searched, embedded and
/// guarded the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Table {
    Semantic,
    Episodic,
    Instructions,
}

impl Table {
    /// Every table that stores an embedding. One model per database covers
    /// them all: another model in any one of them already mixes the vector
    /// space.
    pub const EMBEDDED: [Table; 3] = [Table::Semantic, Table::Episodic, Table::Instructions];

    /// The short name the output uses.
    pub fn as_str(&self) -> &'static str {
        match self {
            Table::Semantic => "semantic",
            Table::Episodic => "episodic",
            Table::Instructions => "instructions",
        }
    }

    /// The SQL table.
    pub fn name(&self) -> &'static str {
        match self {
            Table::Semantic => "semantic_memory",
            Table::Episodic => "episodic_memory",
            Table::Instructions => "instructions",
        }
    }
}

/// Which rows a scope covers by whether they carry an embedding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Embedded {
    /// Every row: keyword-only search, and the counts behind `READY n`.
    Any,
    /// Rows with a vector: the only ones a vector search can rank.
    With,
    /// Rows stored without one, which recall lists in a section of their own.
    Without,
}

/// Which rows a search may see: one table, optionally narrowed to a work item,
/// optionally filtered to what a coworker persona can see (rows tagged to nobody
/// are global, rows tagged to this coworker are theirs, everything else is not),
/// and narrowed by whether the row has an embedding.
#[derive(Debug, Clone)]
pub struct Scope {
    pub table: Table,
    pub project: Option<String>,
    pub coworker: Option<i64>,
    pub embedded: Embedded,
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

/// A memory about to be written: already validated, and embedded when an
/// embedding was available.
///
/// Deliberately a value, not a builder over SQL -- a MongoDB backend gets the
/// same struct. `supersedes` and `coworkers` ride along because they must land
/// in the SAME transaction as the row itself: a merge of N memories into one
/// that half-applies leaves some old rows current and some superseded, which no
/// later pass can tell apart from a deliberate state.
#[derive(Debug, Clone)]
pub struct NewMemory {
    pub table: Table,
    pub project: Option<String>,
    pub topic: Option<String>,
    /// Semantic only.
    pub category: Option<String>,
    /// Episodic only.
    pub event_type: Option<String>,
    /// Episodic only.
    pub importance: Option<String>,
    pub source: Option<String>,
    pub model: Option<String>,
    /// `None` together with `embedding`: a row stored without a vector records
    /// no model, so it does not count towards the one-model-per-database check.
    pub embed_model: Option<String>,
    /// The text as it will be stored, keywords already appended into it.
    pub memory_text: String,
    pub file_reference: Option<String>,
    /// `None` lets the schema default to CURRENT_TIMESTAMP.
    pub created_at: Option<String>,
    /// `None` when no embedding was available; stored as NULL.
    pub embedding: Option<Vec<f32>>,
    /// Current semantic ids this row replaces.
    pub supersedes: Vec<i64>,
    /// A current row of the same table this one replaces: retired in the same
    /// transaction, or nothing is stored. How an instruction is revised.
    pub retires: Option<i64>,
    /// Coworker ids this memory is scoped to. Empty = global.
    pub coworkers: Vec<i64>,
}

/// How `instructions --get` names what it wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstructionKey<'a> {
    Id(i64),
    /// Matched whole, ignoring ASCII case.
    Topic(&'a str),
}

/// Whether an instruction id names a row, and whether that row is current.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionState {
    pub topic: String,
    pub retired: bool,
}

/// An active row as the sleep reports read it: everything but its vector.
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveRow {
    pub id: i64,
    pub created_at: String,
    /// Days since `created_at`, by the database's own date arithmetic; `None`
    /// when it cannot read the date.
    pub age_days: Option<f64>,
    pub topic: Option<String>,
    pub memory_text: String,
    pub embedded: bool,
}

/// What a purge deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Purged {
    pub rows: u64,
    /// `memory_coworkers` rows that scoped the deleted memories.
    pub coworker_tags: u64,
}

/// A coworker persona's row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coworker {
    pub id: i64,
    pub created_at: String,
    pub updated_at: String,
    pub name: String,
    pub expertise: String,
    pub personality: String,
    pub trust_level: String,
    /// `false` once retired. A coworker is never deleted.
    pub active: bool,
}

/// A coworker's current appraisal: the one no later appraisal supersedes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Appraisal {
    pub id: i64,
    pub created_at: String,
    /// The start of the feedback window this appraisal folded in.
    pub period_start: Option<String>,
    pub trust_level: String,
    pub memory_text: String,
}

/// What an appraisal wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Appraised {
    pub id: i64,
    /// The appraisals it superseded: normally the one before it, none for the
    /// first.
    pub superseded: Vec<i64>,
}

/// An authored recall-quality case: a query, the rows it should find, and the
/// `created_at` of each of those rows at the time it was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalCase {
    pub id: String,
    pub class: String,
    /// `Semantic` or `Episodic`; instructions are not measured.
    pub table: Table,
    pub query: String,
    pub expect: Vec<i64>,
    /// `created_at` of each target when the case was authored, in `expect`
    /// order. A reused row id shows up as a stamp that no longer matches.
    pub stamps: Vec<String>,
    pub retired: bool,
}

/// What a row id currently is: whether it exists, when it was created, and
/// whether it is current truth and has a vector to be ranked by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub id: i64,
    pub created_at: String,
    pub current: bool,
    pub embedded: bool,
}

/// One step of a semantic supersede chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainLink {
    pub superseded_by: Option<i64>,
    pub retired: bool,
}

/// One scored run of the harness.
#[derive(Debug, Clone, PartialEq)]
pub struct EvalRun {
    /// `None` lets the schema default to CURRENT_TIMESTAMP.
    pub ran_at: Option<String>,
    pub alpha: f64,
    pub embed_model: String,
    pub n_cases: i64,
    pub r1: f64,
    pub r5: f64,
    pub mrr: f64,
}

/// Whether a listing keeps current rows or the ones that are not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Currency {
    /// Current truth: for semantic memory not superseded and not retired, for an
    /// instruction not retired, and every episodic row.
    Current,
    /// The rest: superseded or retired rows. Episodic memory has none.
    NotCurrent,
}

/// A fixed-shape listing of one table's rows. Every filter present must hold.
/// Deliberately a handful of named fields and not an expression: a backend
/// without SQL answers the same struct.
#[derive(Debug, Clone, Default)]
pub struct RowFilter {
    pub ids: Option<Vec<i64>>,
    pub currency: Option<Currency>,
    /// Episodic rows no sleep pass has sifted yet.
    pub unprocessed: bool,
    /// Matched whole and case-sensitively.
    pub topic: Option<String>,
    pub category: Option<String>,
    pub source: Option<String>,
    pub project: Option<String>,
    /// A stored `created_at`: keeps rows created at or after it.
    pub since: Option<String>,
    /// Add `memory_text` as the last column.
    pub with_text: bool,
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

    /// The ranked hits. With no query vector, a keyword-only ranking of the rows
    /// that hold at least one query token.
    fn search(&self, scope: &Scope, query: &SearchQuery) -> Result<ResultSet>;

    /// How many rows in this table were embedded with a model other than this
    /// one. Non-zero means the vector space is about to be mixed, and cosine
    /// distance across two spaces is meaningless -- the writer refuses.
    fn rows_with_other_embed_model(&self, table: Table, embed_model: &str) -> Result<u64>;

    /// Which of these ids are CURRENT semantic rows (not superseded, not
    /// retired). Asked before any write, so an N-into-1 merge naming one bad id
    /// revises nothing at all rather than half of them.
    fn current_semantic_ids(&self, ids: &[i64]) -> Result<Vec<i64>>;

    /// Id and cosine distance of the nearest active row of this table visible
    /// to these coworkers (empty = the whole table), other than `except`, or
    /// `None` if there is nothing to compare against. The near-duplicate guard.
    fn nearest(
        &self,
        table: Table,
        coworkers: &[i64],
        except: Option<i64>,
        vector: &[f32],
    ) -> Result<Option<(i64, f64)>>;

    /// Id and text of every active row of this table visible to these
    /// coworkers (empty = the whole table): the near-duplicate guard for rows
    /// that have no vector to compare.
    fn current_texts(&self, table: Table, coworkers: &[i64]) -> Result<Vec<(i64, String)>>;

    /// Insert the row, mark everything it supersedes, retire the row it
    /// replaces, and tag its coworkers -- all in ONE transaction. Returns the
    /// MEMORY's id.
    fn insert_memory(&self, m: &NewMemory) -> Result<i64>;

    /// Whether the database holds this table at all. A database from before
    /// the instructions table lacks it until `update` creates it.
    fn has_table(&self, table: Table) -> Result<bool>;

    /// The topic of an instruction and whether it is retired, or `None` when
    /// no row has that id.
    fn instruction_state(&self, id: i64) -> Result<Option<InstructionState>>;

    /// Retire one current instruction: `retired_at` is set, nothing is
    /// deleted. `false` when no current row has that id, and nothing changed.
    fn retire_instruction(&self, id: i64) -> Result<bool>;

    /// The current instructions with this id or topic, full text, newest
    /// first.
    fn get_instructions(&self, key: InstructionKey) -> Result<ResultSet>;

    /// Id, topic and date of every current instruction -- never the text.
    fn list_instructions(&self) -> Result<ResultSet>;

    /// Id and text of the active rows of this table that have no embedding
    /// (current semantic rows, every episodic row, current instructions), by
    /// ascending id, after
    /// `after`, at most `limit`. The cursor, not the NULL alone, is what moves a
    /// run past a row that failed to embed.
    fn unembedded_rows(&self, table: Table, after: i64, limit: u32) -> Result<Vec<(i64, String)>>;

    /// Store each vector with `embed_model`, in ONE transaction, on rows that
    /// are still active and still have no embedding; returns the ids written. A
    /// row another process embedded in the meantime is skipped, never
    /// overwritten.
    fn fill_embeddings(
        &self,
        table: Table,
        embed_model: &str,
        rows: &[(i64, Vec<f32>)],
    ) -> Result<Vec<i64>>;

    /// Active rows of this table other than `id` whose embedding lies within
    /// cosine distance `below` of `vector`, nearest first: the near-duplicate
    /// report.
    fn within(&self, table: Table, id: i64, vector: &[f32], below: f64) -> Result<Vec<(i64, f64)>>;

    /// Every active row of a table (current semantic rows, every episodic
    /// row), oldest first and by id within a tie.
    fn active_rows(&self, table: Table) -> Result<Vec<ActiveRow>>;

    /// Every pair of active, embedded rows closer than `below` in cosine
    /// distance, once each, lower id first.
    fn close_pairs(&self, table: Table, below: f64) -> Result<Vec<(i64, i64)>>;

    /// The rows matching the filter without their text or embedding, oldest
    /// first (ties by id). Columns depend on the table: the descriptive
    /// metadata and the state columns (superseded_by and retired_at, or
    /// processed_at), plus `memory_text` last when asked for.
    fn list_rows(&self, table: Table, filter: &RowFilter) -> Result<ResultSet>;

    /// The full rows with these ids, current or not -- every column but the
    /// embedding -- in no particular order.
    fn show_rows(&self, table: Table, ids: &[i64]) -> Result<ResultSet>;

    /// Which of these ids are rows of this table, current or not.
    fn existing_ids(&self, table: Table, ids: &[i64]) -> Result<Vec<i64>>;

    /// Stamp `processed_at` on these episodic rows, in one statement. Returns
    /// how many rows it stamped.
    fn mark_processed(&self, ids: &[i64]) -> Result<u64>;

    /// Retire one current semantic row: `retired_at` is set, nothing is
    /// deleted. `false` when no current row has that id, and nothing changed.
    fn retire_semantic(&self, id: i64) -> Result<bool>;

    /// Replace the whole topic index with these (topic, keywords) rows, in one
    /// transaction: a full replace, never a merge.
    fn replace_topics(&self, topics: &[(String, String)]) -> Result<()>;

    /// Rows outside `ids` that one of `ids` supersedes: the rows deleting
    /// `ids` would leave pointing at nothing.
    fn stranded_by(&self, ids: &[i64]) -> Result<Vec<i64>>;

    /// Hard-delete these superseded or retired semantic rows and the
    /// `memory_coworkers` rows scoping them, in one transaction. Refuses, and
    /// deletes nothing, unless every id is such a row and none strands another.
    fn purge_semantic(&self, ids: &[i64]) -> Result<Purged>;

    /// The coworker with exactly this name, retired or not.
    fn coworker(&self, name: &str) -> Result<Option<Coworker>>;

    /// Add a coworker, active and supervised; returns its id.
    fn add_coworker(&self, name: &str, expertise: &str, personality: &str) -> Result<i64>;

    /// Store a new appraisal, supersede every current one of this coworker,
    /// and set the coworker's trust level -- in ONE transaction, so exactly
    /// one appraisal is current afterwards. The window it folds in starts at
    /// the appraisal it supersedes, or at the coworker's creation.
    fn appraise(&self, coworker: i64, trust_level: &str, text: &str) -> Result<Appraised>;

    /// Set the trust level without an appraisal.
    fn set_trust(&self, coworker: i64, trust_level: &str) -> Result<()>;

    /// Retire (`false`) or reactivate (`true`) a coworker; nothing is
    /// deleted. `false` when it already was, and nothing changed.
    fn set_active(&self, coworker: i64, active: bool) -> Result<bool>;

    /// Id, name, expertise, trust level and active flag of every coworker,
    /// retired ones included, by id.
    fn list_coworkers(&self) -> Result<ResultSet>;

    /// The coworker's current appraisal, or `None` before the first.
    fn current_appraisal(&self, coworker: i64) -> Result<Option<Appraisal>>;

    /// The current `feedback` memories tagged to this coworker, created at or
    /// after `since`, oldest first.
    fn feedback_since(&self, coworker: i64, since: &str) -> Result<ResultSet>;

    /// Whether the database holds both eval tables. One from before the
    /// harness lacks them, and nothing migrates them in.
    fn has_eval_tables(&self) -> Result<bool>;

    /// Every live eval case (not retired), by id.
    fn eval_cases(&self) -> Result<Vec<EvalCase>>;

    /// The eval case with exactly this id, retired or not.
    fn eval_case(&self, id: &str) -> Result<Option<EvalCase>>;

    /// What each of these ids is in this table, for the ids that exist. A
    /// semantic row is current when it is neither superseded nor retired.
    fn targets(&self, table: Table, ids: &[i64]) -> Result<Vec<Target>>;

    /// The supersede link of one semantic row, or `None` when no row has the id.
    fn chain_link(&self, id: i64) -> Result<Option<ChainLink>>;

    /// Store a new eval case as given; the caller computed the stamps.
    fn insert_eval_case(&self, case: &EvalCase) -> Result<()>;

    /// Replace the targets and their stamps of a live case in ONE statement,
    /// so the two columns can never disagree. `false` when no live case has
    /// this id.
    fn set_eval_targets(&self, case: &str, expect: &[i64], stamps: &[String]) -> Result<bool>;

    /// Retire one live eval case: `retired_at` is set, nothing is deleted.
    /// `false` when no live case has this id.
    fn retire_eval_case(&self, case: &str) -> Result<bool>;

    /// The most recently recorded run (highest id): the baseline a report
    /// diffs against.
    fn last_eval_run(&self) -> Result<Option<EvalRun>>;

    /// The newest `ran_at` of any run.
    fn newest_eval_run_time(&self) -> Result<Option<String>>;

    /// Record one run.
    fn insert_eval_run(&self, run: &EvalRun) -> Result<()>;

    /// Whether a run with exactly these figures and this `ran_at` is recorded.
    fn has_eval_run(&self, run: &EvalRun) -> Result<bool>;

    /// Id and lowercased text of every active row that has a vector: the rows a
    /// vector search ranks.
    fn eval_corpus(&self, table: Table) -> Result<Vec<(i64, String)>>;

    /// Id and cosine distance (six decimals) to `vector` of every active row
    /// that has a vector, by id.
    fn eval_distances(&self, table: Table, vector: &[f32]) -> Result<Vec<(i64, f64)>>;
}

/// Open the configured database, or refuse with the candidate report.
///
/// Never creates one. A missing file almost always means a wrong path, and an
/// empty new database strands the real one -- which is why the refusal carries
/// the report rather than just the path.
pub fn open(cfg: &Config) -> Result<turso::TursoStore> {
    if !cfg.db_exists() {
        let mut msg = String::from("memory DB missing — refusing to silently create an empty one.");
        for line in candidates::missing_report(cfg) {
            msg.push_str(&format!("\n  {line}"));
        }
        return Err(Error::refused(msg));
    }
    turso::TursoStore::open(cfg)
}
