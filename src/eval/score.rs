//! Metrics over ranked cases, and the tables the harness prints.

use std::collections::BTreeMap;

use crate::json::python_float;

/// One case after ranking: what it expected and what came back, best first.
#[derive(Debug, Clone)]
pub struct Judged<'a> {
    pub id: &'a str,
    pub class: &'a str,
    pub query: &'a str,
    pub expect: &'a [i64],
    pub ranked: Vec<i64>,
}

/// A case with no expected id among its top five.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Miss {
    pub id: String,
    pub query: String,
    pub top3: Vec<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Scored {
    pub r1: f64,
    pub r5: f64,
    pub mrr: f64,
    /// recall@5 per class, by class name.
    pub per_class: Vec<(String, f64)>,
    pub misses: Vec<Miss>,
}

/// recall@1, recall@5 and mean reciprocal rank (cut at rank ten) over the
/// cases, plus recall@5 per class and the cases that missed.
pub fn score(cases: &[Judged]) -> Scored {
    let mut r1 = 0.0;
    let mut r5 = 0.0;
    let mut mrr = 0.0;
    let mut per_class: BTreeMap<&str, (f64, usize)> = BTreeMap::new();
    let mut misses = Vec::new();
    for c in cases {
        let wanted = |id: &i64| c.expect.contains(id);
        let hit1 = c.ranked.first().is_some_and(wanted);
        let hit5 = c.ranked.iter().take(5).any(wanted);
        let rank = c.ranked.iter().take(10).position(wanted);
        r1 += f64::from(u8::from(hit1));
        r5 += f64::from(u8::from(hit5));
        mrr += rank.map_or(0.0, |p| 1.0 / (p + 1) as f64);
        let class = per_class.entry(c.class).or_default();
        class.0 += f64::from(u8::from(hit5));
        class.1 += 1;
        if !hit5 {
            misses.push(Miss {
                id: c.id.to_string(),
                query: c.query.chars().take(44).collect(),
                top3: c.ranked.iter().take(3).copied().collect(),
            });
        }
    }
    let n = cases.len().max(1) as f64;
    Scored {
        r1: r1 / n,
        r5: r5 / n,
        mrr: mrr / n,
        per_class: per_class
            .into_iter()
            .map(|(class, (hits, count))| (class.to_string(), hits / count as f64))
            .collect(),
        misses,
    }
}

/// The column header and its rule, without a trailing newline.
pub fn header(classes: &[String]) -> String {
    let mut h = format!("{:<26}{:>6}{:>6}{:>6}  ", "variant", "R@1", "R@5", "MRR");
    for c in classes {
        h.push_str(&format!("{:>11}", c.chars().take(9).collect::<String>()));
    }
    let rule = "-".repeat(h.chars().count());
    format!("{h}\n{rule}")
}

/// One table row.
pub fn row(name: &str, s: &Scored, classes: &[String]) -> String {
    let mut out = format!("{:<26}{:>6.2}{:>6.2}{:>6.2}  ", name, s.r1, s.r5, s.mrr);
    for c in classes {
        match s.per_class.iter().find(|(k, _)| k == c) {
            Some((_, v)) => out.push_str(&format!("{v:>11.2}")),
            None => out.push_str(&format!("{:>11}", "nan")),
        }
    }
    out
}

/// What a sweep concludes: the plateau of recall@5, its best member, and
/// whether the configured alpha still sits inside it.
pub fn verdict(results: &[(f64, Scored)], configured: f64, n_cases: usize) -> String {
    let best_r5 = results.iter().map(|(_, s)| s.r5).fold(f64::MIN, f64::max);
    let plateau: Vec<f64> = results
        .iter()
        .filter(|(_, s)| s.r5 >= best_r5 - 1e-9)
        .map(|(a, _)| *a)
        .collect();
    // Within the plateau, break the tie on MRR -- but the plateau itself is the
    // recommendation; picking its argmax alone would be optimising noise. The
    // first of equals wins, as the reference's `max` did.
    let mut best: Option<(f64, f64, f64)> = None;
    for (a, s) in results.iter().filter(|(a, _)| plateau.contains(a)) {
        let key = (s.mrr, -(a - configured).abs());
        let better = match best {
            None => true,
            Some((_, mrr, near)) => (key.0, key.1) > (mrr, near),
        };
        if better {
            best = Some((*a, key.0, key.1));
        }
    }
    let mid = best.map_or(configured, |(a, _, _)| a);
    let list = plateau
        .iter()
        .map(|a| python_float(*a))
        .collect::<Vec<_>>()
        .join(", ");
    let mut out = format!(
        "\nbest recall@5 {best_r5:.2} on alpha [{list}]\n\
         best-in-plateau: {}   configured RECALL_ALPHA: {}\n",
        python_float(mid),
        python_float(configured)
    );
    if plateau.contains(&configured) {
        out.push_str("VERDICT: configured alpha is still inside the plateau — no change needed.\n");
    } else {
        out.push_str(&format!(
            "VERDICT: configured alpha is OUTSIDE the plateau.\n  \
             ASK THE USER whether to change RECALL_ALPHA to {}. Do not change it\n  \
             unattended — show this table, and note one case is {:.1}pp so a one-case \
             gap is noise.\n  \
             If they accept, RECALL_ALPHA goes in the .env beside the binary; `config` \
             shows where each setting is read from.\n",
            python_float(mid),
            100.0 / n_cases.max(1) as f64
        ));
    }
    out
}

/// The alphas of a sweep. A type of its own because clap reads a `Vec` value
/// of a parser as several values, not one list.
#[derive(Debug, Clone, PartialEq)]
pub struct Alphas(pub Vec<f64>);

/// `--sweep`'s alphas: comma-separated, each a finite number of at least zero.
pub fn parse_alphas(raw: &str) -> Result<Alphas, String> {
    let mut out = Vec::new();
    for part in raw.split(',') {
        let part = part.trim();
        let alpha: f64 = part.parse().map_err(|_| {
            format!("'{part}' is not a number; expected a list such as 0.05,0.1,0.2")
        })?;
        if !alpha.is_finite() || alpha.is_sign_negative() {
            return Err(format!("'{part}' is not a finite number of at least 0"));
        }
        out.push(alpha);
    }
    Ok(Alphas(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn judged<'a>(id: &'a str, class: &'a str, expect: &'a [i64], ranked: Vec<i64>) -> Judged<'a> {
        Judged {
            id,
            class,
            query: "a query",
            expect,
            ranked,
        }
    }

    #[test]
    fn recall_and_reciprocal_rank_are_averaged_over_the_cases() {
        let cases = [
            judged("c1", "a", &[1], vec![1, 2, 3]),
            judged("c2", "a", &[2], vec![1, 2, 3]),
            judged("c3", "b", &[9], vec![1, 2, 3, 4, 5, 6, 7]),
        ];
        let s = score(&cases);
        assert!((s.r1 - 1.0 / 3.0).abs() < 1e-12);
        assert!((s.r5 - 2.0 / 3.0).abs() < 1e-12);
        assert!((s.mrr - 0.5).abs() < 1e-12);
        assert_eq!(
            s.per_class,
            [("a".to_string(), 1.0), ("b".to_string(), 0.0)]
        );
        assert_eq!(
            s.misses,
            [Miss {
                id: "c3".into(),
                query: "a query".into(),
                top3: vec![1, 2, 3]
            }]
        );
    }

    #[test]
    fn a_hit_below_rank_five_counts_towards_mrr_but_not_recall() {
        let cases = [judged("c", "a", &[7], vec![1, 2, 3, 4, 5, 6, 7, 8])];
        let s = score(&cases);
        assert_eq!((s.r1, s.r5), (0.0, 0.0));
        assert!((s.mrr - 1.0 / 7.0).abs() < 1e-12);
        assert_eq!(s.misses.len(), 1);
    }

    #[test]
    fn reciprocal_rank_is_cut_at_ten() {
        let ranked: Vec<i64> = (1..=11).collect();
        let s = score(&[judged("c", "a", &[11], ranked)]);
        assert_eq!(s.mrr, 0.0);
    }

    #[test]
    fn any_expected_id_counts_and_the_best_rank_wins() {
        let s = score(&[judged("c", "a", &[3, 9], vec![9, 1, 3])]);
        assert_eq!((s.r1, s.r5, s.mrr), (1.0, 1.0, 1.0));
        assert!(s.misses.is_empty());
    }

    #[test]
    fn a_case_that_ranked_nothing_scores_zero() {
        let s = score(&[judged("c", "a", &[1], Vec::new())]);
        assert_eq!((s.r1, s.r5, s.mrr), (0.0, 0.0, 0.0));
        assert_eq!(s.misses[0].top3, Vec::<i64>::new());
    }

    #[test]
    fn the_header_and_a_row_line_up_and_classes_are_cut_at_nine_characters() {
        let classes = ["exact".to_string(), "verylongclassname".to_string()];
        assert_eq!(
            header(&classes),
            "variant                      R@1   R@5   MRR        exact  verylongc\n\
             --------------------------------------------------------------------"
        );
        let s = Scored {
            r1: 1.0 / 3.0,
            r5: 2.0 / 3.0,
            mrr: 0.5,
            per_class: vec![("exact".into(), 1.0), ("verylongclassname".into(), 0.0)],
            misses: Vec::new(),
        };
        assert_eq!(
            row("current", &s, &classes),
            "current                     0.33  0.67  0.50         1.00       0.00"
        );
    }

    fn scored(r5: f64, mrr: f64) -> Scored {
        Scored {
            r1: 0.0,
            r5,
            mrr,
            per_class: Vec::new(),
            misses: Vec::new(),
        }
    }

    #[test]
    fn a_configured_alpha_outside_the_plateau_asks_the_user_and_never_decides() {
        let results = [
            (0.05, scored(0.5, 0.5)),
            (0.1, scored(1.0, 0.6)),
            (0.2, scored(1.0, 0.7)),
        ];
        let v = verdict(&results, 0.15, 10);
        assert!(v.contains("best recall@5 1.00 on alpha [0.1, 0.2]"), "{v}");
        assert!(
            v.contains("best-in-plateau: 0.2   configured RECALL_ALPHA: 0.15"),
            "{v}"
        );
        assert!(
            v.contains("VERDICT: configured alpha is OUTSIDE the plateau."),
            "{v}"
        );
        assert!(
            v.contains("ASK THE USER whether to change RECALL_ALPHA to 0.2"),
            "{v}"
        );
        assert!(v.contains("one case is 10.0pp"), "{v}");
        assert!(v.contains(".env"), "the persistent place is the .env: {v}");
    }

    #[test]
    fn a_configured_alpha_inside_the_plateau_needs_no_change() {
        let results = [(0.1, scored(1.0, 0.6)), (0.2, scored(1.0, 0.7))];
        let v = verdict(&results, 0.1, 4);
        assert!(
            v.contains("VERDICT: configured alpha is still inside the plateau — no change needed."),
            "{v}"
        );
        assert!(!v.contains("ASK THE USER"), "{v}");
    }

    #[test]
    fn within_the_plateau_ties_on_mrr_go_to_the_alpha_nearest_the_configured_one() {
        let results = [(0.1, scored(1.0, 0.6)), (0.2, scored(1.0, 0.6))];
        assert!(
            verdict(&results, 0.19, 4).contains("best-in-plateau: 0.2 "),
            "nearer 0.19"
        );
        assert!(
            verdict(&results, 0.11, 4).contains("best-in-plateau: 0.1 "),
            "nearer 0.11"
        );
        assert!(
            verdict(&results, 0.15, 4).contains("best-in-plateau: 0.1 "),
            "an exact tie keeps the first"
        );
    }

    #[test]
    fn alphas_parse_as_a_comma_separated_list_of_non_negative_numbers() {
        assert_eq!(parse_alphas("0.05, 0.1,0.5").unwrap().0, [0.05, 0.1, 0.5]);
        assert_eq!(parse_alphas("0").unwrap().0, [0.0]);
        for bad in ["", " ", "a,b", "0.1,,0.2", "-1", "nan", "inf", "0.1,"] {
            assert!(parse_alphas(bad).is_err(), "{bad:?} must not parse");
        }
    }
}
