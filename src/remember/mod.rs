//! `remember`: store ONE memory, or revise existing ones.
//!
//! Every mechanical guard that keeps this corpus clean lives here. That is the
//! whole point of the command: a permissive writer does not fail, it degrades
//! memory quality invisibly over months, and nothing downstream can tell a bad
//! row from a good one. So the order below is deliberate -- argument errors
//! before the database, the database before Ollama, and the near-duplicate
//! checks before the insert, the cosine one after the embedding it needs.
//!
//! No embedding available is not one of those guards: the row is stored
//! without a vector, and the output says so.

pub mod validate;

use crate::config::Config;
use crate::embed::{self, Embedder};
use crate::error::{Error, Result};
use crate::render;
use crate::store::{self, NewMemory, Store, Table};
use validate::{CATEGORIES, EVENT_TYPES, IMPORTANCES};

/// Cosine distance below which a new semantic memory is considered a duplicate
/// of one already stored. Corpus-calibrated, like RECALL_ALPHA.
pub(crate) const DUP_DIST: f64 = 0.10;

#[derive(Debug, Clone, Default)]
pub struct Args {
    pub table: Option<Table>,
    pub text: String,
    pub topic: Option<String>,
    pub project: Option<String>,
    pub keywords: Option<String>,
    pub source: Option<String>,
    pub model: Option<String>,
    pub file_reference: Option<String>,
    pub created_at: Option<String>,
    pub category: Option<String>,
    pub event_type: Option<String>,
    pub importance: Option<String>,
    pub confirm_baseline: bool,
    pub force: bool,
    pub supersedes: Option<String>,
    pub coworker: Option<String>,
}

/// Everything that can be decided without touching the world.
///
/// Split out so the guards are exercised by unit tests: a refusal that needs a
/// database and an Ollama to reproduce is a refusal nobody re-tests.
#[derive(Debug, Clone)]
struct Validated {
    table: Table,
    memory_text: String,
    topic: Option<String>,
    project: Option<String>,
    source: Option<String>,
    model: Option<String>,
    file_reference: Option<String>,
    created_at: Option<String>,
    category: Option<String>,
    event_type: Option<String>,
    importance: Option<String>,
    supersedes: Vec<i64>,
    coworkers: Vec<String>,
    force: bool,
}

fn validate(args: &Args) -> Result<Validated> {
    let table = args.table.ok_or_else(|| {
        Error::refused("--table is required: 'semantic' (timeless facts) or 'episodic' (events).")
    })?;

    // First, because it is the failure that actually happens: a command
    // substitution that came back empty.
    validate::body(&args.text)?;

    let created_at = args
        .created_at
        .as_deref()
        .map(validate::normalise_created_at)
        .transpose()?;

    // The schema's CHECK constraints, restated ahead of the write so a typo is a
    // refusal naming the alternatives rather than a constraint violation after
    // an embedding has already been paid for.
    let (category, event_type, importance) = match table {
        Table::Semantic => {
            let c = validate::blank_to_none(args.category.as_deref()).ok_or_else(|| {
                Error::refused(format!(
                    "--table semantic needs --category (one of: {}).",
                    CATEGORIES.join(", ")
                ))
            })?;
            (
                Some(validate::one_of("--category", &c, &CATEGORIES)?),
                None,
                None,
            )
        }
        Table::Episodic => {
            let e = validate::blank_to_none(args.event_type.as_deref()).ok_or_else(|| {
                Error::refused(format!(
                    "--table episodic needs --event-type (one of: {}).",
                    EVENT_TYPES.join(", ")
                ))
            })?;
            let i = validate::blank_to_none(args.importance.as_deref()).ok_or_else(|| {
                Error::refused(format!(
                    "--table episodic needs --importance (one of: {}).",
                    IMPORTANCES.join(", ")
                ))
            })?;
            (
                None,
                Some(validate::one_of("--event-type", &e, &EVENT_TYPES)?),
                Some(validate::one_of("--importance", &i, &IMPORTANCES)?),
            )
        }
    };

    // Checked before the length cap, so an over-long baseline rule is reported as
    // the thing that actually needs the user in the loop.
    if category.as_deref() == Some("baseline") && !args.confirm_baseline {
        return Err(Error::refused(
            "'baseline' memories load every session and need the user's explicit \
             confirmation. Ask first, then pass --confirm-baseline.",
        ));
    }

    let topic = validate::blank_to_none(args.topic.as_deref());
    let project = validate::blank_to_none(args.project.as_deref());
    let source = validate::blank_to_none(args.source.as_deref());
    let model = validate::blank_to_none(args.model.as_deref());
    validate::meta_fits("--topic", &topic)?;
    validate::meta_fits("--project", &project)?;
    validate::meta_fits("--source", &source)?;
    validate::meta_fits("--model", &model)?;

    let memory_text = validate::assemble_text(&args.text, args.keywords.as_deref())?;

    let supersedes = match &args.supersedes {
        Some(raw) => {
            let ids = validate::parse_supersedes(raw)?;
            if table != Table::Semantic {
                return Err(Error::refused(
                    "--supersedes is only valid for semantic memories. Episodic memory is \
                     append-only: events recur, and revising one would rewrite history.",
                ));
            }
            ids
        }
        None => Vec::new(),
    };

    Ok(Validated {
        table,
        memory_text,
        topic,
        project,
        source,
        model,
        file_reference: validate::blank_to_none(args.file_reference.as_deref()),
        created_at,
        category,
        event_type,
        importance,
        supersedes,
        coworkers: args
            .coworker
            .as_deref()
            .map(validate::parse_coworkers)
            .unwrap_or_default(),
        force: args.force,
    })
}

/// What a successful write reports.
#[derive(Debug)]
pub struct Stored {
    /// The memory's id. Not printed -- the stdout line keeps the Python
    /// writer's wording -- but what a caller writing in bulk reports.
    #[allow(dead_code)]
    pub id: i64,
    /// The stdout line.
    pub line: String,
    /// A stderr line when the row went in without an embedding although
    /// `EMBEDDINGS=on` says there should be one.
    pub warning: Option<String>,
}

pub fn run(cfg: &Config, args: &Args) -> Result<()> {
    let stored = write(cfg, args, &embed::Ollama::new(cfg))?;
    println!("{}", stored.line);
    if let Some(warning) = stored.warning {
        eprintln!("{warning}");
    }
    Ok(())
}

/// Store one memory with the given embedder; everything but the printing.
pub fn write(cfg: &Config, args: &Args, embedder: &dyn Embedder) -> Result<Stored> {
    let v = validate(args)?;
    let store = store::open(cfg)?;

    // Validate EVERY id before writing anything: a merge of N memories into one
    // must not half-apply, leaving some old rows current and some superseded.
    if !v.supersedes.is_empty() {
        let found = store.current_semantic_ids(&v.supersedes)?;
        let bad: Vec<String> = v
            .supersedes
            .iter()
            .filter(|i| !found.contains(i))
            .map(|i| i.to_string())
            .collect();
        if !bad.is_empty() {
            return Err(Error::refused(format!(
                "--supersedes {}: no current (un-superseded, non-retired) semantic row \
                 with that id — nothing revised, nothing stored.",
                bad.join(",")
            )));
        }
    }

    let mut coworker_ids = Vec::new();
    for name in &v.coworkers {
        match store.resolve_coworker(name)? {
            Some(id) => coworker_ids.push(id),
            None => return Err(Error::refused(format!("no coworker named '{name}'."))),
        }
    }

    // No embedding is not a refusal: the row is stored without one and the
    // output says so, because a memory lost to an Ollama outage is worse than
    // one that keyword recall alone can find until `embed --missing` runs.
    let (vector, missing) = match embed::availability(cfg, embedder) {
        Ok(()) => {
            // One embedding model per database. Mixing vector spaces makes cosine
            // distance meaningless, which corrupts recall for the whole corpus
            // rather than just the new row -- so this is refused, never warned
            // about. A row without a vector cannot mix anything, so only a
            // write that carries one is held to it.
            if store.rows_with_other_embed_model(v.table, &cfg.embed_model)? > 0 {
                return Err(Error::refused(format!(
                    "{}_memory has rows embedded with a different model than '{}'. Mixing \
                     vector spaces makes cosine meaningless — re-embed the DB (rebuild) before \
                     switching models.",
                    v.table.as_str(),
                    cfg.embed_model
                )));
            }
            match embedder.embed(&v.memory_text) {
                Ok(vector) => (Some(vector), None),
                // Ollama failed between the probe and the embed: degrade the
                // same way. A refusal (a wrong-dimension model) still stops.
                Err(Error::Failed(why)) => (None, Some(why)),
                Err(e) => return Err(e),
            }
        }
        Err(why) => (None, Some(why.to_string())),
    };

    // Dedup is semantic-only: episodic memory is append-only because events
    // legitimately recur. --supersedes already says "this replaces that", so the
    // near-duplicate it is replacing is the point.
    if v.table == Table::Semantic && !v.force && v.supersedes.is_empty() {
        if let Some(vector) = &vector {
            let near = store.nearest_semantic(&coworker_ids, vector)?;
            if near.is_some_and(|d| d < DUP_DIST) {
                return Err(Error::refused(format!(
                    "a near-duplicate already exists (cosine {} < {}). Use --supersedes <id> \
                     to replace it, or --force to add anyway.",
                    render::format_real(near.unwrap_or_default()),
                    render::format_real(DUP_DIST)
                )));
            }
        }
        // The guard for rows without a vector, on either side: exact match on
        // the normalised text. Weaker than the cosine guard by design; the
        // near-duplicate report of `embed --missing` and the next sleep pass
        // catch what it lets through.
        let key = validate::dedup_key(&v.memory_text);
        if let Some((id, _)) = store
            .current_semantic_texts(&coworker_ids)?
            .iter()
            .find(|(_, text)| validate::dedup_key(text) == key)
        {
            return Err(Error::refused(format!(
                "an identical memory already exists (id {id}; same text, ignoring case, \
                 whitespace and keywords). Use --supersedes {id} to replace it, or --force to \
                 add anyway."
            )));
        }
    }

    let id = store.insert_memory(&NewMemory {
        table: v.table,
        project: v.project,
        topic: v.topic.clone(),
        category: v.category,
        event_type: v.event_type,
        importance: v.importance,
        source: v.source,
        model: v.model,
        embed_model: vector.as_ref().map(|_| cfg.embed_model.clone()),
        memory_text: v.memory_text,
        file_reference: v.file_reference,
        created_at: v.created_at,
        embedding: vector,
        supersedes: v.supersedes,
        coworkers: coworker_ids,
    })?;

    let stored = format!(
        "stored {} memory ({})",
        v.table.as_str(),
        v.topic.as_deref().unwrap_or("-")
    );
    Ok(match missing {
        None => Stored {
            id,
            line: stored,
            warning: None,
        },
        Some(why) => Stored {
            id,
            line: format!(
                "{stored} WITHOUT embedding — {why}; run embed --missing once Ollama is available"
            ),
            // With EMBEDDINGS=off a row without a vector is the configured
            // state; with it on, the setup is broken and should be fixed.
            warning: cfg.embeddings.then(|| {
                format!(
                    "warning: EMBEDDINGS=on, but this row has no embedding ({why}) — recall \
                     finds it by keyword only until embed --missing runs"
                )
            }),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::EXIT_REFUSED;

    fn semantic(text: &str) -> Args {
        Args {
            table: Some(Table::Semantic),
            text: text.to_string(),
            category: Some("reference".to_string()),
            ..Args::default()
        }
    }

    fn episodic() -> Args {
        Args {
            table: Some(Table::Episodic),
            text: "an event".to_string(),
            event_type: Some("bug_fix".to_string()),
            importance: Some("routine".to_string()),
            ..Args::default()
        }
    }

    fn refusal(args: &Args) -> String {
        let e = validate(args).unwrap_err();
        assert_eq!(
            e.exit_code(),
            EXIT_REFUSED,
            "a guard rail firing must be a refusal: {e}"
        );
        e.to_string()
    }

    #[test]
    fn a_complete_semantic_memory_validates() {
        let v = validate(&semantic("a fact")).unwrap();
        assert_eq!(v.memory_text, "a fact");
        assert_eq!(v.category.as_deref(), Some("reference"));
    }

    #[test]
    fn a_complete_episodic_memory_validates() {
        let v = validate(&episodic()).unwrap();
        assert_eq!(v.event_type.as_deref(), Some("bug_fix"));
        assert_eq!(v.importance.as_deref(), Some("routine"));
        assert!(v.category.is_none());
    }

    /// Every guard below refuses BEFORE anything is opened or embedded, which is
    /// what lets these be unit tests at all.
    #[test]
    fn an_empty_text_is_refused() {
        assert!(refusal(&semantic("   ")).contains("--text is empty"));
    }

    #[test]
    fn a_baseline_needs_the_users_confirmation() {
        let mut a = semantic("a rule");
        a.category = Some("baseline".to_string());
        assert!(refusal(&a).contains("--confirm-baseline"));
        a.confirm_baseline = true;
        assert!(validate(&a).is_ok());
    }

    #[test]
    fn an_over_cap_memory_is_refused() {
        assert!(refusal(&semantic(&"x".repeat(2001))).contains("hard cap 2000"));
    }

    #[test]
    fn a_malformed_created_at_is_refused() {
        let mut a = semantic("a fact");
        a.created_at = Some("March 2024".to_string());
        assert!(refusal(&a).contains("not a date this system can store"));
        a.created_at = Some("2024-03-07".to_string());
        assert_eq!(
            validate(&a).unwrap().created_at.as_deref(),
            Some("2024-03-07 00:00:00")
        );
    }

    #[test]
    fn an_invalid_category_is_refused_and_names_the_set() {
        let mut a = semantic("a fact");
        a.category = Some("gotcha".to_string());
        let msg = refusal(&a);
        assert!(msg.contains("--category 'gotcha'"), "{msg}");
        assert!(msg.contains("reference"), "{msg}");
    }

    #[test]
    fn a_semantic_memory_without_a_category_is_refused() {
        let mut a = semantic("a fact");
        a.category = None;
        assert!(refusal(&a).contains("needs --category"));
    }

    #[test]
    fn an_invalid_event_type_or_importance_is_refused() {
        let mut a = episodic();
        a.event_type = Some("bugfix".to_string());
        assert!(refusal(&a).contains("--event-type 'bugfix'"));
        let mut a = episodic();
        a.importance = Some("critical".to_string());
        assert!(refusal(&a).contains("--importance 'critical'"));
    }

    #[test]
    fn an_episodic_memory_missing_its_required_flags_is_refused() {
        let mut a = episodic();
        a.event_type = None;
        assert!(refusal(&a).contains("needs --event-type"));
        let mut a = episodic();
        a.importance = None;
        assert!(refusal(&a).contains("needs --importance"));
    }

    /// Episodic memory is append-only: revising an event would rewrite history,
    /// and nothing downstream expects a superseded episodic row.
    #[test]
    fn supersedes_is_refused_for_episodic_memory() {
        let mut a = episodic();
        a.supersedes = Some("7".to_string());
        assert!(refusal(&a).contains("only valid for semantic"));
    }

    #[test]
    fn supersedes_accepts_a_csv_of_ids() {
        let mut a = semantic("the merged fact");
        a.supersedes = Some("11,12, 13".to_string());
        assert_eq!(validate(&a).unwrap().supersedes, vec![11, 12, 13]);
    }

    #[test]
    fn a_non_numeric_supersedes_is_refused() {
        let mut a = semantic("a fact");
        a.supersedes = Some("11,oops".to_string());
        assert!(refusal(&a).contains("not a comma-separated id list"));
    }

    #[test]
    fn keywords_are_folded_into_the_stored_text() {
        let mut a = semantic("a fact");
        a.keywords = Some("turso, vfs".to_string());
        assert_eq!(
            validate(&a).unwrap().memory_text,
            "a fact\n\nKeywords: turso, vfs"
        );
    }

    #[test]
    fn an_over_long_topic_is_refused_before_the_insert() {
        let mut a = semantic("a fact");
        a.topic = Some("t".repeat(129));
        assert!(refusal(&a).contains("hard cap 128"));
    }

    #[test]
    fn a_missing_table_is_refused_rather_than_guessed() {
        let mut a = semantic("a fact");
        a.table = None;
        assert!(refusal(&a).contains("--table is required"));
    }

    // ---- writes with and without an embedding, against a scratch database ----

    use crate::embed::Unavailable;
    use crate::embed::fake::Fake;
    use crate::store::Cell;
    use crate::store::turso::tests::{rows, scratch};

    fn down() -> Fake {
        Fake::down(Unavailable::Unreachable {
            url: "http://127.0.0.1:9".into(),
        })
    }

    /// `[embedding IS NULL, embed_model]` of one semantic row.
    fn vector_of(cfg: &Config, id: i64) -> Vec<Cell> {
        rows(
            cfg,
            &format!("SELECT embedding IS NULL, embed_model FROM semantic_memory WHERE id = {id};"),
        )
        .pop()
        .unwrap_or_else(|| panic!("no semantic row {id}"))
    }

    fn semantic_rows(cfg: &Config) -> i64 {
        match rows(cfg, "SELECT count(*) FROM semantic_memory;")[0][0] {
            Cell::Int(n) => n,
            ref other => panic!("{other:?}"),
        }
    }

    fn refused(r: Result<Stored>) -> String {
        let e = r.expect_err("must be refused");
        assert_eq!(e.exit_code(), EXIT_REFUSED, "{e}");
        let msg = e.to_string();
        assert!(msg.starts_with("refused: "), "{msg}");
        msg
    }

    #[test]
    fn without_an_embedder_the_row_is_stored_without_one_and_says_so() {
        let cfg = scratch("rem-no-embedder");
        let s = write(&cfg, &semantic("turso needs the wal flag"), &down())
            .expect("stored, not refused");
        assert_eq!(vector_of(&cfg, s.id), vec![Cell::Int(1), Cell::Null]);
        assert_eq!(
            s.line,
            "stored semantic memory (-) WITHOUT embedding — Ollama unreachable at \
             http://127.0.0.1:9; run embed --missing once Ollama is available"
        );
        // EMBEDDINGS=on promised a vector, so its absence is also flagged.
        let warning = s
            .warning
            .expect("EMBEDDINGS=on must flag the missing vector");
        assert!(warning.starts_with("warning: "), "{warning}");
        assert!(warning.contains("EMBEDDINGS=on"), "{warning}");
    }

    #[test]
    fn embeddings_off_never_calls_the_embedder() {
        let mut cfg = scratch("rem-off");
        cfg.embeddings = false;
        let fake = Fake::up();
        let s = write(&cfg, &semantic("a fact"), &fake).expect("stored");
        assert_eq!(
            fake.calls.get(),
            0,
            "EMBEDDINGS=off must not contact Ollama"
        );
        assert_eq!(vector_of(&cfg, s.id), vec![Cell::Int(1), Cell::Null]);
        assert!(
            s.line.contains("WITHOUT embedding — EMBEDDINGS=off"),
            "{}",
            s.line
        );
        assert!(
            s.warning.is_none(),
            "off is the configured state, not a surprise"
        );
    }

    #[test]
    fn with_an_embedder_the_vector_and_its_model_are_stored() {
        let cfg = scratch("rem-embedded");
        let s = write(&cfg, &semantic("a fact"), &Fake::up()).expect("stored");
        assert_eq!(s.line, "stored semantic memory (-)");
        assert!(s.warning.is_none());
        assert_eq!(
            vector_of(&cfg, s.id),
            vec![Cell::Int(0), Cell::Text("bge-m3".into())]
        );
    }

    #[test]
    fn an_exact_normalized_duplicate_is_refused_and_force_stores_it() {
        let cfg = scratch("rem-text-dedup");
        let mut first = semantic("Turso needs the WAL flag.");
        first.keywords = Some("turso, wal".into());
        let first = write(&cfg, &first, &down()).expect("stored");

        let mut again = semantic("turso   needs the wal\nflag.");
        let msg = refused(write(&cfg, &again, &down()));
        assert!(msg.contains(&format!("--supersedes {}", first.id)), "{msg}");
        assert_eq!(semantic_rows(&cfg), 1, "a refusal stores nothing");

        again.force = true;
        write(&cfg, &again, &down()).expect("--force stores it anyway");
        write(&cfg, &semantic("Turso needs the VFS flag."), &down())
            .expect("a one-word difference is a different memory");
        assert_eq!(semantic_rows(&cfg), 3);
    }

    /// The vector guard cannot see a row without a vector, so an embedded write
    /// identical to an unembedded row is caught by the text guard instead.
    #[test]
    fn an_embedded_write_identical_to_an_unembedded_row_is_refused() {
        let cfg = scratch("rem-text-dedup-mixed");
        write(&cfg, &semantic("a fact"), &down()).expect("stored");
        refused(write(&cfg, &semantic("A fact"), &Fake::up()));
    }

    #[test]
    fn supersedes_skips_the_text_guard() {
        let cfg = scratch("rem-text-supersedes");
        let first = write(&cfg, &semantic("a fact"), &down()).expect("stored");
        let mut b = semantic("a fact");
        b.supersedes = Some(first.id.to_string());
        write(&cfg, &b, &down()).expect("a revision, not a duplicate");
    }

    #[test]
    fn episodic_memory_has_no_text_guard() {
        let cfg = scratch("rem-episodic-twice");
        write(&cfg, &episodic(), &down()).expect("stored");
        write(&cfg, &episodic(), &down()).expect("events recur");
    }

    /// Rows without a vector record no model, so they do not pin one.
    #[test]
    fn a_db_of_only_unembedded_rows_accepts_the_first_embedded_write() {
        let mut cfg = scratch("rem-model-first");
        write(&cfg, &semantic("stored blind"), &down()).expect("stored");
        cfg.embed_model = "another-model".into();
        let s = write(&cfg, &semantic("now with a vector"), &Fake::up())
            .expect("NULL embed_model rows do not count");
        assert_eq!(
            vector_of(&cfg, s.id),
            vec![Cell::Int(0), Cell::Text("another-model".into())]
        );
    }

    #[test]
    fn bge_m3_rows_plus_unembedded_rows_refuse_another_model() {
        let mut cfg = scratch("rem-model-mixed");
        write(&cfg, &semantic("embedded"), &Fake::up()).expect("stored");
        write(&cfg, &semantic("stored blind"), &down()).expect("stored");
        cfg.embed_model = "another-model".into();
        let msg = refused(write(&cfg, &semantic("a third fact"), &Fake::up()));
        assert!(msg.contains("different model"), "{msg}");
        assert_eq!(semantic_rows(&cfg), 2);
    }

    /// A row stored without a vector cannot mix vector spaces, so a configured
    /// model that differs from the database's is no reason to lose the write.
    #[test]
    fn a_write_without_a_vector_is_not_held_to_the_model_check() {
        let mut cfg = scratch("rem-model-blind");
        write(&cfg, &semantic("embedded"), &Fake::up()).expect("stored");
        cfg.embed_model = "another-model".into();
        cfg.embeddings = false;
        let s = write(&cfg, &semantic("stored blind"), &Fake::up()).expect("stored");
        assert_eq!(vector_of(&cfg, s.id), vec![Cell::Int(1), Cell::Null]);
    }
}
