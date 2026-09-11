//! The guards, as pure functions.
//!
//! Every one of these is a refusal the Python writer made before paying for an
//! embedding, and each exists because the failure it prevents is INVISIBLE
//! afterwards: a row holding only its keywords, a date SQLite keeps verbatim and
//! every date function then reads as NULL, a second embedding model quietly
//! entering the corpus. Keeping them here means they are tested without a
//! database, without Ollama, and without a process.

use crate::config::MAX_TEXT;
use crate::error::{Error, Result};

/// `semantic_memory.category` -- the schema CHECK, restated so a typo is a
/// refusal that names the alternatives instead of a constraint violation.
pub const CATEGORIES: [&str; 6] = [
    "baseline",
    "user",
    "feedback",
    "project",
    "reference",
    "pattern",
];
/// `episodic_memory.event_type`.
pub const EVENT_TYPES: [&str; 7] = [
    "project_start",
    "bug_fix",
    "feature_complete",
    "decision",
    "milestone",
    "incident",
    "note",
];
/// `episodic_memory.importance`.
pub const IMPORTANCES: [&str; 3] = ["routine", "notable", "major"];

/// The cap the schema enforces on every metadata column.
pub const MAX_META: usize = 128;

/// `q()` in the Python client mapped both `None` and `""` to SQL NULL. Keeping
/// that here means an empty flag value cannot reach a column as an empty string,
/// which sorts and filters differently from NULL.
pub fn blank_to_none(v: Option<&str>) -> Option<String> {
    v.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// One of a fixed set, or a refusal naming the set.
pub fn one_of(flag: &str, value: &str, allowed: &[&str]) -> Result<String> {
    if allowed.contains(&value) {
        return Ok(value.to_string());
    }
    Err(Error::refused(format!(
        "{flag} '{value}' is not one of: {}. Nothing was stored.",
        allowed.join(", ")
    )))
}

/// Metadata columns are capped at 128 by a schema CHECK. Refuse before the
/// insert so the caller learns which flag was too long, not just that a
/// constraint failed.
pub fn meta_fits(flag: &str, value: &Option<String>) -> Result<()> {
    if let Some(v) = value {
        let n = v.chars().count();
        if n > MAX_META {
            return Err(Error::refused(format!(
                "{flag} is {n} chars; hard cap {MAX_META}. Nothing was stored."
            )));
        }
    }
    Ok(())
}

/// The body, trimmed, or a refusal.
///
/// Checked BEFORE the keywords go on: `--keywords` alone makes the text
/// non-empty, so a lost body would sail through as a row holding nothing but its
/// 'Keywords:' line. In practice that is `--text "$(cat file)"` where the file is
/// missing or empty, which the shell reports as nothing at all. Episodic memory
/// is append-only and never purged, so such a row is permanent -- refuse the
/// write instead.
pub fn body(text: &str) -> Result<&str> {
    let body = text.trim();
    if body.is_empty() {
        return Err(Error::refused(
            "--text is empty. If you passed a command substitution such as \
             --text \"$(cat file)\", the file is missing or empty — nothing was stored.",
        ));
    }
    Ok(body)
}

/// The body, the keywords appended into it, and the cap that covers both.
///
/// Returns the text exactly as it will be stored. `--keywords` is deliberately
/// NOT a column: appending it into `memory_text` is what makes it both embedded
/// and LIKE-searchable, and it therefore counts against the 2000-char cap.
pub fn assemble_text(text: &str, keywords: Option<&str>) -> Result<String> {
    let body = body(text)?;
    // Measured in CHARACTERS, not bytes: the schema's CHECK(length(...)) counts
    // characters too, and this corpus holds German. A byte count would refuse
    // texts the database would have accepted.
    let body_len = body.chars().count();
    let stored = match keywords.map(str::trim).filter(|k| !k.is_empty()) {
        Some(k) => format!("{body}\n\nKeywords: {k}"),
        None => body.to_string(),
    };
    let total = stored.chars().count();
    if total > MAX_TEXT {
        let added = total - body_len;
        // A worker staring at a 1948-char file otherwise has no way to see where
        // "2096 chars" came from.
        let detail = if added > 0 {
            format!(
                " — {body_len} of them your --text, {added} the '--keywords' line \
                 appended into the same field"
            )
        } else {
            String::new()
        };
        return Err(Error::refused(format!(
            "memory is {total} chars; hard cap {MAX_TEXT}{detail}. \
             Tighten it or split into separate memories."
        )));
    }
    Ok(stored)
}

/// `created_at` as it will be stored, or a refusal.
///
/// The mechanical enforcer for the one column nothing else checks. `created_at`
/// is TEXT with no CHECK, so SQLite stores whatever it is handed: measured
/// 2026-09-09, `--created-at "March 2024"` was stored verbatim, `julianday()`
/// then returned NULL, the row fell through `sleep --staleness`'s bucket CASE
/// into `180d+` regardless of its real age, and `min(created_at)` sorted `'M'`
/// after `'2'` so `oldest_current` ignored it altogether. A wrong date is
/// therefore invisible rather than obviously wrong -- and the migration runbook
/// has workers BUILD this string in shell, which is where malformed values come
/// from.
///
/// Accepts `YYYY-MM-DD HH:MM:SS` (what the schema's CURRENT_TIMESTAMP writes and
/// what the date arithmetic needs) or a bare `YYYY-MM-DD`, normalised to midnight
/// so the column stays one fixed width -- `min(created_at)` is a STRING min.
/// Deliberately NOT the ISO `T` separator: julianday() takes it, but `'T' > ' '`
/// byte-wise, so a T-row sorts after every space-separated row of the same second.
pub fn normalise_created_at(raw: &str) -> Result<String> {
    // An empty value used to be treated as "not given", letting the row take
    // CURRENT_TIMESTAMP: a substitution that came back empty would date a
    // year-old fact today while the caller reported the date it meant to use.
    // Omitting the flag is how you ask for "now"; passing it empty is a bug.
    if raw.trim().is_empty() {
        return Err(Error::refused(
            "--created-at is empty. If you passed a command substitution, it produced \
             nothing — nothing was stored. Omit the flag to date the row now, or pass a \
             real 'YYYY-MM-DD HH:MM:SS'.",
        ));
    }
    match parse_stamp(raw.trim()) {
        Some(stamp) => Ok(stamp),
        None => Err(Error::refused(format!(
            "--created-at '{raw}' is not a date this system can store. Pass \
             'YYYY-MM-DD HH:MM:SS' (or a bare 'YYYY-MM-DD', stored as that day at \
             00:00:00), or omit the flag to date the row now. SQLite keeps any other \
             string verbatim and julianday() then returns NULL, so the row lands in \
             sleep --staleness's '180d+' bucket whatever its real age is and \
             min(created_at) sorts it away from oldest_current — nothing was stored."
        ))),
    }
}

/// The shape check plus a real calendar check, so `2024-13-45` is refused.
fn parse_stamp(s: &str) -> Option<String> {
    let (date, time) = match s.split_once(' ') {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };
    let d: Vec<&str> = date.split('-').collect();
    if d.len() != 3 || d[0].len() != 4 || d[1].len() != 2 || d[2].len() != 2 {
        return None;
    }
    let year: u32 = digits(d[0])?;
    let month: u32 = digits(d[1])?;
    let day: u32 = digits(d[2])?;
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    let time = match time {
        None => "00:00:00".to_string(),
        Some(t) => {
            let p: Vec<&str> = t.split(':').collect();
            if p.len() != 3 || p.iter().any(|x| x.len() != 2) {
                return None;
            }
            let (h, m, sec): (u32, u32, u32) = (digits(p[0])?, digits(p[1])?, digits(p[2])?);
            // SQLite's own range: 23:59:59 is the last valid second, no leap second.
            if h > 23 || m > 59 || sec > 59 {
                return None;
            }
            t.to_string()
        }
    };
    Some(format!("{date} {time}"))
}

/// ASCII digits only -- `parse()` alone would accept a leading `+` or Unicode
/// digits, both of which SQLite would then store verbatim.
fn digits(s: &str) -> Option<u32> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) => {
            29
        }
        2 => 28,
        _ => 0,
    }
}

/// `--supersedes` as a list of ids, or a refusal.
pub fn parse_supersedes(raw: &str) -> Result<Vec<i64>> {
    let mut ids = Vec::new();
    for part in raw.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        match part.parse::<i64>() {
            Ok(id) => ids.push(id),
            Err(_) => {
                return Err(Error::refused(format!(
                    "--supersedes '{raw}' is not a comma-separated id list."
                )));
            }
        }
    }
    if ids.is_empty() {
        return Err(Error::refused("--supersedes given with no ids."));
    }
    Ok(ids)
}

/// `--coworker` as a list of names. Empty entries are dropped, matching the
/// Python split; an all-empty value tags nothing rather than failing.
pub fn parse_coworkers(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::EXIT_REFUSED;

    fn refusal(e: Error) -> String {
        assert_eq!(
            e.exit_code(),
            EXIT_REFUSED,
            "must be a refusal, not a failure"
        );
        e.to_string()
    }

    #[test]
    fn keywords_are_appended_into_the_text_not_kept_apart() {
        let t = assemble_text("a fact", Some("turso, vfs")).unwrap();
        assert_eq!(t, "a fact\n\nKeywords: turso, vfs");
    }

    #[test]
    fn no_keywords_leaves_the_body_alone() {
        assert_eq!(assemble_text("  a fact  ", None).unwrap(), "a fact");
        assert_eq!(assemble_text("a fact", Some("  ")).unwrap(), "a fact");
    }

    /// The guard against `--text "$(cat missing-file)"`: keywords alone must not
    /// make the row look non-empty.
    #[test]
    fn an_empty_body_is_refused_even_with_keywords() {
        let msg = refusal(assemble_text("   ", Some("turso")).unwrap_err());
        assert!(msg.contains("--text is empty"), "{msg}");
    }

    #[test]
    fn the_cap_counts_the_keywords_and_says_how_much_they_added() {
        let body = "x".repeat(1990);
        let msg = refusal(assemble_text(&body, Some("alpha, beta")).unwrap_err());
        assert!(msg.contains("1990 of them your --text"), "{msg}");
        assert!(msg.contains("hard cap 2000"), "{msg}");
    }

    #[test]
    fn an_over_cap_body_with_no_keywords_omits_the_breakdown() {
        let msg = refusal(assemble_text(&"x".repeat(2001), None).unwrap_err());
        assert!(msg.contains("memory is 2001 chars"), "{msg}");
        assert!(!msg.contains("--keywords"), "{msg}");
    }

    /// Characters, not bytes: the schema's CHECK(length(...)) counts characters,
    /// so a corpus holding German must not be refused for text SQLite accepts.
    #[test]
    fn the_cap_is_measured_in_characters() {
        let body = "ä".repeat(2000); // 4000 bytes, 2000 characters
        assert!(assemble_text(&body, None).is_ok());
        assert!(assemble_text(&"ä".repeat(2001), None).is_err());
    }

    #[test]
    fn exactly_at_the_cap_is_allowed() {
        assert!(assemble_text(&"x".repeat(2000), None).is_ok());
    }

    #[test]
    fn a_bare_date_is_stored_as_that_day_at_midnight() {
        assert_eq!(
            normalise_created_at("2024-03-07").unwrap(),
            "2024-03-07 00:00:00"
        );
        assert_eq!(
            normalise_created_at("  2024-03-07 13:45:01 ").unwrap(),
            "2024-03-07 13:45:01"
        );
    }

    #[test]
    fn a_prose_date_is_refused_and_explains_the_damage() {
        let msg = refusal(normalise_created_at("March 2024").unwrap_err());
        assert!(msg.contains("julianday"), "{msg}");
        assert!(msg.contains("nothing was stored"), "{msg}");
    }

    #[test]
    fn an_empty_created_at_is_refused_rather_than_meaning_now() {
        let msg = refusal(normalise_created_at("  ").unwrap_err());
        assert!(msg.contains("--created-at is empty"), "{msg}");
    }

    /// The ISO 'T' separator parses in julianday() but sorts after every
    /// space-separated row of the same second, so it is refused on purpose.
    #[test]
    fn the_iso_t_separator_is_refused() {
        assert!(normalise_created_at("2024-03-07T13:45:01").is_err());
    }

    #[test]
    fn impossible_calendar_dates_are_refused() {
        for bad in [
            "2024-13-01",
            "2024-00-01",
            "2024-01-00",
            "2024-02-30",
            "2023-02-29",
            "2024-04-31",
            "2024-1-01",
            "24-01-01",
            "2024-01-01 24:00:00",
            "2024-01-01 12:60:00",
            "2024-01-01 12:00:60",
            "2024-01-01 12:00",
            "+024-01-01",
        ] {
            assert!(
                normalise_created_at(bad).is_err(),
                "{bad} should be refused"
            );
        }
    }

    #[test]
    fn leap_days_are_accepted_in_leap_years_only() {
        assert!(normalise_created_at("2024-02-29").is_ok());
        assert!(normalise_created_at("2000-02-29").is_ok());
        assert!(normalise_created_at("1900-02-29").is_err());
    }

    #[test]
    fn an_unknown_category_names_the_alternatives() {
        let msg = refusal(one_of("--category", "gotcha", &CATEGORIES).unwrap_err());
        assert!(msg.contains("reference"), "{msg}");
        assert!(msg.contains("Nothing was stored"), "{msg}");
        assert!(one_of("--category", "feedback", &CATEGORIES).is_ok());
    }

    #[test]
    fn event_type_and_importance_are_checked_against_the_schema_sets() {
        assert!(one_of("--event-type", "bug_fix", &EVENT_TYPES).is_ok());
        assert!(one_of("--event-type", "bugfix", &EVENT_TYPES).is_err());
        assert!(one_of("--importance", "major", &IMPORTANCES).is_ok());
        assert!(one_of("--importance", "critical", &IMPORTANCES).is_err());
    }

    #[test]
    fn an_over_long_metadata_value_is_refused_before_the_insert() {
        let long = Some("t".repeat(129));
        let msg = refusal(meta_fits("--topic", &long).unwrap_err());
        assert!(msg.contains("129 chars; hard cap 128"), "{msg}");
        assert!(meta_fits("--topic", &Some("t".repeat(128))).is_ok());
        assert!(meta_fits("--topic", &None).is_ok());
    }

    #[test]
    fn an_empty_flag_value_becomes_null_not_an_empty_string() {
        assert_eq!(blank_to_none(Some("  ")), None);
        assert_eq!(blank_to_none(None), None);
        assert_eq!(blank_to_none(Some(" x ")), Some("x".to_string()));
    }

    #[test]
    fn supersedes_parses_a_csv_and_refuses_anything_else() {
        assert_eq!(parse_supersedes("1, 2,3").unwrap(), vec![1, 2, 3]);
        assert_eq!(parse_supersedes(" 42 ").unwrap(), vec![42]);
        assert!(parse_supersedes("1,two").is_err());
        assert!(parse_supersedes(" , ").is_err());
    }

    #[test]
    fn coworkers_parse_as_a_trimmed_name_list() {
        assert_eq!(parse_coworkers("jeff, ada ,"), vec!["jeff", "ada"]);
        assert!(parse_coworkers(" , ").is_empty());
    }
}
