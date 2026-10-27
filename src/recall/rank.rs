//! The hybrid ranking, as pure functions.
//!
//! `score = vector_distance_cos - RECALL_ALPHA * kw`, lowest first, where `kw` is
//! the IDF-weighted share of the query's tokens present in the row, normalised to
//! 0..1 so ALPHA means the same thing however long the query is.
//!
//! This replaced an `ORDER BY kw DESC, dist ASC` lexicographic sort that measured
//! WORSE than no keyword layer at all (R@5 0.84 vs 0.94 on a 32-case eval set), so
//! the shape here is load-bearing and must not be "simplified".
//!
//! Everything is separated from SQL and from the database on purpose: this is the
//! one part of the rewrite that can be wrong without anything looking broken --
//! a subtly different ranking still returns plausible rows.

/// Bound the df probe; queries longer than this are rare.
pub const MAX_CANDIDATES: usize = 24;
/// Tokens carried into the score, chosen by IDF.
pub const MAX_TOKENS: usize = 8;

/// Every distinct word token, in query order, capped.
///
/// Deliberately no length filter and no stopword list -- IDF demotes common words
/// on its own, and an earlier `len >= 4` rule was throwing away sql/wal/api/ef/pr.
pub fn candidate_tokens(query: &str) -> Vec<String> {
    let lowered = query.to_lowercase();
    let mut out: Vec<String> = Vec::new();
    for tok in lowered.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        if tok.is_empty() || out.iter().any(|s| s == tok) {
            continue;
        }
        out.push(tok.to_string());
    }
    // If the cap has to bite, cut the least promising rather than the last-typed:
    // real IDF isn't known yet, but digit-bearing and longer tokens are the cheap
    // proxy for "rare". Order is otherwise irrelevant -- the IDF sort re-orders.
    if out.len() > MAX_CANDIDATES {
        out.sort_by_key(|t| {
            (
                !t.chars().any(|c| c.is_numeric()),
                std::cmp::Reverse(t.chars().count()),
            )
        });
        out.truncate(MAX_CANDIDATES);
    }
    out
}

/// The LIKE pattern a token is searched with.
///
/// `%` and `_` are STRIPPED, not escaped -- inherited verbatim from the Python
/// implementation. It has a visible consequence worth knowing before "fixing" it:
/// `\w+` keeps underscores, so the token `memory_text` is searched as
/// `%memorytext%` and matches nothing. Changing this changes every ranking, so it
/// belongs in a measured eval run, not in a cleanup.
pub fn like_pattern(token: &str) -> String {
    format!("%{}%", token.replace(['%', '_'], ""))
}

/// A token kept for scoring, with the weight it contributes.
#[derive(Debug, Clone, PartialEq)]
pub struct WeightedToken {
    pub token: String,
    pub weight: f64,
}

/// IDF per candidate token, measured against exactly the rows this search will
/// rank, then the `MAX_TOKENS` rarest.
///
/// `dfs[i]` is how many of the `total` rows contain `candidates[i]`.
pub fn token_weights(candidates: &[String], dfs: &[u64], total: u64) -> Vec<WeightedToken> {
    if candidates.is_empty() {
        return Vec::new();
    }
    // A shape mismatch means the probe did not answer the question asked; fall
    // back to flat weights rather than ranking on a number that means nothing.
    if dfs.len() != candidates.len() {
        return candidates
            .iter()
            .take(MAX_TOKENS)
            .map(|t| WeightedToken {
                token: t.clone(),
                weight: 1.0,
            })
            .collect();
    }

    let mut weighted: Vec<WeightedToken> = candidates
        .iter()
        .zip(dfs)
        .map(|(t, df)| WeightedToken {
            token: t.clone(),
            weight: ((total as f64 + 1.0) / (*df as f64 + 1.0)).ln(),
        })
        .collect();
    // Stable, so equal weights keep candidate order -- the Python sort was stable
    // too, and ties are common on a small corpus.
    weighted.sort_by(|a, b| {
        b.weight
            .partial_cmp(&a.weight)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    weighted.truncate(MAX_TOKENS);
    weighted
}

/// The divisor that normalises the keyword score to 0..1.
///
/// Rounded to six decimals because the Python implementation formatted it into
/// the SQL text with `%.6f`, and a seventh digit here would move scores in the
/// last place -- enough to reorder a tie.
pub fn kw_total(tokens: &[WeightedToken]) -> f64 {
    let sum: f64 = tokens.iter().map(|t| t.weight).sum();
    if sum == 0.0 { 1.0 } else { round6(sum) }
}

/// Six-decimal rounding, matching the `%.6f` the weights used to be formatted with.
pub fn round6(x: f64) -> f64 {
    (x * 1_000_000.0).round() / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(q: &str) -> Vec<String> {
        candidate_tokens(q)
    }

    #[test]
    fn tokens_are_lowercased_deduped_and_kept_in_query_order() {
        assert_eq!(toks("Turso WAL turso wal DB"), vec!["turso", "wal", "db"]);
    }

    #[test]
    fn short_tokens_survive_because_idf_is_what_demotes_words() {
        // The rule this replaced (len >= 4) threw all of these away.
        assert_eq!(
            toks("sql wal api ef pr"),
            vec!["sql", "wal", "api", "ef", "pr"]
        );
    }

    #[test]
    fn punctuation_splits_and_underscores_do_not() {
        assert_eq!(
            toks("recall.py --status memory_text"),
            vec!["recall", "py", "status", "memory_text"]
        );
    }

    #[test]
    fn non_ascii_words_are_tokens_too() {
        // The corpus is half German; losing umlauts would silently cost recall.
        assert_eq!(
            toks("Datenbank umbenennen — größe"),
            vec!["datenbank", "umbenennen", "größe"]
        );
    }

    #[test]
    fn over_the_cap_the_rarest_looking_tokens_are_kept_not_the_first_typed() {
        let mut query = String::new();
        for i in 0..30 {
            query.push_str(&format!("w{i:02} ")); // 30 distinct digit-bearing tokens
        }
        query.push_str("aaaaaaaaaaaaaaaaaaaa"); // long, no digits
        let got = toks(&query);
        assert_eq!(got.len(), MAX_CANDIDATES);
        assert!(
            got.iter().all(|t| t.chars().any(|c| c.is_numeric())),
            "digit-bearing tokens win over a long word: {got:?}"
        );
    }

    #[test]
    fn under_the_cap_nothing_is_reordered() {
        assert_eq!(toks("zebra apple mango"), vec!["zebra", "apple", "mango"]);
    }

    /// Inherited quirk, asserted so nobody "fixes" it by accident: `_` is stripped
    /// rather than escaped, so `memory_text` can never match.
    #[test]
    fn like_patterns_strip_wildcards_rather_than_escaping_them() {
        assert_eq!(like_pattern("turso"), "%turso%");
        assert_eq!(like_pattern("memory_text"), "%memorytext%");
        assert_eq!(like_pattern("100%"), "%100%");
    }

    #[test]
    fn a_rare_token_outweighs_a_common_one() {
        let cands = vec!["the".to_string(), "vector32".to_string()];
        let w = token_weights(&cands, &[300, 1], 320);
        assert_eq!(w[0].token, "vector32", "the rarest token must sort first");
        assert!(w[0].weight > w[1].weight);
    }

    #[test]
    fn only_the_eight_rarest_tokens_are_carried_into_the_score() {
        let cands: Vec<String> = (0..12).map(|i| format!("t{i}")).collect();
        let dfs: Vec<u64> = (0..12).map(|i| 12 - i).collect(); // t11 rarest
        let w = token_weights(&cands, &dfs, 100);
        assert_eq!(w.len(), MAX_TOKENS);
        assert_eq!(w[0].token, "t11");
    }

    #[test]
    fn equal_weights_keep_candidate_order() {
        let cands = vec!["b".to_string(), "a".to_string(), "c".to_string()];
        let w = token_weights(&cands, &[5, 5, 5], 50);
        let order: Vec<&str> = w.iter().map(|t| t.token.as_str()).collect();
        assert_eq!(order, vec!["b", "a", "c"]);
    }

    #[test]
    fn a_token_in_every_row_earns_almost_nothing() {
        let w = token_weights(&["everywhere".to_string()], &[100], 100);
        assert!(w[0].weight.abs() < 0.02, "got {}", w[0].weight);
    }

    #[test]
    fn a_mismatched_probe_falls_back_to_flat_weights() {
        let cands = vec!["a".to_string(), "b".to_string()];
        let w = token_weights(&cands, &[1], 10); // wrong length
        assert_eq!(w.len(), 2);
        assert!(w.iter().all(|t| t.weight == 1.0));
    }

    #[test]
    fn the_normaliser_never_divides_by_zero() {
        let zero = vec![WeightedToken {
            token: "x".into(),
            weight: 0.0,
        }];
        assert_eq!(kw_total(&zero), 1.0);
        assert_eq!(kw_total(&[]), 1.0);
    }

    #[test]
    fn the_normaliser_is_rounded_to_six_decimals() {
        let t = vec![
            WeightedToken {
                token: "a".into(),
                weight: 1.0 / 3.0,
            },
            WeightedToken {
                token: "b".into(),
                weight: 1.0 / 7.0,
            },
        ];
        assert_eq!(kw_total(&t), 0.476190);
    }

    #[test]
    fn a_row_holding_every_token_scores_exactly_one() {
        let t = token_weights(&["a".to_string(), "b".to_string()], &[3, 9], 100);
        let total = kw_total(&t);
        let all: f64 = t.iter().map(|x| round6(x.weight)).sum::<f64>() / total;
        assert!((all - 1.0).abs() < 1e-6, "got {all}");
    }
}
