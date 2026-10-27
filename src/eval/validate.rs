//! `--validate`: does every live case still point at what it was written for.

use std::collections::HashSet;

use crate::config::Config;
use crate::error::Result;
use crate::eval::Output;
use crate::store::{Store, Table};

/// Check every live case against the rows it points at.
///
/// A target is fine when it is a current row whose `created_at` is the stamp
/// taken when the case was written. Anything else is one of three things: the
/// row was superseded (its successor is named), it is gone, or its id was
/// reused by an unrelated memory -- `id INTEGER PRIMARY KEY` has no
/// AUTOINCREMENT, so a purged high id comes back on the next insert, and the
/// stamp is the only thing that sees it.
pub fn validate(cfg: &Config, store: &impl Store) -> Result<Output> {
    let cases = store.eval_cases()?;
    let mut text = format!(
        "eval_cases: {} live case(s) in {}\n\n",
        cases.len(),
        cfg.db_path.display()
    );
    if cases.is_empty() {
        text.push_str(
            "  nothing to validate — no eval cases have been authored yet, so no case\n  \
             can be pointing at a purged, superseded or reused row.\n  \
             Expected on a fresh install and on a corpus that was just imported by\n  \
             the migration runbook. Author cases with `eval --add-case` (never\n  \
             auto-generated from memory_text), or migrate a legacy file set with\n  \
             `eval --import <dir>`.\n",
        );
        return Ok(Output {
            text,
            ..Output::default()
        });
    }

    let mut problems = 0;
    for case in &cases {
        let targets = store.targets(case.table, &case.expect)?;
        let mut alive = 0;
        let mut notes = Vec::new();
        for (n, &id) in case.expect.iter().enumerate() {
            let row = targets.iter().find(|t| t.id == id && t.current);
            match (row, case.stamps.get(n)) {
                (None, _) => {
                    let successor = match case.table {
                        Table::Semantic => survivor(store, id)?,
                        _ => None,
                    };
                    notes.push(match successor {
                        Some(next) => format!("{id} -> {next} (superseded; repoint)"),
                        None => {
                            format!("{id} -> GONE (purged or retired; retire the case)")
                        }
                    });
                }
                (Some(_), None) => notes.push(format!(
                    "{id} -> NO STAMP recorded; nothing can tell whether the id was reused"
                )),
                (Some(t), Some(stamp)) if &t.created_at != stamp => notes.push(format!(
                    "{id} -> ID REUSED (created_at {} != {stamp}); the case now points at \
                     a different memory",
                    t.created_at
                )),
                (Some(_), Some(_)) => alive += 1,
            }
        }
        if notes.is_empty() {
            continue;
        }
        problems += 1;
        let state = if alive == 0 {
            "NO VALID TARGET LEFT".to_string()
        } else {
            format!("{alive} target(s) still valid")
        };
        text.push_str(&format!("  {:<6} [{}] {state}\n", case.id, case.class));
        for note in notes {
            text.push_str(&format!("           {note}\n"));
        }
    }

    if problems == 0 {
        text.push_str("  all cases point at live, current rows with matching timestamps.\n");
        return Ok(Output {
            text,
            ..Output::default()
        });
    }
    text.push_str(&format!(
        "\n{problems} case(s) need attention. A target that moved down a supersede chain is \
         moved with\n`eval --repoint <case> <new-id>`. A case whose targets all went GONE or \
         got REUSED should be\nRETIRED (`eval --retire-case <case>`), not repointed at a \
         loosely-related row — a repointed case\nquietly changes what the metric measures.\n"
    ));
    Ok(Output {
        text,
        failure: Some(format!("{problems} eval case(s) need attention")),
        ..Output::default()
    })
}

/// Follow a semantic supersede chain to the current row, or `None` when the
/// chain ends retired or at a row that is gone.
fn survivor(store: &impl Store, id: i64) -> Result<Option<i64>> {
    let mut seen = HashSet::new();
    let mut cur = id;
    while seen.insert(cur) {
        let Some(link) = store.chain_link(cur)? else {
            return Ok(None);
        };
        match link.superseded_by {
            None => return Ok((!link.retired).then_some(cur)),
            Some(next) => cur = next,
        }
    }
    // A chain that loops back on itself has no current end.
    Ok(None)
}

#[cfg(test)]
mod tests {
    use crate::embed::Unavailable;
    use crate::embed::fake::Fake;
    use crate::eval::testing::*;
    use crate::eval::{Mode, execute};
    use crate::store::Table;
    use crate::store::turso::tests::{put, retire_row, rows};

    const STAMP: &str = "2026-01-01 00:00:00";

    fn validate(cfg: &crate::config::Config) -> crate::eval::Output {
        exec(cfg, Mode::Validate).expect("validate")
    }

    #[test]
    fn an_empty_eval_set_has_nothing_to_validate_and_that_is_not_a_failure() {
        let (cfg, _) = db("val-empty");
        let out = validate(&cfg);
        assert_eq!(out.failure, None);
        assert!(
            out.text.starts_with(&format!(
                "eval_cases: 0 live case(s) in {}\n",
                cfg.db_path.display()
            )),
            "{}",
            out.text
        );
        assert!(out.text.contains("nothing to validate"), "{}", out.text);
    }

    #[test]
    fn cases_that_point_at_current_rows_with_matching_stamps_are_clean() {
        let (cfg, ids) = db("val-clean");
        raw_case(
            &cfg,
            "s01",
            "semantic",
            "semantic",
            &ids[0].to_string(),
            STAMP,
        );
        raw_case(
            &cfg,
            "s02",
            "exact",
            "semantic",
            &format!("{},{}", ids[1], ids[2]),
            &format!("{STAMP},{STAMP}"),
        );
        let out = validate(&cfg);
        assert_eq!(out.failure, None);
        assert!(
            out.text.starts_with(&format!(
                "eval_cases: 2 live case(s) in {}\n\n",
                cfg.db_path.display()
            )),
            "{}",
            out.text
        );
        assert!(
            out.text
                .contains("  all cases point at live, current rows with matching timestamps."),
            "{}",
            out.text
        );
    }

    #[test]
    fn a_superseded_target_is_reported_with_its_successor_and_fails_the_run() {
        let (cfg, ids) = db("val-superseded");
        supersede(&cfg, ids[0], ids[1]);
        raw_case(
            &cfg,
            "s01",
            "semantic",
            "semantic",
            &ids[0].to_string(),
            STAMP,
        );
        let out = validate(&cfg);
        let want = format!(
            "  s01    [semantic] NO VALID TARGET LEFT\n           {} -> {} (superseded; repoint)\n",
            ids[0], ids[1]
        );
        assert!(out.text.contains(&want), "{}", out.text);
        assert!(
            out.text.contains("1 case(s) need attention."),
            "{}",
            out.text
        );
        assert!(out.failure.is_some());
    }

    #[test]
    fn a_chain_of_supersedes_is_followed_to_the_current_row() {
        let (cfg, ids) = db("val-chain");
        supersede(&cfg, ids[0], ids[1]);
        supersede(&cfg, ids[1], ids[2]);
        raw_case(
            &cfg,
            "s01",
            "semantic",
            "semantic",
            &ids[0].to_string(),
            STAMP,
        );
        let out = validate(&cfg);
        assert!(
            out.text
                .contains(&format!("{} -> {} (superseded; repoint)", ids[0], ids[2])),
            "{}",
            out.text
        );
    }

    #[test]
    fn a_chain_that_ends_retired_or_in_a_purged_row_is_gone() {
        let (cfg, ids) = db("val-gone");
        // Superseded by a row that was retired.
        supersede(&cfg, ids[0], ids[1]);
        retire_row(&cfg, Table::Semantic, ids[1]);
        // Retired outright.
        retire_row(&cfg, Table::Semantic, ids[2]);
        // Purged: no row at all.
        rows(
            &cfg,
            &format!("DELETE FROM semantic_memory WHERE id = {};", ids[6]),
        );
        for (case, id) in [("s01", ids[0]), ("s02", ids[2]), ("s03", ids[6])] {
            raw_case(&cfg, case, "semantic", "semantic", &id.to_string(), STAMP);
        }
        let out = validate(&cfg);
        for id in [ids[0], ids[2], ids[6]] {
            assert!(
                out.text.contains(&format!(
                    "{id} -> GONE (purged or retired; retire the case)"
                )),
                "{}",
                out.text
            );
        }
        assert!(
            out.text.contains("3 case(s) need attention."),
            "{}",
            out.text
        );
        assert!(out.text.contains("eval --retire-case"), "{}", out.text);
        assert!(out.text.contains("eval --repoint"), "{}", out.text);
    }

    /// `id INTEGER PRIMARY KEY` has no AUTOINCREMENT: a purged top id comes
    /// back on the next insert. Only the stamp sees it.
    #[test]
    fn a_reused_id_is_caught_by_its_stamp() {
        let (cfg, ids) = db("val-reused");
        let top = *ids.last().unwrap();
        raw_case(&cfg, "s01", "semantic", "semantic", &top.to_string(), STAMP);
        rows(
            &cfg,
            &format!("DELETE FROM semantic_memory WHERE id = {top};"),
        );
        let again = put(&cfg, Table::Semantic, "an unrelated memory", true);
        assert_eq!(again, top, "the fixture must reproduce the reuse");
        restamp(&cfg, Table::Semantic, again, "2026-06-01 12:00:00");

        let out = validate(&cfg);
        assert!(
            out.text.contains(&format!(
                "{top} -> ID REUSED (created_at 2026-06-01 12:00:00 != {STAMP}); \
                 the case now points at a different memory"
            )),
            "{}",
            out.text
        );
        assert!(out.failure.is_some());
    }

    #[test]
    fn a_case_with_one_live_target_left_says_so() {
        let (cfg, ids) = db("val-partial");
        retire_row(&cfg, Table::Semantic, ids[1]);
        raw_case(
            &cfg,
            "s01",
            "semantic",
            "semantic",
            &format!("{},{}", ids[0], ids[1]),
            &format!("{STAMP},{STAMP}"),
        );
        let out = validate(&cfg);
        assert!(
            out.text
                .contains("  s01    [semantic] 1 target(s) still valid\n"),
            "{}",
            out.text
        );
        assert!(!out.text.contains("NO VALID TARGET LEFT"), "{}", out.text);
    }

    #[test]
    fn a_retired_case_is_not_validated() {
        let (cfg, ids) = db("val-retired-case");
        rows(
            &cfg,
            &format!("DELETE FROM semantic_memory WHERE id = {};", ids[6]),
        );
        raw_case(
            &cfg,
            "s01",
            "semantic",
            "semantic",
            &ids[6].to_string(),
            STAMP,
        );
        rows(
            &cfg,
            "UPDATE eval_cases SET retired_at = datetime('now') WHERE id = 's01';",
        );
        let out = validate(&cfg);
        assert_eq!(out.failure, None);
        assert!(out.text.contains("nothing to validate"), "{}", out.text);
    }

    #[test]
    fn episodic_targets_are_checked_in_the_episodic_table() {
        let (cfg, _) = db("val-episodic");
        let event = put(&cfg, Table::Episodic, "something happened", true);
        raw_case(
            &cfg,
            "e01",
            "episodic",
            "episodic",
            &event.to_string(),
            STAMP,
        );
        assert_eq!(validate(&cfg).failure, None);
        rows(
            &cfg,
            &format!("DELETE FROM episodic_memory WHERE id = {event};"),
        );
        let out = validate(&cfg);
        assert!(
            out.text.contains(&format!("{event} -> GONE")),
            "an episodic row has no supersede chain: {}",
            out.text
        );
        assert!(out.failure.is_some());
    }

    #[test]
    fn validation_never_contacts_the_embedder() {
        let (cfg, ids) = db("val-offline");
        raw_case(
            &cfg,
            "s01",
            "semantic",
            "semantic",
            &ids[0].to_string(),
            STAMP,
        );
        let down = Fake::down(Unavailable::Unreachable {
            url: "http://127.0.0.1:9".into(),
        });
        let out = execute(&cfg, &Mode::Validate, &down).expect("validate");
        assert_eq!(out.failure, None);
        assert_eq!(down.calls.get(), 0);
    }

    #[test]
    fn validation_writes_nothing() {
        let (cfg, ids) = db("val-readonly");
        supersede(&cfg, ids[0], ids[1]);
        raw_case(
            &cfg,
            "s01",
            "semantic",
            "semantic",
            &ids[0].to_string(),
            STAMP,
        );
        validate(&cfg);
        assert_eq!(
            count(
                &cfg,
                "SELECT count(*) FROM eval_cases WHERE retired_at IS NULL;"
            ),
            1
        );
        assert_eq!(count(&cfg, "SELECT count(*) FROM eval_runs;"), 0);
    }

    #[test]
    fn a_target_without_a_recorded_stamp_cannot_be_trusted() {
        let (cfg, ids) = db("val-no-stamp");
        // Two ids, one stamp: the second target can never be checked.
        raw_case(
            &cfg,
            "s01",
            "semantic",
            "semantic",
            &format!("{},{}", ids[0], ids[1]),
            STAMP,
        );
        let out = validate(&cfg);
        assert!(
            out.text
                .contains(&format!("{} -> NO STAMP recorded", ids[1])),
            "{}",
            out.text
        );
        assert!(out.text.contains("1 target(s) still valid"), "{}", out.text);
        assert!(out.failure.is_some());
    }

    #[test]
    fn a_chain_that_loops_has_no_current_end() {
        let (cfg, ids) = db("val-loop");
        supersede(&cfg, ids[0], ids[1]);
        supersede(&cfg, ids[1], ids[0]);
        raw_case(
            &cfg,
            "s01",
            "semantic",
            "semantic",
            &ids[0].to_string(),
            STAMP,
        );
        let out = validate(&cfg);
        assert!(
            out.text.contains(&format!("{} -> GONE", ids[0])),
            "{}",
            out.text
        );
    }
}
