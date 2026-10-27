//! `coworkers`: named personas with scoped memory and trust-gated autonomy.
//!
//! The writes: add one, appraise one -- a new appraisal supersedes the previous
//! one in the same transaction -- set its trust level, retire it (`active = 0`;
//! a coworker is never deleted) and reactivate it. The reads: the list of every
//! coworker, and the load contract as one command -- the coworker's row, its
//! current appraisal, and the current feedback tagged to it since then.
//!
//! None of it needs Ollama: these tables have no embedding. Memories are tagged
//! to a coworker by `remember --coworker`; untagged memories are global.

use crate::claude_md::shell_word;
use crate::config::{Config, MAX_TEXT};
use crate::error::{Error, Result};
use crate::remember::validate;
use crate::render;
use crate::store::{self, Appraisal, Cell, Coworker, ResultSet, Store};

/// The schema's CHECK on `trust_level`, in order of autonomy.
pub const TRUST_LEVELS: [&str; 3] = ["supervised", "trusted", "autonomous"];

/// The schema's caps: `name`, and `expertise`/`personality`. An appraisal's
/// text has the memory cap, `MAX_TEXT`.
const MAX_NAME: usize = 64;
const MAX_PROFILE: usize = 3000;

/// One invocation. The CLI admits exactly one per run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Add {
        name: String,
        expertise: String,
        personality: String,
    },
    Appraise {
        name: String,
        trust: String,
        text: String,
    },
    SetTrust {
        name: String,
        level: String,
    },
    Retire(String),
    Reactivate(String),
    List,
    Load(String),
}

pub fn run(cfg: &Config, mode: &Mode) -> Result<()> {
    print!("{}", execute(cfg, mode)?);
    Ok(())
}

/// What one invocation prints, or a refusal. Argument errors are refused
/// before the database is opened.
pub fn execute(cfg: &Config, mode: &Mode) -> Result<String> {
    match mode {
        Mode::Add {
            name,
            expertise,
            personality,
        } => add(cfg, name, expertise, personality),
        Mode::Appraise { name, trust, text } => appraise(cfg, name.trim(), trust, text),
        Mode::SetTrust { name, level } => set_trust(cfg, name.trim(), level),
        Mode::Retire(name) => set_active(cfg, name.trim(), false),
        Mode::Reactivate(name) => set_active(cfg, name.trim(), true),
        Mode::List => list(cfg),
        Mode::Load(name) => load(cfg, name.trim()),
    }
}

fn add(cfg: &Config, name: &str, expertise: &str, personality: &str) -> Result<String> {
    let name = filled("--name", name)?;
    // `remember --coworker` splits its value on commas, so a coworker with a
    // comma in the name could be added but never tagged.
    if name.contains(',') {
        return Err(Error::refused(format!(
            "--name '{name}' contains a comma: remember --coworker takes a comma-separated \
             list, so this coworker could never be tagged. Nothing was stored."
        )));
    }
    validate::fits("--name", name, MAX_NAME)?;
    let expertise = filled("--expertise", expertise)?;
    validate::fits("--expertise", expertise, MAX_PROFILE)?;
    let personality = filled("--personality", personality)?;
    validate::fits("--personality", personality, MAX_PROFILE)?;

    let store = store::open(cfg)?;
    // A name is unique forever: a retired coworker keeps theirs.
    if let Some(existing) = store.coworker(name)? {
        return Err(Error::refused(format!(
            "coworker '{name}' already exists (id {}). Use --reactivate if retired, or pick a \
             different name.",
            existing.id
        )));
    }
    store.add_coworker(name, expertise, personality)?;
    Ok(format!("added coworker '{name}'\n"))
}

fn appraise(cfg: &Config, name: &str, trust: &str, text: &str) -> Result<String> {
    let trust = trust_level(trust, "--trust")?;
    let text = validate::body(text)?;
    validate::fits("--text", text, MAX_TEXT)?;
    let store = store::open(cfg)?;
    let coworker = find(&store, name)?;
    let done = store.appraise(coworker.id, trust, text)?;
    let what = match done.superseded.as_slice() {
        [] => "first appraisal".to_string(),
        [one] => format!("superseded appraisal {one}"),
        several => format!(
            "superseded appraisals {}",
            several
                .iter()
                .map(i64::to_string)
                .collect::<Vec<_>>()
                .join(",")
        ),
    };
    Ok(format!("appraised '{name}': trust={trust} ({what})\n"))
}

fn set_trust(cfg: &Config, name: &str, level: &str) -> Result<String> {
    let level = trust_level(level, "trust level")?;
    let store = store::open(cfg)?;
    let coworker = find(&store, name)?;
    store.set_trust(coworker.id, level)?;
    Ok(format!("'{name}' trust_level -> {level}\n"))
}

/// Retire or reactivate. Nothing is deleted: a retired coworker keeps its
/// row, its appraisals and every memory tagged to it.
fn set_active(cfg: &Config, name: &str, active: bool) -> Result<String> {
    let store = store::open(cfg)?;
    let coworker = find(&store, name)?;
    if !store.set_active(coworker.id, active)? {
        return Err(Error::refused(format!(
            "coworker '{name}' is already {} — nothing changed.",
            if active { "active" } else { "retired" }
        )));
    }
    Ok(format!(
        "'{name}' {}\n",
        if active { "reactivated" } else { "retired" }
    ))
}

/// Every coworker, retired ones included.
fn list(cfg: &Config) -> Result<String> {
    let rs = store::open(cfg)?.list_coworkers()?;
    if rs.rows.is_empty() {
        return Ok("===== coworkers (0) =====\n\
                   no coworker yet — add one: coworkers --add --name <name> --expertise <text> \
                   --personality <text>\n"
            .to_string());
    }
    Ok(format!(
        "===== coworkers ({}; active = 0 means retired) =====\n{}\n",
        rs.rows.len(),
        render::line(&rs)
    ))
}

/// The load contract: the coworker, its current appraisal, the current
/// feedback tagged to it since that appraisal (since it was added, before the
/// first), and what the agent does with them. A retired coworker says so
/// first.
fn load(cfg: &Config, name: &str) -> Result<String> {
    let store = store::open(cfg)?;
    let coworker = find(&store, name)?;
    let appraisal = store.current_appraisal(coworker.id)?;
    let (since, from) = match &appraisal {
        Some(a) => (a.created_at.clone(), "the current appraisal".to_string()),
        None => (coworker.created_at.clone(), format!("{name} was added")),
    };
    let feedback = store.feedback_since(coworker.id, &since)?;

    let mut out = String::new();
    if !coworker.active {
        out.push_str(&format!(
            "RETIRED: coworker '{name}' is retired (active = 0) — say so before proceeding; \
             coworkers --reactivate {} brings them back.\n",
            shell_word(name)
        ));
    }
    out.push_str(&format!("===== coworker '{name}' =====\n"));
    out.push_str(&render::line(&coworker_row(&coworker)));
    out.push('\n');
    match &appraisal {
        Some(a) => {
            out.push_str("===== current appraisal =====\n");
            out.push_str(&render::line(&appraisal_row(a)));
        }
        None => out.push_str("===== current appraisal: none yet =====\n"),
    }
    out.push('\n');
    let n = feedback.rows.len();
    out.push_str(&format!(
        "===== feedback tagged to {name} since {from} ({since}): {n} {}, oldest first =====\n",
        if n == 1 { "row" } else { "rows" }
    ));
    out.push_str(&render::line(&feedback));
    out.push('\n');
    out.push_str(&rules(&coworker));
    Ok(out)
}

/// What a loaded coworker changes, and what it never does: the baseline is
/// a floor at every trust level; the level decides whether the coworker's
/// own learnings need the user's ok before they are stored.
fn rules(c: &Coworker) -> String {
    let gate = if c.trust_level == "supervised" {
        "supervised: propose those to the user and store one only after their ok".to_string()
    } else {
        format!(
            "{}: store those directly and mention it in passing",
            c.trust_level
        )
    };
    format!(
        "Adopt {name}'s expertise, personality and trust level ({level}) for this session. \
         Baseline rules stay a hard floor — no trust level loosens them.\n\
         Feedback while loaded: remember --coworker {word} --category feedback — the user's \
         corrections and praise with --source user-stated, {name}'s own learnings with --source \
         self-observed ({gate}).\n",
        name = c.name,
        level = c.trust_level,
        word = shell_word(&c.name),
    )
}

fn coworker_row(c: &Coworker) -> ResultSet {
    ResultSet {
        columns: [
            "id",
            "created_at",
            "updated_at",
            "name",
            "expertise",
            "personality",
            "trust_level",
            "active",
        ]
        .map(String::from)
        .to_vec(),
        rows: vec![vec![
            Cell::Int(c.id),
            Cell::Text(c.created_at.clone()),
            Cell::Text(c.updated_at.clone()),
            Cell::Text(c.name.clone()),
            Cell::Text(c.expertise.clone()),
            Cell::Text(c.personality.clone()),
            Cell::Text(c.trust_level.clone()),
            Cell::Int(i64::from(c.active)),
        ]],
    }
}

fn appraisal_row(a: &Appraisal) -> ResultSet {
    ResultSet {
        columns: [
            "id",
            "created_at",
            "period_start",
            "trust_level",
            "memory_text",
        ]
        .map(String::from)
        .to_vec(),
        rows: vec![vec![
            Cell::Int(a.id),
            Cell::Text(a.created_at.clone()),
            a.period_start.clone().map_or(Cell::Null, Cell::Text),
            Cell::Text(a.trust_level.clone()),
            Cell::Text(a.memory_text.clone()),
        ]],
    }
}

/// The coworker with exactly this name, or a refusal.
fn find(store: &impl Store, name: &str) -> Result<Coworker> {
    store
        .coworker(name)?
        .ok_or_else(|| Error::refused(format!("no coworker named '{name}'.")))
}

/// One of the three levels, exactly, or the reference's refusal naming
/// `what`.
fn trust_level<'a>(level: &'a str, what: &str) -> Result<&'a str> {
    if TRUST_LEVELS.contains(&level) {
        return Ok(level);
    }
    let quoted: Vec<String> = TRUST_LEVELS.iter().map(|t| format!("'{t}'")).collect();
    Err(Error::refused(format!(
        "{what} must be one of ({}).",
        quoted.join(", ")
    )))
}

/// The value trimmed, or a refusal when nothing is left.
fn filled<'a>(flag: &str, value: &'a str) -> Result<&'a str> {
    let v = value.trim();
    if v.is_empty() {
        return Err(Error::refused(format!(
            "{flag} is empty — nothing was stored."
        )));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::EXIT_REFUSED;
    use crate::store::Cell;
    use crate::store::turso::tests::{rows, scratch};

    fn ok(cfg: &Config, mode: Mode) -> String {
        execute(cfg, &mode).unwrap_or_else(|e| panic!("{mode:?}: {e}"))
    }

    fn refused(cfg: &Config, mode: Mode) -> String {
        let e = execute(cfg, &mode).expect_err("must be refused");
        assert_eq!(e.exit_code(), EXIT_REFUSED, "{e}");
        let msg = e.to_string();
        assert!(msg.starts_with("refused: "), "{msg}");
        msg
    }

    fn add(name: &str) -> Mode {
        Mode::Add {
            name: name.into(),
            expertise: "rust, sql".into(),
            personality: "terse, sceptical".into(),
        }
    }

    fn appraise(name: &str, trust: &str, text: &str) -> Mode {
        Mode::Appraise {
            name: name.into(),
            trust: trust.into(),
            text: text.into(),
        }
    }

    /// Every row of the three coworker tables: what "nothing changed" means.
    fn dump(cfg: &Config) -> Vec<Vec<Cell>> {
        let mut all = rows(cfg, "SELECT * FROM coworkers ORDER BY id;");
        all.extend(rows(cfg, "SELECT * FROM appraisals ORDER BY id;"));
        all.extend(rows(
            cfg,
            "SELECT * FROM memory_coworkers ORDER BY memory_table, memory_id, coworker_id;",
        ));
        all
    }

    fn sql(cfg: &Config, statement: &str) {
        rows(cfg, statement);
    }

    /// A coworker inserted directly, with a fixed creation date.
    fn put_coworker(cfg: &Config, name: &str, created_at: &str) -> i64 {
        sql(
            cfg,
            &format!(
                "INSERT INTO coworkers (name, expertise, personality, created_at, updated_at) \
                 VALUES ('{name}', 'rust, sql', 'terse, sceptical', '{created_at}', '{created_at}');"
            ),
        );
        match rows(
            cfg,
            &format!("SELECT id FROM coworkers WHERE name = '{name}';"),
        )[0][0]
        {
            Cell::Int(id) => id,
            ref other => panic!("{other:?}"),
        }
    }

    /// An appraisal inserted directly, current unless superseded later.
    fn put_appraisal(cfg: &Config, coworker: i64, created_at: &str, text: &str) -> i64 {
        sql(
            cfg,
            &format!(
                "INSERT INTO appraisals (coworker_id, created_at, period_start, trust_level, \
                 memory_text) VALUES ({coworker}, '{created_at}', NULL, 'trusted', '{text}');"
            ),
        );
        match rows(cfg, "SELECT max(id) FROM appraisals;")[0][0] {
            Cell::Int(id) => id,
            ref other => panic!("{other:?}"),
        }
    }

    /// A semantic memory of this category, tagged to these coworkers.
    fn put_memory(
        cfg: &Config,
        category: &str,
        created_at: &str,
        text: &str,
        coworkers: &[i64],
    ) -> i64 {
        sql(
            cfg,
            &format!(
                "INSERT INTO semantic_memory (category, source, topic, memory_text, created_at, \
                 updated_at) VALUES ('{category}', 'user-stated', 'review', '{text}', \
                 '{created_at}', '{created_at}');"
            ),
        );
        let id = match rows(cfg, "SELECT max(id) FROM semantic_memory;")[0][0] {
            Cell::Int(id) => id,
            ref other => panic!("{other:?}"),
        };
        for c in coworkers {
            sql(
                cfg,
                &format!(
                    "INSERT INTO memory_coworkers (memory_table, memory_id, coworker_id) \
                     VALUES ('semantic', {id}, {c});"
                ),
            );
        }
        id
    }

    /// The `memory_text` values of an output, in order.
    fn texts(out: &str) -> Vec<&str> {
        out.lines()
            .filter_map(|l| l.trim_start().strip_prefix("memory_text = "))
            .collect()
    }

    // ---- refusals: exit 2, `refused:`, nothing changed -----------------------

    /// Trust levels are the schema's three, exactly; anything else is refused
    /// with the reference's own wording, before the database is opened.
    #[test]
    fn a_bad_trust_level_is_refused_on_appraise_and_set_trust() {
        let cfg = scratch("cw-trust");
        ok(&cfg, add("jeff"));
        let before = dump(&cfg);
        for bad in ["bogus", "Trusted", "", " trusted"] {
            let msg = refused(&cfg, appraise("jeff", bad, "good reviews"));
            assert_eq!(
                msg,
                "refused: --trust must be one of ('supervised', 'trusted', 'autonomous')."
            );
            let msg = refused(
                &cfg,
                Mode::SetTrust {
                    name: "jeff".into(),
                    level: bad.into(),
                },
            );
            assert_eq!(
                msg,
                "refused: trust level must be one of ('supervised', 'trusted', 'autonomous')."
            );
        }
        assert_eq!(dump(&cfg), before);

        // An argument error does not depend on the database being there.
        let mut missing = cfg.clone();
        missing.db_path = missing.db_path.with_file_name("absent.db");
        let msg = refused(&missing, appraise("jeff", "bogus", "x"));
        assert!(msg.contains("--trust must be one of"), "{msg}");
        assert!(!missing.db_path.exists());
    }

    /// The schema's caps, refused before the write and naming the flag -- not
    /// left to a CHECK violation. Characters, not bytes.
    #[test]
    fn an_over_cap_field_is_refused_naming_the_flag() {
        let cfg = scratch("cw-caps");
        for (mode, want) in [
            (
                Mode::Add {
                    name: "n".repeat(65),
                    expertise: "x".into(),
                    personality: "y".into(),
                },
                "--name is 65 chars; hard cap 64.",
            ),
            (
                Mode::Add {
                    name: "jeff".into(),
                    expertise: "ä".repeat(3001),
                    personality: "y".into(),
                },
                "--expertise is 3001 chars; hard cap 3000.",
            ),
            (
                Mode::Add {
                    name: "jeff".into(),
                    expertise: "x".into(),
                    personality: "p".repeat(3001),
                },
                "--personality is 3001 chars; hard cap 3000.",
            ),
        ] {
            let msg = refused(&cfg, mode);
            assert!(msg.contains(want), "{msg}");
            assert!(msg.contains("Nothing was stored"), "{msg}");
        }
        assert!(dump(&cfg).is_empty());

        // Exactly at the caps is fine.
        ok(
            &cfg,
            Mode::Add {
                name: "n".repeat(64),
                expertise: "ä".repeat(3000),
                personality: "ö".repeat(3000),
            },
        );
        let name = "n".repeat(64);
        let msg = refused(&cfg, appraise(&name, "trusted", &"t".repeat(2001)));
        assert!(
            msg.contains("--text is 2001 chars; hard cap 2000."),
            "{msg}"
        );
        let before = dump(&cfg);
        assert_eq!(before.len(), 1, "the coworker, and no appraisal");
        ok(&cfg, appraise(&name, "trusted", &"ü".repeat(2000)));
        assert_eq!(
            rows(&cfg, "SELECT count(*) FROM appraisals;"),
            vec![vec![Cell::Int(1)]]
        );
    }

    #[test]
    fn a_blank_field_is_refused() {
        let cfg = scratch("cw-blank");
        for (mode, want) in [
            (
                Mode::Add {
                    name: "  ".into(),
                    expertise: "x".into(),
                    personality: "y".into(),
                },
                "--name is empty",
            ),
            (
                Mode::Add {
                    name: "jeff".into(),
                    expertise: "".into(),
                    personality: "y".into(),
                },
                "--expertise is empty",
            ),
            (
                Mode::Add {
                    name: "jeff".into(),
                    expertise: "x".into(),
                    personality: " \n".into(),
                },
                "--personality is empty",
            ),
        ] {
            let msg = refused(&cfg, mode);
            assert!(msg.contains(want), "{msg}");
        }
        assert!(dump(&cfg).is_empty());
        ok(&cfg, add("jeff"));
        let msg = refused(&cfg, appraise("jeff", "trusted", "   "));
        assert!(msg.contains("--text is empty"), "{msg}");
        assert_eq!(
            rows(&cfg, "SELECT count(*) FROM appraisals;"),
            vec![vec![Cell::Int(0)]]
        );
    }

    /// `remember --coworker` takes a comma-separated list, so a name with a
    /// comma in it could never be tagged.
    #[test]
    fn a_name_with_a_comma_is_refused() {
        let cfg = scratch("cw-comma");
        let msg = refused(&cfg, add("jeff,ada"));
        assert!(msg.contains("comma"), "{msg}");
        assert!(msg.contains("remember --coworker"), "{msg}");
        assert!(dump(&cfg).is_empty());
    }

    /// A name is unique forever: a retired coworker's name is taken too.
    #[test]
    fn a_duplicate_name_is_refused_with_the_existing_id() {
        let cfg = scratch("cw-dup");
        ok(&cfg, add("jeff"));
        let before = dump(&cfg);
        let want = "refused: coworker 'jeff' already exists (id 1). Use --reactivate if retired, \
                    or pick a different name.";
        assert_eq!(refused(&cfg, add("jeff")), want);
        assert_eq!(refused(&cfg, add(" jeff ")), want, "names are trimmed");
        ok(&cfg, Mode::Retire("jeff".into()));
        let retired = dump(&cfg);
        assert_eq!(refused(&cfg, add("jeff")), want);
        assert_eq!(dump(&cfg), retired);
        assert_eq!(before.len(), retired.len());
    }

    #[test]
    fn an_unknown_name_is_refused_by_every_command_that_takes_one() {
        let cfg = scratch("cw-unknown");
        ok(&cfg, add("jeff"));
        let before = dump(&cfg);
        for (mode, name) in [
            (appraise("nobody", "trusted", "x"), "nobody"),
            (Mode::Load("nobody".into()), "nobody"),
            (Mode::Retire("nobody".into()), "nobody"),
            (Mode::Reactivate("nobody".into()), "nobody"),
            (
                Mode::SetTrust {
                    name: "nobody".into(),
                    level: "trusted".into(),
                },
                "nobody",
            ),
            // Matched exactly, as `remember` and `recall` match them.
            (Mode::Load("Jeff".into()), "Jeff"),
        ] {
            assert_eq!(
                refused(&cfg, mode),
                format!("refused: no coworker named '{name}'.")
            );
        }
        assert_eq!(dump(&cfg), before);
    }

    /// Retiring twice, or reactivating an active coworker, would change
    /// nothing -- so it is refused and writes nothing, `updated_at` included.
    #[test]
    fn retiring_a_retired_or_reactivating_an_active_coworker_is_refused() {
        let cfg = scratch("cw-noop");
        put_coworker(&cfg, "jeff", "2026-01-01 00:00:00");
        let msg = refused(&cfg, Mode::Reactivate("jeff".into()));
        assert!(msg.contains("coworker 'jeff' is already active"), "{msg}");
        assert!(msg.contains("nothing changed"), "{msg}");
        ok(&cfg, Mode::Retire("jeff".into()));
        sql(
            &cfg,
            "UPDATE coworkers SET updated_at = '2026-01-02 00:00:00';",
        );
        let before = dump(&cfg);
        let msg = refused(&cfg, Mode::Retire("jeff".into()));
        assert!(msg.contains("coworker 'jeff' is already retired"), "{msg}");
        assert!(msg.contains("nothing changed"), "{msg}");
        assert_eq!(dump(&cfg), before);
    }

    #[test]
    fn a_missing_database_is_refused_and_never_created() {
        let mut cfg = scratch("cw-missing");
        cfg.db_path = cfg.db_path.with_file_name("absent.db");
        for mode in [
            add("jeff"),
            appraise("jeff", "trusted", "x"),
            Mode::List,
            Mode::Load("jeff".into()),
            Mode::Retire("jeff".into()),
        ] {
            let msg = refused(&cfg, mode);
            assert!(msg.contains("memory DB missing"), "{msg}");
        }
        assert!(!cfg.db_path.exists());
    }

    // ---- the writes ----------------------------------------------------------

    /// The reference's own words, and a coworker that starts active and
    /// supervised.
    #[test]
    fn add_stores_an_active_supervised_coworker() {
        let cfg = scratch("cw-add");
        assert_eq!(ok(&cfg, add(" jeff ")), "added coworker 'jeff'\n");
        assert_eq!(
            rows(
                &cfg,
                "SELECT id, name, expertise, personality, trust_level, active FROM coworkers;"
            ),
            vec![vec![
                Cell::Int(1),
                Cell::Text("jeff".into()),
                Cell::Text("rust, sql".into()),
                Cell::Text("terse, sceptical".into()),
                Cell::Text("supervised".into()),
                Cell::Int(1),
            ]]
        );
    }

    /// The acceptance criterion: two appraisals, one current. Each folds in
    /// the window since the one before it (since the coworker was added, for
    /// the first), and sets the coworker's trust level.
    #[test]
    fn appraising_twice_leaves_exactly_one_current_appraisal() {
        let cfg = scratch("cw-appraise");
        let jeff = put_coworker(&cfg, "jeff", "2026-01-01 00:00:00");
        assert_eq!(
            ok(&cfg, appraise("jeff", "trusted", "good reviews")),
            "appraised 'jeff': trust=trusted (first appraisal)\n"
        );
        sql(
            &cfg,
            "UPDATE appraisals SET created_at = '2026-02-01 00:00:00';",
        );
        assert_eq!(
            ok(&cfg, appraise("jeff", "autonomous", "great reviews")),
            "appraised 'jeff': trust=autonomous (superseded appraisal 1)\n"
        );
        assert_eq!(
            rows(
                &cfg,
                "SELECT id, coworker_id, period_start, trust_level, memory_text, superseded_by \
                 FROM appraisals ORDER BY id;"
            ),
            vec![
                vec![
                    Cell::Int(1),
                    Cell::Int(jeff),
                    Cell::Text("2026-01-01 00:00:00".into()),
                    Cell::Text("trusted".into()),
                    Cell::Text("good reviews".into()),
                    Cell::Int(2),
                ],
                vec![
                    Cell::Int(2),
                    Cell::Int(jeff),
                    Cell::Text("2026-02-01 00:00:00".into()),
                    Cell::Text("autonomous".into()),
                    Cell::Text("great reviews".into()),
                    Cell::Null,
                ],
            ]
        );
        assert_eq!(
            rows(
                &cfg,
                "SELECT trust_level, updated_at > '2026-01-01 00:00:00' FROM coworkers;"
            ),
            vec![vec![Cell::Text("autonomous".into()), Cell::Int(1)]]
        );
    }

    /// Appraisals are per coworker: appraising one never supersedes another's.
    #[test]
    fn an_appraisal_supersedes_only_its_own_coworkers() {
        let cfg = scratch("cw-appraise-own");
        put_coworker(&cfg, "jeff", "2026-01-01 00:00:00");
        put_coworker(&cfg, "ada", "2026-01-01 00:00:00");
        ok(&cfg, appraise("ada", "trusted", "fine"));
        ok(&cfg, appraise("jeff", "trusted", "fine too"));
        assert_eq!(
            ok(&cfg, appraise("jeff", "supervised", "slipped")),
            "appraised 'jeff': trust=supervised (superseded appraisal 2)\n"
        );
        assert_eq!(
            rows(
                &cfg,
                "SELECT id, superseded_by FROM appraisals ORDER BY id;"
            ),
            vec![
                vec![Cell::Int(1), Cell::Null],
                vec![Cell::Int(2), Cell::Int(3)],
                vec![Cell::Int(3), Cell::Null],
            ]
        );
    }

    /// Two current appraisals -- left by an earlier writer or a hand edit --
    /// are both superseded by the next one, which leaves exactly one current.
    #[test]
    fn an_appraisal_supersedes_every_current_one() {
        let cfg = scratch("cw-appraise-heal");
        let jeff = put_coworker(&cfg, "jeff", "2026-01-01 00:00:00");
        put_appraisal(&cfg, jeff, "2026-02-01 00:00:00", "one");
        put_appraisal(&cfg, jeff, "2026-03-01 00:00:00", "two");
        assert_eq!(
            ok(&cfg, appraise("jeff", "trusted", "three")),
            "appraised 'jeff': trust=trusted (superseded appraisals 1,2)\n"
        );
        assert_eq!(
            rows(
                &cfg,
                "SELECT id, period_start, superseded_by FROM appraisals ORDER BY id;"
            ),
            vec![
                vec![Cell::Int(1), Cell::Null, Cell::Int(3)],
                vec![Cell::Int(2), Cell::Null, Cell::Int(3)],
                vec![
                    Cell::Int(3),
                    Cell::Text("2026-03-01 00:00:00".into()),
                    Cell::Null
                ],
            ]
        );
    }

    #[test]
    fn set_trust_changes_the_level_without_an_appraisal() {
        let cfg = scratch("cw-set-trust");
        ok(&cfg, add("jeff"));
        assert_eq!(
            ok(
                &cfg,
                Mode::SetTrust {
                    name: "jeff".into(),
                    level: "trusted".into(),
                }
            ),
            "'jeff' trust_level -> trusted\n"
        );
        assert_eq!(
            rows(&cfg, "SELECT trust_level FROM coworkers;"),
            vec![vec![Cell::Text("trusted".into())]]
        );
        assert_eq!(
            rows(&cfg, "SELECT count(*) FROM appraisals;"),
            vec![vec![Cell::Int(0)]]
        );
    }

    /// The acceptance criterion: retire then reactivate loses nothing. Only
    /// `active` and `updated_at` ever change; appraisals and tags stay.
    #[test]
    fn retire_then_reactivate_round_trips_with_no_data_loss() {
        let cfg = scratch("cw-round-trip");
        let jeff = put_coworker(&cfg, "jeff", "2026-01-01 00:00:00");
        ok(&cfg, appraise("jeff", "trusted", "good reviews"));
        put_memory(
            &cfg,
            "feedback",
            "2026-05-01 00:00:00",
            "be blunter",
            &[jeff],
        );
        let snapshot = |cfg: &Config| {
            let mut all = rows(
                cfg,
                "SELECT id, created_at, name, expertise, personality, trust_level FROM coworkers;",
            );
            all.extend(rows(cfg, "SELECT * FROM appraisals ORDER BY id;"));
            all.extend(rows(cfg, "SELECT * FROM memory_coworkers;"));
            all.extend(rows(cfg, "SELECT * FROM semantic_memory;"));
            all
        };
        let active = |cfg: &Config| rows(cfg, "SELECT active FROM coworkers;");
        let before = snapshot(&cfg);

        assert_eq!(ok(&cfg, Mode::Retire("jeff".into())), "'jeff' retired\n");
        assert_eq!(active(&cfg), vec![vec![Cell::Int(0)]]);
        assert_eq!(snapshot(&cfg), before, "retired, not deleted");

        assert_eq!(
            ok(&cfg, Mode::Reactivate("jeff".into())),
            "'jeff' reactivated\n"
        );
        assert_eq!(active(&cfg), vec![vec![Cell::Int(1)]]);
        assert_eq!(snapshot(&cfg), before);
    }

    // ---- the reads -------------------------------------------------------------

    /// The template's List query -- id, name, expertise, trust level, active,
    /// retired coworkers included -- in tursodb's line format under a header.
    #[test]
    fn list_prints_every_coworker_retired_included() {
        let cfg = scratch("cw-list");
        put_coworker(&cfg, "jeff", "2026-01-01 00:00:00");
        put_coworker(&cfg, "ada", "2026-01-02 00:00:00");
        ok(&cfg, Mode::Retire("ada".into()));
        let before = dump(&cfg);
        assert_eq!(
            ok(&cfg, Mode::List),
            "===== coworkers (2; active = 0 means retired) =====\n\
             \x20        id = 1\n\
             \x20      name = jeff\n\
             \x20 expertise = rust, sql\n\
             trust_level = supervised\n\
             \x20    active = 1\n\
             \n\
             \x20        id = 2\n\
             \x20      name = ada\n\
             \x20 expertise = rust, sql\n\
             trust_level = supervised\n\
             \x20    active = 0\n\
             \n"
        );
        assert_eq!(dump(&cfg), before, "a pure reader");
    }

    #[test]
    fn list_of_no_coworker_says_so() {
        let cfg = scratch("cw-list-empty");
        assert_eq!(
            ok(&cfg, Mode::List),
            "===== coworkers (0) =====\n\
             no coworker yet — add one: coworkers --add --name <name> --expertise <text> \
             --personality <text>\n"
        );
    }

    /// The acceptance criterion: two appraisals, feedback on both sides of the
    /// newer one -- only the rows after it. Same second counts as after:
    /// timestamps have one-second resolution, and a row shown in two reviews
    /// costs less than one shown in none.
    #[test]
    fn load_lists_only_the_feedback_since_the_current_appraisal() {
        let cfg = scratch("cw-load-window");
        let jeff = put_coworker(&cfg, "jeff", "2026-01-01 00:00:00");
        let ada = put_coworker(&cfg, "ada", "2026-01-01 00:00:00");
        let first = put_appraisal(&cfg, jeff, "2026-02-01 00:00:00", "first");
        let current = put_appraisal(&cfg, jeff, "2026-03-01 00:00:00", "second");
        sql(
            &cfg,
            &format!("UPDATE appraisals SET superseded_by = {current} WHERE id = {first};"),
        );
        let fb = |at: &str, text: &str, tags: &[i64]| put_memory(&cfg, "feedback", at, text, tags);
        fb("2026-01-15 00:00:00", "before the first", &[jeff]);
        fb("2026-02-15 00:00:00", "between the two", &[jeff]);
        fb("2026-03-01 00:00:00", "same second", &[jeff]);
        fb("2026-04-01 00:00:00", "after, later", &[jeff]);
        fb("2026-03-15 00:00:00", "after", &[jeff]);
        fb(
            "2026-03-20 00:00:00",
            "after, shared with ada",
            &[jeff, ada],
        );
        fb("2026-03-21 00:00:00", "after, ada only", &[ada]);
        fb("2026-03-22 00:00:00", "after, global", &[]);
        put_memory(
            &cfg,
            "project",
            "2026-03-23 00:00:00",
            "not feedback",
            &[jeff],
        );
        let revised = fb("2026-03-24 00:00:00", "after, superseded", &[jeff]);
        let gone = fb("2026-03-25 00:00:00", "after, retired", &[jeff]);
        let newer = fb("2026-03-26 00:00:00", "the revision", &[jeff]);
        sql(
            &cfg,
            &format!("UPDATE semantic_memory SET superseded_by = {newer} WHERE id = {revised};"),
        );
        sql(
            &cfg,
            &format!("UPDATE semantic_memory SET retired_at = '2026-03-27' WHERE id = {gone};"),
        );

        let out = ok(&cfg, Mode::Load("jeff".into()));
        assert_eq!(
            texts(&out),
            vec![
                "second",
                "same second",
                "after",
                "after, shared with ada",
                "the revision",
                "after, later",
            ],
            "{out}"
        );
    }

    /// Without an appraisal the window starts when the coworker was added.
    #[test]
    fn load_without_an_appraisal_lists_the_feedback_since_the_coworker_was_added() {
        let cfg = scratch("cw-load-no-appraisal");
        let jeff = put_coworker(&cfg, "jeff", "2026-02-01 00:00:00");
        put_memory(&cfg, "feedback", "2026-01-15 00:00:00", "before", &[jeff]);
        put_memory(&cfg, "feedback", "2026-02-01 00:00:00", "as added", &[jeff]);
        put_memory(&cfg, "feedback", "2026-03-01 00:00:00", "after", &[jeff]);
        let out = ok(&cfg, Mode::Load("jeff".into()));
        assert_eq!(texts(&out), vec!["as added", "after"], "{out}");
        assert!(
            out.contains("===== current appraisal: none yet =====\n"),
            "{out}"
        );
        assert!(
            out.contains(
                "===== feedback tagged to jeff since jeff was added (2026-02-01 00:00:00): 2 \
                 rows, oldest first =====\n"
            ),
            "{out}"
        );
    }

    /// The whole output, byte for byte: the coworker row, the appraisal and
    /// the feedback in tursodb's line format, then what the agent does with
    /// them.
    #[test]
    fn load_prints_the_coworker_the_appraisal_the_feedback_and_the_rules() {
        let cfg = scratch("cw-load-format");
        let jeff = put_coworker(&cfg, "jeff", "2026-01-01 00:00:00");
        sql(
            &cfg,
            "UPDATE coworkers SET trust_level = 'trusted', updated_at = '2026-03-01 00:00:00';",
        );
        put_appraisal(&cfg, jeff, "2026-03-01 00:00:00", "solid reviews");
        sql(
            &cfg,
            "UPDATE appraisals SET period_start = '2026-01-01 00:00:00';",
        );
        put_memory(
            &cfg,
            "feedback",
            "2026-03-02 00:00:00",
            "be blunter",
            &[jeff],
        );
        let before = dump(&cfg);
        assert_eq!(
            ok(&cfg, Mode::Load("jeff".into())),
            "===== coworker 'jeff' =====\n\
             \x20        id = 1\n\
             \x20created_at = 2026-01-01 00:00:00\n\
             \x20updated_at = 2026-03-01 00:00:00\n\
             \x20      name = jeff\n\
             \x20 expertise = rust, sql\n\
             personality = terse, sceptical\n\
             trust_level = trusted\n\
             \x20    active = 1\n\
             \n\
             ===== current appraisal =====\n\
             \x20         id = 1\n\
             \x20 created_at = 2026-03-01 00:00:00\n\
             period_start = 2026-01-01 00:00:00\n\
             \x20trust_level = trusted\n\
             \x20memory_text = solid reviews\n\
             \n\
             ===== feedback tagged to jeff since the current appraisal (2026-03-01 00:00:00): 1 \
             row, oldest first =====\n\
             \x20        id = 1\n\
             \x20created_at = 2026-03-02 00:00:00\n\
             \x20    source = user-stated\n\
             \x20     topic = review\n\
             memory_text = be blunter\n\
             \n\
             Adopt jeff's expertise, personality and trust level (trusted) for this session. \
             Baseline rules stay a hard floor — no trust level loosens them.\n\
             Feedback while loaded: remember --coworker jeff --category feedback — the user's \
             corrections and praise with --source user-stated, jeff's own learnings with --source \
             self-observed (trusted: store those directly and mention it in passing).\n"
        );
        assert_eq!(dump(&cfg), before, "a pure reader");
    }

    /// The acceptance criterion: a retired coworker's load says so first.
    #[test]
    fn load_of_a_retired_coworker_says_so_on_its_first_line() {
        let cfg = scratch("cw-load-retired");
        put_coworker(&cfg, "jeff", "2026-01-01 00:00:00");
        let active = ok(&cfg, Mode::Load("jeff".into()));
        assert!(!active.contains("RETIRED"), "{active}");
        ok(&cfg, Mode::Retire("jeff".into()));
        let out = ok(&cfg, Mode::Load("jeff".into()));
        assert_eq!(
            out.lines().next(),
            Some(
                "RETIRED: coworker 'jeff' is retired (active = 0) — say so before proceeding; \
                 coworkers --reactivate jeff brings them back."
            ),
            "{out}"
        );
        // Everything after that line is the same load.
        let rest = out.split_once('\n').unwrap().1;
        assert!(rest.starts_with("===== coworker 'jeff' =====\n"), "{out}");
        assert!(rest.contains("     active = 0\n"), "{out}");
    }

    /// Baseline rules are a floor at every trust level; what the level
    /// changes is whether the coworker's own learnings need the user's ok.
    #[test]
    fn load_states_the_baseline_floor_and_what_the_trust_level_allows() {
        let cfg = scratch("cw-load-trust");
        put_coworker(&cfg, "jeff smith", "2026-01-01 00:00:00");
        for (level, gate) in [
            (
                "supervised",
                "(supervised: propose those to the user and store one only after their ok).",
            ),
            (
                "trusted",
                "(trusted: store those directly and mention it in passing).",
            ),
            (
                "autonomous",
                "(autonomous: store those directly and mention it in passing).",
            ),
        ] {
            sql(
                &cfg,
                &format!("UPDATE coworkers SET trust_level = '{level}';"),
            );
            let out = ok(&cfg, Mode::Load("jeff smith".into()));
            assert!(
                out.contains(&format!(
                    "Adopt jeff smith's expertise, personality and trust level ({level}) for this \
                     session. Baseline rules stay a hard floor — no trust level loosens them.\n"
                )),
                "{out}"
            );
            assert!(
                out.contains("remember --coworker \"jeff smith\" --category feedback"),
                "{out}"
            );
            assert!(out.ends_with(&format!("{gate}\n")), "{level}: {out}");
        }
    }
}
