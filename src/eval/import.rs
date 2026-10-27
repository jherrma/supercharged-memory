//! `--import`: one-shot migration of the legacy file set into the database.

use std::collections::HashSet;
use std::path::Path;

use serde_json::Value;

use crate::error::{Error, Result};
use crate::eval::Output;
use crate::eval::cases::{self, NewCase};
use crate::store::{EvalCase, EvalRun, Store, Table};

/// Migrate a legacy `eval.jsonl` (and `history.jsonl`, when there is one) into
/// the database.
///
/// Everything is read and checked first: a file with one bad line imports
/// nothing, rather than the lines before it. Idempotent -- a case whose label is
/// already in the database, and a run with the same date and figures, are
/// skipped. A case's stamps are read from its targets' rows as they are now.
pub fn import(store: &impl Store, dir: &Path) -> Result<Output> {
    let cases_text = read(&dir.join("eval.jsonl"))?.ok_or_else(|| {
        Error::refused(format!(
            "no eval.jsonl in {}. Nothing was imported.",
            dir.display()
        ))
    })?;

    let mut seen = HashSet::new();
    let mut skipped = 0;
    let mut to_add = Vec::new();
    for (n, line) in lines(&cases_text) {
        let at = format!("eval.jsonl line {n}");
        let c = parse_case(&at, line)?;
        if !seen.insert(c.id.clone()) || store.eval_case(&c.id)?.is_some() {
            skipped += 1;
            continue;
        }
        let found = store.targets(c.table, &c.expect)?;
        let mut stamps = Vec::new();
        for id in &c.expect {
            match found.iter().find(|t| t.id == *id) {
                Some(t) => stamps.push(t.created_at.clone()),
                None => {
                    return Err(Error::refused(format!(
                        "{at}: case {}: target row {id} does not exist in {}_memory — fix the \
                         file before importing. Nothing was imported.",
                        c.id,
                        c.table.as_str()
                    )));
                }
            }
        }
        to_add.push(EvalCase { stamps, ..c });
    }

    let mut runs = Vec::new();
    if let Some(text) = read(&dir.join("history.jsonl"))? {
        for (n, line) in lines(&text) {
            runs.push(parse_run(&format!("history.jsonl line {n}"), line)?);
        }
    }

    for c in &to_add {
        store.insert_eval_case(c)?;
    }
    let mut imported_runs = 0;
    for run in &runs {
        // A re-import must not duplicate the baseline.
        if !store.has_eval_run(run)? {
            store.insert_eval_run(run)?;
            imported_runs += 1;
        }
    }
    Ok(Output {
        text: format!(
            "imported {} case(s) ({skipped} already present), {imported_runs} run(s).\n\
             The source files are now redundant — the database is the source of truth and the \
             dump backup covers it. The query-embedding cache stays where it is (derived data).\n",
            to_add.len()
        ),
        ..Output::default()
    })
}

/// A file's text, or `None` when it is not there.
fn read(path: &Path) -> Result<Option<String>> {
    match std::fs::read(path) {
        Ok(bytes) => String::from_utf8(bytes).map(Some).map_err(|_| {
            Error::refused(format!(
                "{} is not UTF-8 text. Nothing was imported.",
                path.display()
            ))
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::refused(format!(
            "cannot read {}: {e}. Nothing was imported.",
            path.display()
        ))),
    }
}

/// The non-blank lines of a file with their 1-based numbers.
fn lines(text: &str) -> impl Iterator<Item = (usize, &str)> {
    text.lines()
        .enumerate()
        .map(|(i, l)| (i + 1, l))
        .filter(|(_, l)| !l.trim().is_empty())
}

fn object(at: &str, line: &str) -> Result<serde_json::Map<String, Value>> {
    match serde_json::from_str::<Value>(line) {
        Ok(Value::Object(o)) => Ok(o),
        Ok(_) => Err(Error::refused(format!(
            "{at}: not a JSON object. Nothing was imported."
        ))),
        Err(e) => Err(Error::refused(format!(
            "{at}: not valid JSON: {e}. Nothing was imported."
        ))),
    }
}

fn parse_case(at: &str, line: &str) -> Result<EvalCase> {
    let o = object(at, line)?;
    let text = |key: &str| -> Result<String> {
        match o.get(key) {
            Some(Value::String(s)) => Ok(s.clone()),
            _ => Err(Error::refused(format!(
                "{at}: case {}: '{key}' is missing or not a string. Nothing was imported.",
                o.get("id").and_then(Value::as_str).unwrap_or("?")
            ))),
        }
    };
    let id = text("id")?;
    let table = match text("table")?.as_str() {
        "semantic" => Table::Semantic,
        "episodic" => Table::Episodic,
        other => {
            return Err(Error::refused(format!(
                "{at}: case {id}: table '{other}' is neither semantic nor episodic. \
                 Nothing was imported."
            )));
        }
    };
    let expect: Vec<String> = match o.get("expect") {
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| match v {
                Value::Number(n) => Ok(n.to_string()),
                _ => Err(Error::refused(format!(
                    "{at}: case {id}: 'expect' must hold integer ids. Nothing was imported."
                ))),
            })
            .collect::<Result<_>>()?,
        _ => Vec::new(),
    };
    if expect.is_empty() {
        return Err(Error::refused(format!(
            "{at}: case {id}: 'expect' is missing or empty. Nothing was imported."
        )));
    }
    let (class, query) = (text("class")?, text("query")?);
    // The same checks as `--add-case`, so an imported case is one that could
    // have been authored.
    let parsed = cases::parse(&NewCase {
        id: &id,
        class: &class,
        table,
        query: &query,
        expect: &expect.join(","),
    })
    .map_err(|e| Error::refused(format!("{at}: case {id}: {e}")))?;
    Ok(EvalCase {
        id: parsed.id().to_string(),
        class: parsed.class().to_string(),
        table,
        query: parsed.query().to_string(),
        expect: parsed.expect().to_vec(),
        stamps: Vec::new(),
        retired: false,
    })
}

fn parse_run(at: &str, line: &str) -> Result<EvalRun> {
    let o = object(at, line)?;
    let missing = |key: &str| {
        Error::refused(format!(
            "{at}: '{key}' is missing or of the wrong type. Nothing was imported."
        ))
    };
    let real = |key: &str| {
        o.get(key)
            .and_then(Value::as_f64)
            .ok_or_else(|| missing(key))
    };
    Ok(EvalRun {
        ran_at: Some(
            o.get("date")
                .and_then(Value::as_str)
                .ok_or_else(|| missing("date"))?
                .to_string(),
        ),
        alpha: real("alpha")?,
        embed_model: o
            .get("embed_model")
            .and_then(Value::as_str)
            .ok_or_else(|| missing("embed_model"))?
            .to_string(),
        n_cases: o
            .get("n_cases")
            .and_then(Value::as_i64)
            .ok_or_else(|| missing("n_cases"))?,
        r1: real("r1")?,
        r5: real("r5")?,
        mrr: real("mrr")?,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::config::Config;
    use crate::eval::testing::*;
    use crate::eval::{Mode, Output};
    use crate::store::turso::TursoStore;
    use crate::store::turso::tests::rows;
    use crate::store::{Cell, Store, Table};

    fn dir(name: &str, cases: Option<&str>, history: Option<&str>) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sm-evalimport-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        if let Some(c) = cases {
            std::fs::write(d.join("eval.jsonl"), c).unwrap();
        }
        if let Some(h) = history {
            std::fs::write(d.join("history.jsonl"), h).unwrap();
        }
        d
    }

    fn import(cfg: &Config, d: &std::path::Path) -> crate::error::Result<Output> {
        exec(cfg, Mode::Import(d.to_path_buf()))
    }

    fn case_line(id: &str, table: &str, expect: &[i64]) -> String {
        format!(
            r#"{{"id": "{id}", "class": "semantic", "table": "{table}", "query": "query {id}", "expect": {expect:?}}}"#
        )
    }

    const HISTORY: &str = concat!(
        r#"{"date": "2026-08-18", "alpha": 0.15, "embed_model": "bge-m3", "n_cases": 2, "r1": 0.5, "r5": 1.0, "mrr": 0.75}"#,
        "\n",
        r#"{"date": "2026-08-20", "alpha": 0.2, "embed_model": "bge-m3", "n_cases": 2, "r1": 1.0, "r5": 1.0, "mrr": 1.0}"#,
        "\n"
    );

    #[test]
    fn legacy_cases_and_runs_land_in_the_database_with_stamps_read_from_the_rows() {
        let (cfg, ids) = db("import-ok");
        restamp(&cfg, Table::Semantic, ids[1], "2026-04-05 06:07:08");
        let cases = format!(
            "{}\n\n{}\n",
            case_line("s01", "semantic", &[ids[0], ids[1]]),
            case_line("s02", "semantic", &[ids[2]])
        );
        let d = dir("ok", Some(&cases), Some(HISTORY));
        let out = import(&cfg, &d).unwrap();
        assert!(
            out.text
                .starts_with("imported 2 case(s) (0 already present), 2 run(s).\n"),
            "{}",
            out.text
        );

        let store = TursoStore::open(&cfg).unwrap();
        let c = store.eval_case("s01").unwrap().unwrap();
        assert_eq!(c.expect, vec![ids[0], ids[1]]);
        assert_eq!(c.stamps, ["2026-01-01 00:00:00", "2026-04-05 06:07:08"]);
        assert_eq!(
            (c.class.as_str(), c.table, c.query.as_str()),
            ("semantic", Table::Semantic, "query s01")
        );
        assert_eq!(
            rows(
                &cfg,
                "SELECT ran_at, alpha, embed_model, n_cases, r1, r5, mrr FROM eval_runs ORDER BY id;"
            ),
            vec![
                vec![
                    Cell::Text("2026-08-18".into()),
                    Cell::Real(0.15),
                    Cell::Text("bge-m3".into()),
                    Cell::Int(2),
                    Cell::Real(0.5),
                    Cell::Real(1.0),
                    Cell::Real(0.75)
                ],
                vec![
                    Cell::Text("2026-08-20".into()),
                    Cell::Real(0.2),
                    Cell::Text("bge-m3".into()),
                    Cell::Int(2),
                    Cell::Real(1.0),
                    Cell::Real(1.0),
                    Cell::Real(1.0)
                ],
            ]
        );
        assert_eq!(exec(&cfg, Mode::Validate).unwrap().failure, None);
    }

    #[test]
    fn importing_twice_adds_nothing() {
        let (cfg, ids) = db("import-twice");
        let cases = format!("{}\n", case_line("s01", "semantic", &[ids[0]]));
        let d = dir("twice", Some(&cases), Some(HISTORY));
        import(&cfg, &d).unwrap();
        let out = import(&cfg, &d).unwrap();
        assert!(
            out.text
                .starts_with("imported 0 case(s) (1 already present), 0 run(s).\n"),
            "{}",
            out.text
        );
        assert_eq!(count(&cfg, "SELECT count(*) FROM eval_cases;"), 1);
        assert_eq!(count(&cfg, "SELECT count(*) FROM eval_runs;"), 2);
    }

    #[test]
    fn a_label_repeated_in_the_file_is_imported_once() {
        let (cfg, ids) = db("import-repeat");
        let line = case_line("s01", "semantic", &[ids[0]]);
        let d = dir("repeat", Some(&format!("{line}\n{line}\n")), None);
        let out = import(&cfg, &d).unwrap();
        assert!(
            out.text
                .starts_with("imported 1 case(s) (1 already present), 0 run(s).\n"),
            "{}",
            out.text
        );
    }

    #[test]
    fn the_history_file_is_optional() {
        let (cfg, ids) = db("import-no-history");
        let d = dir(
            "nohist",
            Some(&format!("{}\n", case_line("s01", "semantic", &[ids[0]]))),
            None,
        );
        let out = import(&cfg, &d).unwrap();
        assert!(
            out.text.contains("1 case(s)") && out.text.contains("0 run(s)"),
            "{}",
            out.text
        );
    }

    #[test]
    fn a_directory_without_cases_is_refused() {
        let (cfg, _) = db("import-no-cases");
        let d = dir("nocases", None, Some(HISTORY));
        let msg = refusal(import(&cfg, &d));
        assert!(msg.contains("eval.jsonl"), "{msg}");
        assert_eq!(
            count(&cfg, "SELECT count(*) FROM eval_runs;"),
            0,
            "not even the history"
        );
    }

    #[test]
    fn a_file_that_cannot_be_imported_is_refused_whole_and_nothing_is_written() {
        let (cfg, ids) = db("import-refusals");
        let good = case_line("s01", "semantic", &[ids[0]]);
        let bad_inputs = [
            (case_line("s02", "semantic", &[9999]), "9999"),
            (case_line("s02", "instructions", &[ids[0]]), "instructions"),
            ("{ not json".to_string(), "line 2"),
            (
                format!(
                    r#"{{"id": "s02", "class": "semantic", "table": "semantic", "query": "", "expect": [{}]}}"#,
                    ids[0]
                ),
                "query",
            ),
            (
                r#"{"id": "s02", "class": "semantic", "table": "semantic", "query": "q", "expect": []}"#
                    .to_string(),
                "expect",
            ),
            (r#"{"id": "s02"}"#.to_string(), "s02"),
        ];
        for (n, (line, needle)) in bad_inputs.iter().enumerate() {
            let d = dir(
                &format!("bad{n}"),
                Some(&format!("{good}\n{line}\n")),
                Some(HISTORY),
            );
            let msg = refusal(import(&cfg, &d));
            assert!(msg.contains(needle), "{needle}: {msg}");
            assert_eq!(
                count(&cfg, "SELECT count(*) FROM eval_cases;"),
                0,
                "{needle}: the valid case before it was not imported either"
            );
            assert_eq!(
                count(&cfg, "SELECT count(*) FROM eval_runs;"),
                0,
                "{needle}"
            );
        }
    }

    #[test]
    fn a_malformed_history_line_is_refused_before_anything_is_written() {
        let (cfg, ids) = db("import-bad-history");
        let d = dir(
            "badhist",
            Some(&format!("{}\n", case_line("s01", "semantic", &[ids[0]]))),
            Some("{\"date\": \"2026-08-18\"}\n"),
        );
        let msg = refusal(import(&cfg, &d));
        assert!(msg.contains("history.jsonl"), "{msg}");
        assert_eq!(count(&cfg, "SELECT count(*) FROM eval_cases;"), 0);
    }
}
