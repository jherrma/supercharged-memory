//! `instructions`: the directives the user wants followed on demand -- "how I
//! want release notes written", "the checklist before a deploy".
//!
//! They are not memories: an instruction is a directive to the agent, not
//! knowledge about an event or a behaviour. Two properties follow. Nothing here
//! is loaded at session start -- the agent asks when the user says "use
//! instruction X". And nothing in sleep reads the table: no consolidation, no
//! clustering, no purge. An instruction is revised or retired only by
//! `instruct` (`write`), and a retired one stays, soft-deleted.
//!
//! The reader is pure, like `recall`, and ranks through recall's own search.

pub mod write;

use crate::config::Config;
use crate::embed::{self, Embedder};
use crate::error::{Error, Result};
use crate::recall::{self, Within};
use crate::render;
use crate::store::{InstructionKey, Store, Table};

/// Hits shown when `--k` is not given, as for `recall`.
pub const DEFAULT_K: u32 = 5;

/// One lookup. The CLI admits exactly one per run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// Hybrid search over the current instructions.
    Search {
        query: String,
        k: u32,
        project: Option<String>,
    },
    /// The current instructions with this id or topic, full text.
    Get(String),
    /// Id, topic and date of every current instruction; never the text.
    List,
}

pub fn run(cfg: &Config, lookup: &Lookup) -> Result<()> {
    print!("{}", read(cfg, &embed::Ollama::new(cfg), lookup)?);
    Ok(())
}

/// What a lookup prints, in `tursodb -m line` format under a header line.
pub fn read(cfg: &Config, embedder: &dyn Embedder, lookup: &Lookup) -> Result<String> {
    // Refused before anything is opened: an argument error should not depend
    // on whether the database happens to be reachable.
    match lookup {
        Lookup::Search { query, .. } if query.trim().is_empty() => {
            return Err(Error::refused(
                "the query is empty — say what to search for, or use --get <id|topic> / --list.",
            ));
        }
        Lookup::Get(key) if key.trim().is_empty() => {
            return Err(Error::refused(
                "--get is empty — name an instruction's id or topic, or use --list.",
            ));
        }
        _ => {}
    }
    let store = write::open(cfg)?;
    match lookup {
        Lookup::Search { query, k, project } => search(&store, cfg, embedder, query, *k, project),
        Lookup::Get(key) => get(&store, key.trim()),
        Lookup::List => list(&store),
    }
}

/// Recall's ranking over the current instructions. Being recall's, a word in
/// every instruction weighs nothing by keyword -- on a table of a few rows a
/// real case -- so a search without a hit says so and points at the list
/// rather than ending at a bare header.
fn search(
    store: &impl Store,
    cfg: &Config,
    embedder: &dyn Embedder,
    query: &str,
    k: u32,
    project: &Option<String>,
) -> Result<String> {
    let found = recall::search_in(
        cfg,
        store,
        embedder,
        &[Table::Instructions],
        &Within {
            project: project.clone(),
            coworker: None,
            k,
        },
        query,
    )?;
    let mut out = found.text;
    if found.hits == 0 {
        let current = store.list_instructions()?.rows.len();
        out.push_str(&format!(
            "no instruction matched by keyword — instructions --list shows all {current} current \
             (instructions --get <id|topic> reads one)\n"
        ));
    }
    Ok(out)
}

/// "Use instruction X": every current row with that id or topic, newest
/// first. A key that is a number is an id first; if no current row has that
/// id, it is read as a topic, so a topic made of digits still works.
fn get(store: &impl Store, key: &str) -> Result<String> {
    let mut rs = match key.parse::<i64>() {
        Ok(id) => store.get_instructions(InstructionKey::Id(id))?,
        Err(_) => store.get_instructions(InstructionKey::Topic(key))?,
    };
    if rs.rows.is_empty() && key.parse::<i64>().is_ok() {
        rs = store.get_instructions(InstructionKey::Topic(key))?;
    }
    if rs.rows.is_empty() {
        // Said, with the next step: an empty answer would read as "there is
        // no such instruction" when a search might still find it.
        return Ok(format!(
            "===== instruction '{key}' (none current) =====\n\
             no current instruction has the id or topic '{key}' — search instead: \
             instructions \"{key}\"\n"
        ));
    }
    Ok(format!(
        "===== instruction '{key}' ({} current, newest first) =====\n{}\n",
        rs.rows.len(),
        render::line(&rs)
    ))
}

/// Every current instruction by id, topic and date: browsing costs little
/// context because the text is never shown.
fn list(store: &impl Store) -> Result<String> {
    let rs = store.list_instructions()?;
    let mut out = format!(
        "===== instructions ({} current; full text: instructions --get <id|topic>) =====\n",
        rs.rows.len()
    );
    if rs.rows.is_empty() {
        out.push_str(
            "no current instruction — store one when the user asks: instruct --topic <name> \
             --text <directive>\n",
        );
    } else {
        out.push_str(&render::line(&rs));
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::Unavailable;
    use crate::embed::fake::Fake;
    use crate::error::EXIT_REFUSED;
    use crate::recall;
    use crate::store::Table;
    use crate::store::turso::TursoStore;
    use crate::store::turso::tests::{
        angle, put, put_instruction, put_vector, retire_row, rows, scratch,
    };

    fn down() -> Fake {
        Fake::down(Unavailable::Unreachable {
            url: "http://127.0.0.1:9".into(),
        })
    }

    fn get(cfg: &Config, key: &str) -> String {
        read(cfg, &Fake::up(), &Lookup::Get(key.into())).unwrap()
    }

    fn search(cfg: &Config, embedder: &dyn Embedder, query: &str) -> String {
        read(
            cfg,
            embedder,
            &Lookup::Search {
                query: query.into(),
                k: DEFAULT_K,
                project: None,
            },
        )
        .unwrap()
    }

    fn refused(r: Result<String>) -> String {
        let e = r.expect_err("must be refused");
        assert_eq!(e.exit_code(), EXIT_REFUSED, "{e}");
        let msg = e.to_string();
        assert!(msg.starts_with("refused: "), "{msg}");
        msg
    }

    /// The `memory_text` values of an output, in order.
    fn texts(out: &str) -> Vec<&str> {
        out.lines()
            .filter_map(|l| l.trim_start().strip_prefix("memory_text = "))
            .collect()
    }

    /// Two instructions under one topic, a retired third, one under another.
    fn release_notes(name: &str) -> (Config, i64, i64, i64, i64) {
        let cfg = scratch(name);
        let old = put_instruction(
            &cfg,
            "release-notes",
            "bullet points",
            "2026-01-01 00:00:00",
            true,
        );
        let new = put_instruction(
            &cfg,
            "release-notes",
            "link every issue",
            "2026-02-01 00:00:00",
            true,
        );
        let gone = put_instruction(
            &cfg,
            "release-notes",
            "the retired wording",
            "2026-03-01 00:00:00",
            true,
        );
        retire_row(&cfg, Table::Instructions, gone);
        let other = put_instruction(
            &cfg,
            "deploy",
            "run the checklist",
            "2026-01-15 00:00:00",
            true,
        );
        (cfg, old, new, gone, other)
    }

    // ---- --get -------------------------------------------------------------

    /// "Use instruction release-notes": every current row of that topic, full
    /// text, newest first -- whatever the case the user said it in.
    #[test]
    fn get_by_topic_returns_every_current_row_newest_first() {
        let (cfg, old, new, _, _) = release_notes("instr-get-topic");
        let out = get(&cfg, "Release-Notes");
        assert!(
            out.starts_with("===== instruction 'Release-Notes' (2 current, newest first) =====\n"),
            "{out}"
        );
        assert_eq!(texts(&out), vec!["link every issue", "bullet points"]);
        assert!(out.contains(&format!("id = {new}\n")), "{out}");
        assert!(out.contains(&format!("id = {old}\n")), "{out}");
        assert!(!out.contains("retired wording"), "{out}");
        assert!(out.contains("topic = release-notes"), "{out}");
    }

    #[test]
    fn get_by_id_returns_that_row_and_never_a_retired_one() {
        let (cfg, _, _, gone, other) = release_notes("instr-get-id");
        assert_eq!(
            texts(&get(&cfg, &other.to_string())),
            vec!["run the checklist"]
        );
        let out = get(&cfg, &gone.to_string());
        assert!(texts(&out).is_empty(), "{out}");
        assert!(out.contains("(none current)"), "{out}");
    }

    /// A topic made of digits is still a topic when no row has that id.
    #[test]
    fn a_numeric_key_that_names_no_id_is_read_as_a_topic() {
        let cfg = scratch("instr-get-numeric");
        put_instruction(
            &cfg,
            "2026",
            "the yearly review",
            "2026-01-01 00:00:00",
            false,
        );
        assert_eq!(texts(&get(&cfg, "2026")), vec!["the yearly review"]);
    }

    /// Nothing found is said, with the next step -- never an empty answer.
    #[test]
    fn get_without_a_match_says_so_and_how_to_search() {
        let (cfg, ..) = release_notes("instr-get-none");
        assert_eq!(
            get(&cfg, "changelog"),
            "===== instruction 'changelog' (none current) =====\n\
             no current instruction has the id or topic 'changelog' — search instead: \
             instructions \"changelog\"\n"
        );
    }

    // ---- --list ------------------------------------------------------------

    /// Browsing costs little context: id, topic and date, never the text.
    #[test]
    fn list_shows_id_topic_and_date_of_current_rows_never_the_text() {
        let (cfg, old, new, gone, other) = release_notes("instr-list");
        let out = read(&cfg, &Fake::up(), &Lookup::List).unwrap();
        assert!(
            out.starts_with(
                "===== instructions (3 current; full text: instructions --get <id|topic>) =====\n"
            ),
            "{out}"
        );
        let listed: Vec<&str> = out
            .lines()
            .filter_map(|l| l.trim_start().strip_prefix("id = "))
            .collect();
        assert_eq!(
            listed,
            vec![other.to_string(), new.to_string(), old.to_string()]
        );
        assert!(!out.contains(&format!("id = {gone}\n")), "{out}");
        for text in ["bullet points", "link every issue", "run the checklist"] {
            assert!(!out.contains(text), "{text} in {out}");
        }
        assert!(out.contains("created_at = 2026-02-01 00:00:00"), "{out}");
    }

    #[test]
    fn an_empty_list_says_so() {
        let cfg = scratch("instr-list-empty");
        let out = read(&cfg, &Fake::up(), &Lookup::List).unwrap();
        assert!(out.contains("(0 current"), "{out}");
        assert!(out.contains("no current instruction"), "{out}");
    }

    // ---- search ------------------------------------------------------------

    #[test]
    fn search_never_returns_a_retired_instruction_and_names_each_hit_by_id() {
        let (cfg, _, new, _, _) = release_notes("instr-search");
        for embedder in [&Fake::up() as &dyn Embedder, &down()] {
            let out = search(&cfg, embedder, "the retired wording issue");
            assert!(!out.contains("the retired wording"), "{out}");
            assert!(out.contains(&format!("id = {new}\n")), "{out}");
            assert!(out.contains("link every issue"), "{out}");
        }
    }

    #[test]
    fn search_without_embeddings_is_keyword_only() {
        let (cfg, ..) = release_notes("instr-search-kw");
        let out = search(&cfg, &down(), "checklist");
        assert!(
            out.starts_with("===== instructions (Ollama down — keyword-only, top 5) =====\n"),
            "{out}"
        );
        assert_eq!(texts(&out), vec!["run the checklist"]);
    }

    /// A row written while Ollama was down is found by keyword in vector mode
    /// too, in a section of its own.
    #[test]
    fn an_unembedded_instruction_with_a_keyword_hit_is_found_in_vector_mode() {
        let (cfg, ..) = release_notes("instr-search-blind");
        put_instruction(
            &cfg,
            "deploy",
            "tag the release first",
            "2026-04-01 00:00:00",
            false,
        );
        let out = search(&cfg, &Fake::up(), "tag");
        let section = out
            .split("===== instructions: 1 without embedding (keyword-only) =====\n")
            .nth(1)
            .unwrap_or_else(|| panic!("no unembedded section:\n{out}"));
        assert!(section.contains("tag the release first"), "{out}");
    }

    /// The ranking is recall's, so a word in every instruction weighs nothing
    /// by keyword -- on a small table that is a real case. A search without a
    /// hit says so and points at the list, rather than printing a bare header.
    #[test]
    fn a_search_without_a_hit_says_so_and_points_at_the_list() {
        let cfg = scratch("instr-search-none");
        put_instruction(
            &cfg,
            "deploy",
            "run the checklist",
            "2026-01-01 00:00:00",
            false,
        );
        let out = search(&cfg, &down(), "checklist");
        assert_eq!(
            out,
            "===== instructions (Ollama down — keyword-only, top 5) =====\n\n\
             no instruction matched by keyword — instructions --list shows all 1 current \
             (instructions --get <id|topic> reads one)\n"
        );
        let (cfg, ..) = release_notes("instr-search-some");
        let out = search(&cfg, &down(), "checklist");
        assert!(!out.contains("no instruction matched"), "{out}");
    }

    #[test]
    fn project_narrows_the_search() {
        let cfg = scratch("instr-search-project");
        let a = put_instruction(
            &cfg,
            "deploy",
            "deploy checklist one",
            "2026-01-01 00:00:00",
            true,
        );
        put_instruction(
            &cfg,
            "deploy",
            "deploy checklist two",
            "2026-01-01 00:00:00",
            true,
        );
        rows(
            &cfg,
            &format!("UPDATE instructions SET project = 'p1' WHERE id = {a};"),
        );
        let out = read(
            &cfg,
            &Fake::up(),
            &Lookup::Search {
                query: "deploy checklist".into(),
                k: 5,
                project: Some("p1".into()),
            },
        )
        .unwrap();
        assert_eq!(texts(&out), vec!["deploy checklist one"]);
    }

    /// The ranking is recall's: the same candidates in `semantic_memory` and in
    /// `instructions` come back in the same order, with the same scores.
    #[test]
    fn recall_and_instructions_rank_the_same_candidates_in_the_same_order() {
        let cfg = scratch("instr-rank-parity");
        let corpus = [
            "turso needs the multiprocess wal flag on every open",
            "ollama serves the bge-m3 embedding model",
            "zsh does not word-split an unquoted variable",
            "turso picks the iocp vfs on windows",
            "the wal grows until a checkpoint",
            "release notes list every merged pull request",
        ];
        for (i, text) in corpus.iter().enumerate() {
            put_vector(&cfg, Table::Semantic, text, &angle(i % 3, i as f64 * 7.0));
            put_vector(
                &cfg,
                Table::Instructions,
                text,
                &angle(i % 3, i as f64 * 7.0),
            );
        }
        let store = TursoStore::open(&cfg).unwrap();
        let query = "turso wal checkpoint";
        // Ranked by keyword alone or by a fixed query vector: both orders agree.
        for embedder in [&Fake::up() as &dyn Embedder, &down()] {
            let as_memories = recall::search(
                &cfg,
                &store,
                embedder,
                &recall::Args {
                    query: None,
                    table: recall::TableArg::Semantic,
                    project: None,
                    k: 6,
                    coworker: None,
                    baseline: false,
                    topics: None,
                    candidates: false,
                    count: false,
                },
                query,
            )
            .unwrap();
            let as_instructions = read(
                &cfg,
                embedder,
                &Lookup::Search {
                    query: query.into(),
                    k: 6,
                    project: None,
                },
            )
            .unwrap();
            assert!(texts(&as_memories).len() >= 3, "{as_memories}");
            assert_eq!(texts(&as_instructions), texts(&as_memories));
            let scores = |out: &str| -> Vec<String> {
                out.lines()
                    .filter(|l| {
                        let l = l.trim_start();
                        l.starts_with("dist = ")
                            || l.starts_with("kw = ")
                            || l.starts_with("score = ")
                    })
                    .map(str::to_string)
                    .collect()
            };
            assert_eq!(scores(&as_instructions), scores(&as_memories));
        }
    }

    // ---- refusals ----------------------------------------------------------

    /// An empty question is refused before anything is opened.
    #[test]
    fn an_empty_query_or_key_is_refused_before_the_database_is_opened() {
        let mut cfg = scratch("instr-empty");
        cfg.db_path = cfg.db_path.with_file_name("not-there.db");
        let blank = Lookup::Search {
            query: "  ".into(),
            k: 5,
            project: None,
        };
        assert!(refused(read(&cfg, &Fake::up(), &blank)).contains("empty"));
        assert!(refused(read(&cfg, &Fake::up(), &Lookup::Get(" ".into()))).contains("empty"));
        assert!(!cfg.db_path.exists());
    }

    #[test]
    fn a_database_without_the_instructions_table_is_refused() {
        let cfg = scratch("instr-no-table");
        crate::store::turso::dump::tests::batch(
            &cfg,
            "DROP INDEX IF EXISTS idx_instr_retired; DROP TABLE IF EXISTS instructions;",
        );
        for lookup in [
            Lookup::List,
            Lookup::Get("deploy".into()),
            Lookup::Search {
                query: "deploy".into(),
                k: 5,
                project: None,
            },
        ] {
            let msg = refused(read(&cfg, &Fake::up(), &lookup));
            assert!(msg.contains("run update"), "{lookup:?}: {msg}");
        }
    }

    // ---- never at session start, never in sleep ------------------------------

    /// What a session starts with and what sleep reads print byte for byte the
    /// same with and without instruction rows -- including ones that would
    /// cluster with the memories, quote a checkable artifact, or are retired.
    #[test]
    fn session_start_and_sleep_print_the_same_with_and_without_instructions() {
        let memories = |name: &str| {
            let cfg = scratch(name);
            put_vector(
                &cfg,
                Table::Semantic,
                "run scripts/recall.py --status first",
                &angle(0, 0.0),
            );
            put_vector(
                &cfg,
                Table::Semantic,
                "status before anything else",
                &angle(0, 5.0),
            );
            put_vector(&cfg, Table::Semantic, "unrelated", &angle(1, 0.0));
            put(&cfg, Table::Semantic, "stored while Ollama was down", false);
            put_vector(&cfg, Table::Episodic, "ran status", &angle(0, 0.0));
            put_vector(&cfg, Table::Episodic, "ran status again", &angle(0, 3.0));
            rows(
                &cfg,
                "UPDATE semantic_memory SET category = 'baseline', topic = 'start' WHERE id = 2;",
            );
            rows(
                &cfg,
                "INSERT INTO topic_keywords (topic, keywords, updated_at) \
                 VALUES ('status', 'READY, DEGRADED', '2026-01-01 00:00:00');",
            );
            cfg
        };
        let without = memories("instr-invisible-without");
        let with = memories("instr-invisible-with");
        put_vector(
            &with,
            Table::Instructions,
            "run scripts/recall.py --status first",
            &angle(0, 0.0),
        );
        put_vector(
            &with,
            Table::Instructions,
            "status, then baseline",
            &angle(0, 1.0),
        );
        let gone = put(
            &with,
            Table::Instructions,
            "a retired one, never embedded",
            false,
        );
        retire_row(&with, Table::Instructions, gone);
        put_instruction(
            &with,
            "start",
            "load the baseline",
            "2026-01-01 00:00:00",
            true,
        );

        let session = |cfg: &Config| {
            let store = TursoStore::open(cfg).unwrap();
            let mut out = crate::status::report(cfg, &Fake::up()).join("\n");
            out.push_str(&recall::baseline(&store).unwrap());
            out.push_str(&recall::topics(&store, "").unwrap());
            out
        };
        assert_eq!(session(&with), session(&without));
        assert!(
            session(&with).contains("unembedded: semantic 1"),
            "{}",
            session(&with)
        );

        use crate::sleep::{Mode, execute};
        for table in [Table::Semantic, Table::Episodic] {
            for mode in [
                Mode::Cluster {
                    table,
                    threshold: None,
                },
                Mode::ContradictionCandidates {
                    table,
                    threshold: None,
                },
                Mode::VerifyCandidates { table, limit: 0 },
                Mode::Staleness { table },
            ] {
                let report = |cfg: &Config| execute(cfg, &mode, &mut std::io::empty()).unwrap();
                assert_eq!(report(&with), report(&without), "{mode:?}");
            }
        }
    }
}
