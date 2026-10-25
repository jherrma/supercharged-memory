//! Release versions and their order: semantic versioning's precedence rules.
//!
//! `update` compares the version the `.env` records with its own, and orders
//! the migration notes compiled into it, by this order alone. A version that
//! does not parse is refused, never guessed at: a note keyed by a typo would
//! otherwise be printed in the wrong place, or never.

use std::cmp::Ordering;
use std::fmt;

/// A parsed version. Build metadata (`+…`) is kept for display only: it has no
/// place in the order.
#[derive(Debug, Clone)]
pub struct Version {
    text: String,
    core: [u64; 3],
    /// Pre-release identifiers (`1.0.0-rc.1` holds `rc`, `1`); empty for a release.
    pre: Vec<Ident>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Ident {
    Number(u64),
    Text(String),
}

impl Version {
    /// `MAJOR.MINOR.PATCH`, optionally `-pre.release` and `+build`, exactly as
    /// semver 2.0.0 writes them: no `v` prefix, no surrounding whitespace, no
    /// leading zero in a number, no empty identifier.
    pub fn parse(s: &str) -> Result<Version, String> {
        let bad = |why: &str| format!("{s:?} is not a version (MAJOR.MINOR.PATCH): {why}");
        let (rest, build) = match s.split_once('+') {
            Some((rest, build)) => (rest, Some(build)),
            None => (s, None),
        };
        if let Some(build) = build {
            identifiers(build).map_err(|why| bad(&format!("build metadata {why}")))?;
        }
        let (core, pre) = match rest.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (rest, None),
        };

        let parts: Vec<&str> = core.split('.').collect();
        if parts.len() != 3 {
            return Err(bad("it needs exactly three numbers"));
        }
        let mut numbers = [0u64; 3];
        for (n, part) in numbers.iter_mut().zip(&parts) {
            *n = number(part).ok_or_else(|| bad(&format!("{part:?} is not a number")))?;
        }

        let pre = match pre {
            None => Vec::new(),
            Some(pre) => identifiers(pre)
                .map_err(|why| bad(&format!("pre-release {why}")))?
                .into_iter()
                .map(|id| {
                    if id.bytes().all(|b| b.is_ascii_digit()) {
                        number(id)
                            .map(Ident::Number)
                            .ok_or_else(|| bad(&format!("pre-release number {id:?}")))
                    } else {
                        Ok(Ident::Text(id.to_string()))
                    }
                })
                .collect::<Result<_, _>>()?,
        };
        Ok(Version {
            text: s.to_string(),
            core: numbers,
            pre,
        })
    }
}

/// A numeric identifier: ASCII digits, no leading zero, fits in a u64.
fn number(s: &str) -> Option<u64> {
    let digits = !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits || (s.len() > 1 && s.starts_with('0')) {
        return None;
    }
    s.parse().ok()
}

/// Dot-separated identifiers of `[0-9A-Za-z-]`, none empty.
fn identifiers(s: &str) -> Result<Vec<&str>, String> {
    let ids: Vec<&str> = s.split('.').collect();
    for id in &ids {
        if id.is_empty() {
            return Err("has an empty identifier".into());
        }
        if !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Err(format!(
                "{id:?} holds a character other than 0-9, A-Z, a-z, -"
            ));
        }
    }
    Ok(ids)
}

impl Ord for Version {
    /// Major, minor, patch numerically; a pre-release before its release;
    /// pre-release identifiers left to right, numbers numerically and before
    /// text, text in ASCII order, and a longer list after its own prefix.
    fn cmp(&self, other: &Self) -> Ordering {
        self.core
            .cmp(&other.core)
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => self.pre.cmp(&other.pre),
            })
    }
}

impl Ord for Ident {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Ident::Number(a), Ident::Number(b)) => a.cmp(b),
            (Ident::Number(_), Ident::Text(_)) => Ordering::Less,
            (Ident::Text(_), Ident::Number(_)) => Ordering::Greater,
            (Ident::Text(a), Ident::Text(b)) => a.cmp(b),
        }
    }
}

impl PartialOrd for Ident {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Equal precedence: two versions that differ only in build metadata are the
/// same release.
impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Version {}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap_or_else(|e| panic!("{s:?} should parse: {e}"))
    }

    #[test]
    fn versions_compare_by_precedence() {
        let cases: &[(&str, &str, Ordering)] = &[
            // equal
            ("0.1.0", "0.1.0", Ordering::Equal),
            ("1.2.3", "1.2.3+build.7", Ordering::Equal),
            ("1.0.0-rc.1+a", "1.0.0-rc.1+b", Ordering::Equal),
            // newer / older, each part numerically
            ("0.2.0", "0.1.0", Ordering::Greater),
            ("0.1.0", "0.2.0", Ordering::Less),
            ("0.10.0", "0.9.0", Ordering::Greater),
            ("1.0.0", "0.99.99", Ordering::Greater),
            ("0.1.10", "0.1.9", Ordering::Greater),
            ("2.0.0", "10.0.0", Ordering::Less),
            // a pre-release comes before its release
            ("1.0.0-alpha", "1.0.0", Ordering::Less),
            ("1.0.0", "1.0.0-rc.1", Ordering::Greater),
            ("1.0.0-rc.1", "0.9.9", Ordering::Greater),
        ];
        for (a, b, want) in cases {
            assert_eq!(v(a).cmp(&v(b)), *want, "{a} vs {b}");
            assert_eq!(v(b).cmp(&v(a)), want.reverse(), "{b} vs {a}");
        }
    }

    /// semver.org's own example chain, every pair in order.
    #[test]
    fn pre_releases_order_as_the_specification_lists_them() {
        let chain = [
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-alpha.beta",
            "1.0.0-beta",
            "1.0.0-beta.2",
            "1.0.0-beta.11",
            "1.0.0-rc.1",
            "1.0.0",
        ];
        for (i, a) in chain.iter().enumerate() {
            for b in &chain[i + 1..] {
                assert!(v(a) < v(b), "{a} < {b}");
            }
        }
    }

    #[test]
    fn a_malformed_version_is_refused() {
        for bad in [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "v1.2.3",
            " 1.2.3",
            "1.2.3 ",
            "01.2.3",
            "1.02.3",
            "1.2.x",
            "1.2.-3",
            "1.2.3-",
            "1.2.3-a..b",
            "1.2.3-01",
            "1.2.3-a_b",
            "1.2.3+",
            "1.2.3+a..b",
            "99999999999999999999.0.0",
        ] {
            let err = Version::parse(bad).expect_err(bad);
            assert!(err.contains(&format!("{bad:?}")), "{bad:?}: {err}");
        }
    }

    #[test]
    fn a_version_displays_as_written() {
        assert_eq!(v("1.0.0-rc.1+build.5").to_string(), "1.0.0-rc.1+build.5");
        // Leading zeros are allowed in build metadata, which has no order.
        assert_eq!(v("1.0.0+007").to_string(), "1.0.0+007");
    }
}
