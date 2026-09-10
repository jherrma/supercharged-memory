//! Build script: stamps the binary with the commit it was built from, the build
//! date, and the Turso SDK version read out of Cargo.lock.
//!
//! `UPDATE.md` compares the commit against the sync stamp in ~/.claude/CLAUDE.md
//! to catch a repository that moved without its binary, so this is load-bearing
//! output rather than decoration.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=Cargo.lock");
    println!("cargo:rerun-if-env-changed=SM_COMMIT");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");

    println!("cargo:rustc-env=SM_COMMIT={}", commit());
    println!("cargo:rustc-env=SM_BUILD_DATE={}", build_date());
    println!("cargo:rustc-env=SM_TURSO_VERSION={}", turso_version());
}

/// The release build script passes SM_COMMIT explicitly; a local `cargo build`
/// falls back to asking git, and says "unknown" outside a work tree.
fn commit() -> String {
    if let Ok(sha) = std::env::var("SM_COMMIT")
        && !sha.is_empty()
    {
        return sha;
    }
    let out = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let sha = String::from_utf8_lossy(&o.stdout).trim().to_string();
            let dirty = Command::new("git")
                .args(["status", "--porcelain"])
                .output()
                .map(|o| !o.stdout.is_empty())
                .unwrap_or(false);
            if dirty { format!("{sha}-dirty") } else { sha }
        }
        _ => "unknown".to_string(),
    }
}

/// UTC date, honouring SOURCE_DATE_EPOCH so a reproducible build can pin it.
fn build_date() -> String {
    let secs: u64 = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        });
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Howard Hinnant's days-to-civil algorithm. Four lines of arithmetic is a
/// better trade than a date crate for one line of --version output.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Read the linked Turso version out of Cargo.lock rather than hardcoding it,
/// so it cannot drift from what is actually compiled in. Story 003 adds the
/// dependency; until then this reports "not linked".
fn turso_version() -> String {
    let lock = match std::fs::read_to_string("Cargo.lock") {
        Ok(s) => s,
        Err(_) => return "unknown".to_string(),
    };
    let mut in_turso = false;
    for line in lock.lines() {
        let line = line.trim();
        if line == "[[package]]" {
            in_turso = false;
        } else if line == r#"name = "turso""# {
            in_turso = true;
        } else if in_turso && let Some(rest) = line.strip_prefix("version = ") {
            return rest.trim_matches('"').to_string();
        }
    }
    "not linked".to_string()
}
