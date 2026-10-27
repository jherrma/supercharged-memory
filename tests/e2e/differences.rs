//! Every deliberate difference between the binary and the Python reference.
//!
//! A comparison either finds both outputs identical, or names the entries
//! below that explain the difference. An entry with an `undo` maps the
//! binary's run back to what the reference prints; the comparison then has to
//! match exactly, and each named entry has to change something, so an entry
//! cannot outlive the difference it describes. An entry without an `undo` is
//! asserted by the test that names it, as the new behaviour. A difference no
//! entry explains fails the suite, and so does an entry no test exercises.

use std::collections::BTreeSet;

use crate::harness::{Run, Side};

pub struct Difference {
    pub id: &'static str,
    /// The command, or the script on the reference's side, it concerns.
    pub command: &'static str,
    /// What the Python reference does.
    pub reference: &'static str,
    /// What the binary does instead.
    pub binary: &'static str,
    /// The change that made it, described in words.
    pub change: &'static str,
    /// Maps a run back to what the reference prints. Applied to both sides:
    /// most act on the binary's run only.
    pub undo: Option<fn(&mut Run, Side)>,
}

fn rust_only(run: &mut Run, side: Side, f: impl FnOnce(&mut Run)) {
    if side == Side::Rust {
        f(run)
    }
}

fn replace_all(run: &mut Run, side: Side, pairs: &[(&str, &str)]) {
    rust_only(run, side, |r| {
        for (binary, reference) in pairs {
            r.stdout = r.stdout.replace(binary, reference);
            r.stderr = r.stderr.replace(binary, reference);
        }
    })
}

fn drop_lines(text: &str, starting: &str) -> String {
    text.split_inclusive('\n')
        .filter(|l| !l.starts_with(starting))
        .collect()
}

/// Remove one top-level key from a JSON object printed with one-space
/// indentation (Python's `json.dumps(v, indent=1)`), fixing the comma of the
/// line before it when it was the last key.
pub fn drop_json_key(text: &str, key: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let head = format!(" \"{key}\": ");
    let Some(start) = lines.iter().position(|l| l.starts_with(&head)) else {
        return text.to_string();
    };
    let value = &lines[start][head.len()..];
    let mut end = start;
    if value == "{" || value == "[" {
        while end + 1 < lines.len()
            && !(lines[end].starts_with(" }") || lines[end].starts_with(" ]"))
        {
            end += 1;
        }
    }
    let was_last = !lines[end].ends_with(',');
    let mut out: Vec<String> = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        if (start..=end).contains(&i) {
            continue;
        }
        out.push(l.to_string());
    }
    if was_last && start > 0 {
        let prev = &mut out[start - 1];
        if let Some(stripped) = prev.strip_suffix(',') {
            *prev = stripped.to_string();
        }
    }
    out.join("\n")
}

pub const REFUSAL_EXIT: Difference = Difference {
    id: "refusal-exit-code",
    command: "every command",
    reference: "a refusal ends the script with sys.exit(message): exit status 1",
    binary: "a refusal exits 2, so a caller tells a guard (2) from a failure (1) and a usage error (64)",
    change: "the binary gave refusals an exit status of their own",
    undo: Some(|r, side| {
        rust_only(r, side, |r| {
            if r.code == Some(2) {
                r.code = Some(1)
            }
        })
    }),
};

pub const USAGE_EXIT: Difference = Difference {
    id: "usage-error",
    command: "every command",
    reference: "argparse prints its usage text and exits 2",
    binary: "clap prints `error: ...` and its usage and exits 64",
    change: "the command line moved from argparse to clap, with 64 as the usage-error status",
    undo: Some(|r, side| match side {
        Side::Rust if r.code == Some(64) && r.stderr.starts_with("error: ") => {
            r.code = Some(2);
            r.stderr = "<usage error>\n".into();
        }
        Side::Python if r.code == Some(2) && r.stderr.contains(": error: ") => {
            r.stderr = "<usage error>\n".into();
        }
        _ => {}
    }),
};

pub const REFUSAL_PREFIX: Difference = Difference {
    id: "refusal-prefix",
    command: "every command needing the database; recall without a query",
    reference: "a few refusals print their message bare (`memory DB missing — ...`, `provide a query, ...`)",
    binary: "every refusal starts with `refused: `, the prefix the runbooks parse",
    change: "the binary prefixes every refusal uniformly",
    undo: Some(|r, side| {
        rust_only(r, side, |r| {
            if let Some(rest) = r.stderr.strip_prefix("refused: ") {
                r.stderr = rest.to_string();
            }
        })
    }),
};

pub const STATUS_REASON: Difference = Difference {
    id: "status-names-the-reason",
    command: "status (recall.py --status)",
    reference: "prints `DEGRADED n` alone",
    binary: "adds `no embeddings: <reason> — ...` (Ollama unreachable, the model not pulled, or EMBEDDINGS=off)",
    change: "embeddings became optional, and every degradation names its cause",
    undo: Some(|r, side| {
        rust_only(r, side, |r| {
            r.stdout = drop_lines(&r.stdout, "no embeddings: ")
        })
    }),
};

pub const STATUS_UNEMBEDDED: Difference = Difference {
    id: "status-counts-unembedded-rows",
    command: "status (recall.py --status)",
    reference: "says nothing about rows without a vector",
    binary: "adds `unembedded: semantic a, episodic b, instructions c — run embed --missing`",
    change: "rows can be stored without a vector and filled in later by `embed --missing`",
    undo: Some(|r, side| {
        rust_only(r, side, |r| {
            r.stdout = drop_lines(&r.stdout, "unembedded: ")
        })
    }),
};

pub const STATUS_ERROR_TEXT: Difference = Difference {
    id: "status-error-message",
    command: "status (recall.py --status)",
    reference: "`ERROR` followed by the tursodb CLI's diagnostic (`Error: Corrupt database: ...`)",
    binary: "`ERROR` followed by the engine's own message",
    change: "the binary opens the database through the engine library, not the tursodb CLI",
    undo: Some(|r, _| {
        if let Some(rest) = r.stdout.strip_prefix("ERROR ") {
            let tail = rest.split_once('\n').map_or("", |(_, t)| t);
            r.stdout = format!("ERROR <message>\n{tail}");
        }
    }),
};

pub const DEGRADED_WRITE: Difference = Difference {
    id: "write-without-embedding",
    command: "remember",
    reference: "refuses when Ollama does not answer: `refused: Ollama not reachable ...`",
    binary: "stores the row without a vector, says `stored ... WITHOUT embedding` on stdout and why on a `warning:` line",
    change: "embeddings became optional; `embed --missing` fills the vector in later",
    undo: None,
};

pub const RECALL_UNEMBEDDED: Difference = Difference {
    id: "recall-lists-unembedded-rows",
    command: "recall",
    reference: "leaves rows without a vector out of a search, but counts them in the keyword IDF",
    binary: "ranks them by keyword in a section of their own per table, and weighs the main ranking's keywords over the rows it ranks",
    change: "rows can be stored without a vector; a search must still find them",
    undo: Some(|r, side| {
        rust_only(r, side, |r| {
            let mut out = String::new();
            let mut skipping = false;
            for line in r.stdout.split_inclusive('\n') {
                if line.starts_with("===== ") {
                    skipping = line.contains(" without embedding (keyword-only) =====");
                }
                if !skipping {
                    out.push_str(line);
                }
            }
            r.stdout = out;
        })
    }),
};

pub const COMMAND_NAMES: Difference = Difference {
    id: "messages-name-subcommands",
    command: "recall --baseline, remember, sleep --purge, find-existing-memory, eval --report",
    reference: "messages name the scripts (`recall.py --status`, `sleep.py --staleness`, `remember.py`, `scripts/supercharged-memory-backup.sh`)",
    binary: "messages name the binary's subcommands",
    change: "the binary replaced the scripts, so its texts point at its own commands",
    undo: Some(|r, side| {
        replace_all(
            r,
            side,
            &[
                (
                    "check `supercharged-memory status` and `--candidates`",
                    "check `recall.py --status` and `recall.py --candidates`",
                ),
                ("in sleep --staleness's", "in sleep.py --staleness's"),
                (
                    "Run backup first — a purge has no other undo.",
                    "Run scripts/supercharged-memory-backup.sh first — a purge has no other undo.",
                ),
                ("remember appends", "remember.py appends"),
                (
                    "shipped recall @ RECALL_ALPHA",
                    "shipped recall.py @ RECALL_ALPHA",
                ),
                (
                    "or use --baseline / --topics / --candidates / --count",
                    "or use --baseline / --status / --count",
                ),
            ],
        )
    }),
};

pub const PURGE_GATE: Difference = Difference {
    id: "purge-gate-reads-the-wal",
    command: "sleep --purge",
    reference: "needs a dump newer than the database's main file",
    binary: "needs a dump newer than the database's last write, the main file or a non-empty -wal, and says so",
    change: "the engine writes to the -wal and checkpoints later, so the main file alone no longer dates the last write",
    undo: Some(|r, side| {
        replace_all(
            r,
            side,
            &[
                (
                    "is not newer than the database's last write, so",
                    "is older than the database itself, so",
                ),
                (
                    "you are about to delete. Run backup first.",
                    "you are about to delete. Run scripts/supercharged-memory-backup.sh first.",
                ),
                (
                    "ages the database past the dump",
                    "ages the DB past the dump",
                ),
            ],
        )
    }),
};

pub const FIND_QUIET_EXIT: Difference = Difference {
    id: "probe-quiet-exit",
    command: "find-existing-memory --quiet",
    reference: "exits 1 when it prints `nothing`",
    binary: "exits 0 for both words; the word is the answer",
    change: "a 1 reads as a failure to every caller of the binary",
    undo: Some(|r, side| {
        rust_only(r, side, |r| {
            if r.stdout == "nothing\n" && r.code == Some(0) {
                r.code = Some(1)
            }
        })
    }),
};

pub const FIND_REPORTED_SKIPS: Difference = Difference {
    id: "probe-reports-every-skip",
    command: "find-existing-memory",
    reference: "drops a `memory/` that is a dangling symlink (and four similar paths) silently",
    binary: "lists each in `unreadable`, so `nothing_to_do` turns false",
    change: "a skip that lands in no field reads as \"nothing to migrate\"",
    undo: None,
};

pub const MARK_UNKNOWN: Difference = Difference {
    id: "mark-processed-unknown-id",
    command: "sleep --mark-processed",
    reference: "stamps the ids it knows and ignores an unknown one",
    binary: "refuses the whole call and stamps nothing",
    change: "a typo in an id list must not pass as a completed sift",
    undo: None,
};

pub const SLEEP_NO_EMBEDDING_NOTE: Difference = Difference {
    id: "cluster-notes-unembedded-rows",
    command: "sleep --cluster, --contradiction-candidates",
    reference: "lists rows without a vector in `no_embedding` and nothing else",
    binary: "adds a `no_embedding_note` saying they were skipped, not judged",
    change: "rows can be stored without a vector; a report must not read their absence as a finding",
    undo: Some(|r, side| {
        rust_only(r, side, |r| {
            r.stdout = drop_json_key(&r.stdout, "no_embedding_note")
        })
    }),
};

pub const STALENESS_UNEMBEDDED: Difference = Difference {
    id: "staleness-counts-unembedded-rows",
    command: "sleep --staleness",
    reference: "reports the age buckets only",
    binary: "adds `n_without_embedding: {semantic, episodic}`",
    change: "rows can be stored without a vector",
    undo: Some(|r, side| {
        rust_only(r, side, |r| {
            r.stdout = drop_json_key(&r.stdout, "n_without_embedding")
        })
    }),
};

pub const VERIFY_CLASSES: Difference = Difference {
    id: "verify-candidates-more-classes",
    command: "sleep --verify-candidates",
    reference: "four artifact classes: paths, flags, env, versions",
    binary: "also `commands` (a subcommand of the binary) and `python` (`python3`, `*.py`); `env` also covers every `.env` key",
    change: "the binary's own command names and the Python era are what a verify pass now has to re-check",
    undo: None,
};

pub const THRESHOLD_RANGE: Difference = Difference {
    id: "cluster-threshold-range",
    command: "sleep --cluster, --contradiction-candidates --threshold",
    reference: "takes any number, so `--threshold 0` finds no pair and prints an empty result",
    binary: "a threshold must lie above 0 and at most 2: anything else is a usage error (64)",
    change: "a threshold no distance can meet must not read as \"no near-duplicates\"",
    undo: None,
};

pub const CLUSTER_NO_VECTORS: Difference = Difference {
    id: "cluster-without-any-vector-refused",
    command: "sleep --cluster, --contradiction-candidates",
    reference: "with no row in scope carrying a vector, prints no clusters and every id under `no_embedding`",
    binary: "refuses (exit 2) and points at `embed --missing`",
    change: "rows can be stored without a vector; a table none of whose rows was compared has no finding to report",
    undo: None,
};

pub const TOPICS_VALIDATED: Difference = Difference {
    id: "rebuild-topics-validates-first",
    command: "sleep --rebuild-topics",
    reference: "a payload that is not JSON, or an item without `keywords`, ends in a Python traceback (exit 1)",
    binary: "refuses (exit 2) before writing anything, naming what is wrong",
    change: "the whole payload is validated before the replace",
    undo: None,
};

pub const MARK_COUNTS_ROWS: Difference = Difference {
    id: "mark-processed-counts-rows",
    command: "sleep --mark-processed",
    reference: "reports how many ids it was given: `3,3` says `marked 2`",
    binary: "reports how many rows it stamped: `3,3` says `marked 1`",
    change: "the ids are deduplicated and the count says what was written",
    undo: None,
};

pub const SLEEP_ONE_MODE: Difference = Difference {
    id: "sleep-one-mode-per-run",
    command: "sleep",
    reference: "runs the first of several modes given and ignores the rest",
    binary: "refuses a second mode as a usage error (64)",
    change: "`--purge 5 --table episodic` silently purged semantic row 5",
    undo: None,
};

pub const COWORKER_NOOP: Difference = Difference {
    id: "coworker-no-op-refused",
    command: "coworkers --retire, --reactivate",
    reference: "reports success for a coworker already in that state",
    binary: "refuses: nothing changed",
    change: "a no-op must not read as a change",
    undo: None,
};

pub const COWORKER_COMMA: Difference = Difference {
    id: "coworker-name-without-comma",
    command: "coworkers --add",
    reference: "adds a coworker whose name holds a comma",
    binary: "refuses it (exit 2)",
    change: "`remember --coworker` splits its value on commas, so such a coworker could never be tagged",
    undo: None,
};

pub const TRUST_REFUSED: Difference = Difference {
    id: "unknown-trust-level-refused",
    command: "coworkers --appraise, --set-trust",
    reference: "argparse rejects an unknown --trust as a usage error (its usage text, exit 2)",
    binary: "refuses it: `refused: --trust must be one of ('supervised', 'trusted', 'autonomous').` (exit 2)",
    change: "the trust level is checked by the command, with the reference's own message, not by the parser",
    undo: None,
};

pub const BACKUP_OUTPUT: Difference = Difference {
    id: "backup-name-and-output",
    command: "backup (scripts/supercharged-memory-backup.sh)",
    reference: "writes `<date>-supercharged-memory.sql.gz` with tursodb's .dump and prints `[backup] ok: <path>`",
    binary: "writes `<date>-supercharged-memory-native.sql.gz` itself, prints only the path on stdout and its report on stderr",
    change: "a native dump, named so the two tools never prune each other's files, with a capturable stdout",
    undo: Some(|r, side| {
        replace_all(
            r,
            side,
            &[(
                "-supercharged-memory-native.sql.gz",
                "-supercharged-memory.sql.gz",
            )],
        )
    }),
};

pub const RESTORE_REPORT: Difference = Difference {
    id: "restore-report",
    command: "restore",
    reference: "lists the tables it saw INSERTs for, sorted by name",
    binary: "lists every table the dump creates or writes, in dump order, and adds where the restored file is not yet in use",
    change: "the dump is split by the engine's own parser and counted per statement",
    undo: None,
};

pub const RESTORE_TRUNCATED: Difference = Difference {
    id: "restore-cut-off-dump",
    command: "restore",
    reference: "stops before writing: `dump ends inside an unterminated string literal`, exit 1, no prefix",
    binary: "replays the readable part into a scratch file, reports the per-table mismatch and where the dump breaks, exits 1 with `error:`",
    change: "a cut-off dump shows what it still holds; nothing is left at --out either way",
    undo: None,
};

pub const RESTORE_FORCE: Difference = Difference {
    id: "restore-force-replaces-after-verifying",
    command: "restore --force",
    reference: "deletes the target first, so a restore that then fails leaves nothing there",
    binary: "replaces the target only once the restore is verified; a failed one leaves it as it was",
    change: "--force must not turn a bad dump into a lost file",
    undo: None,
};

pub const RESTORE_DEFAULT_DUMP: Difference = Difference {
    id: "restore-default-dump-is-gzip",
    command: "restore without --dump",
    reference: "takes the newest `*supercharged-memory*.sql*` in BACKUP_DIR, a plain `.sql` included",
    binary: "takes the newest `*supercharged-memory*.sql.gz`, what `backup` writes, and refuses when there is none",
    change: "the default is a dump the backup made and verified",
    undo: None,
};

pub const RESTORE_LIVE: Difference = Difference {
    id: "restore-never-over-the-live-database",
    command: "restore --out <configured database>",
    reference: "refuses only because the file exists, and deletes it first under --force",
    binary: "refuses the configured database by name, even with --force",
    change: "a restore never replaces the database in use",
    undo: None,
};

pub const EVAL_VALIDATE_TEXT: Difference = Difference {
    id: "eval-validate-hints",
    command: "eval --validate (eval-harness.py --validate)",
    reference: "its hint names an UPDATE statement to retire a case",
    binary: "names `eval --repoint` and `eval --retire-case`, and repeats the verdict as an `error:` line on stderr",
    change: "the eval-case writes became subcommands",
    undo: Some(|r, side| {
        rust_only(r, side, |r| {
            r.stdout = r.stdout.replace(
                "A target that moved down a supersede chain is moved with\n`eval --repoint <case> <new-id>`. A case whose targets all went GONE or got REUSED should be\nRETIRED (`eval --retire-case <case>`), not repointed at a loosely-related row — a repointed case\nquietly changes what the metric measures.",
                "A case whose targets all went GONE or got REUSED should be RETIRED\n(UPDATE eval_cases SET retired_at=CURRENT_TIMESTAMP WHERE id='...'), not repointed at a\nloosely-related row — a repointed case quietly changes what the metric measures.",
            );
            r.stderr = drop_lines(&r.stderr, "error: ");
        })
    }),
};

pub const EVAL_NO_CASES: Difference = Difference {
    id: "eval-scoring-without-cases",
    command: "eval --report, --sweep, --variants",
    reference: "`no eval cases in the DB (table eval_cases is empty).`, exit 1",
    binary: "refuses (exit 2) and names `eval --add-case` and `eval --import <dir>`",
    change: "the eval-case writes became subcommands, and an empty set is a refusal like every other",
    undo: None,
};

pub const EVAL_CACHE_PER_MODEL: Difference = Difference {
    id: "eval-cache-per-model",
    command: "eval (the query-vector cache)",
    reference: "one cache file, `<db dir>/eval/qvec.json`, whatever the model",
    binary: "one file per embedding model, `<db dir>/eval/qvec-<model>.json`; the two implementations do not share a cache",
    change: "a cache must never hand one model's vectors to a search in another's space",
    undo: None,
};

pub const EVAL_NO_EMBEDDINGS: Difference = Difference {
    id: "eval-scoring-needs-embeddings",
    command: "eval --report, --sweep, --variants",
    reference: "scores from its query-vector cache when every query is in it; otherwise `Ollama is down and some queries are not cached — cannot score.`, exit 1",
    binary: "refuses (exit 2, `refused: cannot score without embeddings: ...`) even when every query vector is cached, and names the modes that still work",
    change: "the scoring modes need embeddings, the cache only saves the calls",
    undo: None,
};

pub const BACKFILL_SKIPS: Difference = Difference {
    id: "backfill-reports-every-skip",
    command: "backfill",
    reference: "skips an empty file silently, stores a non-UTF-8 file with replacement characters, names files by their base name, and stores a re-run's files again",
    binary: "reports an empty and a non-UTF-8 file as skips, names files relative to --dir, and a re-run meets the duplicate guard",
    change: "backfill shares remember's write path and guards",
    undo: None,
};

pub const RECALL_ALPHA_INVALID: Difference = Difference {
    id: "recall-alpha-invalid",
    command: "recall with an unusable RECALL_ALPHA",
    reference: "dies with a ValueError traceback, exit 1",
    binary: "refuses (exit 2) naming the value and where it came from",
    change: "configuration is read and validated before any command runs",
    undo: None,
};

pub const SEED_DROPPED: Difference = Difference {
    id: "seed-dropped",
    command: "seed (scripts/seed.py)",
    reference: "a script with an empty list to fill in by hand",
    binary: "no such command; a seeded start is a few `remember` calls or `backfill --dir`",
    change: "the seed command was dropped",
    undo: None,
};

pub const INSTALLER_REPLACED: Difference = Difference {
    id: "installer-replaced",
    command: "init, update (scripts/install-claude-md.sh)",
    reference: "renders the template from a checkout into ~/.claude/CLAUDE.md",
    binary: "`init` sets up a machine from the binary alone and `update` reports what changed; not compared line by line",
    change: "the binary carries the template, the schema and the runbooks itself",
    undo: None,
};

pub const TABLE: &[&Difference] = &[
    &REFUSAL_EXIT,
    &USAGE_EXIT,
    &REFUSAL_PREFIX,
    &STATUS_REASON,
    &STATUS_UNEMBEDDED,
    &STATUS_ERROR_TEXT,
    &DEGRADED_WRITE,
    &RECALL_UNEMBEDDED,
    &COMMAND_NAMES,
    &PURGE_GATE,
    &FIND_QUIET_EXIT,
    &FIND_REPORTED_SKIPS,
    &MARK_UNKNOWN,
    &MARK_COUNTS_ROWS,
    &SLEEP_NO_EMBEDDING_NOTE,
    &STALENESS_UNEMBEDDED,
    &VERIFY_CLASSES,
    &THRESHOLD_RANGE,
    &CLUSTER_NO_VECTORS,
    &TOPICS_VALIDATED,
    &SLEEP_ONE_MODE,
    &COWORKER_NOOP,
    &COWORKER_COMMA,
    &TRUST_REFUSED,
    &BACKUP_OUTPUT,
    &RESTORE_REPORT,
    &RESTORE_TRUNCATED,
    &RESTORE_FORCE,
    &RESTORE_DEFAULT_DUMP,
    &RESTORE_LIVE,
    &EVAL_VALIDATE_TEXT,
    &EVAL_NO_CASES,
    &EVAL_CACHE_PER_MODEL,
    &EVAL_NO_EMBEDDINGS,
    &BACKFILL_SKIPS,
    &RECALL_ALPHA_INVALID,
    &SEED_DROPPED,
    &INSTALLER_REPLACED,
];

/// The table as Markdown, for the written report.
pub fn markdown() -> String {
    let mut out = String::from(
        "| id | command | the reference | the binary | the change that made it |\n|---|---|---|---|---|\n",
    );
    for d in TABLE {
        out.push_str(&format!(
            "| `{}` | {} | {} | {} | {} |\n",
            d.id, d.command, d.reference, d.binary, d.change
        ));
    }
    out
}

/// The ids of the entries no test asserted.
pub fn unexercised(exercised: &BTreeSet<&'static str>) -> Vec<&'static str> {
    TABLE
        .iter()
        .map(|d| d.id)
        .filter(|id| !exercised.contains(id))
        .collect()
}
