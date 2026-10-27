//! MIGRATING-FROM-PYTHON.md is the one document a machine installed from a
//! checkout follows to move onto the binary. Every command it hands the reader
//! must be a command this binary has.

use std::fs;
use std::path::Path;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_supercharged-memory");
const FILE: &str = "MIGRATING-FROM-PYTHON.md";

fn notice() -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(FILE))
        .unwrap_or_else(|e| panic!("{FILE} must exist at the repository root: {e}"))
}

/// What the binary prints for `<args>`; nothing inherited, no database.
fn run(args: &[&str]) -> (Option<i32>, String) {
    let home = Path::new(env!("CARGO_TARGET_TMPDIR")).join("migrating-home");
    fs::create_dir_all(&home).unwrap();
    let out = Command::new(BIN)
        .env_clear()
        .env("HOME", &home)
        .env("OLLAMA_URL", "http://127.0.0.1:9")
        .args(args)
        .output()
        .unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// Every code span and fenced line of the notice, one string each.
fn code(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut fenced = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
        } else if fenced {
            found.push(line.trim().to_string());
        } else {
            found.extend(
                line.split('`')
                    .skip(1)
                    .step_by(2)
                    .map(|s| s.trim().to_string()),
            );
        }
    }
    found
}

/// `(subcommand, flags)` for each `supercharged-memory <subcommand> ...` in code.
fn commands(text: &str) -> Vec<(String, Vec<String>)> {
    let mut found = Vec::new();
    for span in code(text) {
        let mut rest = span.as_str();
        while let Some(at) = rest.find("supercharged-memory ") {
            rest = &rest[at + "supercharged-memory ".len()..];
            let sub: String = rest
                .chars()
                .take_while(|c| c.is_ascii_lowercase() || *c == '-')
                .collect();
            if sub.is_empty() {
                continue;
            }
            let flags = rest
                .split_whitespace()
                .filter_map(|w| w.strip_prefix("--"))
                .map(|w| w.trim_end_matches(['.', ',', ';', ')']).to_string())
                .filter(|w| !w.is_empty())
                .collect();
            found.push((sub, flags));
        }
    }
    found
}

#[test]
fn the_notice_exists_and_has_every_section() {
    let t = notice();
    for heading in [
        "what changes",
        "what stays the same",
        "before you start",
        "steps",
        "verify",
        "managed block",
        "optional",
        "roll",
    ] {
        assert!(
            t.lines()
                .any(|l| l.starts_with('#') && l.to_lowercase().contains(heading)),
            "no heading mentions {heading:?}"
        );
    }
}

#[test]
fn every_command_the_notice_names_exists() {
    let found = commands(&notice());
    assert!(found.len() >= 10, "only {} commands found", found.len());
    let mut offenders = Vec::new();
    for (sub, flags) in &found {
        let (code, help) = run(&[sub, "--help"]);
        if code != Some(0) {
            offenders.push(format!("no subcommand {sub:?}"));
            continue;
        }
        for flag in flags {
            if flag != "help" && !help.contains(&format!("--{flag}")) {
                offenders.push(format!("`{sub}` has no --{flag}"));
            }
        }
    }
    assert!(offenders.is_empty(), "{}", offenders.join("\n"));
}

#[test]
fn every_subcommand_the_notice_depends_on_is_named() {
    let named: Vec<String> = commands(&notice()).into_iter().map(|(s, _)| s).collect();
    for want in [
        "init", "config", "status", "update", "backup", "restore", "recall",
    ] {
        assert!(
            named.iter().any(|s| s == want),
            "the notice never shows `{want}`"
        );
    }
}

#[test]
fn the_notice_names_the_python_commands_it_uses_as_they_are_in_the_repository() {
    let t = notice();
    for script in [
        "scripts/supercharged-memory-backup.sh",
        "scripts/restore.py",
    ] {
        assert!(t.contains(script), "the notice never names {script}");
        assert!(
            Path::new(env!("CARGO_MANIFEST_DIR")).join(script).exists(),
            "{script} is not in the repository"
        );
    }
}

#[test]
fn the_notice_never_tells_anyone_to_delete_the_scripts_or_the_database() {
    let t = notice().to_lowercase();
    for bad in [
        "rm -r",
        "rm scripts",
        "delete the scripts",
        "git rm",
        "remove the scripts",
    ] {
        assert!(!t.contains(bad), "the notice says {bad:?}");
    }
    assert!(t.contains("stay in the repository") || t.contains("remain in the repository"));
}

#[test]
fn the_notice_states_the_facts_the_code_decides() {
    let t = notice();
    for want in [
        "same file",
        "beside",
        ".env",
        "instructions",
        "BACKUP_DIR",
        "settings.json",
        "install-claude-md.sh",
        "--readonly",
    ] {
        assert!(t.contains(want), "the notice lacks {want:?}");
    }
}

#[test]
fn the_notice_has_no_release_url_and_no_working_notes() {
    let t = notice();
    assert!(!t.contains("http"), "the notice carries a URL");
    assert!(
        !t.to_lowercase().contains("story"),
        "the notice cites a story"
    );
}
