//! Near-miss "did you mean" suggestions for method/field typos, scored with a restricted
//! Damerau-Levenshtein distance discounted by length difference — derived by measuring rustc
//! 1.97.1's own suggestions rather than by reading rustc's source (see TICKET-007 `## Digest`).

// `did_you_mean` has no caller yet — `Checker::error_help` (step 7) and the `expr.rs`/`pattern.rs`/
// `sig.rs` call sites (steps 11-15) wire it next in this same ticket. DELETE THIS ATTRIBUTE once
// they land — it exists only to let the scorer land and be tested on its own commit first.
#![allow(dead_code)]

/// Restricted Damerau-Levenshtein distance (insert/delete/substitute cost 1, adjacent
/// transposition cost 1) over `char`s, bailing out early past `limit`.
pub(super) fn edit_distance(a: &str, b: &str, limit: usize) -> Option<usize> {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let n = a.len();
    let m = b.len();
    if n.abs_diff(m) > limit {
        return None;
    }
    // classic O(n*m) DP table; small identifier lengths make this cheap.
    let mut d = vec![vec![0usize; m + 1]; n + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            let mut best = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = best;
        }
    }
    let result = d[n][m];
    if result <= limit { Some(result) } else { None }
}

/// Score a candidate against the looked-up name, discounting a length difference and rejecting
/// a pair where one string is under half the length of the other. Lower is better; `None` means
/// no match within `limit`.
pub(super) fn score(lookup: &str, cand: &str, limit: usize) -> Option<usize> {
    let n = lookup.chars().count();
    let m = cand.chars().count();
    let len_diff = n.abs_diff(m);
    let big = n * 2 < m || m * 2 < n;
    let d = edit_distance(lookup, cand, limit + len_diff)?;
    if !big && d >= len_diff && d - len_diff <= limit {
        Some(d - len_diff)
    } else if d <= limit {
        Some(d)
    } else {
        None
    }
}

/// Find the best-scoring candidate for `lookup` among `candidates`, in the given order, or an
/// exact case-insensitive match. Ties break on candidate order — callers that want a
/// deterministic result over a `HashMap`-backed table must sort `candidates` first.
pub(super) fn did_you_mean(lookup: &str, candidates: &[String]) -> Option<String> {
    if let Some(c) = candidates.iter().find(|c| c.eq_ignore_ascii_case(lookup)) {
        return Some(format!("did you mean '{c}'?"));
    }
    let mut limit = std::cmp::max(lookup.chars().count(), 3) / 3;
    let mut best: Option<(usize, &String)> = None;
    for cand in candidates {
        if let Some(s) = score(lookup, cand, limit)
            && best.as_ref().is_none_or(|(bs, _)| s < *bs)
        {
            best = Some((s, cand));
            if s == 0 {
                break;
            }
            limit = s.saturating_sub(1);
        }
    }
    best.map(|(_, c)| format!("did you mean '{c}'?"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transposition_costs_one() {
        assert_eq!(edit_distance("eln", "len", 1), Some(1));
    }

    #[test]
    fn length_difference_is_discounted() {
        assert_eq!(
            did_you_mean("lenght", &["len".to_string()]),
            Some("did you mean 'len'?".to_string())
        );
    }

    #[test]
    fn substring_typo_suggests() {
        assert!(did_you_mean("lenxyz", &["len".to_string()]).is_some());
    }

    #[test]
    fn big_length_difference_suggests_nothing() {
        assert_eq!(did_you_mean("lenqqqqqq", &["len".to_string()]), None);
    }

    #[test]
    fn unrelated_name_suggests_nothing() {
        assert_eq!(
            did_you_mean("xyz", &["len".to_string(), "push".to_string()]),
            None
        );
    }

    #[test]
    fn ties_break_on_candidate_order() {
        assert_eq!(
            did_you_mean("le", &["len".to_string(), "lex".to_string()]),
            Some("did you mean 'len'?".to_string())
        );
    }
}
