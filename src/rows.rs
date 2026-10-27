//! `list` and `show`: the fixed-shape reads of the memory tables.
//!
//! With the `tursodb` CLI optional, every read a runbook step needs has to be a
//! command of this binary. The set is closed on purpose -- named filters, no
//! expression language -- and a read nobody can do here is a filter to add, not
//! a SQL passthrough. Both commands are pure readers: nothing is written,
//! nothing is created, and no embedder is asked for anything.
//!
//! `list` is skinny -- metadata, never the text unless a preview or the keyword
//! line is asked for. `show` is the full row. The embedding is never printed.

use crate::config::Config;
use crate::error::{Error, Result};
use crate::remember::validate;
use crate::render;
use crate::store::{self, Cell, Currency, ResultSet, RowFilter, Store, Table};

/// One `list` invocation.
#[derive(Debug, Clone, Default)]
pub struct ListArgs {
    pub table: Option<Table>,
    pub ids: Option<String>,
    pub current: bool,
    pub not_current: bool,
    pub unprocessed: bool,
    pub topic: Option<String>,
    pub category: Option<String>,
    pub source: Option<String>,
    pub project: Option<String>,
    pub since: Option<String>,
    pub preview: Option<usize>,
    pub keywords: bool,
}

/// What `show` found.
#[derive(Debug)]
pub struct Shown {
    pub out: String,
    /// Asked for, in the order asked, and not in the table.
    pub missing: Vec<i64>,
}

/// `list`, as printed.
pub fn run_list(cfg: &Config, args: &ListArgs) -> Result<()> {
    print!("{}", list(cfg, args)?);
    Ok(())
}

/// `show`, as printed: the rows that exist on stdout, then the ids that do not
/// as an error, so a worker cannot take a partial read for a complete one.
pub fn run_show(cfg: &Config, table: Table, ids: &str) -> Result<()> {
    let shown = show(cfg, table, ids)?;
    print!("{}", shown.out);
    if shown.missing.is_empty() {
        return Ok(());
    }
    let missing: Vec<String> = shown.missing.iter().map(i64::to_string).collect();
    Err(Error::failed(format!(
        "no {} row with id {} — the rows printed are only part of what was asked for.",
        table.as_str(),
        missing.join(", ")
    )))
}

/// The listing as a result set, before it is rendered.
pub(crate) fn query(cfg: &Config, args: &ListArgs) -> Result<ResultSet> {
    let table = args.table.expect("list always names a table");
    let (filter, want) = filter(table, args)?;
    let store = open(cfg, table)?;
    let mut rs = store.list_rows(table, &filter)?;
    if filter.with_text {
        derive_text_columns(&mut rs, &want);
    }
    Ok(rs)
}

/// What `list` prints.
pub fn list(cfg: &Config, args: &ListArgs) -> Result<String> {
    Ok(render::line(&query(cfg, args)?))
}

/// What `show` prints and which ids it did not find.
pub fn show(cfg: &Config, table: Table, ids: &str) -> Result<Shown> {
    let ids = parse_ids("show", ids)?;
    let store = open(cfg, table)?;
    let mut rs = store.show_rows(table, &ids)?;
    let mut by_id: Vec<(i64, Vec<Cell>)> = rs
        .rows
        .drain(..)
        .filter_map(|row| match row.first() {
            Some(Cell::Int(id)) => Some((*id, row)),
            _ => None,
        })
        .collect();
    let mut missing = Vec::new();
    for id in ids {
        match by_id.iter().position(|(found, _)| *found == id) {
            Some(at) => rs.rows.push(by_id.swap_remove(at).1),
            None => missing.push(id),
        }
    }
    Ok(Shown {
        out: render::line(&rs),
        missing,
    })
}

/// Which text-derived columns a listing adds.
struct TextColumns {
    preview: Option<usize>,
    keywords: bool,
}

/// The store filter for these arguments, or a refusal. Everything that can be
/// wrong with the arguments alone is decided here, before a database is opened.
fn filter(table: Table, args: &ListArgs) -> Result<(RowFilter, TextColumns)> {
    if args.current && args.not_current {
        return Err(Error::refused(
            "--current and --not-current contradict each other — pick one, or neither for every row.",
        ));
    }
    if args.unprocessed && table != Table::Episodic {
        return Err(Error::refused(format!(
            "--unprocessed is for the episodic table (only it has a sleep pass to wait for), \
             not {}.",
            table.as_str()
        )));
    }
    if let Some(category) = &args.category {
        if table != Table::Semantic {
            return Err(Error::refused(format!(
                "--category is for the semantic table (only it has categories), not {}.",
                table.as_str()
            )));
        }
        validate::one_of("--category", category, &validate::CATEGORIES)?;
    }
    let since = match &args.since {
        Some(raw) => Some(validate::normalise_created_at(raw).map_err(|e| match e {
            Error::Refused(why) => Error::refused(why.replace("--created-at", "--since")),
            other => other,
        })?),
        None => None,
    };
    let ids = match &args.ids {
        Some(raw) => Some(parse_ids("--ids", raw)?),
        None => None,
    };
    let want = TextColumns {
        preview: args.preview,
        keywords: args.keywords,
    };
    let currency = match (args.current, args.not_current) {
        (true, _) => Some(Currency::Current),
        (_, true) => Some(Currency::NotCurrent),
        _ => None,
    };
    Ok((
        RowFilter {
            ids,
            currency,
            unprocessed: args.unprocessed,
            topic: args.topic.clone(),
            category: args.category.clone(),
            source: args.source.clone(),
            project: args.project.clone(),
            since,
            with_text: want.preview.is_some() || want.keywords,
        },
        want,
    ))
}

/// Ids as given, in that order, each once; anything else is refused. `list`
/// sorts by date anyway and `show` prints in this order, so a worker's list
/// is answered row for row.
fn parse_ids(what: &str, csv: &str) -> Result<Vec<i64>> {
    let mut ids: Vec<i64> = Vec::new();
    for part in csv.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let id = part.parse::<i64>().map_err(|_| {
            Error::refused(format!(
                "{what}: not an id: {part:?} — ids are comma-separated integers."
            ))
        })?;
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    if ids.is_empty() {
        return Err(Error::refused(format!("{what}: no ids given.")));
    }
    Ok(ids)
}

/// A table the listing can read: the instructions table only once `update` has
/// created it.
fn open(cfg: &Config, table: Table) -> Result<impl Store> {
    let store = store::open(cfg)?;
    if table == Table::Instructions && !store.has_table(Table::Instructions)? {
        return Err(Error::refused(
            "this database has no instructions table yet — run update, which creates it.",
        ));
    }
    Ok(store)
}

/// Replace the trailing `memory_text` column with the preview and keywords
/// columns that were asked for.
fn derive_text_columns(rs: &mut ResultSet, want: &TextColumns) {
    rs.columns.pop();
    if want.preview.is_some() {
        rs.columns.push("preview".into());
    }
    if want.keywords {
        rs.columns.push("keywords".into());
    }
    for row in &mut rs.rows {
        let text = match row.pop() {
            Some(Cell::Text(t)) => t,
            _ => String::new(),
        };
        if let Some(n) = want.preview {
            row.push(Cell::Text(text.chars().take(n).collect()));
        }
        if want.keywords {
            row.push(match validate::keywords_of(&text) {
                Some(k) => Cell::Text(k.to_string()),
                None => Cell::Null,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Cell;
    use crate::store::turso::tests::{put, retire_row, rows as sql, scratch};

    const SKINNY_SEMANTIC: [&str; 9] = [
        "id",
        "created_at",
        "topic",
        "category",
        "source",
        "project",
        "file_reference",
        "superseded_by",
        "retired_at",
    ];
    const SKINNY_EPISODIC: [&str; 9] = [
        "id",
        "created_at",
        "topic",
        "event_type",
        "importance",
        "source",
        "project",
        "file_reference",
        "processed_at",
    ];
    const SKINNY_INSTRUCTIONS: [&str; 7] = [
        "id",
        "created_at",
        "topic",
        "source",
        "project",
        "file_reference",
        "retired_at",
    ];

    /// Five semantic rows with vectors, three episodic, two instructions.
    ///
    /// semantic 1 current, keyword line; 2 superseded by 3; 3 current, no
    /// keywords; 4 retired, multi-byte text; 5 a current pattern.
    /// episodic 1 and 2 share a timestamp and are unsifted; 3 is older and sifted.
    fn fixture(name: &str) -> Config {
        let cfg = scratch(name);
        let sem = [
            (
                "deploy needs a tag\n\nKeywords: tag, release",
                "2026-01-01 00:00:00",
                "deploy",
                "reference",
                "user-stated",
                "'P1'",
                "NULL",
            ),
            (
                "an old deploy fact",
                "2026-01-02 00:00:00",
                "deploy",
                "reference",
                "migration",
                "NULL",
                "'notes/a.md'",
            ),
            (
                "deploy now needs a signed tag",
                "2026-01-03 00:00:00",
                "deploy",
                "reference",
                "self-observed",
                "NULL",
                "NULL",
            ),
            (
                "zürich ünïcode fact, long enough to be cut: abcdefghij",
                "2026-01-04 00:00:00",
                "other",
                "project",
                "migration",
                "'P1'",
                "'notes/b.md'",
            ),
            (
                "a recurring bug\n\nKeywords: bug, pattern",
                "2026-01-05 00:00:00",
                "patterns",
                "pattern",
                "sleep",
                "NULL",
                "NULL",
            ),
        ];
        for (text, at, topic, category, source, project, file) in sem {
            let id = put(&cfg, Table::Semantic, text, true);
            sql(
                &cfg,
                &format!(
                    "UPDATE semantic_memory SET created_at='{at}', topic='{topic}', \
                     category='{category}', source='{source}', project={project}, \
                     file_reference={file} WHERE id={id};"
                ),
            );
        }
        sql(
            &cfg,
            "UPDATE semantic_memory SET superseded_by=3 WHERE id=2;",
        );
        retire_row(&cfg, Table::Semantic, 4);

        for (at, topic, processed) in [
            ("2026-01-03 00:00:00", "t1", "NULL"),
            ("2026-01-03 00:00:00", "t2", "NULL"),
            ("2026-01-02 00:00:00", "t3", "'2026-01-04 00:00:00'"),
        ] {
            let id = put(&cfg, Table::Episodic, "an event", true);
            sql(
                &cfg,
                &format!(
                    "UPDATE episodic_memory SET created_at='{at}', topic='{topic}', \
                     processed_at={processed}, source='user-stated' WHERE id={id};"
                ),
            );
        }

        let live = put(
            &cfg,
            Table::Instructions,
            "write release notes tersely",
            true,
        );
        sql(
            &cfg,
            &format!(
                "UPDATE instructions SET topic='release notes', created_at='2026-02-01 00:00:00' WHERE id={live};"
            ),
        );
        let dead = put(&cfg, Table::Instructions, "an old way", true);
        sql(
            &cfg,
            &format!(
                "UPDATE instructions SET topic='old way', created_at='2026-02-02 00:00:00' WHERE id={dead};"
            ),
        );
        retire_row(&cfg, Table::Instructions, dead);
        cfg
    }

    fn args(table: Table) -> ListArgs {
        ListArgs {
            table: Some(table),
            ..ListArgs::default()
        }
    }

    fn names(rs: &ResultSet) -> Vec<&str> {
        rs.columns.iter().map(String::as_str).collect()
    }

    fn col(rs: &ResultSet, name: &str) -> Vec<Cell> {
        let i = rs
            .columns
            .iter()
            .position(|c| c == name)
            .unwrap_or_else(|| panic!("no column {name} in {:?}", rs.columns));
        rs.rows.iter().map(|r| r[i].clone()).collect()
    }

    fn ids(rs: &ResultSet) -> Vec<i64> {
        col(rs, "id")
            .into_iter()
            .map(|c| match c {
                Cell::Int(n) => n,
                other => panic!("{other:?}"),
            })
            .collect()
    }

    fn text(s: &str) -> Cell {
        Cell::Text(s.to_string())
    }

    fn refusal(r: Result<ResultSet>) -> String {
        match r {
            Err(e @ Error::Refused(_)) => e.to_string(),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    // ---- the inventory: one test per read the runbooks make ------------------

    #[test]
    fn unprocessed_episodic_rows_come_with_the_sifting_columns_oldest_first() {
        let cfg = fixture("inv-unprocessed");
        let rs = query(
            &cfg,
            &ListArgs {
                unprocessed: true,
                ..args(Table::Episodic)
            },
        )
        .unwrap();
        assert_eq!(names(&rs), SKINNY_EPISODIC);
        assert_eq!(ids(&rs), vec![1, 2]);
        assert_eq!(col(&rs, "topic"), vec![text("t1"), text("t2")]);
        assert_eq!(col(&rs, "event_type"), vec![text("note"), text("note")]);
        assert_eq!(
            col(&rs, "importance"),
            vec![text("routine"), text("routine")]
        );
        assert_eq!(col(&rs, "processed_at"), vec![Cell::Null, Cell::Null]);
    }

    #[test]
    fn every_episodic_row_is_listed_by_created_at_then_id() {
        let cfg = fixture("order");
        let rs = query(&cfg, &args(Table::Episodic)).unwrap();
        assert_eq!(ids(&rs), vec![3, 1, 2]);
    }

    #[test]
    fn one_topics_current_rows() {
        let cfg = fixture("inv-topic");
        let rs = query(
            &cfg,
            &ListArgs {
                current: true,
                topic: Some("deploy".into()),
                ..args(Table::Semantic)
            },
        )
        .unwrap();
        assert_eq!(names(&rs), SKINNY_SEMANTIC);
        assert_eq!(ids(&rs), vec![1, 3]);
        assert_eq!(
            col(&rs, "source"),
            vec![text("user-stated"), text("self-observed")]
        );
        assert_eq!(col(&rs, "project"), vec![text("P1"), Cell::Null]);
    }

    #[test]
    fn the_topic_is_matched_whole_and_case_sensitively() {
        let cfg = fixture("topic-exact");
        for wrong in ["depl", "Deploy", "deploy "] {
            let rs = query(
                &cfg,
                &ListArgs {
                    topic: Some(wrong.into()),
                    ..args(Table::Semantic)
                },
            )
            .unwrap();
            assert!(rs.rows.is_empty(), "{wrong:?}");
        }
    }

    #[test]
    fn every_current_row_with_its_keyword_tail() {
        let cfg = fixture("inv-keywords");
        let rs = query(
            &cfg,
            &ListArgs {
                current: true,
                keywords: true,
                ..args(Table::Semantic)
            },
        )
        .unwrap();
        let mut want: Vec<&str> = SKINNY_SEMANTIC.to_vec();
        want.push("keywords");
        assert_eq!(names(&rs), want);
        assert_eq!(ids(&rs), vec![1, 3, 5]);
        assert_eq!(
            col(&rs, "keywords"),
            vec![text("tag, release"), Cell::Null, text("bug, pattern")]
        );
        assert!(!names(&rs).contains(&"memory_text"));
    }

    #[test]
    fn rows_that_are_not_current_with_an_eighty_character_preview() {
        let cfg = fixture("inv-not-current");
        let rs = query(
            &cfg,
            &ListArgs {
                not_current: true,
                preview: Some(80),
                ..args(Table::Semantic)
            },
        )
        .unwrap();
        let mut want: Vec<&str> = SKINNY_SEMANTIC.to_vec();
        want.push("preview");
        assert_eq!(names(&rs), want);
        assert_eq!(ids(&rs), vec![2, 4]);
        assert_eq!(col(&rs, "superseded_by"), vec![Cell::Int(3), Cell::Null]);
        assert_eq!(
            col(&rs, "retired_at"),
            vec![Cell::Null, text("2026-01-02 00:00:00")]
        );
        assert_eq!(
            col(&rs, "preview"),
            vec![
                text("an old deploy fact"),
                text("zürich ünïcode fact, long enough to be cut: abcdefghij")
            ]
        );
    }

    #[test]
    fn a_preview_cuts_at_characters_not_bytes() {
        let cfg = fixture("preview-chars");
        let rs = query(
            &cfg,
            &ListArgs {
                ids: Some("4".into()),
                preview: Some(8),
                ..args(Table::Semantic)
            },
        )
        .unwrap();
        assert_eq!(col(&rs, "preview"), vec![text("zürich ü")]);
    }

    #[test]
    fn a_preview_and_the_keywords_can_be_asked_for_together() {
        let cfg = fixture("preview-and-keywords");
        let rs = query(
            &cfg,
            &ListArgs {
                ids: Some("1".into()),
                preview: Some(6),
                keywords: true,
                ..args(Table::Semantic)
            },
        )
        .unwrap();
        assert_eq!(&names(&rs)[9..], ["preview", "keywords"]);
        assert_eq!(col(&rs, "preview"), vec![text("deploy")]);
        assert_eq!(col(&rs, "keywords"), vec![text("tag, release")]);
    }

    #[test]
    fn merge_inputs_by_id_come_oldest_first_whatever_the_order_given() {
        let cfg = fixture("inv-ids");
        let rs = query(
            &cfg,
            &ListArgs {
                ids: Some("5, 3,1".into()),
                ..args(Table::Semantic)
            },
        )
        .unwrap();
        assert_eq!(ids(&rs), vec![1, 3, 5]);
        assert_eq!(
            col(&rs, "created_at"),
            vec![
                text("2026-01-01 00:00:00"),
                text("2026-01-03 00:00:00"),
                text("2026-01-05 00:00:00")
            ]
        );
    }

    #[test]
    fn ids_that_do_not_exist_are_simply_absent() {
        let cfg = fixture("ids-absent");
        let rs = query(
            &cfg,
            &ListArgs {
                ids: Some("1,99".into()),
                ..args(Table::Semantic)
            },
        )
        .unwrap();
        assert_eq!(ids(&rs), vec![1]);
    }

    #[test]
    fn the_pattern_rows() {
        let cfg = fixture("inv-category");
        let rs = query(
            &cfg,
            &ListArgs {
                category: Some("pattern".into()),
                ..args(Table::Semantic)
            },
        )
        .unwrap();
        assert_eq!(ids(&rs), vec![5]);
        assert_eq!(col(&rs, "category"), vec![text("pattern")]);
    }

    #[test]
    fn rows_since_a_timestamp_include_that_second() {
        let cfg = fixture("inv-since");
        let at = |since: &str| {
            ids(&query(
                &cfg,
                &ListArgs {
                    current: true,
                    since: Some(since.into()),
                    keywords: true,
                    ..args(Table::Semantic)
                },
            )
            .unwrap())
        };
        assert_eq!(at("2026-01-03 00:00:00"), vec![3, 5]);
        assert_eq!(at("2026-01-03 00:00:01"), vec![5]);
        // A bare date is that day at midnight, as remember --created-at reads it.
        assert_eq!(at("2026-01-03"), vec![3, 5]);
        assert_eq!(at("2026-01-06"), Vec::<i64>::new());
    }

    #[test]
    fn migrated_rows_with_where_they_came_from_current_or_not() {
        let cfg = fixture("inv-source");
        let rs = query(
            &cfg,
            &ListArgs {
                source: Some("migration".into()),
                ..args(Table::Semantic)
            },
        )
        .unwrap();
        assert_eq!(ids(&rs), vec![2, 4]);
        assert_eq!(
            col(&rs, "file_reference"),
            vec![text("notes/a.md"), text("notes/b.md")]
        );
        assert_eq!(col(&rs, "superseded_by"), vec![Cell::Int(3), Cell::Null]);
    }

    #[test]
    fn filters_are_anded() {
        let cfg = fixture("anded");
        let rs = query(
            &cfg,
            &ListArgs {
                source: Some("migration".into()),
                project: Some("P1".into()),
                not_current: true,
                ..args(Table::Semantic)
            },
        )
        .unwrap();
        assert_eq!(ids(&rs), vec![4]);
    }

    #[test]
    fn instructions_list_their_own_columns_and_current_means_not_retired() {
        let cfg = fixture("instructions");
        let all = query(&cfg, &args(Table::Instructions)).unwrap();
        assert_eq!(names(&all), SKINNY_INSTRUCTIONS);
        assert_eq!(ids(&all), vec![1, 2]);
        let current = query(
            &cfg,
            &ListArgs {
                current: true,
                ..args(Table::Instructions)
            },
        )
        .unwrap();
        assert_eq!(ids(&current), vec![1]);
        assert_eq!(col(&current, "topic"), vec![text("release notes")]);
        let gone = query(
            &cfg,
            &ListArgs {
                not_current: true,
                ..args(Table::Instructions)
            },
        )
        .unwrap();
        assert_eq!(ids(&gone), vec![2]);
    }

    #[test]
    fn every_episodic_row_is_current_so_none_is_not_current() {
        let cfg = fixture("episodic-current");
        let current = query(
            &cfg,
            &ListArgs {
                current: true,
                ..args(Table::Episodic)
            },
        )
        .unwrap();
        assert_eq!(ids(&current).len(), 3);
        let gone = query(
            &cfg,
            &ListArgs {
                not_current: true,
                ..args(Table::Episodic)
            },
        )
        .unwrap();
        assert!(gone.rows.is_empty());
    }

    // ---- the writer's keyword line comes back out ----------------------------

    #[test]
    fn keywords_round_trip_with_the_writer() {
        use crate::embed::fake::Fake;
        use crate::remember::{self, Args as Remember};
        let cfg = scratch("keywords-round-trip");
        for keywords in ["a, b", "turso, vfs, Zürich, \"quoted\"", "x"] {
            let stored = remember::write(
                &cfg,
                &Remember {
                    table: Some(Table::Semantic),
                    text: format!("a body about {keywords}"),
                    keywords: Some(keywords.into()),
                    category: Some("reference".into()),
                    ..Remember::default()
                },
                &Fake::up(),
            )
            .unwrap();
            let rs = query(
                &cfg,
                &ListArgs {
                    ids: Some(stored.id.to_string()),
                    keywords: true,
                    ..args(Table::Semantic)
                },
            )
            .unwrap();
            assert_eq!(col(&rs, "keywords"), vec![text(keywords)]);
        }
    }

    // ---- nothing prints an embedding -----------------------------------------

    #[test]
    fn no_listing_or_row_carries_an_embedding_column() {
        let cfg = fixture("no-embedding");
        for table in [Table::Semantic, Table::Episodic, Table::Instructions] {
            let rs = query(
                &cfg,
                &ListArgs {
                    preview: Some(5),
                    keywords: true,
                    ..args(table)
                },
            )
            .unwrap();
            assert!(!rs.rows.is_empty());
            assert!(!names(&rs).contains(&"embedding"), "{table:?}");
            let shown = show(&cfg, table, "1").unwrap();
            assert!(!shown.out.contains("embedding"), "{table:?}");
            assert!(!shown.out.contains("vector"), "{table:?}");
        }
    }

    // ---- show ----------------------------------------------------------------

    #[test]
    fn show_prints_the_full_rows_in_the_order_given() {
        let cfg = fixture("show-order");
        let shown = show(&cfg, Table::Semantic, "3,1").unwrap();
        assert!(shown.missing.is_empty());
        let first = shown.out.find("deploy now needs a signed tag").unwrap();
        let second = shown
            .out
            .find("deploy needs a tag\n\nKeywords: tag, release")
            .unwrap();
        assert!(first < second, "{}", shown.out);
        for column in [
            "id",
            "created_at",
            "updated_at",
            "project",
            "topic",
            "category",
            "source",
            "model",
            "embed_model",
            "memory_text",
            "file_reference",
            "superseded_by",
            "retired_at",
        ] {
            assert!(
                shown
                    .out
                    .lines()
                    .any(|l| l.trim_start().starts_with(&format!("{column} = "))),
                "no {column}: {}",
                shown.out
            );
        }
        // One blank line between rows and none after the last, like recall.
        let starts: Vec<usize> = shown
            .out
            .lines()
            .enumerate()
            .filter(|(_, l)| l.trim_start().starts_with("id = "))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(starts.len(), 2, "{}", shown.out);
        assert_eq!(
            shown.out.lines().nth(starts[1] - 1),
            Some(""),
            "{}",
            shown.out
        );
        assert!(shown.out.ends_with("retired_at = \n"), "{}", shown.out);
    }

    #[test]
    fn show_reads_superseded_and_retired_rows_too() {
        let cfg = fixture("show-history");
        let shown = show(&cfg, Table::Semantic, "2,4").unwrap();
        assert!(shown.missing.is_empty());
        assert!(shown.out.contains("an old deploy fact"));
        assert!(shown.out.contains("superseded_by = 3"));
        assert!(shown.out.contains("zürich ünïcode"));
    }

    #[test]
    fn show_names_the_missing_ids_and_still_prints_the_rest() {
        let cfg = fixture("show-missing");
        let shown = show(&cfg, Table::Semantic, "99,1,77").unwrap();
        assert_eq!(shown.missing, vec![99, 77]);
        assert!(shown.out.contains("deploy needs a tag"));
        let err = run_show(&cfg, Table::Semantic, "99,1,77").unwrap_err();
        assert_eq!(err.exit_code(), crate::error::EXIT_ERROR);
        let msg = err.to_string();
        assert!(msg.starts_with("error: "), "{msg}");
        assert!(msg.contains("99, 77"), "{msg}");
        assert!(msg.contains("semantic"), "{msg}");
    }

    #[test]
    fn show_prints_an_id_once_however_often_it_is_named() {
        let cfg = fixture("show-dup");
        let shown = show(&cfg, Table::Semantic, "1,1, 1").unwrap();
        assert_eq!(shown.out.matches("an old").count(), 0);
        assert_eq!(shown.out.matches("deploy needs a tag").count(), 1);
    }

    #[test]
    fn show_reads_each_table_with_its_own_columns() {
        let cfg = fixture("show-tables");
        let e = show(&cfg, Table::Episodic, "1").unwrap();
        for column in ["event_type", "importance", "processed_at", "memory_text"] {
            assert!(e.out.contains(&format!("{column} = ")), "{column}");
        }
        assert!(!e.out.contains("superseded_by"));
        let i = show(&cfg, Table::Instructions, "1").unwrap();
        assert!(i.out.contains("memory_text = write release notes tersely"));
        assert!(!i.out.contains("category"));
    }

    // ---- empty is an answer --------------------------------------------------

    #[test]
    fn a_filter_that_matches_nothing_prints_nothing_and_succeeds() {
        let cfg = fixture("empty");
        let a = ListArgs {
            topic: Some("no such topic".into()),
            ..args(Table::Semantic)
        };
        assert_eq!(list(&cfg, &a).unwrap(), "");
        assert!(run_list(&cfg, &a).is_ok());
    }

    #[test]
    fn an_empty_table_lists_nothing() {
        let cfg = scratch("empty-table");
        assert_eq!(list(&cfg, &args(Table::Episodic)).unwrap(), "");
    }

    #[test]
    fn list_prints_in_the_line_format() {
        let cfg = fixture("line-format");
        let a = ListArgs {
            ids: Some("1".into()),
            ..args(Table::Semantic)
        };
        let out = list(&cfg, &a).unwrap();
        assert_eq!(out, crate::render::line(&query(&cfg, &a).unwrap()));
        assert!(out.starts_with("            id = 1\n"), "{out}");
        assert!(out.contains("\n      category = reference\n"), "{out}");
    }

    // ---- refusals ------------------------------------------------------------

    #[test]
    fn unprocessed_is_for_episodic_rows_only() {
        let cfg = fixture("refuse-unprocessed");
        for table in [Table::Semantic, Table::Instructions] {
            let msg = refusal(query(
                &cfg,
                &ListArgs {
                    unprocessed: true,
                    ..args(table)
                },
            ));
            assert!(msg.starts_with("refused: "), "{msg}");
            assert!(msg.contains("--unprocessed"), "{msg}");
            assert!(msg.contains("episodic"), "{msg}");
        }
    }

    #[test]
    fn category_is_for_semantic_rows_only() {
        let cfg = fixture("refuse-category");
        for table in [Table::Episodic, Table::Instructions] {
            let msg = refusal(query(
                &cfg,
                &ListArgs {
                    category: Some("pattern".into()),
                    ..args(table)
                },
            ));
            assert!(msg.contains("--category"), "{msg}");
            assert!(msg.contains("semantic"), "{msg}");
        }
    }

    #[test]
    fn an_unknown_category_is_refused_with_the_alternatives() {
        let cfg = fixture("refuse-category-value");
        let msg = refusal(query(
            &cfg,
            &ListArgs {
                category: Some("gotcha".into()),
                ..args(Table::Semantic)
            },
        ));
        assert!(msg.contains("--category"), "{msg}");
        assert!(msg.contains("pattern"), "{msg}");
    }

    #[test]
    fn current_and_not_current_together_are_refused() {
        let cfg = fixture("refuse-both");
        let msg = refusal(query(
            &cfg,
            &ListArgs {
                current: true,
                not_current: true,
                ..args(Table::Semantic)
            },
        ));
        assert!(msg.contains("--current"), "{msg}");
        assert!(msg.contains("--not-current"), "{msg}");
    }

    #[test]
    fn a_date_the_writer_would_refuse_is_refused() {
        let cfg = fixture("refuse-since");
        for bad in ["March 2024", "2026-13-01", "2026-01-01T00:00:00", "  ", ""] {
            let msg = refusal(query(
                &cfg,
                &ListArgs {
                    since: Some(bad.into()),
                    ..args(Table::Semantic)
                },
            ));
            assert!(msg.contains("--since"), "{bad:?}: {msg}");
        }
    }

    #[test]
    fn ids_must_be_integers_and_there_must_be_some() {
        let cfg = fixture("refuse-ids");
        for bad in ["1,x", "1;2", "", " , ", "1.5"] {
            let msg = refusal(query(
                &cfg,
                &ListArgs {
                    ids: Some(bad.into()),
                    ..args(Table::Semantic)
                },
            ));
            assert!(msg.contains("ids") || msg.contains("id"), "{bad:?}: {msg}");
        }
        for bad in ["1,x", ""] {
            let err = show(&cfg, Table::Semantic, bad).unwrap_err();
            assert_eq!(err.exit_code(), crate::error::EXIT_REFUSED, "{bad:?}");
            assert!(err.to_string().starts_with("refused: "));
        }
    }

    #[test]
    fn a_refusal_comes_before_the_database_is_opened() {
        let cfg = scratch("refuse-before-open");
        std::fs::remove_file(&cfg.db_path).unwrap();
        let msg = refusal(query(
            &cfg,
            &ListArgs {
                unprocessed: true,
                ..args(Table::Semantic)
            },
        ));
        assert!(msg.contains("--unprocessed"), "{msg}");
    }

    #[test]
    fn a_missing_database_is_refused_and_not_created() {
        let cfg = scratch("refuse-no-db");
        std::fs::remove_file(&cfg.db_path).unwrap();
        let msg = refusal(query(&cfg, &args(Table::Semantic)));
        assert!(msg.contains("refusing to silently create"), "{msg}");
        assert!(show(&cfg, Table::Semantic, "1").is_err());
        assert!(!cfg.db_path.exists());
    }

    #[test]
    fn a_database_without_the_instructions_table_points_at_update() {
        let cfg = fixture("refuse-no-instructions");
        sql(&cfg, "DROP TABLE instructions;");
        let msg = refusal(query(&cfg, &args(Table::Instructions)));
        assert!(msg.contains("update"), "{msg}");
        assert!(show(&cfg, Table::Instructions, "1").is_err());
        // The other tables are unaffected.
        assert!(!query(&cfg, &args(Table::Semantic)).unwrap().rows.is_empty());
    }

    #[test]
    fn reading_writes_nothing() {
        let cfg = fixture("pure-reader");
        let before = sql(
            &cfg,
            "SELECT count(*), max(id), max(updated_at) FROM semantic_memory;",
        );
        let _ = list(&cfg, &args(Table::Semantic)).unwrap();
        let _ = show(&cfg, Table::Semantic, "1,2").unwrap();
        assert_eq!(
            before,
            sql(
                &cfg,
                "SELECT count(*), max(id), max(updated_at) FROM semantic_memory;"
            )
        );
    }
}
