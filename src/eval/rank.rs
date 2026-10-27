//! The rankers the harness compares, as pure functions.
//!
//! `shipped` is the ranking `recall` applies; the other three exist only to
//! justify it. The frozen legacy ranker keeps its own tokenizer and stop-word
//! list on purpose: borrowing `recall`'s tokens would make the baseline track
//! the new code and destroy the comparison the harness exists to make.

use crate::recall::rank::{candidate_tokens, kw_total, like_pattern, round6, token_weights};

/// One row a query can be ranked against: its cosine distance to the query,
/// and its lowercased text.
#[derive(Debug, Clone, Copy)]
pub struct Candidate<'a> {
    pub id: i64,
    pub dist: f64,
    pub text: &'a str,
}

/// Words `recall` stopped filtering on 2026-08-18. Frozen.
const LEGACY_STOP: [&str; 56] = [
    "the", "and", "for", "you", "with", "this", "that", "are", "was", "not", "but", "can", "how",
    "why", "what", "when", "does", "should", "into", "from", "your", "have", "has", "will",
    "would", "about", "them", "then", "there", "here", "which", "were", "been", "und", "der",
    "die", "das", "den", "dem", "ein", "eine", "ist", "wie", "was", "mit", "für", "nicht", "auch",
    "oder", "aber", "wird", "sich", "dass", "man", "noch", "nur",
];

/// The frozen legacy tokenizer: words of at least four characters that are not
/// stop words, plus any word holding a digit; distinct, in query order, the
/// first eight.
pub fn legacy_tokens(query: &str) -> Vec<String> {
    let lowered = query.to_lowercase();
    let mut out: Vec<String> = Vec::new();
    for tok in lowered.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        let wanted = tok.chars().any(|c| c.is_numeric())
            || (tok.chars().count() >= 4 && !LEGACY_STOP.contains(&tok));
        if wanted && !out.iter().any(|t| t == tok) {
            out.push(tok.to_string());
        }
    }
    out.truncate(8);
    out
}

/// Every id, ranked by what `recall` would do at this `alpha`: cosine distance
/// minus alpha times the IDF-weighted keyword share, lowest first. Ties keep
/// the order the candidates came in.
///
/// The tokens, their IDF over these rows and the weights rounded to six
/// decimals are `recall`'s own, so only the sort is done here.
pub fn shipped(query: &str, rows: &[Candidate], alpha: f64) -> Vec<i64> {
    let candidates = candidate_tokens(query);
    let dfs: Vec<u64> = candidates
        .iter()
        .map(|t| {
            let needle = literal(t);
            rows.iter().filter(|r| r.text.contains(&needle)).count() as u64
        })
        .collect();
    let tokens = token_weights(&candidates, &dfs, rows.len() as u64);
    let total = kw_total(&tokens);
    let kw = |r: &Candidate| -> f64 {
        tokens
            .iter()
            .filter(|t| r.text.contains(&literal(&t.token)))
            .map(|t| round6(t.weight))
            .sum::<f64>()
            / total
    };
    let mut scored: Vec<(f64, i64)> = rows
        .iter()
        .map(|r| {
            let credit = if tokens.is_empty() { 0.0 } else { kw(r) };
            (r.dist - alpha * credit, r.id)
        })
        .collect();
    scored.sort_by(|a, b| a.0.total_cmp(&b.0));
    scored.into_iter().map(|(_, id)| id).collect()
}

/// What a token's LIKE pattern matches: the token with `%` and `_` stripped,
/// as `recall` strips them.
fn literal(token: &str) -> String {
    like_pattern(token).trim_matches('%').to_string()
}

/// The ranker `recall` replaced: most legacy tokens found first, distance
/// within a tie.
pub fn legacy(query: &str, rows: &[Candidate]) -> Vec<i64> {
    let tokens = legacy_tokens(query);
    let mut scored: Vec<(usize, f64, i64)> = rows
        .iter()
        .map(|r| {
            let found = tokens
                .iter()
                .filter(|t| r.text.contains(t.as_str()))
                .count();
            (found, r.dist, r.id)
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.total_cmp(&b.1)));
    scored.into_iter().map(|(_, _, id)| id).collect()
}

/// Cosine distance alone.
pub fn vector_only(rows: &[Candidate]) -> Vec<i64> {
    let mut scored: Vec<(f64, i64)> = rows.iter().map(|r| (r.dist, r.id)).collect();
    scored.sort_by(|a, b| a.0.total_cmp(&b.0));
    scored.into_iter().map(|(_, id)| id).collect()
}

/// Legacy tokens weighted by IDF, highest keyword weight first, distance
/// within a tie.
pub fn idf_lexicographic(query: &str, rows: &[Candidate]) -> Vec<i64> {
    let tokens = legacy_tokens(query);
    let n = rows.len() as f64;
    let weights: Vec<f64> = tokens
        .iter()
        .map(|t| {
            let df = rows.iter().filter(|r| r.text.contains(t.as_str())).count() as f64;
            ((n + 1.0) / (df + 1.0)).ln()
        })
        .collect();
    let mut scored: Vec<(f64, f64, i64)> = rows
        .iter()
        .map(|r| {
            let kw: f64 = tokens
                .iter()
                .zip(&weights)
                .filter(|(t, _)| r.text.contains(t.as_str()))
                .map(|(_, w)| w)
                .sum();
            (kw, r.dist, r.id)
        })
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.total_cmp(&b.1)));
    scored.into_iter().map(|(_, _, id)| id).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows<'a>(spec: &[(i64, f64, &'a str)]) -> Vec<Candidate<'a>> {
        spec.iter()
            .map(|&(id, dist, text)| Candidate { id, dist, text })
            .collect()
    }

    #[test]
    fn legacy_tokens_drop_stop_words_and_short_words_but_keep_digits() {
        assert_eq!(
            legacy_tokens("How does the WAL flag work with Turso v0.7.2 and the wal again"),
            ["flag", "work", "turso", "v0", "7", "2", "again"]
        );
    }

    #[test]
    fn legacy_tokens_stop_at_eight() {
        let q = "t1 t2 t3 t4 t5 t6 t7 t8 t9 t10";
        assert_eq!(
            legacy_tokens(q),
            ["t1", "t2", "t3", "t4", "t5", "t6", "t7", "t8"]
        );
    }

    #[test]
    fn vector_only_ranks_by_distance_and_keeps_input_order_on_a_tie() {
        let r = rows(&[(1, 0.3, "a"), (2, 0.1, "b"), (3, 0.3, "c"), (4, 0.2, "d")]);
        assert_eq!(vector_only(&r), [2, 4, 1, 3]);
    }

    #[test]
    fn shipped_at_alpha_zero_is_the_vector_order() {
        let r = rows(&[
            (1, 0.30, "the turso wal flag"),
            (2, 0.20, "zsh quoting"),
            (3, 0.25, "something else"),
        ]);
        assert_eq!(shipped("turso", &r, 0.0), [2, 3, 1]);
    }

    #[test]
    fn shipped_lets_a_rare_keyword_pull_a_farther_row_up() {
        let r = rows(&[
            (1, 0.30, "the turso wal flag"),
            (2, 0.20, "zsh quoting"),
            (3, 0.25, "something else"),
        ]);
        // 0.30 - 0.15 * 1.0 = 0.15, ahead of 0.20 and 0.25.
        assert_eq!(shipped("turso", &r, 0.15), [1, 2, 3]);
    }

    #[test]
    fn shipped_gives_a_word_in_every_row_no_credit() {
        let r = rows(&[
            (1, 0.30, "common words"),
            (2, 0.20, "common ground"),
            (3, 0.25, "common sense"),
        ]);
        // IDF of a word every row holds is ln(4/4) = 0, however large alpha is.
        assert_eq!(shipped("common", &r, 5.0), [2, 3, 1]);
    }

    #[test]
    fn shipped_with_no_tokens_ranks_by_distance() {
        let r = rows(&[(1, 0.3, "a"), (2, 0.1, "b")]);
        assert_eq!(shipped("", &r, 0.15), [2, 1]);
        assert_eq!(shipped("  ?! ", &r, 0.15), [2, 1]);
    }

    #[test]
    fn shipped_weighs_the_eight_rarest_tokens_and_normalises_to_one() {
        // A row holding every query token earns exactly alpha, however many
        // tokens there are: 0.40 - 0.2 = 0.20 beats 0.30.
        let r = rows(&[(1, 0.40, "aa bb cc dd"), (2, 0.30, "zz")]);
        assert_eq!(shipped("aa bb cc dd", &r, 0.2), [1, 2]);
        assert_eq!(shipped("aa bb cc dd", &r, 0.05), [2, 1]);
    }

    #[test]
    fn the_legacy_ranker_counts_tokens_before_it_looks_at_distance() {
        let r = rows(&[
            (1, 0.50, "turso flag"),
            (2, 0.10, "turso only"),
            (3, 0.05, "nothing"),
        ]);
        assert_eq!(legacy("turso flag", &r), [1, 2, 3]);
    }

    #[test]
    fn the_idf_lexicographic_ranker_prefers_one_rare_word_over_two_common_ones() {
        let r = rows(&[
            (1, 0.9, "rareword"),
            (2, 0.5, "common1 common2"),
            (3, 0.4, "common1 common2 filler"),
        ]);
        let q = "rareword common1 common2";
        assert_eq!(legacy(q, &r), [3, 2, 1], "counting alone favours the pair");
        assert_eq!(idf_lexicographic(q, &r), [1, 3, 2]);
    }
}
