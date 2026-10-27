//! Backup and restore: each side backs up with its own tool and restores its
//! own dump and the other side's; every restored database must hold exactly
//! the rows of the source. Then the restores that must refuse.

use std::io::Read;

use crate::differences::{
    BACKUP_OUTPUT, RESTORE_DEFAULT_DUMP, RESTORE_FORCE, RESTORE_LIVE, RESTORE_REPORT,
    RESTORE_TRUNCATED,
};
use crate::harness::{Ctx, Fixture, cmd};
use crate::runner::Test;

pub const TESTS: &[Test] = crate::tests![
    backups_restore_on_both_sides_and_across,
    restore_refuses_the_live_database_and_a_cut_off_dump,
    restore_force_keeps_the_target_and_the_default_dump_is_gzip,
];

const T: &str = "backup";

fn today() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

fn backups_restore_on_both_sides_and_across(ctx: &Ctx) {
    let p = ctx.pair(T, "round-trip", Fixture::Full);
    let source = p.rs.state();
    let (rs, py) = p.run(&cmd(&["backup"]));
    let rs_dump = format!(
        "{ROOT}/backups/{}-supercharged-memory-native.sql.gz",
        today(),
        ROOT = "{ROOT}"
    );
    assert_eq!(rs.ok(), format!("{rs_dump}\n"));
    assert!(rs.stderr.lines().all(|l| l.starts_with("backup: ")), "{rs}");
    assert_eq!(
        p.rs.list("backups"),
        [format!("{}-supercharged-memory-native.sql.gz", today())]
    );
    let rs_dump_path = p.rs.path(&format!(
        "backups/{}-supercharged-memory-native.sql.gz",
        today()
    ));

    let restored = p.rs.run(&cmd(&["restore", "--out", "{ROOT}/restored.db"]));
    let text = restored.ok();
    assert!(
        text.contains("\nRESTORE OK — every row in the dump is present in {ROOT}/restored.db\n"),
        "{restored}"
    );
    let table_lines: Vec<&str> = text
        .lines()
        .skip_while(|l| !l.starts_with("table "))
        .skip(1)
        .take_while(|l| !l.starts_with("RESTORE"))
        .map(|l| l.split_whitespace().next().unwrap())
        .collect();
    assert_eq!(
        table_lines,
        [
            "semantic_memory",
            "episodic_memory",
            "topic_keywords",
            "coworkers",
            "memory_coworkers",
            "appraisals",
            "eval_cases",
            "eval_runs",
            "instructions",
        ],
        "{restored}"
    );
    assert_eq!(
        p.rs.state_of("restored.db"),
        source,
        "the binary's own restore lost or changed rows"
    );

    let Some(py_machine) = &p.py else {
        ctx.skip(T, "backup and restore against the reference scripts");
        return;
    };
    let py = py.unwrap();
    let py_name = format!("{}-supercharged-memory.sql.gz", today());
    assert_eq!(
        py.ok(),
        format!("[backup] ok: {{ROOT}}/backups/{py_name}\n")
    );
    assert_eq!(py_machine.list("backups"), std::slice::from_ref(&py_name));
    ctx.asserted(&BACKUP_OUTPUT);
    let py_dump_path = py_machine.path(&format!("backups/{py_name}"));
    // The dump itself is tursodb's .dump, byte for byte.
    assert!(
        gunzip(&rs_dump_path) == gunzip(&py_dump_path),
        "the binary's dump differs from tursodb's .dump of the same database"
    );

    let own = py_machine.run(&cmd(&["restore", "--out", "{ROOT}/restored.db"]));
    assert!(
        own.ok()
            .ends_with("\nRESTORE OK — every row in the dump is present in {ROOT}/restored.db\n"),
        "{own}"
    );
    let reference_order: Vec<&str> = own
        .stdout
        .lines()
        .skip_while(|l| !l.starts_with("table "))
        .skip(1)
        .take_while(|l| !l.is_empty())
        .map(|l| l.split_whitespace().next().unwrap())
        .collect();
    let mut sorted = table_lines.clone();
    sorted.sort();
    assert_eq!(
        reference_order, sorted,
        "the reference lists its tables by name:\n{own}"
    );
    ctx.asserted(&RESTORE_REPORT);
    assert_eq!(
        py_machine.state_of("restored.db"),
        source,
        "the reference's own restore"
    );

    // Each side restores the other's dump.
    let cross = p.rs.run(&cmd(&[
        "restore",
        "--dump",
        &py_dump_path.display().to_string(),
        "--out",
        "{ROOT}/cross.db",
    ]));
    assert!(cross.ok().contains("\nRESTORE OK"), "{cross}");
    assert_eq!(
        p.rs.state_of("cross.db"),
        source,
        "the binary restoring the reference's dump"
    );
    let cross = py_machine.run(&cmd(&[
        "restore",
        "--dump",
        &rs_dump_path.display().to_string(),
        "--out",
        "{ROOT}/cross.db",
    ]));
    assert!(cross.ok().contains("\nRESTORE OK"), "{cross}");
    assert_eq!(
        py_machine.state_of("cross.db"),
        source,
        "the reference restoring the binary's dump"
    );
    p.same_state("after backup and restore");
}

fn gunzip(gz: &std::path::Path) -> String {
    let mut sql = String::new();
    flate2::read::GzDecoder::new(std::fs::File::open(gz).unwrap())
        .read_to_string(&mut sql)
        .unwrap();
    sql
}

/// The dump as text, cut inside the literal of a row's text so the tail is
/// an unterminated string.
fn cut_dump(gz: &std::path::Path) -> String {
    let sql = gunzip(gz);
    let at = sql
        .find("Keywords: turso, busy")
        .expect("row 8 in the dump");
    sql[..at].to_string()
}

fn restore_refuses_the_live_database_and_a_cut_off_dump(ctx: &Ctx) {
    let p = ctx.pair(T, "refusals", Fixture::Full);
    let source = p.rs.state();
    p.run(&cmd(&["backup"]));

    // The live database as --out: never, not even with --force.
    let live = cmd(&[
        "restore",
        "--out",
        "{ROOT}/data/turso/supercharged-memory.db",
    ]);
    let (rs, py) = p.run(&live);
    assert!(rs.refused().contains("is the configured database"), "{rs}");
    let forced = p.rs.run(&cmd(&[
        "restore",
        "--out",
        "{ROOT}/data/turso/supercharged-memory.db",
        "--force",
    ]));
    assert!(
        forced.refused().contains("not even with --force"),
        "{forced}"
    );
    assert_eq!(
        p.rs.state(),
        source,
        "a refused restore changed the live database"
    );
    match py {
        Some(py) => {
            assert_eq!(py.code, Some(1), "{py}");
            assert!(
                py.stderr.starts_with(
                    "refusing to write to existing file: {ROOT}/data/turso/supercharged-memory.db"
                ),
                "{py}"
            );
            ctx.asserted(&RESTORE_LIVE);
        }
        None => ctx.skip(T, "restore over the live database"),
    }
    p.same_state("after the refused restores");

    // A dump cut off inside a string literal: nothing may be left at --out.
    let dump = p.rs.path(&format!(
        "backups/{}-supercharged-memory-native.sql.gz",
        today()
    ));
    let cut = cut_dump(&dump);
    p.write("cut.sql", &cut);
    let (rs, py) = p.run(&cmd(&[
        "restore",
        "--dump",
        "{ROOT}/cut.sql",
        "--out",
        "{ROOT}/cut.db",
    ]));
    let err = rs.failed();
    assert!(err.contains("RESTORE INCOMPLETE"), "{rs}");
    assert!(rs.stdout.contains("<<< MISMATCH"), "{rs}");
    assert!(
        p.rs.list("").iter().all(|f| !f.starts_with("cut.db")),
        "the binary left a file at --out"
    );
    match py {
        Some(py) => {
            assert_eq!(py.code, Some(1), "{py}");
            assert_eq!(
                py.stderr, "dump ends inside an unterminated string literal — truncated file?\n",
                "{py}"
            );
            let py_machine = p.py.as_ref().unwrap();
            assert!(
                py_machine.list("").iter().all(|f| !f.starts_with("cut.db")),
                "the reference left a file"
            );
            ctx.asserted(&RESTORE_TRUNCATED);
        }
        None => ctx.skip(T, "restore of a cut-off dump"),
    }
}

fn restore_force_keeps_the_target_and_the_default_dump_is_gzip(ctx: &Ctx) {
    let p = ctx.pair(T, "force", Fixture::Full);
    p.run(&cmd(&["backup"]));
    let dump = gunzip(&p.rs.path(&format!(
        "backups/{}-supercharged-memory-native.sql.gz",
        today()
    )));

    // --force onto a scratch file, with a dump that cannot be restored.
    p.place(Fixture::Schema, "scratch.db");
    let cut = &dump[..dump.find("Keywords: turso, busy").unwrap()];
    p.write("cut.sql", cut);
    let force = cmd(&[
        "restore",
        "--dump",
        "{ROOT}/cut.sql",
        "--out",
        "{ROOT}/scratch.db",
        "--force",
    ]);
    let (rs, py) = p.run(&force);
    rs.failed();
    assert_eq!(
        p.rs.state_of("scratch.db"),
        ctx.fixture_state(Fixture::Schema),
        "a failed --force restore touched its target"
    );
    match py {
        Some(py) => {
            assert_eq!(py.code, Some(1), "{py}");
            let py_machine = p.py.as_ref().unwrap();
            assert!(
                !py_machine.path("scratch.db").exists(),
                "the reference kept its target"
            );
            ctx.asserted(&RESTORE_FORCE);
        }
        None => ctx.skip(T, "restore --force with a dump that fails"),
    }

    // BACKUP_DIR holding only a plain .sql dump.
    let plain = ctx.pair(T, "plain-dump", Fixture::Full);
    plain.write("backups/2025-12-01-supercharged-memory.sql", &dump);
    let (rs, py) = plain.run(&cmd(&["restore", "--out", "{ROOT}/plain.db"]));
    assert!(
        rs.refused().contains("*supercharged-memory*.sql.gz"),
        "{rs}"
    );
    assert!(!plain.rs.path("plain.db").exists());
    match py {
        Some(py) => {
            assert!(py.ok().contains("\nRESTORE OK"), "{py}");
            ctx.asserted(&RESTORE_DEFAULT_DUMP);
        }
        None => ctx.skip(T, "restore from a plain .sql dump by default"),
    }
}
