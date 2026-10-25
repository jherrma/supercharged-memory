//! `--staleness` and `--verify-candidates`: what deep sleep reviews by age.
//!
//! Both read the active rows and decide nothing. Staleness is the age profile
//! a pass reports next to its verified-stale count; the verify list is the rows
//! that quote something checkable -- a path, a flag, an environment variable, a
//! version, one of this binary's commands -- oldest first, for workers to check
//! against `<exe> <cmd> --help` and the machine. Neither needs Ollama.
//!
//! There is deliberately no retrieval-frequency signal: ranking by how often a
//! row is recalled would make `recall` a writer on every query, and keeping it
//! a pure reader was the better trade. Age plus artifact density is the proxy.

use crate::error::Result;
use crate::json::Json;
use crate::status;
use crate::store::{ActiveRow, Store, Table};

/// The age buckets, youngest first.
pub const BUCKETS: [&str; 4] = ["0-30d", "30-90d", "90-180d", "180d+"];

/// The bucket for an age in days. An age the database could not compute falls
/// in the oldest bucket, as the reference's `CASE … ELSE` put it there.
pub fn bucket(age_days: Option<f64>) -> &'static str {
    match age_days {
        Some(d) if d < 30.0 => BUCKETS[0],
        Some(d) if d < 90.0 => BUCKETS[1],
        Some(d) if d < 180.0 => BUCKETS[2],
        _ => BUCKETS[3],
    }
}

/// `--staleness`' JSON: the age profile of the active rows, and how many
/// active rows of each memory table have no embedding -- the per-pass view of
/// the count `status` reports every session. Reported, never stored.
pub fn staleness(store: &impl Store, table: Table) -> Result<Json> {
    let rows = store.active_rows(table)?;
    let mut counts = [0usize; BUCKETS.len()];
    for r in &rows {
        let b = bucket(r.age_days);
        counts[BUCKETS
            .iter()
            .position(|x| *x == b)
            .expect("a known bucket")] += 1;
    }
    let oldest = rows
        .iter()
        .map(|r| r.created_at.as_str())
        .min()
        .unwrap_or("");
    // The memory tables only: the instructions are outside sleep.
    let unembedded = status::unembedded_in(store, &[Table::Semantic, Table::Episodic])?;
    Ok(Json::object([
        ("table", Json::from(table.as_str())),
        ("n_current", Json::from(rows.len())),
        (
            "buckets",
            Json::object(BUCKETS.iter().zip(counts).map(|(b, n)| (*b, Json::from(n)))),
        ),
        ("oldest_current", Json::from(oldest)),
        (
            "n_without_embedding",
            Json::object(
                unembedded
                    .into_iter()
                    .map(|(t, n)| (t, Json::Int(n.try_into().unwrap_or(i64::MAX)))),
            ),
        ),
        (
            "note",
            Json::from(
                "age is not wrongness — an old row about a stable fact is fine. Read this \
                 next to D7's stale count, not alone.",
            ),
        ),
    ]))
}

/// Every artifact class, in the order a candidate lists them. The first four
/// are the reference's; `commands` and `python` name what changed with the
/// binary: its own subcommands, and the scripts it replaced.
pub const CLASSES: [&str; 6] = ["paths", "flags", "env", "versions", "commands", "python"];

/// What counts as naming this binary's interface: each subcommand with its
/// long flags, and every key the `.env` may hold.
#[derive(Debug, Clone)]
pub struct Vocabulary {
    pub commands: Vec<(String, Vec<String>)>,
    pub keys: Vec<&'static str>,
}

impl Vocabulary {
    /// Read off the CLI definition and the config keys, so a new subcommand,
    /// flag or key is matched without anyone remembering to list it here.
    pub fn of_this_binary() -> Vocabulary {
        let cli = <crate::cli::Cli as clap::CommandFactory>::command();
        Vocabulary {
            commands: cli
                .get_subcommands()
                .filter(|c| c.get_name() != "help")
                .map(|c| {
                    let flags = c
                        .get_arguments()
                        .filter_map(|a| a.get_long())
                        .map(str::to_string)
                        .collect();
                    (c.get_name().to_string(), flags)
                })
                .collect(),
            keys: crate::config::KEYS.to_vec(),
        }
    }
}

// The reference's four patterns ran as SQL `LIKE` and `GLOB` over the stored
// text; these are the same tests, byte for byte. Every character they look
// for is ASCII, which never occurs inside a multi-byte UTF-8 sequence.

/// A repo path the reference looked for. Case-insensitive, as SQL `LIKE`.
fn names_a_repo_path(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "scripts/",
        "instructions/",
        "investigations/",
        "migration-steps/",
    ]
    .iter()
    .any(|p| lower.contains(p))
}

/// `--` followed by a lowercase letter: a flag, not a prose dash.
fn names_a_flag(text: &str) -> bool {
    text.as_bytes()
        .windows(3)
        .any(|w| w[0] == b'-' && w[1] == b'-' && w[2].is_ascii_lowercase())
}

/// Two capitals, an underscore, a capital: `RECALL_ALPHA`, not `ALWAYS`.
fn names_an_env_var(text: &str) -> bool {
    text.as_bytes().windows(4).any(|w| {
        w[0].is_ascii_uppercase()
            && w[1].is_ascii_uppercase()
            && w[2] == b'_'
            && w[3].is_ascii_uppercase()
    })
}

/// Two dots between digits, or a `v` before one: `0.7.2`, `v1.2` -- not a
/// decimal like `0.96875`.
fn names_a_version(text: &str) -> bool {
    let b = text.as_bytes();
    b.windows(5).any(|w| {
        w[0].is_ascii_digit()
            && w[1] == b'.'
            && w[2].is_ascii_digit()
            && w[3] == b'.'
            && w[4].is_ascii_digit()
    }) || b
        .windows(4)
        .any(|w| w[0] == b'v' && w[1].is_ascii_digit() && w[2] == b'.' && w[3].is_ascii_digit())
}

/// A byte that continues a command or flag name.
fn continues_a_name(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

/// `word` at the start of `s`, ending there: `status` in `status first`, not
/// in `statuses`.
fn starts_with_word(s: &str, word: &str) -> bool {
    s.strip_prefix(word)
        .is_some_and(|rest| rest.bytes().next().is_none_or(|b| !continues_a_name(b)))
}

/// One of this binary's commands: after the binary's own name
/// (`supercharged-memory status`, `.exe`, quoted), or bare but followed by one
/// of that command's own flags (`embed --missing`). The bare form is held to
/// the real flags so `git status --short` is not taken for ours.
fn names_a_command(text: &str, vocab: &Vocabulary) -> bool {
    const NAME: &str = "supercharged-memory";
    let after_the_binary = text.match_indices(NAME).any(|(at, _)| {
        let rest = &text[at + NAME.len()..];
        let rest = rest.strip_prefix(".exe").unwrap_or(rest);
        let rest = rest.strip_prefix(['"', '\'']).unwrap_or(rest);
        let command = rest.trim_start_matches(' ');
        command.len() < rest.len()
            && vocab
                .commands
                .iter()
                .any(|(c, _)| starts_with_word(command, c))
    });
    after_the_binary
        || vocab.commands.iter().any(|(command, flags)| {
            text.match_indices(command.as_str()).any(|(at, _)| {
                let before = text[..at].bytes().next_back();
                if before.is_some_and(|b| continues_a_name(b) || b == b'.' || b == b'/') {
                    return false;
                }
                text[at + command.len()..]
                    .strip_prefix(" --")
                    .is_some_and(|rest| flags.iter().any(|f| starts_with_word(rest, f)))
            })
        })
}

/// `python3`, or a `.py` file name: what the binary replaced, so a row naming
/// either predates it.
fn names_python(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("python3")
        || lower.match_indices(".py").any(|(at, _)| {
            let b = lower.as_bytes();
            at > 0
                && continues_a_name(b[at - 1])
                && b.get(at + 3)
                    .is_none_or(|n| !(n.is_ascii_alphanumeric() || *n == b'_'))
        })
}

/// The classes `text` mentions, in `CLASSES` order.
pub fn artifact_classes(text: &str, vocab: &Vocabulary) -> Vec<&'static str> {
    let present = [
        names_a_repo_path(text),
        names_a_flag(text),
        names_an_env_var(text) || vocab.keys.iter().any(|k| text.contains(k)),
        names_a_version(text),
        names_a_command(text, vocab),
        names_python(text),
    ];
    CLASSES
        .iter()
        .zip(present)
        .filter_map(|(class, yes)| yes.then_some(*class))
        .collect()
}

/// `--verify-candidates`' JSON. A `limit` of 0 lists every candidate.
///
/// Oldest first, since staleness risk grows with age; the number of classes a
/// row mentions breaks a tie. The worker reads each row itself -- the list
/// carries ids and classes, never the text.
pub fn verify_candidates(store: &impl Store, table: Table, limit: u32) -> Result<Json> {
    let vocab = Vocabulary::of_this_binary();
    let rows = store.active_rows(table)?;
    let mut candidates: Vec<(&ActiveRow, Vec<&str>)> = rows
        .iter()
        .map(|r| (r, artifact_classes(&r.memory_text, &vocab)))
        .filter(|(_, classes)| !classes.is_empty())
        .collect();
    // Stable, over rows already in (created_at, id) order.
    candidates.sort_by(|(a, ca), (b, cb)| {
        a.created_at
            .cmp(&b.created_at)
            .then(cb.len().cmp(&ca.len()))
    });
    let n_candidates = candidates.len();
    let shown = match limit {
        0 => n_candidates,
        n => n_candidates.min(n as usize),
    };
    let listed = candidates[..shown]
        .iter()
        .map(|(r, classes)| {
            Json::object([
                ("id", Json::Int(r.id)),
                ("topic", Json::from(r.topic.clone().unwrap_or_default())),
                ("created_at", Json::from(r.created_at.as_str())),
                // Truncated toward zero, as SQL's CAST(… AS INTEGER) did.
                ("age_days", Json::Int(r.age_days.map_or(0, |d| d as i64))),
                ("artifacts", Json::from(classes.clone())),
                ("artifact_classes", Json::from(classes.len())),
            ])
        })
        .collect();
    Ok(Json::object([
        ("table", Json::from(table.as_str())),
        ("n_current", Json::from(rows.len())),
        ("n_candidates", Json::from(n_candidates)),
        ("n_no_artifacts", Json::from(rows.len() - n_candidates)),
        ("limit", Json::Int(i64::from(limit))),
        ("candidates", Json::Array(listed)),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::sleep::{Mode, execute};
    use crate::store::Cell;
    use crate::store::turso::tests::{put, rows, scratch};

    fn run(cfg: &Config, mode: Mode) -> String {
        execute(cfg, &mode, &mut std::io::empty()).unwrap()
    }

    fn json(out: &str) -> serde_json::Value {
        serde_json::from_str(out).unwrap_or_else(|e| panic!("{e}: {out}"))
    }

    fn text(cfg: &Config, sql: &str) -> String {
        match &rows(cfg, sql)[0][0] {
            Cell::Text(s) => s.clone(),
            Cell::Int(n) => n.to_string(),
            other => panic!("{other:?}"),
        }
    }

    fn date(cfg: &Config, table: Table, id: i64, days_ago: u32) {
        rows(
            cfg,
            &format!(
                "UPDATE {}_memory SET created_at = datetime('now', '-{days_ago} days') \
                 WHERE id = {id};",
                table.as_str()
            ),
        );
    }

    // ---- --staleness --------------------------------------------------------

    #[test]
    fn buckets_split_at_30_90_and_180_days() {
        for (age, want) in [
            (Some(-3.0), "0-30d"),
            (Some(0.0), "0-30d"),
            (Some(29.999), "0-30d"),
            (Some(30.0), "30-90d"),
            (Some(89.99), "30-90d"),
            (Some(90.0), "90-180d"),
            (Some(179.9), "90-180d"),
            (Some(180.0), "180d+"),
            (Some(4000.0), "180d+"),
            (None, "180d+"),
        ] {
            assert_eq!(bucket(age), want, "{age:?}");
        }
    }

    /// The age profile of current rows, plus the unembedded count per memory
    /// table -- active rows only, as `status` counts them.
    #[test]
    fn staleness_profiles_current_rows_and_counts_the_unembedded() {
        let cfg = scratch("staleness");
        let ids: Vec<i64> = [5, 45, 100, 120, 400]
            .iter()
            .enumerate()
            .map(|(i, days)| {
                let id = put(&cfg, Table::Semantic, &format!("row {i}"), i % 2 == 0);
                date(&cfg, Table::Semantic, id, *days);
                id
            })
            .collect();
        let old = put(&cfg, Table::Semantic, "superseded, unembedded", false);
        date(&cfg, Table::Semantic, old, 1000);
        rows(
            &cfg,
            &format!(
                "UPDATE semantic_memory SET superseded_by = {} WHERE id = {old};",
                ids[0]
            ),
        );
        put(&cfg, Table::Episodic, "blind event", false);
        put(&cfg, Table::Episodic, "embedded event", true);

        let oldest = text(
            &cfg,
            &format!(
                "SELECT created_at FROM semantic_memory WHERE id = {};",
                ids[4]
            ),
        );
        let out = run(
            &cfg,
            Mode::Staleness {
                table: Table::Semantic,
            },
        );
        assert_eq!(
            out,
            format!(
                "{{\n \"table\": \"semantic\",\n \"n_current\": 5,\n \"buckets\": {{\n  \
                 \"0-30d\": 1,\n  \"30-90d\": 1,\n  \"90-180d\": 2,\n  \"180d+\": 1\n }},\n \
                 \"oldest_current\": \"{oldest}\",\n \"n_without_embedding\": {{\n  \
                 \"semantic\": 2,\n  \"episodic\": 1\n }},\n \"note\": \"age is not wrongness \
                 \\u2014 an old row about a stable fact is fine. Read this next to D7's stale \
                 count, not alone.\"\n}}"
            )
        );
    }

    /// The unembedded count stays per memory table: an instruction without a
    /// vector is `status`'s and `embed --missing`'s business, not sleep's.
    #[test]
    fn staleness_never_counts_the_instructions() {
        let cfg = scratch("staleness-instructions");
        put(&cfg, Table::Semantic, "blind fact", false);
        put(&cfg, Table::Instructions, "blind directive", false);
        let v = json(&run(
            &cfg,
            Mode::Staleness {
                table: Table::Semantic,
            },
        ));
        assert_eq!(
            v["n_without_embedding"],
            serde_json::json!({"semantic": 1, "episodic": 0})
        );
    }

    #[test]
    fn episodic_staleness_covers_every_row() {
        let cfg = scratch("staleness-episodic");
        let a = put(&cfg, Table::Episodic, "a", true);
        put(&cfg, Table::Episodic, "b", true);
        rows(
            &cfg,
            "UPDATE episodic_memory SET processed_at = '2026-01-02';",
        );
        date(&cfg, Table::Episodic, a, 60);
        let v = json(&run(
            &cfg,
            Mode::Staleness {
                table: Table::Episodic,
            },
        ));
        assert_eq!(v["table"], "episodic");
        assert_eq!(v["n_current"], 2);
        assert_eq!(v["buckets"]["30-90d"], 1);
        assert_eq!(v["buckets"]["180d+"], 1, "2026-01-01 is over 180 days old");
        assert_eq!(
            v["n_without_embedding"],
            serde_json::json!({"semantic": 0, "episodic": 0})
        );
    }

    #[test]
    fn an_empty_table_has_zero_buckets_and_no_oldest_row() {
        let cfg = scratch("staleness-empty");
        let v = json(&run(
            &cfg,
            Mode::Staleness {
                table: Table::Semantic,
            },
        ));
        assert_eq!(v["n_current"], 0);
        assert_eq!(
            v["buckets"],
            serde_json::json!({"0-30d": 0, "30-90d": 0, "90-180d": 0, "180d+": 0})
        );
        assert_eq!(v["oldest_current"], "");
    }

    // ---- artifact classes ---------------------------------------------------

    /// The reference computed the four classes in SQL; these strings are run
    /// through those exact predicates and through the port.
    #[test]
    fn the_reference_classes_match_their_sql_predicates() {
        let cfg = scratch("classes-parity");
        type Port = fn(&str) -> bool;
        let predicates: [(&str, Port); 4] = [
            (
                "memory_text LIKE '%scripts/%' OR memory_text LIKE '%instructions/%' \
                 OR memory_text LIKE '%investigations/%' OR memory_text LIKE '%migration-steps/%'",
                names_a_repo_path,
            ),
            ("memory_text GLOB '*--[a-z]*'", names_a_flag),
            ("memory_text GLOB '*[A-Z][A-Z]_[A-Z]*'", names_an_env_var),
            (
                "memory_text GLOB '*[0-9].[0-9].[0-9]*' OR memory_text GLOB '*v[0-9].[0-9]*'",
                names_a_version,
            ),
        ];
        let samples = [
            "",
            "see scripts/recall.py",
            "SCRIPTS/Recall.PY",
            "Instructions/SLEEP.md",
            "investigations/2026-08-18",
            "migration-steps/x.md",
            "migrationsteps/ and scripts",
            "a -- b",
            "--Flag",
            "--flag",
            "x---a",
            "—a –b",
            "ALWAYS NEVER WHAT WAS ACTUALLY BAD",
            "RECALL_ALPHA",
            "A_B",
            "AB_c",
            "XY_Z",
            "ÄB_C",
            "aXY_Zb",
            "0.96875",
            "1.2.3",
            "v1.2",
            "V1.2",
            "v1.",
            "1.2",
            "über 3.4.5",
            "Turso 0.7.2",
            "a.b.c",
            "1..2.3",
            "line one\n--x\nRECALL_ALPHA",
            "%_ wildcards _% in text",
            "ümlaut ß v9.0 and --ä",
        ];
        for (sql, port) in predicates {
            for s in samples {
                let id = put(&cfg, Table::Semantic, s, false);
                let want = rows(
                    &cfg,
                    &format!("SELECT ({sql}) FROM semantic_memory WHERE id = {id};"),
                )[0][0]
                    == Cell::Int(1);
                assert_eq!(port(s), want, "{sql} on {s:?}");
            }
        }
    }

    fn vocab() -> Vocabulary {
        Vocabulary::of_this_binary()
    }

    #[test]
    fn the_vocabulary_is_read_off_the_cli_and_the_config_keys() {
        let v = vocab();
        let flags = |cmd: &str| {
            v.commands
                .iter()
                .find(|(c, _)| c == cmd)
                .map(|(_, f)| f.clone())
                .unwrap_or_else(|| panic!("{cmd} missing"))
        };
        assert!(flags("embed").contains(&"missing".to_string()));
        assert!(flags("sleep").contains(&"confirm-purge".to_string()));
        assert!(flags("status").is_empty());
        assert!(!v.commands.iter().any(|(c, _)| c == "help"));
        assert!(v.keys.contains(&"EMBEDDINGS"));
        assert_eq!(v.keys.len(), crate::config::KEYS.len());
    }

    #[test]
    fn commands_are_this_binarys_subcommands_and_their_flags() {
        let v = vocab();
        for yes in [
            "supercharged-memory status",
            "run `supercharged-memory recall --topics` first",
            "\"/x y/supercharged-memory\" backup",
            "C:/Tools/supercharged-memory.exe   init -y",
            "embed --missing",
            "then sleep --purge 3 --confirm-purge",
            "(recall --k 3)",
            "supercharged-memory find-existing-memory",
        ] {
            assert!(artifact_classes(yes, &v).contains(&"commands"), "{yes:?}");
        }
        for no in [
            "supercharged-memory-backup.sh",
            "the supercharged-memory repo",
            "supercharged-memory: README",
            "supercharged-memory statuses",
            "git status --short",
            "git restore --staged x",
            "git config --global",
            "recall.py --status",
            "deep-sleep --purge",
            "recall --topicsx",
            "sleep --pur",
            "status",
        ] {
            assert!(!artifact_classes(no, &v).contains(&"commands"), "{no:?}");
        }
    }

    /// Rows naming the scripts the binary replaced are exactly the stale ones
    /// a verify pass exists to find.
    #[test]
    fn python_names_the_scripts_and_the_interpreter() {
        let v = vocab();
        for yes in [
            "python3 scripts/recall.py",
            "PYTHON3 -I x",
            "memlib.py",
            "see recall.py.",
            "x.py",
            "run_eval.py --x",
        ] {
            assert!(artifact_classes(yes, &v).contains(&"python"), "{yes:?}");
        }
        for no in [
            "the Python reference",
            "pyproject.toml",
            "foo.pyc",
            "a .py file",
            "copy.pyx",
        ] {
            assert!(!artifact_classes(no, &v).contains(&"python"), "{no:?}");
        }
    }

    #[test]
    fn env_also_names_a_dotenv_key_without_an_underscore() {
        let v = vocab();
        assert_eq!(artifact_classes("EMBEDDINGS=off", &v), vec!["env"]);
        assert_eq!(artifact_classes("RECALL_ALPHA", &v), vec!["env"]);
        assert!(artifact_classes("embeddings are optional", &v).is_empty());
    }

    #[test]
    fn classes_come_in_a_fixed_order() {
        assert_eq!(
            artifact_classes(
                "python3 scripts/sleep.py --purge needs RECALL_ALPHA 0.7.2; now sleep --purge",
                &vocab()
            ),
            CLASSES.to_vec()
        );
    }

    // ---- --verify-candidates ------------------------------------------------

    /// Rows written in the binary's vocabulary: not one of them matches the
    /// reference's four patterns, so the old heuristics found nothing here.
    const POST_REWRITE: [&str; 4] = [
        "Run `supercharged-memory status` first; DEGRADED means no embeddings.",
        "Set EMBEDDINGS=off in the .env beside the binary to stop it contacting Ollama.",
        "`/home/u/.local/bin/supercharged-memory recall` ranks by distance and keywords.",
        "C:/Tools/supercharged-memory.exe backup writes a gzipped dump.",
    ];

    #[test]
    fn the_post_rewrite_corpus_yields_candidates_the_reference_missed() {
        let cfg = scratch("verify-post-rewrite");
        let mut want = Vec::new();
        for text in POST_REWRITE {
            assert!(
                !(names_a_repo_path(text)
                    || names_a_flag(text)
                    || names_an_env_var(text)
                    || names_a_version(text)),
                "{text:?} already matched a reference class"
            );
            want.push(put(&cfg, Table::Semantic, text, false));
        }
        put(
            &cfg,
            Table::Semantic,
            "The user prefers short answers.",
            false,
        );
        let legacy = put(
            &cfg,
            Table::Semantic,
            "python3 scripts/recall.py prints READY n.",
            false,
        );
        want.push(legacy);

        let v = json(&run(
            &cfg,
            Mode::VerifyCandidates {
                table: Table::Semantic,
                limit: 0,
            },
        ));
        let mut got: Vec<i64> = v["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_i64().unwrap())
            .collect();
        got.sort();
        assert_eq!(got, want);
        assert_eq!(v["n_current"], 6);
        assert_eq!(v["n_candidates"], 5);
        assert_eq!(v["n_no_artifacts"], 1);
        let row = v["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == legacy)
            .unwrap();
        assert_eq!(row["artifacts"], serde_json::json!(["paths", "python"]));
        assert_eq!(row["artifact_classes"], 2);
    }

    /// The reference's layout, with a NULL topic printed as "".
    #[test]
    fn verify_candidates_prints_the_reference_layout() {
        let cfg = scratch("verify-layout");
        let id = put(&cfg, Table::Semantic, "use scripts/recall.py --k 3", false);
        put(&cfg, Table::Semantic, "nothing checkable", false);
        let age = text(
            &cfg,
            "SELECT CAST(julianday('now') - julianday('2026-01-01 00:00:00') AS INTEGER);",
        );
        let out = run(
            &cfg,
            Mode::VerifyCandidates {
                table: Table::Semantic,
                limit: 0,
            },
        );
        assert_eq!(
            out,
            format!(
                "{{\n \"table\": \"semantic\",\n \"n_current\": 2,\n \"n_candidates\": 1,\n \
                 \"n_no_artifacts\": 1,\n \"limit\": 0,\n \"candidates\": [\n  {{\n   \
                 \"id\": {id},\n   \"topic\": \"\",\n   \"created_at\": \"2026-01-01 00:00:00\",\n   \
                 \"age_days\": {age},\n   \"artifacts\": [\n    \"paths\",\n    \"flags\",\n    \
                 \"python\"\n   ],\n   \"artifact_classes\": 3\n  }}\n ]\n}}"
            )
        );
    }

    /// Oldest first; within one creation time, the row naming more classes.
    #[test]
    fn candidates_are_oldest_first_with_class_count_breaking_ties() {
        let cfg = scratch("verify-order");
        let young = put(&cfg, Table::Semantic, "scripts/a.py --x 1.2.3", false);
        let one = put(&cfg, Table::Semantic, "only --flags here", false);
        let three = put(&cfg, Table::Semantic, "RECALL_ALPHA --x 0.7.2", false);
        let old = put(&cfg, Table::Semantic, "an old --flag", false);
        let gone = put(&cfg, Table::Semantic, "superseded --flag", false);
        date(&cfg, Table::Semantic, young, 1);
        date(&cfg, Table::Semantic, old, 999);
        rows(
            &cfg,
            &format!(
                "UPDATE semantic_memory SET created_at = '2026-03-03 00:00:00' \
                 WHERE id IN ({one}, {three}, {gone});"
            ),
        );
        rows(
            &cfg,
            &format!("UPDATE semantic_memory SET superseded_by = {young} WHERE id = {gone};"),
        );
        let v = json(&run(
            &cfg,
            Mode::VerifyCandidates {
                table: Table::Semantic,
                limit: 0,
            },
        ));
        let ids: Vec<i64> = v["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_i64().unwrap())
            .collect();
        assert_eq!(ids, vec![old, three, one, young]);
        assert_eq!(v["n_current"], 4);

        let v = json(&run(
            &cfg,
            Mode::VerifyCandidates {
                table: Table::Semantic,
                limit: 2,
            },
        ));
        assert_eq!(v["limit"], 2);
        assert_eq!(v["n_candidates"], 4, "the count is before the limit");
        assert_eq!(v["candidates"].as_array().unwrap().len(), 2);
        assert_eq!(v["candidates"][0]["id"], old);
    }

    #[test]
    fn episodic_rows_can_be_verified_too() {
        let cfg = scratch("verify-episodic");
        let e = put(&cfg, Table::Episodic, "ran embed --missing", false);
        let v = json(&run(
            &cfg,
            Mode::VerifyCandidates {
                table: Table::Episodic,
                limit: 0,
            },
        ));
        assert_eq!(v["table"], "episodic");
        assert_eq!(v["candidates"][0]["id"], e);
        assert_eq!(
            v["candidates"][0]["artifacts"],
            serde_json::json!(["flags", "commands"])
        );
    }
}
