//! Output rendering, byte-compatible with `tursodb -m line`.
//!
//! The runbooks quote this format and an agent reads it every session, so the
//! rewrite reproduces it rather than inventing something tidier: column names
//! right-aligned to the widest, ` = `, the value, and one blank line BETWEEN rows
//! (never after the last).
//!
//! Numbers are the fiddly part. tursodb renders reals the way SQLite does --
//! `%!.15g` with a `.0` forced onto anything integral -- so `0.5` prints as `0.5`,
//! `2.0` as `2.0` and `1.0/3` as `0.333333333333333`, not Rust's 17-digit default.

use crate::store::{Cell, ResultSet};

pub fn line(rs: &ResultSet) -> String {
    if rs.rows.is_empty() {
        return String::new();
    }
    let width = rs
        .columns
        .iter()
        .map(|c| c.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for (i, row) in rs.rows.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        for (name, cell) in rs.columns.iter().zip(row) {
            let pad = width.saturating_sub(name.chars().count());
            out.push_str(&" ".repeat(pad));
            out.push_str(name);
            out.push_str(" = ");
            out.push_str(&cell.render());
            out.push('\n');
        }
    }
    out
}

impl Cell {
    pub fn render(&self) -> String {
        match self {
            Cell::Null => String::new(),
            Cell::Int(n) => n.to_string(),
            Cell::Real(x) => format_real(*x),
            Cell::Text(s) => s.clone(),
        }
    }
}

/// SQLite's `%!.15g`: fifteen significant digits, trailing zeros stripped, and a
/// `.0` appended to anything that came out looking like an integer.
pub fn format_real(x: f64) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Inf".into() } else { "-Inf".into() };
    }
    let mut s = format_g(x, 15);
    if !s.contains(['.', 'e', 'E']) {
        s.push_str(".0");
    }
    s
}

fn format_g(x: f64, precision: usize) -> String {
    if x == 0.0 {
        return "0".into();
    }
    let sci = format!("{:.*e}", precision - 1, x);
    let (mantissa, exp) = sci
        .split_once('e')
        .expect("scientific format has an exponent");
    let exp: i32 = exp.parse().expect("exponent is an integer");
    if exp < -4 || exp >= precision as i32 {
        format!(
            "{}e{}{:02}",
            strip_zeros(mantissa),
            if exp < 0 { "-" } else { "+" },
            exp.abs()
        )
    } else {
        let decimals = (precision as i32 - 1 - exp).max(0) as usize;
        strip_zeros(&format!("{x:.decimals$}"))
    }
}

fn strip_zeros(s: &str) -> String {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every expectation here was read back from `tursodb -m line` on 0.7.2.
    #[test]
    fn reals_match_sqlites_fifteen_significant_digits() {
        assert_eq!(format_real(0.5), "0.5");
        assert_eq!(format_real(0.0), "0.0");
        assert_eq!(format_real(2.0), "2.0");
        assert_eq!(format_real(1.0 / 3.0), "0.333333333333333");
        assert_eq!(format_real(-0.5031), "-0.5031");
        assert_eq!(format_real(0.175), "0.175");
    }

    #[test]
    fn very_small_and_very_large_reals_go_scientific_like_sqlite() {
        assert_eq!(format_real(1e-7), "1e-07");
        assert_eq!(format_real(1.5e20), "1.5e+20");
    }

    fn rs() -> ResultSet {
        ResultSet {
            columns: vec!["id".into(), "topic".into(), "project".into()],
            rows: vec![
                vec![
                    Cell::Int(2),
                    Cell::Text("zsh word-splitting".into()),
                    Cell::Null,
                ],
                vec![
                    Cell::Int(3),
                    Cell::Text("MISSING means wrong path".into()),
                    Cell::Text("869e".into()),
                ],
            ],
        }
    }

    /// Byte-for-byte against real tursodb output for the same query shape.
    #[test]
    fn line_mode_right_aligns_names_and_separates_rows_with_one_blank_line() {
        let want = "     id = 2\n  topic = zsh word-splitting\nproject = \n\
                    \n     id = 3\n  topic = MISSING means wrong path\nproject = 869e\n";
        assert_eq!(line(&rs()), want);
    }

    #[test]
    fn there_is_no_trailing_blank_line_after_the_last_row() {
        let out = line(&rs());
        assert!(out.ends_with("project = 869e\n"));
        assert!(!out.ends_with("\n\n"));
    }

    #[test]
    fn an_empty_result_renders_as_nothing_at_all() {
        let empty = ResultSet {
            columns: vec!["id".into()],
            rows: vec![],
        };
        assert_eq!(line(&empty), "");
    }

    #[test]
    fn null_and_empty_string_are_indistinguishable_as_they_are_in_tursodb() {
        let a = ResultSet {
            columns: vec!["p".into()],
            rows: vec![vec![Cell::Null]],
        };
        let b = ResultSet {
            columns: vec!["p".into()],
            rows: vec![vec![Cell::Text(String::new())]],
        };
        assert_eq!(line(&a), line(&b));
    }

    #[test]
    fn a_multiline_value_is_printed_raw_because_memories_contain_newlines() {
        let rs = ResultSet {
            columns: vec!["memory_text".into()],
            rows: vec![vec![Cell::Text("line one\nline two".into())]],
        };
        assert_eq!(line(&rs), "memory_text = line one\nline two\n");
    }
}
