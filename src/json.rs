//! JSON output, byte-compatible with Python's `json.dumps(value, indent=1)`.
//!
//! The probes the runbooks gate on printed their reports through that call, so
//! the rewrite reproduces it rather than inventing a tidier layout: one space per
//! nesting level, `,` at the end of a line, `": "` between key and value, `[]` and
//! `{}` for empty containers, and every character outside printable ASCII escaped
//! as `\uXXXX` (`ensure_ascii`), as a surrogate pair above the BMP.
//!
//! Its own type rather than `serde_json::Value`, whose objects sort their keys
//! (no `preserve_order`): the reference prints them in insertion order, and a
//! reader comparing the two field by field should not have to re-sort.

use std::fmt::Write;

/// A JSON value whose objects keep their keys in insertion order.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Int(i64),
    /// Printed the way Python's `repr` prints a float, which is what
    /// `json.dumps` writes.
    Float(f64),
    Str(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    pub fn object<K: Into<String>>(pairs: impl IntoIterator<Item = (K, Json)>) -> Json {
        Json::Object(pairs.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }
}

impl From<bool> for Json {
    fn from(b: bool) -> Self {
        Json::Bool(b)
    }
}

impl From<usize> for Json {
    fn from(n: usize) -> Self {
        Json::Int(n as i64)
    }
}

impl From<&str> for Json {
    fn from(s: &str) -> Self {
        Json::Str(s.to_string())
    }
}

impl From<String> for Json {
    fn from(s: String) -> Self {
        Json::Str(s)
    }
}

impl<T: Into<Json>> From<Option<T>> for Json {
    fn from(v: Option<T>) -> Self {
        v.map_or(Json::Null, Into::into)
    }
}

impl<T: Into<Json>> From<Vec<T>> for Json {
    fn from(v: Vec<T>) -> Self {
        Json::Array(v.into_iter().map(Into::into).collect())
    }
}

/// `json.dumps(value, indent=1)`, without the trailing newline `print` adds.
pub fn dumps(value: &Json) -> String {
    let mut out = String::new();
    write_value(&mut out, value, 0);
    out
}

fn write_value(out: &mut String, value: &Json, level: usize) {
    match value {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Json::Int(n) => {
            let _ = write!(out, "{n}");
        }
        Json::Float(x) => out.push_str(&python_float(*x)),
        Json::Str(s) => write_str(out, s),
        Json::Array(items) if items.is_empty() => out.push_str("[]"),
        Json::Object(pairs) if pairs.is_empty() => out.push_str("{}"),
        Json::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                separate(out, i, level + 1);
                write_value(out, item, level + 1);
            }
            close(out, level, ']');
        }
        Json::Object(pairs) => {
            out.push('{');
            for (i, (key, item)) in pairs.iter().enumerate() {
                separate(out, i, level + 1);
                write_str(out, key);
                out.push_str(": ");
                write_value(out, item, level + 1);
            }
            close(out, level, '}');
        }
    }
}

/// Python's `repr(float)`: the shortest digits that round-trip, positional
/// from 1e-4 up to (not including) 1e16 and scientific outside it, with a
/// `.0` on an integral value; `NaN` and `Infinity` as `json.dumps` spells them.
pub fn python_float(x: f64) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    // `{:e}` is Rust's shortest round-trip digits, e.g. `-2.2e-1`.
    let sci = format!("{x:e}");
    let (mantissa, exp) = sci.split_once('e').expect("{:e} has an exponent");
    let exp: i32 = exp.parse().expect("the exponent is an integer");
    let (sign, mantissa) = match mantissa.strip_prefix('-') {
        Some(m) => ("-", m),
        None => ("", mantissa),
    };
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    if !(-4..16).contains(&exp) {
        let (first, rest) = digits.split_at(1);
        let rest = if rest.is_empty() {
            String::new()
        } else {
            format!(".{rest}")
        };
        let exp_sign = if exp < 0 { '-' } else { '+' };
        return format!("{sign}{first}{rest}e{exp_sign}{:02}", exp.abs());
    }
    if exp < 0 {
        let zeros = "0".repeat((-exp - 1) as usize);
        return format!("{sign}0.{zeros}{digits}");
    }
    let point = exp as usize + 1;
    if digits.len() <= point {
        format!("{sign}{digits}{}.0", "0".repeat(point - digits.len()))
    } else {
        format!("{sign}{}.{}", &digits[..point], &digits[point..])
    }
}

/// The `,` after the previous item, then a new line indented to `level`.
fn separate(out: &mut String, i: usize, level: usize) {
    if i > 0 {
        out.push(',');
    }
    out.push('\n');
    out.push_str(&" ".repeat(level));
}

fn close(out: &mut String, level: usize, bracket: char) {
    out.push('\n');
    out.push_str(&" ".repeat(level));
    out.push(bracket);
}

/// A quoted string, escaped the way `ensure_ascii` escapes it: the short forms
/// for the usual control characters, `\u` with lowercase hex for everything else
/// outside `' '..='~'`.
fn write_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' => out.push(c),
            _ => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    let _ = write!(out, "\\u{unit:04x}");
                }
            }
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every expected string below is Python 3's own output for the same value.

    #[test]
    fn nesting_and_empty_containers_are_laid_out_like_python() {
        let v = Json::object([
            ("a", Json::Array(vec![])),
            ("b", Json::Object(vec![])),
            (
                "c",
                Json::Array(vec![
                    Json::Int(1),
                    Json::object([("d", Json::Null), ("e", true.into()), ("f", false.into())]),
                ]),
            ),
            ("n", Json::Int(-3)),
            ("s", "x".into()),
        ]);
        assert_eq!(
            dumps(&v),
            "{\n \"a\": [],\n \"b\": {},\n \"c\": [\n  1,\n  {\n   \"d\": null,\n   \"e\": true,\n   \"f\": false\n  }\n ],\n \"n\": -3,\n \"s\": \"x\"\n}"
        );
        assert_eq!(
            dumps(&Json::Array(vec![
                Json::Array(vec![]),
                Json::Object(vec![]),
                Json::Array(vec![Json::Array(vec![])]),
            ])),
            "[\n [],\n {},\n [\n  []\n ]\n]"
        );
        assert_eq!(dumps(&Json::Array(vec![])), "[]");
        assert_eq!(dumps(&Json::Object(vec![])), "{}");
        assert_eq!(dumps(&Json::Null), "null");
    }

    /// `ensure_ascii`: anything outside printable ASCII is a `\u` escape, with a
    /// surrogate pair above the BMP; `/` is left alone.
    #[test]
    fn strings_are_escaped_like_ensure_ascii() {
        assert_eq!(
            dumps(&"q\"b\\ n\n t\t r\r b\u{8} f\u{c} c\u{1}\u{1f} del\u{7f} ü € 😀 / ~".into()),
            "\"q\\\"b\\\\ n\\n t\\t r\\r b\\b f\\f c\\u0001\\u001f del\\u007f \\u00fc \\u20ac \\ud83d\\ude00 / ~\""
        );
    }

    #[test]
    fn keys_are_escaped_too_and_keep_their_order() {
        let v = Json::object([("z\"ü", Json::Int(1)), ("a", Json::Int(2))]);
        assert_eq!(dumps(&v), "{\n \"z\\\"\\u00fc\": 1,\n \"a\": 2\n}");
    }

    /// Every expectation is Python 3's `json.dumps` of the same float.
    #[test]
    fn floats_print_like_python_repr() {
        for (x, want) in [
            (0.22, "0.22"),
            (0.25, "0.25"),
            (0.15, "0.15"),
            (1.0, "1.0"),
            (2.0, "2.0"),
            (100.0, "100.0"),
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (-2.5, "-2.5"),
            (1234.5, "1234.5"),
            (0.1 + 0.2, "0.30000000000000004"),
            (0.0001, "0.0001"),
            (0.00012, "0.00012"),
            (9.5e-5, "9.5e-05"),
            (1e-5, "1e-05"),
            (1.5e-7, "1.5e-07"),
            (5e-324, "5e-324"),
            (1e15, "1000000000000000.0"),
            (9999999999999998.0, "9999999999999998.0"),
            (123456789012345.6, "123456789012345.6"),
            (1e16, "1e+16"),
            (1e22, "1e+22"),
            (f64::MAX, "1.7976931348623157e+308"),
            (f64::NAN, "NaN"),
            (f64::INFINITY, "Infinity"),
            (f64::NEG_INFINITY, "-Infinity"),
        ] {
            assert_eq!(python_float(x), want, "{x:e}");
        }
        assert_eq!(
            dumps(&Json::object([("threshold", Json::Float(0.22))])),
            "{\n \"threshold\": 0.22\n}"
        );
    }

    #[test]
    fn options_and_vectors_convert() {
        assert_eq!(Json::from(None::<usize>), Json::Null);
        assert_eq!(Json::from(Some(3usize)), Json::Int(3));
        assert_eq!(
            Json::from(vec!["a", "b"]),
            Json::Array(vec!["a".into(), "b".into()])
        );
    }
}
