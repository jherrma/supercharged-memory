use crate::error::{Error, Result};
use crate::eval::Output;
use crate::store::{EvalCase, Store, Table, Target};

/// The schema's caps on a case.
const MAX_CLASS: usize = 32;
const MAX_QUERY: usize = 512;
const MAX_EXPECT_IDS: usize = 256;
const MAX_STAMPS: usize = 512;

/// A case as typed on the command line.
pub struct NewCase<'a> {
    pub id: &'a str,
    pub class: &'a str,
    pub table: Table,
    pub query: &'a str,
    pub expect: &'a str,
}

/// A case whose arguments passed every check that needs no database.
pub struct Parsed {
    id: String,
    class: String,
    table: Table,
    query: String,
    expect: Vec<i64>,
}

impl Parsed {
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn class(&self) -> &str {
        &self.class
    }
    pub fn query(&self) -> &str {
        &self.query
    }
    pub fn expect(&self) -> &[i64] {
        &self.expect
    }
}

/// One integer id, or a refusal naming what it was for.
pub fn parse_id(what: &str, raw: &str) -> Result<i64> {
    raw.trim().parse().map_err(|_| {
        Error::refused(format!(
            "{what} '{raw}' is not an integer id. Nothing was changed."
        ))
    })
}

/// The arguments of a new case, checked against the schema before anything is
/// written, each refusal naming the flag.
pub fn parse(case: &NewCase) -> Result<Parsed> {
    let id = case.id.trim();
    if id.is_empty() {
        return Err(Error::refused("--id is empty. Nothing was written."));
    }
    let class = case.class.trim();
    if class.is_empty() {
        return Err(Error::refused("--class is empty. Nothing was written."));
    }
    let n = class.chars().count();
    if n > MAX_CLASS {
        return Err(Error::refused(format!(
            "--class is {n} chars; hard cap {MAX_CLASS}. Nothing was written."
        )));
    }
    let query = case.query.trim();
    let n = query.chars().count();
    if !(1..=MAX_QUERY).contains(&n) {
        return Err(Error::refused(format!(
            "--query is {n} chars; it must be 1 to {MAX_QUERY}. Nothing was written."
        )));
    }
    let mut expect = Vec::new();
    for part in case.expect.split(',') {
        let id = part.trim().parse::<i64>().map_err(|_| {
            Error::refused(format!(
                "--expect '{}' is not a comma-separated list of integer ids. \
                 Nothing was written.",
                case.expect
            ))
        })?;
        if expect.contains(&id) {
            return Err(Error::refused(format!(
                "--expect names id {id} twice. Nothing was written."
            )));
        }
        expect.push(id);
    }
    let n = join(&expect).len();
    if n > MAX_EXPECT_IDS {
        return Err(Error::refused(format!(
            "--expect is {n} chars as a list; hard cap {MAX_EXPECT_IDS}. Nothing was written."
        )));
    }
    Ok(Parsed {
        id: id.to_string(),
        class: class.to_string(),
        table: case.table,
        query: query.to_string(),
        expect,
    })
}

fn join(ids: &[i64]) -> String {
    ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
}

/// Why a row is not a valid target, or `None` when it is current. A semantic
/// row that is not current was either superseded or retired; the chain says
/// which.
fn why_not_current(
    store: &impl Store,
    table: Table,
    id: i64,
    found: Option<&Target>,
) -> Result<Option<String>> {
    let Some(t) = found else {
        return Ok(Some(format!("no {} row {id}", table.as_str())));
    };
    if t.current {
        return Ok(None);
    }
    Ok(Some(match store.chain_link(id)? {
        Some(link) => match link.superseded_by {
            Some(by) => format!("{id} is superseded by {by}"),
            None => format!("{id} is retired"),
        },
        None => format!("{id} is not current"),
    }))
}

/// Every target that is missing or not current, or the targets themselves.
fn current_targets(store: &impl Store, table: Table, ids: &[i64]) -> Result<Vec<Target>> {
    let found = store.targets(table, ids)?;
    let mut problems = Vec::new();
    let mut out = Vec::new();
    for &id in ids {
        let t = found.iter().find(|t| t.id == id);
        match why_not_current(store, table, id, t)? {
            Some(why) => problems.push(why),
            None => out.extend(t.cloned()),
        }
    }
    if problems.is_empty() {
        return Ok(out);
    }
    Err(Error::refused(format!(
        "target(s) cannot be used: {}. A case may only point at current {} rows. \
         Nothing was changed.",
        problems.join("; "),
        table.as_str()
    )))
}

/// Author a case. The stamps are read from the targets here, never typed: a
/// stamp that could be typed is a stamp that could be wrong.
pub fn add(store: &impl Store, case: &Parsed) -> Result<Output> {
    if let Some(existing) = store.eval_case(&case.id)? {
        return Err(Error::refused(format!(
            "case '{}' already exists{} — a label is never reused. Pick another --id.",
            case.id,
            if existing.retired {
                " (it is retired, and a retired label is still taken)"
            } else {
                ""
            }
        )));
    }
    // D6.3's rule: a new case draws its class from those already in use, so a
    // typo does not open a class of one. The first case may name any.
    let live = store.eval_cases()?;
    if !live.is_empty() && !live.iter().any(|c| c.class == case.class) {
        let mut classes: Vec<&str> = live.iter().map(|c| c.class.as_str()).collect();
        classes.sort_unstable();
        classes.dedup();
        return Err(Error::refused(format!(
            "--class '{}' is not a class in use: {}. Pick one of those. Nothing was written.",
            case.class,
            classes.join(", ")
        )));
    }

    let targets = current_targets(store, case.table, &case.expect)?;
    let stamps: Vec<String> = targets.iter().map(|t| t.created_at.clone()).collect();
    let n = stamps.join(",").len();
    if n > MAX_STAMPS {
        return Err(Error::refused(format!(
            "--expect names too many targets: their stamps are {n} chars; hard cap {MAX_STAMPS}. \
             Nothing was written."
        )));
    }
    store.insert_eval_case(&EvalCase {
        id: case.id.clone(),
        class: case.class.clone(),
        table: case.table,
        query: case.query.clone(),
        expect: case.expect.clone(),
        stamps: stamps.clone(),
        retired: false,
    })?;
    Ok(Output {
        text: format!(
            "added case {} [{}]: {} target(s) {} (stamps {})\n",
            case.id,
            case.class,
            case.table.as_str(),
            join(&case.expect),
            stamps.join(",")
        ),
        ..Output::default()
    })
}

/// A live case, or a refusal saying why there is none.
fn live_case(store: &impl Store, id: &str) -> Result<EvalCase> {
    match store.eval_case(id)? {
        None => Err(Error::refused(format!(
            "no eval case '{id}'. Nothing was changed."
        ))),
        Some(c) if c.retired => Err(Error::refused(format!(
            "eval case '{id}' is retired. Nothing was changed."
        ))),
        Some(c) => Ok(c),
    }
}

/// Move one target of a case to a new current row and take that row's stamp,
/// in one statement.
///
/// Only the moved target's stamp changes. Re-stamping the others would bless
/// whatever their ids point at now, which is exactly the reuse the stamps exist
/// to catch -- and for the same reason a case is never repointed at a target it
/// already has.
pub fn repoint(store: &impl Store, case: &str, to: i64, from: Option<i64>) -> Result<Output> {
    let case = live_case(store, case)?;
    let table = case.table;
    let target = current_targets(store, table, &[to])?;
    if case.expect.contains(&to) {
        return Err(Error::refused(format!(
            "{to} is already a target of case '{}'. Re-pointing at it would only re-stamp it, \
             and a re-stamp is what hides a reused id: retire the case instead. \
             Nothing was changed.",
            case.id
        )));
    }
    if case.stamps.len() != case.expect.len() {
        return Err(Error::refused(format!(
            "case '{}' has {} ids but {} stamps, so it cannot be repointed safely: retire it \
             and add it again. Nothing was changed.",
            case.id,
            case.expect.len(),
            case.stamps.len()
        )));
    }
    let at = match from {
        Some(old) => case
            .expect
            .iter()
            .position(|&id| id == old)
            .ok_or_else(|| {
                Error::refused(format!(
                    "--from {old} is not a target of case '{}' (targets: {}). Nothing was changed.",
                    case.id,
                    join(&case.expect)
                ))
            })?,
        None if case.expect.len() == 1 => 0,
        None => {
            return Err(Error::refused(format!(
                "case '{}' has {} targets ({}): say which one moves with --from <id>. \
                 Nothing was changed.",
                case.id,
                case.expect.len(),
                join(&case.expect)
            )));
        }
    };
    let old = case.expect[at];
    let stamp = target[0].created_at.clone();
    let mut expect = case.expect.clone();
    let mut stamps = case.stamps.clone();
    expect[at] = to;
    stamps[at] = stamp.clone();
    if !store.set_eval_targets(&case.id, &expect, &stamps)? {
        return Err(Error::refused(format!(
            "case '{}' was retired in the meantime. Nothing was changed.",
            case.id
        )));
    }
    Ok(Output {
        text: format!(
            "repointed case {}: {old} -> {to} (stamp {stamp})\n",
            case.id
        ),
        ..Output::default()
    })
}

/// Retire a case: `retired_at` is set and the row stays.
pub fn retire(store: &impl Store, case: &str) -> Result<Output> {
    let case = match store.eval_case(case)? {
        None => {
            return Err(Error::refused(format!(
                "no eval case '{case}'. Nothing was changed."
            )));
        }
        Some(c) if c.retired => {
            return Err(Error::refused(format!(
                "eval case '{case}' is already retired. Nothing was changed."
            )));
        }
        Some(c) => c,
    };
    if !store.retire_eval_case(&case.id)? {
        return Err(Error::refused(format!(
            "eval case '{}' was retired in the meantime. Nothing was changed.",
            case.id
        )));
    }
    Ok(Output {
        text: format!(
            "retired case {} (retired_at set; nothing was deleted)\n",
            case.id
        ),
        ..Output::default()
    })
}

#[cfg(test)]
mod tests {
    use crate::config::Config;
    use crate::eval::testing::*;
    use crate::eval::{Mode, Output};
    use crate::store::turso::TursoStore;
    use crate::store::turso::tests::{put, retire_row, rows};
    use crate::store::{Store, Table};

    fn add(
        cfg: &Config,
        id: &str,
        class: &str,
        query: &str,
        expect: &str,
    ) -> crate::error::Result<Output> {
        exec(
            cfg,
            Mode::AddCase {
                id: id.into(),
                class: class.into(),
                table: Table::Semantic,
                query: query.into(),
                expect: expect.into(),
            },
        )
    }

    fn cases(cfg: &Config) -> i64 {
        count(cfg, "SELECT count(*) FROM eval_cases;")
    }

    fn case(cfg: &Config, id: &str) -> crate::store::EvalCase {
        TursoStore::open(cfg)
            .unwrap()
            .eval_case(id)
            .unwrap()
            .expect("the case")
    }

    // ---- add-case ---------------------------------------------------------

    #[test]
    fn the_stamps_of_a_new_case_are_the_targets_created_at_in_expect_order() {
        let (cfg, ids) = db("add-stamps");
        restamp(&cfg, Table::Semantic, ids[0], "2026-02-03 04:05:06");
        restamp(&cfg, Table::Semantic, ids[3], "2026-03-04 05:06:07");
        let out = add(
            &cfg,
            "s01",
            "semantic",
            "how do I open it",
            &format!("{}, {}", ids[3], ids[0]),
        )
        .unwrap();
        let c = case(&cfg, "s01");
        assert_eq!(c.expect, vec![ids[3], ids[0]]);
        assert_eq!(c.stamps, ["2026-03-04 05:06:07", "2026-02-03 04:05:06"]);
        assert_eq!(
            (c.class.as_str(), c.table, c.query.as_str()),
            ("semantic", Table::Semantic, "how do I open it")
        );
        assert!(!c.retired);
        assert!(out.text.contains("s01"), "{}", out.text);
        assert!(
            out.text.contains("2026-03-04 05:06:07"),
            "the stamps are shown: {}",
            out.text
        );
        // The same rule `--validate` applies: nothing to flag.
        assert_eq!(exec(&cfg, Mode::Validate).unwrap().failure, None);
    }

    #[test]
    fn an_episodic_case_is_stamped_from_the_episodic_table() {
        let (cfg, _) = db("add-episodic");
        let event = put(&cfg, Table::Episodic, "something happened", true);
        restamp(&cfg, Table::Episodic, event, "2026-07-08 09:10:11");
        exec(
            &cfg,
            Mode::AddCase {
                id: "e01".into(),
                class: "episodic".into(),
                table: Table::Episodic,
                query: "what happened".into(),
                expect: event.to_string(),
            },
        )
        .unwrap();
        let c = case(&cfg, "e01");
        assert_eq!(c.table, Table::Episodic);
        assert_eq!(c.stamps, ["2026-07-08 09:10:11"]);
    }

    #[test]
    fn the_first_case_may_name_any_class_and_later_ones_must_reuse_one() {
        let (cfg, ids) = db("add-class");
        let first = ids[0].to_string();
        add(&cfg, "s01", "brand-new-class", "q one", &first).unwrap();
        let msg = refusal(add(&cfg, "s02", "another-class", "q two", &first));
        assert!(msg.contains("another-class"), "{msg}");
        assert!(
            msg.contains("brand-new-class"),
            "names the classes that exist: {msg}"
        );
        assert_eq!(cases(&cfg), 1);
        add(&cfg, "s02", "brand-new-class", "q two", &first).unwrap();
        assert_eq!(cases(&cfg), 2);
    }

    #[test]
    fn a_class_only_a_retired_case_uses_is_not_present() {
        let (cfg, ids) = db("add-class-retired");
        raw_case(
            &cfg,
            "s01",
            "old-class",
            "semantic",
            &ids[0].to_string(),
            "2026-01-01 00:00:00",
        );
        rows(
            &cfg,
            "UPDATE eval_cases SET retired_at = datetime('now') WHERE id = 's01';",
        );
        // The live set is empty again, so any class is allowed.
        add(&cfg, "s02", "fresh-class", "q", &ids[0].to_string()).unwrap();
        assert_eq!(case(&cfg, "s02").class, "fresh-class");
    }

    #[test]
    fn the_query_is_one_to_five_hundred_and_twelve_characters_not_bytes() {
        let (cfg, ids) = db("add-query");
        let t = ids[0].to_string();
        for (n, q) in [
            (1, String::new()),
            (2, "   ".to_string()),
            (3, "q".repeat(513)),
        ] {
            let msg = refusal(add(&cfg, &format!("s{n}"), "semantic", &q, &t));
            assert!(msg.contains("--query"), "{msg}");
        }
        assert_eq!(cases(&cfg), 0);
        add(&cfg, "ok1", "semantic", &"q".repeat(512), &t).unwrap();
        // 512 two-byte characters are 1024 bytes, and within the schema's cap.
        add(&cfg, "ok2", "semantic", &"ä".repeat(512), &t).unwrap();
        assert_eq!(cases(&cfg), 2);
    }

    #[test]
    fn a_label_and_a_class_must_fit_their_columns() {
        let (cfg, ids) = db("add-label");
        let t = ids[0].to_string();
        let msg = refusal(add(&cfg, "  ", "semantic", "q", &t));
        assert!(msg.contains("--id"), "{msg}");
        let msg = refusal(add(&cfg, "s01", "", "q", &t));
        assert!(msg.contains("--class"), "{msg}");
        let msg = refusal(add(&cfg, "s01", &"c".repeat(33), "q", &t));
        assert!(msg.contains("--class") && msg.contains("32"), "{msg}");
        add(&cfg, "s01", &"c".repeat(32), "q", &t).unwrap();
        assert_eq!(cases(&cfg), 1);
    }

    #[test]
    fn a_duplicate_label_is_refused_even_when_the_first_case_is_retired() {
        let (cfg, ids) = db("add-duplicate");
        let t = ids[0].to_string();
        add(&cfg, "s01", "semantic", "q", &t).unwrap();
        let msg = refusal(add(&cfg, "s01", "semantic", "another query", &t));
        assert!(msg.contains("s01") && msg.contains("already"), "{msg}");
        rows(
            &cfg,
            "UPDATE eval_cases SET retired_at = datetime('now') WHERE id = 's01';",
        );
        let msg = refusal(add(&cfg, "s01", "semantic", "another query", &t));
        assert!(
            msg.contains("retired"),
            "a retired label is still taken: {msg}"
        );
        assert_eq!(cases(&cfg), 1);
        assert_eq!(case(&cfg, "s01").query, "q", "the first case is untouched");
    }

    #[test]
    fn the_expected_ids_must_be_a_clean_list_of_distinct_integers() {
        let (cfg, ids) = db("add-expect");
        let t = ids[0];
        for bad in ["", " ", "abc", "1,,2", "1,x", &format!("{t},{t}"), "1.5"] {
            let msg = refusal(add(&cfg, "s01", "semantic", "q", bad));
            assert!(msg.contains("--expect"), "{bad:?}: {msg}");
        }
        assert_eq!(cases(&cfg), 0);
    }

    #[test]
    fn the_expected_ids_and_their_stamps_must_fit_their_columns() {
        let (cfg, _) = db("add-expect-cap");
        let many = (100_000..100_050)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let msg = refusal(add(&cfg, "s01", "semantic", "q", &many));
        assert!(msg.contains("256"), "{msg}");
        assert_eq!(cases(&cfg), 0);
    }

    #[test]
    fn a_target_that_is_missing_or_not_current_is_refused_naming_it() {
        let (cfg, ids) = db("add-targets");
        supersede(&cfg, ids[0], ids[1]);
        retire_row(&cfg, Table::Semantic, ids[2]);
        let msg = refusal(add(&cfg, "s01", "semantic", "q", "9999"));
        assert!(
            msg.contains("9999") && msg.contains("no semantic row"),
            "{msg}"
        );
        let msg = refusal(add(
            &cfg,
            "s01",
            "semantic",
            "q",
            &format!("{},{}", ids[3], ids[0]),
        ));
        assert!(
            msg.contains(&ids[0].to_string()) && msg.contains("superseded"),
            "{msg}"
        );
        let msg = refusal(add(&cfg, "s01", "semantic", "q", &ids[2].to_string()));
        assert!(
            msg.contains(&ids[2].to_string()) && msg.contains("retired"),
            "{msg}"
        );
        assert_eq!(
            cases(&cfg),
            0,
            "nothing written, not even for the valid target"
        );
    }

    #[test]
    fn an_id_that_exists_only_in_the_other_table_is_missing() {
        let (cfg, ids) = db("add-wrong-table");
        // The semantic ids exist; the episodic table is empty.
        let msg = refusal(exec(
            &cfg,
            Mode::AddCase {
                id: "e01".into(),
                class: "episodic".into(),
                table: Table::Episodic,
                query: "q".into(),
                expect: ids[0].to_string(),
            },
        ));
        assert!(msg.contains("no episodic row"), "{msg}");
    }

    // ---- repoint ----------------------------------------------------------

    fn repoint(
        cfg: &Config,
        case: &str,
        to: &str,
        from: Option<i64>,
    ) -> crate::error::Result<Output> {
        exec(
            cfg,
            Mode::Repoint {
                case: case.into(),
                to: to.into(),
                from,
            },
        )
    }

    #[test]
    fn repointing_moves_the_target_and_refreshes_its_stamp_so_validation_is_clean() {
        let (cfg, ids) = db("repoint-ok");
        restamp(&cfg, Table::Semantic, ids[1], "2026-05-06 07:08:09");
        raw_case(
            &cfg,
            "s01",
            "semantic",
            "semantic",
            &ids[0].to_string(),
            "2026-01-01 00:00:00",
        );
        supersede(&cfg, ids[0], ids[1]);
        assert!(exec(&cfg, Mode::Validate).unwrap().failure.is_some());

        let out = repoint(&cfg, "s01", &ids[1].to_string(), None).unwrap();
        let c = case(&cfg, "s01");
        assert_eq!(c.expect, vec![ids[1]]);
        assert_eq!(c.stamps, ["2026-05-06 07:08:09"]);
        assert!(
            out.text.contains("s01") && out.text.contains("2026-05-06 07:08:09"),
            "{}",
            out.text
        );
        assert_eq!(exec(&cfg, Mode::Validate).unwrap().failure, None);
    }

    #[test]
    fn in_a_case_with_several_targets_only_the_named_one_moves_and_the_others_keep_their_stamps() {
        let (cfg, ids) = db("repoint-several");
        restamp(&cfg, Table::Semantic, ids[4], "2026-09-09 09:09:09");
        // The second target's stamp is stale on purpose: a reused id that
        // repointing the first must NOT quietly bless.
        raw_case(
            &cfg,
            "s01",
            "semantic",
            "semantic",
            &format!("{},{}", ids[0], ids[1]),
            "2026-01-01 00:00:00,2000-01-01 00:00:00",
        );
        supersede(&cfg, ids[0], ids[4]);
        let msg = refusal(repoint(&cfg, "s01", &ids[4].to_string(), None));
        assert!(
            msg.contains("--from"),
            "which target to replace must be said: {msg}"
        );
        assert_eq!(case(&cfg, "s01").expect, vec![ids[0], ids[1]], "unchanged");

        repoint(&cfg, "s01", &ids[4].to_string(), Some(ids[0])).unwrap();
        let c = case(&cfg, "s01");
        assert_eq!(c.expect, vec![ids[4], ids[1]], "position kept");
        assert_eq!(c.stamps, ["2026-09-09 09:09:09", "2000-01-01 00:00:00"]);
    }

    #[test]
    fn repointing_refuses_what_it_cannot_do_and_changes_nothing() {
        let (cfg, ids) = db("repoint-refusals");
        supersede(&cfg, ids[2], ids[3]);
        retire_row(&cfg, Table::Semantic, ids[5]);
        raw_case(
            &cfg,
            "s01",
            "semantic",
            "semantic",
            &ids[0].to_string(),
            "2026-01-01 00:00:00",
        );
        raw_case(
            &cfg,
            "s02",
            "semantic",
            "semantic",
            &format!("{},{}", ids[0], ids[1]),
            "2026-01-01 00:00:00,2026-01-01 00:00:00",
        );
        raw_case(
            &cfg,
            "s03",
            "semantic",
            "semantic",
            &ids[1].to_string(),
            "2026-01-01 00:00:00",
        );
        rows(
            &cfg,
            "UPDATE eval_cases SET retired_at = datetime('now') WHERE id = 's03';",
        );
        let before = (case(&cfg, "s01"), case(&cfg, "s02"), case(&cfg, "s03"));

        let new = ids[4].to_string();
        let cases = [
            (repoint(&cfg, "nope", &new, None), "nope"),
            (repoint(&cfg, "s03", &new, None), "retired"),
            (repoint(&cfg, "s01", "abc", None), "integer"),
            (repoint(&cfg, "s01", "9999", None), "no semantic row"),
            (
                repoint(&cfg, "s01", &ids[2].to_string(), None),
                "superseded",
            ),
            (repoint(&cfg, "s01", &ids[5].to_string(), None), "retired"),
            // Repointing at the target it already has would bless a reused id.
            (repoint(&cfg, "s01", &ids[0].to_string(), None), "already"),
            (
                repoint(&cfg, "s02", &ids[1].to_string(), Some(ids[0])),
                "already",
            ),
            (repoint(&cfg, "s02", &new, Some(ids[6])), "--from"),
        ];
        for (result, needle) in cases {
            let msg = refusal(result);
            assert!(msg.contains(needle), "{needle}: {msg}");
        }
        assert_eq!(
            before,
            (case(&cfg, "s01"), case(&cfg, "s02"), case(&cfg, "s03"))
        );
    }

    // ---- retire-case ------------------------------------------------------

    #[test]
    fn retiring_a_case_keeps_the_row_and_drops_it_from_the_live_set() {
        let (cfg, ids) = db("retire-ok");
        raw_case(
            &cfg,
            "s01",
            "semantic",
            "semantic",
            &ids[0].to_string(),
            "2026-01-01 00:00:00",
        );
        raw_case(
            &cfg,
            "s02",
            "semantic",
            "semantic",
            &ids[1].to_string(),
            "2026-01-01 00:00:00",
        );
        let out = exec(&cfg, Mode::RetireCase("s01".into())).unwrap();
        assert!(
            out.text.contains("s01") && out.text.contains("nothing was deleted"),
            "{}",
            out.text
        );
        assert_eq!(cases(&cfg), 2, "the row is still there");
        assert_eq!(
            count(
                &cfg,
                "SELECT count(*) FROM eval_cases WHERE retired_at IS NOT NULL;"
            ),
            1
        );
        let v = exec(&cfg, Mode::Validate).unwrap();
        assert!(
            v.text.starts_with(&format!(
                "eval_cases: 1 live case(s) in {}",
                cfg.db_path.display()
            )),
            "{}",
            v.text
        );
    }

    #[test]
    fn retiring_an_unknown_or_already_retired_case_is_refused() {
        let (cfg, ids) = db("retire-refusals");
        raw_case(
            &cfg,
            "s01",
            "semantic",
            "semantic",
            &ids[0].to_string(),
            "2026-01-01 00:00:00",
        );
        let msg = refusal(exec(&cfg, Mode::RetireCase("nope".into())));
        assert!(msg.contains("nope"), "{msg}");
        exec(&cfg, Mode::RetireCase("s01".into())).unwrap();
        let msg = refusal(exec(&cfg, Mode::RetireCase("s01".into())));
        assert!(msg.contains("already retired"), "{msg}");
    }

    #[test]
    fn a_missing_database_is_refused_and_never_created() {
        let cfg = {
            let (mut cfg, _) = db("cases-missing-db");
            cfg.db_path = cfg.db_path.with_file_name("not-there.db");
            cfg
        };
        let msg = refusal(exec(&cfg, Mode::RetireCase("s01".into())));
        assert!(
            msg.contains("missing") || msg.contains("no database"),
            "{msg}"
        );
        assert!(!cfg.db_path.exists());
    }
}
