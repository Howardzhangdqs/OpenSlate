//! Code normalization for `run_code` input.
//!
//! The model is asked for an async arrow function, but may wrap it in a
//! markdown fence, emit bare statements, or both. [`prepare`] turns raw
//! model output into an ordered list of candidate code strings the executor
//! can try (syntax errors trigger a retry with the next candidate, see
//! `PTC_PLAN.md` §6.1). No JS parser in v1 — heuristics only.

/// Strip a surrounding markdown code fence, if any.
///
/// Handles ```` ```js ... ``` ```` / ```` ```typescript ... ``` ```` and bare
/// ```` ``` ... ``` ```` wrappers; returns the trimmed inner code otherwise.
fn strip_fence(code: &str) -> &str {
    let trimmed = code.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    // Drop the language tag on the opening fence line (```js, ```ts, …).
    let body = match rest.find('\n') {
        Some(idx) => &rest[idx + 1..],
        None => "",
    };
    let body = body.trim_end();
    let body = body.strip_suffix("```").unwrap_or(body);
    body.trim()
}

/// Heuristic: does this look like an (async) arrow function expression the
/// trigger can call directly? `async () => …` / `async x => …` / `(a, b) => …`.
fn looks_like_arrow(code: &str) -> bool {
    code.starts_with("async") || (code.starts_with('(') && code.contains("=>"))
}

/// Build the ordered, deduplicated list of candidate code strings for the
/// executor to evaluate.
///
/// Order is by preference:
/// 1. the stripped code as-is, when it already looks like an arrow function
///    (direct pass-through — preserves `async () => expr` bodies that would
///    otherwise become statement blocks returning `undefined`);
/// 2. the stripped code wrapped as `async () => { ... }` for bare statement
///    bodies.
pub fn prepare(code: &str) -> Vec<String> {
    let stripped = strip_fence(code);
    let mut candidates = Vec::new();
    if looks_like_arrow(stripped) {
        candidates.push(stripped.to_string());
    }
    candidates.push(format!("async () => {{\n{stripped}\n}}"));
    // Dedup while preserving order (identical candidates waste a retry).
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut deduped: Vec<String> = Vec::new();
    for candidate in candidates {
        if seen.insert(candidate.clone()) {
            deduped.push(candidate);
        }
    }
    deduped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_js_fence_and_wraps_bare_statements() {
        let code = "```js\nconst a = 1;\nconsole.log(a);\n```";
        assert_eq!(
            prepare(code),
            vec!["async () => {\nconst a = 1;\nconsole.log(a);\n}".to_string()]
        );
    }

    #[test]
    fn strips_bare_fence() {
        let code = "```\nreturn 1 + 2;\n```";
        assert_eq!(
            prepare(code),
            vec!["async () => {\nreturn 1 + 2;\n}".to_string()]
        );
    }

    #[test]
    fn arrow_passes_through_first() {
        let code = "async () => { return 42; }";
        assert_eq!(
            prepare(code),
            vec![
                "async () => { return 42; }".to_string(),
                "async () => {\nasync () => { return 42; }\n}".to_string(),
            ]
        );
    }

    #[test]
    fn paren_arrow_with_arrow_token_passes_through() {
        let code = "```js\n(a, b) => a + b\n```";
        assert_eq!(
            prepare(code),
            vec![
                "(a, b) => a + b".to_string(),
                "async () => {\n(a, b) => a + b\n}".to_string(),
            ]
        );
    }

    #[test]
    fn expression_arrow_without_async_still_wrapped() {
        // Starts with '(' but contains no '=>': not recognized as an arrow.
        let code = "(1 + 2) * 3";
        assert_eq!(
            prepare(code),
            vec!["async () => {\n(1 + 2) * 3\n}".to_string()]
        );
    }

    #[test]
    fn no_fence_is_trimmed() {
        let code = "  \n  const x = 1;  \n";
        assert_eq!(
            prepare(code),
            vec!["async () => {\nconst x = 1;\n}".to_string()]
        );
    }
}
