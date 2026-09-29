//! Slash-completion engine (slash-1) — pure, stateless ranking
//! functions shared by the command list (`/name…`) and the argument
//! lists (`/copy …`, `/model …`).
//!
//! Ranking (spec): case-insensitive SUBSEQUENCE fuzzy match with
//! PREFIX hits ranked first; within each class the caller's candidate
//! order (the [`crate::slash::registry`] order) is the stable
//! tiebreak. An empty query matches everything (all-prefix class).
//!
//! The initial selection: an exact (case-insensitive) match wins,
//! then the FIRST prefix hit, else item 0.

/// Case-insensitive prefix test.
fn is_prefix(query: &str, target: &str) -> bool {
    target.to_lowercase().starts_with(&query.to_lowercase())
}

/// Case-insensitive subsequence: every query char appears in `target`
/// in order (not necessarily contiguously).
fn is_subsequence(query: &str, target: &str) -> bool {
    let mut haystack: Vec<char> = target.to_lowercase().chars().collect();
    for qc in query.to_lowercase().chars() {
        match haystack.iter().position(|&tc| tc == qc) {
            Some(i) => haystack.drain(..=i),
            None => return false,
        };
    }
    true
}

/// Filter `candidates` (caller's registry order) by the fuzzy match:
/// prefix hits first, then subsequence-only hits; each class keeps
/// the candidate order (stable tiebreak). Returns indices into
/// `candidates`. An empty query matches ALL candidates.
pub fn filter(query: &str, candidates: &[&str]) -> Vec<usize> {
    if query.is_empty() {
        return (0..candidates.len()).collect();
    }
    let mut prefixes = Vec::new();
    let mut subs = Vec::new();
    for (i, c) in candidates.iter().enumerate() {
        if is_prefix(query, c) {
            prefixes.push(i);
        } else if is_subsequence(query, c) {
            subs.push(i);
        }
    }
    prefixes.extend(subs);
    prefixes
}

/// The initial selection index into `matches` (indices into
/// `candidates`): exact (case-insensitive) match > first prefix hit
/// > 0. Empty `matches` yields 0 (callers close instead).
pub fn initial_selection(query: &str, candidates: &[&str], matches: &[usize]) -> usize {
    if matches.is_empty() {
        return 0;
    }
    let q = query.to_lowercase();
    if let Some(exact) = matches
        .iter()
        .position(|&i| candidates[i].to_lowercase() == q)
    {
        return exact;
    }
    matches
        .iter()
        .position(|&i| is_prefix(query, candidates[i]))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real registry order (mirror of `slash::registry` names —
    /// kept local so the engine stays spec-agnostic).
    const NAMES: [&str; 8] = [
        "help", "exit", "new", "status", "agents", "model", "copy", "mouse",
    ];

    #[test]
    fn empty_query_matches_everything() {
        assert_eq!(filter("", &NAMES), (0..8).collect::<Vec<_>>());
    }

    #[test]
    fn prefix_hits_rank_before_subsequence_hits() {
        // "mo": model + mouse are prefix hits; no other candidate
        // contains m…o in order (copy has no m). Registry order keeps
        // model (5) before mouse (7).
        assert_eq!(filter("mo", &NAMES), vec![5, 7]);
        // "e": exit is the ONLY prefix hit; the subsequence class
        // keeps registry order (help, new, agents, model, mouse —
        // every candidate with an `e` somewhere after position 0).
        assert_eq!(filter("e", &NAMES), vec![1, 0, 2, 4, 5, 7]);
    }

    #[test]
    fn subsequence_matches_noncontiguous() {
        // "ml" — m…l in order: model only (mouse has no l).
        assert_eq!(filter("ml", &NAMES), vec![5]);
        // "as" — a…s in order: status (a@2, s@5) and agents (a@0,
        // s@5); neither is a prefix; registry order keeps status(3)
        // before agents(4).
        assert_eq!(filter("as", &NAMES), vec![3, 4]);
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert_eq!(filter("MO", &NAMES), vec![5, 7]);
        assert_eq!(filter("He", &NAMES), vec![0]);
        assert_eq!(filter("cOpY", &NAMES), vec![6]);
    }

    #[test]
    fn no_match_yields_empty() {
        assert!(filter("zzz", &NAMES).is_empty());
        assert!(filter("qq", &NAMES).is_empty());
    }

    #[test]
    fn initial_selection_exact_beats_prefix_beats_zero() {
        // Exact: "model" hits its own match list first even though a
        // prefix hit exists earlier in the match list order.
        let m = filter("model", &NAMES);
        assert_eq!(m, vec![5]);
        assert_eq!(initial_selection("model", &NAMES, &m), 0); // only match

        // Exact beats first-prefix: query "new" over the empty-query
        // full list — the exact match (index 2) is selected, not the
        // list head.
        let full = filter("", &NAMES);
        assert_eq!(initial_selection("new", &NAMES, &full), 2);

        // First prefix: query "mo" over the full list → matches hold
        // model at position 5 — first prefix hit within the MATCH
        // list is that same entry.
        assert_eq!(initial_selection("mo", &NAMES, &full), 5);

        // Neither exact nor prefix: subsequence-only query "as"
        // selects the first match (status).
        let m = filter("as", &NAMES);
        assert_eq!(initial_selection("as", &NAMES, &m), 0);

        // Degenerate: empty match list.
        assert_eq!(initial_selection("zzz", &NAMES, &[]), 0);
    }

    #[test]
    fn argument_lists_filter_the_same_way() {
        let copy_args = ["all", "tool"];
        assert_eq!(filter("", &copy_args), vec![0, 1]);
        assert_eq!(filter("a", &copy_args), vec![0]);
        assert_eq!(filter("t", &copy_args), vec![1]);
        assert_eq!(filter("al", &copy_args), vec![0]);
        assert!(filter("z", &copy_args).is_empty());
        // "l" is a subsequence of both; neither is a prefix.
        assert_eq!(filter("l", &copy_args), vec![0, 1]);

        let aliases = ["fast", "main"];
        assert_eq!(filter("fa", &aliases), vec![0]);
        assert_eq!(filter("m", &aliases), vec![1]);
    }
}
