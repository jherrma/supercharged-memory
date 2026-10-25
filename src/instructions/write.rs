//! `instruct`: store an instruction, revise one, or retire one.
//!
//! The guards are `remember`'s, through the same code: empty text, the
//! 2000-char cap with the keywords counted in it, metadata caps, one embedding
//! model per database -- here checked across every table that stores one, so an
//! empty instructions table cannot let a second model in -- and the
//! near-duplicate check, by vector or by normalised text. Nothing beyond that is
//! instruction-specific: several current instructions may share a topic.
//!
//! A revision is retire + insert in one transaction; there is no supersede
//! chain and no forward link, the retired row and the timestamps are the audit
//! trail. Nothing is ever deleted.

use crate::config::Config;
use crate::embed::{self, Embedder};
use crate::error::{Error, Result};
use crate::remember::{self, DUP_DIST, Duplicate, Stored, validate};
use crate::render;
use crate::store::{self, NewMemory, Store, Table, turso::TursoStore};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Args {
    pub topic: Option<String>,
    pub text: String,
    pub keywords: Option<String>,
    pub source: Option<String>,
    pub model: Option<String>,
    pub project: Option<String>,
    pub file_reference: Option<String>,
    pub force: bool,
    /// The current instruction this one replaces.
    pub replaces: Option<i64>,
}

/// One invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Store(Args),
    Retire(i64),
}

pub fn run(cfg: &Config, mode: &Mode) -> Result<()> {
    match mode {
        Mode::Store(args) => {
            let stored = write(cfg, args, &embed::Ollama::new(cfg))?;
            println!("{}", stored.line);
            if let Some(warning) = stored.warning {
                eprintln!("{warning}");
            }
        }
        Mode::Retire(id) => println!("{}", retire(cfg, *id)?),
    }
    Ok(())
}

/// Everything that can be decided without the database: the stored text and
/// the topic, or a refusal.
struct Validated {
    topic: String,
    memory_text: String,
}

fn validate(args: &Args) -> Result<Validated> {
    // An instruction is fetched by its topic -- "use instruction X" -- so one
    // without a topic could only ever be found by search.
    let topic = validate::blank_to_none(args.topic.as_deref()).ok_or_else(|| {
        Error::refused(
            "--topic is empty: an instruction is looked up by its topic (instructions --get \
             <topic>). Nothing was stored.",
        )
    })?;
    let memory_text = validate::assemble_text(&args.text, args.keywords.as_deref())?;
    validate::meta_fits("--topic", &Some(topic.clone()))?;
    for (flag, value) in [
        ("--project", &args.project),
        ("--source", &args.source),
        ("--model", &args.model),
    ] {
        validate::meta_fits(flag, &validate::blank_to_none(value.as_deref()))?;
    }
    Ok(Validated { topic, memory_text })
}

/// The database to write to, or a refusal when it predates the instructions
/// table.
pub(crate) fn open(cfg: &Config) -> Result<TursoStore> {
    let store = store::open(cfg)?;
    if !store.has_table(Table::Instructions)? {
        return Err(Error::refused(
            "this database has no instructions table yet — run update, which creates it.",
        ));
    }
    Ok(store)
}

/// Store one instruction with the given embedder; everything but the printing.
pub fn write(cfg: &Config, args: &Args, embedder: &dyn Embedder) -> Result<Stored> {
    let v = validate(args)?;
    let store = open(cfg)?;

    // Checked before anything is embedded or written. The insert checks again
    // inside its transaction, so a row retired in between stores nothing.
    if let Some(old) = args.replaces {
        match store.instruction_state(old)? {
            None => {
                return Err(Error::refused(format!(
                    "--replaces {old}: no instruction with id {old} — nothing was stored."
                )));
            }
            Some(state) if state.retired => {
                return Err(Error::refused(format!(
                    "--replaces {old}: instruction {old} ({}) is already retired — nothing was \
                     stored. Revise a current one: instructions --get {}",
                    state.topic, state.topic
                )));
            }
            Some(_) => {}
        }
    }

    let (vector, missing) =
        remember::embedding(cfg, &store, embedder, &v.memory_text, &Table::EMBEDDED)?;

    // The row being replaced may be duplicated -- rewording it is the point --
    // but no other current instruction.
    if !args.force {
        match remember::duplicate(
            &store,
            Table::Instructions,
            &[],
            args.replaces,
            vector.as_deref(),
            &v.memory_text,
        )? {
            Some(Duplicate::Near { id, dist }) => {
                return Err(Error::refused(format!(
                    "a near-duplicate instruction already exists (id {id}, cosine {} < {}). Use \
                     --replaces {id} to revise it, or --force to add anyway.",
                    render::format_real(dist),
                    render::format_real(DUP_DIST)
                )));
            }
            Some(Duplicate::Identical { id }) => {
                return Err(Error::refused(format!(
                    "an identical instruction already exists (id {id}; same text, ignoring case, \
                     whitespace and keywords). Use --replaces {id} to revise it, or --force to \
                     add anyway."
                )));
            }
            None => {}
        }
    }

    let id = store.insert_memory(&NewMemory {
        table: Table::Instructions,
        project: validate::blank_to_none(args.project.as_deref()),
        topic: Some(v.topic.clone()),
        category: None,
        event_type: None,
        importance: None,
        source: validate::blank_to_none(args.source.as_deref()),
        model: validate::blank_to_none(args.model.as_deref()),
        embed_model: vector.as_ref().map(|_| cfg.embed_model.clone()),
        memory_text: v.memory_text,
        file_reference: validate::blank_to_none(args.file_reference.as_deref()),
        created_at: None,
        embedding: vector,
        supersedes: Vec::new(),
        retires: args.replaces,
        coworkers: Vec::new(),
    })?;

    let mut line = format!("stored instruction {id} ({})", v.topic);
    if let Some(old) = args.replaces {
        line.push_str(&format!(", replacing instruction {old} (now retired)"));
    }
    Ok(remember::stored(cfg, id, line, missing))
}

/// Retire one current instruction, with no replacement. The only way an
/// instruction leaves the current set; the row stays.
pub fn retire(cfg: &Config, id: i64) -> Result<String> {
    let store = open(cfg)?;
    let state = store.instruction_state(id)?.ok_or_else(|| {
        Error::refused(format!(
            "--retire {id}: no instruction with id {id} — nothing changed."
        ))
    })?;
    if state.retired || !store.retire_instruction(id)? {
        return Err(Error::refused(format!(
            "--retire {id}: instruction {id} ({}) is already retired — nothing changed.",
            state.topic
        )));
    }
    Ok(format!("retired instruction {id} ({})", state.topic))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::Unavailable;
    use crate::embed::fake::Fake;
    use crate::error::{EXIT_REFUSED, Error};
    use crate::store::turso::tests::{
        angle, put, put_vector, retire_row, rows, scratch, set_vector,
    };
    use crate::store::{Cell, Table};

    fn args(topic: &str, text: &str) -> Args {
        Args {
            topic: Some(topic.to_string()),
            text: text.to_string(),
            ..Args::default()
        }
    }

    fn down() -> Fake {
        Fake::down(Unavailable::Unreachable {
            url: "http://127.0.0.1:9".into(),
        })
    }

    fn refused<T: std::fmt::Debug>(r: Result<T>) -> String {
        let e = r.expect_err("must be refused");
        assert_eq!(e.exit_code(), EXIT_REFUSED, "{e}");
        let msg = e.to_string();
        assert!(msg.starts_with("refused: "), "{msg}");
        msg
    }

    /// Every column of every instruction: what "nothing was stored" means.
    fn dump(cfg: &Config) -> Vec<Vec<Cell>> {
        rows(
            cfg,
            "SELECT id, created_at, updated_at, project, topic, source, model, embed_model, \
             memory_text, file_reference, hex(embedding), retired_at FROM instructions ORDER BY id;",
        )
    }

    /// An embedder that always answers with one fixed vector.
    struct Fixed(Vec<f32>);

    impl Embedder for Fixed {
        fn probe(&self) -> std::result::Result<(), Unavailable> {
            Ok(())
        }
        fn embed(&self, _text: &str) -> Result<Vec<f32>> {
            Ok(self.0.clone())
        }
    }

    /// An embedder that does something to the database while it embeds, as
    /// another process would between the checks and the insert.
    struct Meddling<F: Fn()>(F);

    impl<F: Fn()> Embedder for Meddling<F> {
        fn probe(&self) -> std::result::Result<(), Unavailable> {
            Ok(())
        }
        fn embed(&self, text: &str) -> Result<Vec<f32>> {
            (self.0)();
            Ok(Fake::vector(text))
        }
    }

    // ---- refusals: exit 2, `refused:`, nothing stored ---------------------

    #[test]
    fn an_empty_text_is_refused() {
        let cfg = scratch("instruct-empty");
        let msg = refused(write(&cfg, &args("deploy", "   "), &Fake::up()));
        assert!(msg.contains("--text is empty"), "{msg}");
        let mut with_keywords = args("deploy", "");
        with_keywords.keywords = Some("deploy, checklist".into());
        refused(write(&cfg, &with_keywords, &Fake::up()));
        assert!(dump(&cfg).is_empty());
    }

    /// An instruction is fetched by its topic, so one without a topic could
    /// only ever be found by search.
    #[test]
    fn a_missing_or_blank_topic_is_refused() {
        let cfg = scratch("instruct-no-topic");
        let msg = refused(write(&cfg, &args("  ", "run the checklist"), &Fake::up()));
        assert!(msg.contains("--topic"), "{msg}");
        let mut none = args("x", "run the checklist");
        none.topic = None;
        assert!(refused(write(&cfg, &none, &Fake::up())).contains("--topic"));
        assert!(dump(&cfg).is_empty());
    }

    /// The keywords go into the same field and count against the cap, and the
    /// refusal says how much each part took.
    #[test]
    fn an_over_cap_text_is_refused_with_the_keyword_breakdown() {
        let cfg = scratch("instruct-cap");
        let mut a = args("deploy", &"x".repeat(1990));
        a.keywords = Some("deploy, checklist".into());
        let msg = refused(write(&cfg, &a, &Fake::up()));
        assert!(msg.contains("hard cap 2000"), "{msg}");
        assert!(msg.contains("1990 of them your --text"), "{msg}");
        assert!(msg.contains("'--keywords' line"), "{msg}");
        // Characters, not bytes.
        write(&cfg, &args("umlauts", &"ä".repeat(2000)), &down()).expect("2000 characters fit");
        assert_eq!(dump(&cfg).len(), 1);
    }

    #[test]
    fn metadata_over_128_characters_is_refused_naming_the_flag() {
        let cfg = scratch("instruct-meta");
        let long = "m".repeat(129);
        for flag in ["--topic", "--source", "--model", "--project"] {
            let mut a = args("deploy", "run the checklist");
            match flag {
                "--topic" => a.topic = Some(long.clone()),
                "--source" => a.source = Some(long.clone()),
                "--model" => a.model = Some(long.clone()),
                _ => a.project = Some(long.clone()),
            }
            let msg = refused(write(&cfg, &a, &Fake::up()));
            assert!(
                msg.contains(&format!("{flag} is 129 chars; hard cap 128")),
                "{msg}"
            );
        }
        assert!(dump(&cfg).is_empty());
    }

    /// One model per database ACROSS tables: another model in any of the three
    /// refuses -- an empty instructions table must not let a second model in.
    #[test]
    fn another_embedding_model_in_any_table_is_refused() {
        for table in Table::EMBEDDED {
            let cfg = scratch(&format!("instruct-model-{}", table.as_str()));
            let id = put(&cfg, table, "embedded elsewhere", true);
            set_vector(&cfg, table, id, &Fake::vector("x"), "nomic-embed-text");
            let before = dump(&cfg);
            let fake = Fake::up();
            let msg = refused(write(&cfg, &args("deploy", "run the checklist"), &fake));
            assert!(msg.contains(table.name()), "{table:?}: {msg}");
            assert!(msg.contains("different model"), "{msg}");
            assert_eq!(dump(&cfg), before, "{table:?}");
            assert_eq!(fake.calls.get(), 1, "probed, never embedded");
        }
    }

    #[test]
    fn a_near_duplicate_by_vector_is_refused_and_force_stores_it() {
        let cfg = scratch("instruct-near");
        let first = put_vector(
            &cfg,
            Table::Instructions,
            "write release notes as bullets",
            &angle(0, 0.0),
        );
        // 1 - cos 10° = 0.0152: a different text, but the same directive.
        let close = Fixed(angle(0, 10.0));
        let mut a = args("release-notes", "release notes go in bullet points");
        let msg = refused(write(&cfg, &a, &close));
        assert!(msg.contains("near-duplicate"), "{msg}");
        assert!(msg.contains(&format!("id {first}")), "{msg}");
        assert!(msg.contains(&format!("--replaces {first}")), "{msg}");
        assert_eq!(dump(&cfg).len(), 1);

        a.force = true;
        write(&cfg, &a, &close).expect("--force stores it anyway");
        // 1 - cos 40° = 0.234: far enough to be another instruction.
        write(
            &cfg,
            &args("deploy", "run the checklist"),
            &Fixed(angle(0, 40.0)),
        )
        .expect("not a duplicate");
        assert_eq!(dump(&cfg).len(), 3);
    }

    /// Without a vector on either side, the same normalised text is the guard.
    #[test]
    fn an_identical_text_without_a_vector_is_refused_and_force_stores_it() {
        let cfg = scratch("instruct-identical");
        let mut first = args("deploy", "Run the checklist before a deploy.");
        first.keywords = Some("deploy".into());
        let first = write(&cfg, &first, &down()).expect("stored");
        let mut again = args("deploy", "run the   CHECKLIST before\na deploy.");
        let msg = refused(write(&cfg, &again, &down()));
        assert!(msg.contains("identical"), "{msg}");
        assert!(msg.contains(&format!("--replaces {}", first.id)), "{msg}");
        // An embedded write cannot see an unembedded twin by vector.
        refused(write(&cfg, &again, &Fake::up()));
        assert_eq!(dump(&cfg).len(), 1);
        again.force = true;
        write(&cfg, &again, &down()).expect("--force stores it anyway");
        assert_eq!(dump(&cfg).len(), 2);
    }

    #[test]
    fn replaces_an_unknown_or_retired_instruction_is_refused() {
        let cfg = scratch("instruct-replaces-bad");
        let gone = write(&cfg, &args("deploy", "old checklist"), &down())
            .unwrap()
            .id;
        retire_row(&cfg, Table::Instructions, gone);
        let before = dump(&cfg);
        for (target, want) in [(99, "no instruction with id 99"), (gone, "already retired")] {
            let mut a = args("deploy", "new checklist");
            a.replaces = Some(target);
            let msg = refused(write(&cfg, &a, &Fake::up()));
            assert!(msg.contains(want), "{msg}");
            assert!(msg.contains("nothing was stored"), "{msg}");
            assert_eq!(dump(&cfg), before);
        }
    }

    #[test]
    fn retire_of_an_unknown_or_retired_instruction_is_refused() {
        let cfg = scratch("instruct-retire-bad");
        let id = write(&cfg, &args("deploy", "a checklist"), &down())
            .unwrap()
            .id;
        assert!(refused(retire(&cfg, 99)).contains("no instruction with id 99"));
        retire(&cfg, id).expect("retired");
        let before = dump(&cfg);
        let msg = refused(retire(&cfg, id));
        assert!(msg.contains("already retired"), "{msg}");
        assert!(msg.contains("nothing changed"), "{msg}");
        assert_eq!(dump(&cfg), before);
    }

    /// A database from before the table: the writer says what to run.
    #[test]
    fn a_database_without_the_instructions_table_is_refused() {
        let cfg = scratch("instruct-no-table");
        crate::store::turso::dump::tests::batch(
            &cfg,
            "DROP INDEX IF EXISTS idx_instr_retired; DROP TABLE IF EXISTS instructions;",
        );
        let msg = refused(write(
            &cfg,
            &args("deploy", "run the checklist"),
            &Fake::up(),
        ));
        assert!(msg.contains("run update"), "{msg}");
        assert!(refused(retire(&cfg, 1)).contains("run update"));
    }

    // ---- what is stored ----------------------------------------------------

    #[test]
    fn an_instruction_is_stored_with_its_metadata_its_keywords_and_its_vector() {
        let cfg = scratch("instruct-stored");
        let a = Args {
            topic: Some(" release-notes ".into()),
            text: "Bullet points, newest first.".into(),
            keywords: Some("changelog, release".into()),
            source: Some("user-stated".into()),
            model: Some("claude-opus-5-5".into()),
            project: Some("869e7xzp6".into()),
            file_reference: Some("docs/RELEASING.md".into()),
            ..Args::default()
        };
        let s = write(&cfg, &a, &Fake::up()).expect("stored");
        assert_eq!(
            s.line,
            format!("stored instruction {} (release-notes)", s.id)
        );
        assert!(s.warning.is_none());
        let text = "Bullet points, newest first.\n\nKeywords: changelog, release";
        assert_eq!(
            rows(
                &cfg,
                &format!(
                    "SELECT topic, project, source, model, file_reference, embed_model, \
                     memory_text, retired_at FROM instructions WHERE id = {};",
                    s.id
                )
            ),
            vec![vec![
                Cell::Text("release-notes".into()),
                Cell::Text("869e7xzp6".into()),
                Cell::Text("user-stated".into()),
                Cell::Text("claude-opus-5-5".into()),
                Cell::Text("docs/RELEASING.md".into()),
                Cell::Text("bge-m3".into()),
                Cell::Text(text.into()),
                Cell::Null,
            ]]
        );
        // The vector is of the stored text, keywords included.
        assert!(
            crate::store::turso::tests::distance(
                &cfg,
                Table::Instructions,
                s.id,
                &Fake::vector(text)
            ) < 1e-6
        );
    }

    /// No embedding available is no refusal: the row goes in without one, the
    /// output says so, and `embed --missing` fills it in later.
    #[test]
    fn without_ollama_it_is_stored_without_an_embedding_and_embed_missing_fills_it() {
        let cfg = scratch("instruct-degraded");
        let s = write(&cfg, &args("deploy", "run the checklist"), &down()).expect("stored");
        assert_eq!(
            s.line,
            format!(
                "stored instruction {} (deploy) WITHOUT embedding — Ollama unreachable at \
                 http://127.0.0.1:9; run embed --missing once Ollama is available",
                s.id
            )
        );
        let warning = s
            .warning
            .expect("EMBEDDINGS=on must flag the missing vector");
        assert!(warning.starts_with("warning: EMBEDDINGS=on"), "{warning}");
        let vector = |cfg: &Config| {
            rows(
                cfg,
                &format!(
                    "SELECT embedding IS NULL, embed_model FROM instructions WHERE id = {};",
                    s.id
                ),
            )
        };
        assert_eq!(vector(&cfg), vec![vec![Cell::Int(1), Cell::Null]]);

        let report = crate::embed::missing::fill(
            &cfg,
            &crate::embed::missing::Args {
                tables: Table::EMBEDDED.to_vec(),
                limit: None,
                batch: 10,
            },
            &Fake::up(),
        )
        .unwrap();
        assert!(report.failure.is_none());
        assert_eq!(
            vector(&cfg),
            vec![vec![Cell::Int(0), Cell::Text("bge-m3".into())]]
        );
    }

    #[test]
    fn embeddings_off_never_calls_the_embedder() {
        let mut cfg = scratch("instruct-off");
        cfg.embeddings = false;
        let fake = Fake::up();
        let s = write(&cfg, &args("deploy", "run the checklist"), &fake).expect("stored");
        assert_eq!(
            fake.calls.get(),
            0,
            "EMBEDDINGS=off must not contact Ollama"
        );
        assert!(
            s.line.contains("WITHOUT embedding — EMBEDDINGS=off"),
            "{}",
            s.line
        );
        assert!(s.warning.is_none());
    }

    /// No limit per topic: several current instructions may share one.
    #[test]
    fn two_distinct_instructions_with_one_topic_are_both_stored() {
        let cfg = scratch("instruct-same-topic");
        let a = write(&cfg, &args("release-notes", "bullet points"), &Fake::up()).unwrap();
        let b = write(
            &cfg,
            &args("release-notes", "link every issue"),
            &Fake::up(),
        )
        .unwrap();
        assert_ne!(a.id, b.id);
        assert_eq!(
            rows(
                &cfg,
                "SELECT count(*) FROM instructions WHERE topic = 'release-notes' \
                 AND retired_at IS NULL;"
            ),
            vec![vec![Cell::Int(2)]]
        );
    }

    /// A revision retires the row it replaces, and the near-duplicate check
    /// skips that row only: rewording it is the point, duplicating another
    /// current instruction is still refused.
    #[test]
    fn replaces_retires_the_old_row_and_the_twin_check_skips_it_only() {
        let cfg = scratch("instruct-replaces");
        let old = write(&cfg, &args("release-notes", "bullet points"), &Fake::up())
            .unwrap()
            .id;
        let other = write(&cfg, &args("deploy", "run the checklist"), &Fake::up())
            .unwrap()
            .id;

        let mut a = args("release-notes", "bullet points");
        a.replaces = Some(old);
        let s = write(&cfg, &a, &Fake::up()).expect("a revision, not a duplicate");
        assert_eq!(
            s.line,
            format!(
                "stored instruction {} (release-notes), replacing instruction {old} (now retired)",
                s.id
            )
        );
        assert_eq!(
            rows(
                &cfg,
                "SELECT id, retired_at IS NOT NULL FROM instructions ORDER BY id;"
            ),
            vec![
                vec![Cell::Int(old), Cell::Int(1)],
                vec![Cell::Int(other), Cell::Int(0)],
                vec![Cell::Int(s.id), Cell::Int(0)],
            ]
        );

        let mut b = args("deploy", "run the checklist");
        b.replaces = Some(s.id);
        let msg = refused(write(&cfg, &b, &Fake::up()));
        assert!(msg.contains(&format!("id {other}")), "{msg}");
        assert_eq!(dump(&cfg).len(), 3);
    }

    /// The row to replace is retired by someone else after the checks: the
    /// insert that already ran is rolled back with it, so neither the new row
    /// nor a retirement is left.
    #[test]
    fn replaces_is_atomic_when_the_old_row_goes_away_after_the_checks() {
        let cfg = scratch("instruct-replaces-race");
        let old = write(&cfg, &args("deploy", "old checklist"), &down())
            .unwrap()
            .id;
        let meddling = Meddling(|| retire_row(&cfg, Table::Instructions, old));
        let before = rows(&cfg, "SELECT count(*) FROM instructions;");
        let mut a = args("deploy", "new checklist");
        a.replaces = Some(old);
        let msg = refused(write(&cfg, &a, &meddling));
        assert!(msg.contains("nothing was stored"), "{msg}");
        assert_eq!(rows(&cfg, "SELECT count(*) FROM instructions;"), before);
    }

    #[test]
    fn retire_soft_deletes_one_instruction_and_deletes_nothing() {
        let cfg = scratch("instruct-retire");
        let id = write(&cfg, &args("deploy", "run the checklist"), &down())
            .unwrap()
            .id;
        write(&cfg, &args("deploy", "and tag the release"), &down()).unwrap();
        assert_eq!(
            retire(&cfg, id).unwrap(),
            format!("retired instruction {id} (deploy)")
        );
        assert_eq!(
            rows(
                &cfg,
                "SELECT id, retired_at IS NOT NULL FROM instructions ORDER BY id;"
            ),
            vec![
                vec![Cell::Int(id), Cell::Int(1)],
                vec![Cell::Int(id + 1), Cell::Int(0)],
            ]
        );
    }

    /// A retired instruction is history, not a duplicate to guard against.
    #[test]
    fn a_retired_instruction_does_not_block_its_own_wording() {
        let cfg = scratch("instruct-after-retire");
        let id = write(&cfg, &args("deploy", "run the checklist"), &down())
            .unwrap()
            .id;
        retire(&cfg, id).unwrap();
        let again = write(&cfg, &args("deploy", "run the checklist"), &down()).expect("stored");
        assert_ne!(again.id, id);
        assert_eq!(
            rows(
                &cfg,
                "SELECT id, retired_at IS NULL FROM instructions ORDER BY id;"
            ),
            vec![
                vec![Cell::Int(id), Cell::Int(0)],
                vec![Cell::Int(again.id), Cell::Int(1)],
            ]
        );
    }

    /// The embed step is the shared path's: an embedder that refuses (a model
    /// of the wrong dimension) stops the write rather than degrading it.
    #[test]
    fn a_refusal_from_the_embedder_stops_the_write() {
        let cfg = scratch("instruct-dim");
        struct Refusing;
        impl Embedder for Refusing {
            fn probe(&self) -> std::result::Result<(), Unavailable> {
                Ok(())
            }
            fn embed(&self, _: &str) -> Result<Vec<f32>> {
                Err(Error::refused("embedding dim 768 != expected 1024"))
            }
        }
        refused(write(&cfg, &args("deploy", "run the checklist"), &Refusing));
        assert!(dump(&cfg).is_empty());
    }
}
