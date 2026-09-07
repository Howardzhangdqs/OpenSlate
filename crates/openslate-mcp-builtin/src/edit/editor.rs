//! Pure context-patch engine.
//!
//! Applies a small "context patch" to a text file. The patch format is
//! LLM-friendly and line-oriented (not a unified diff):
//!
//! ```text
//! @@ anchor-substring     ← optional hunk header (anchor)
//!  context line            (leading space)
//! -line to delete
//! +line to add
//! ```
//!
//! Parsing rules:
//! - A line starting with `@@ ` opens a new hunk and sets its anchor — the
//!   anchor is a short substring copied verbatim from the file. Unified-diff
//!   line-number headers (`@@ -1,5 +1,5 @@ ...`, including any trailing
//!   text after the second `@@`) are recognized and tolerated as plain hunk
//!   separators with **no anchor**: models frequently emit them out of habit
//!   and the hunk body is usually still correct.
//! - Lines starting with `-` / `+` are deletions / additions; lines starting
//!   with a space (or empty lines between hunks) are context.
//! - Anything else is invalid.
//! - A hunk's *search text* is its context + `-` lines in order; it must be
//!   non-empty (a pure addition needs surrounding context lines).
//! - A blank line ends the current hunk (hunks are separated by blank lines),
//!   so a blank line cannot serve as context inside a hunk; blank context is
//!   instead expressed with `-` / `+` on an empty line.
//!
//! Resolution finds each hunk's search text in the original file (exact
//! line-aligned match); an optional `@@ anchor` disambiguates multiple
//! matches by requiring a line *containing the anchor* within 50 lines above
//! the match start or inside the matched span itself. An anchor that appears
//! nowhere in the file is reported as a distinct
//! [`EditError::AnchorNotFound`] — that almost always means a
//! unified-diff-style header was written instead of a verbatim anchor.
//! Hunks must not overlap. The file is rewritten atomically
//! (temp file in the same directory + rename) and line endings / trailing
//! newline are preserved.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Number of lines above a match start scanned for the `@@` anchor.
const ANCHOR_WINDOW_LINES: usize = 50;

/// Outcome of a successful patch application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditResult {
    pub hunks: usize,
    pub additions: usize,
    pub deletions: usize,
}

/// Errors produced while applying a context patch.
#[derive(Debug, thiserror::Error)]
pub enum EditError {
    #[error("hunk {hunk} did not match")]
    NoMatch { hunk: usize },

    #[error("hunk {hunk}: anchor text not found in file")]
    AnchorNotFound { hunk: usize },

    #[error("hunk {hunk} matched {matches} locations")]
    AmbiguousMatch { hunk: usize, matches: usize },

    #[error("patch contains overlapping hunks")]
    OverlappingHunks,

    #[error("invalid patch: {0}")]
    InvalidPatch(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// One parsed hunk.
#[derive(Debug, Default)]
struct Hunk {
    /// Optional anchor substring (from the `@@ ` header) used to disambiguate.
    anchor: Option<String>,
    /// Context + deletion lines, in order — the text to locate in the file.
    search: Vec<String>,
    /// Context + addition lines, in order — the replacement text.
    replacement: Vec<String>,
    additions: usize,
    deletions: usize,
}

impl Hunk {
    fn push_context(&mut self, line: &str) {
        self.search.push(line.to_owned());
        self.replacement.push(line.to_owned());
    }

    fn push_delete(&mut self, line: &str) {
        self.search.push(line.to_owned());
        self.deletions += 1;
    }

    fn push_add(&mut self, line: &str) {
        self.replacement.push(line.to_owned());
        self.additions += 1;
    }
}

/// Skip `\d+(,\d+)?` at the front of `s`, returning the remainder.
/// Hand-rolled to avoid a regex dependency.
fn skip_unified_range(s: &str) -> Option<&str> {
    let first_end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    if first_end == 0 {
        return None;
    }
    let rest = &s[first_end..];
    let after_comma = match rest.strip_prefix(',') {
        Some(r) => r,
        None => return Some(rest),
    };
    let second_end = after_comma
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(after_comma.len());
    if second_end == 0 {
        None
    } else {
        Some(&after_comma[second_end..])
    }
}

/// True when the remainder of a `@@ ` header line is a unified-diff
/// line-number header (`-\d+(,\d+)?\s+\+\d+(,\d+)?`, optionally followed by
/// ` @@` and trailing context text — all ignored). Such headers carry no
/// anchor for us; they are plain hunk separators.
fn is_unified_diff_header(rest: &str) -> bool {
    let Some(after_minus) = rest.strip_prefix('-') else {
        return false;
    };
    let Some(after_old_range) = skip_unified_range(after_minus) else {
        return false;
    };
    let trimmed = after_old_range.trim_start();
    if trimmed.len() == after_old_range.len() {
        return false; // `\s+` requires at least one whitespace
    }
    let Some(after_plus) = trimmed.strip_prefix('+') else {
        return false;
    };
    skip_unified_range(after_plus).is_some()
}

/// Classify and fold one raw patch line into the hunk list.
/// A blank line closes the current hunk (hunk separator semantics).
fn push_line(
    hunks: &mut Vec<Hunk>,
    current: &mut Option<Hunk>,
    raw: &str,
) -> Result<(), EditError> {
    if let Some(rest) = raw.strip_prefix("@@ ") {
        if let Some(h) = current.take() {
            ensure_search(hunks, h)?;
        }
        let rest = rest.trim_end();
        // Unified-diff headers are tolerated as separators without an anchor.
        let anchor = if is_unified_diff_header(rest) {
            None
        } else {
            Some(rest.to_owned())
        };
        *current = Some(Hunk {
            anchor,
            ..Hunk::default()
        });
    } else if raw.is_empty() {
        if let Some(h) = current.take() {
            ensure_search(hunks, h)?;
        }
    } else if let Some(line) = raw.strip_prefix('-') {
        current.get_or_insert_with(Hunk::default).push_delete(line);
    } else if let Some(line) = raw.strip_prefix('+') {
        current.get_or_insert_with(Hunk::default).push_add(line);
    } else if let Some(line) = raw.strip_prefix(' ') {
        current.get_or_insert_with(Hunk::default).push_context(line);
    } else {
        return Err(EditError::InvalidPatch(format!(
            "unrecognized line (missing ' ', '-', '+' or '@@ ' prefix): {:?}",
            raw
        )));
    }
    Ok(())
}

/// Move a finished hunk into the list, validating its search text is usable.
/// The hunk's 1-based index is derived from its future position.
fn ensure_search(hunks: &mut Vec<Hunk>, hunk: Hunk) -> Result<(), EditError> {
    if hunk.search.is_empty() {
        return Err(EditError::InvalidPatch(format!(
            "hunk {} has empty search text; pure additions need context lines",
            hunks.len() + 1
        )));
    }
    hunks.push(hunk);
    Ok(())
}

/// Parse the patch text into hunks (see module docs for the grammar).
fn parse_patch(patch: &str) -> Result<Vec<Hunk>, EditError> {
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut current: Option<Hunk> = None;

    for raw in patch.lines() {
        push_line(&mut hunks, &mut current, raw)?;
    }
    if let Some(h) = current.take() {
        ensure_search(&mut hunks, h)?;
    }

    if hunks.is_empty() {
        return Err(EditError::InvalidPatch(
            "patch contains no hunks".to_owned(),
        ));
    }
    Ok(hunks)
}

/// Strip the line terminator of a file line kept via `split_inclusive('\n')`.
fn logical_line(line: &str) -> &str {
    line.strip_suffix('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .unwrap_or(line)
}

/// Locate every hunk's search text in the file. Returns one span
/// (start..end, end-exclusive, line indices) per hunk, in hunk order.
fn resolve_all_hunks(hunks: &[Hunk], lines: &[&str]) -> Result<Vec<(usize, usize)>, EditError> {
    let mut spans = Vec::with_capacity(hunks.len());
    for (idx, hunk) in hunks.iter().enumerate() {
        let n = hunk.search.len();
        let mut candidates: Vec<usize> = (0..lines.len().saturating_sub(n - 1))
            .filter(|&start| {
                lines[start..start + n]
                    .iter()
                    .zip(&hunk.search)
                    .all(|(file_line, search_line)| *file_line == search_line.as_str())
            })
            .collect();

        if let Some(anchor) = &hunk.anchor {
            // An anchor that appears nowhere in the file is reported as a
            // distinct error: the caller almost certainly wrote a
            // unified-diff-style header instead of a verbatim substring, and
            // a bare NO_MATCH would hide that root cause.
            if !lines.iter().any(|l| l.contains(anchor.as_str())) {
                return Err(EditError::AnchorNotFound { hunk: idx + 1 });
            }
            candidates.retain(|&start| {
                // The window covers the 50 lines above the match *and* the
                // matched span itself: an anchor that is a substring of the
                // hunk's own first context line is the most natural anchor
                // spelling and must hit.
                let end = start + n;
                let window_start = start.saturating_sub(ANCHOR_WINDOW_LINES);
                lines[window_start..end]
                    .iter()
                    .any(|l| l.contains(anchor.as_str()))
            });
        }

        match candidates.len() {
            0 => return Err(EditError::NoMatch { hunk: idx + 1 }),
            1 => spans.push((candidates[0], candidates[0] + n)),
            matches => {
                return Err(EditError::AmbiguousMatch {
                    hunk: idx + 1,
                    matches,
                });
            }
        }
    }
    Ok(spans)
}

/// Reject any pair of hunks whose spans intersect.
fn ensure_non_overlapping(spans: &[(usize, usize)]) -> Result<(), EditError> {
    let mut sorted: Vec<(usize, usize)> = spans.to_vec();
    sorted.sort_by_key(|s| s.0);
    for pair in sorted.windows(2) {
        if pair[1].0 < pair[0].1 {
            return Err(EditError::OverlappingHunks);
        }
    }
    Ok(())
}

/// Splice every hunk into `lines` (which keep their terminators), applying
/// from the last span to the first so earlier indices stay valid. The
/// replaced region's terminator style is propagated to the new lines, and a
/// missing terminator on the file's final line is not introduced.
fn apply_hunks(lines: &mut Vec<String>, hunks: &[Hunk], spans: &[(usize, usize)]) {
    let mut order: Vec<usize> = (0..hunks.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(spans[i].0));

    for i in order {
        let (start, end) = spans[i];
        let hunk = &hunks[i];

        let last_original = &lines[end - 1];
        let ends_with_eol = last_original.ends_with('\n');
        let eol = if last_original.ends_with("\r\n") {
            "\r\n"
        } else {
            "\n"
        };

        let mut new_lines: Vec<String> = Vec::with_capacity(hunk.replacement.len());
        for (j, line) in hunk.replacement.iter().enumerate() {
            let is_last = j + 1 == hunk.replacement.len();
            // If the replaced region ended without a terminator (file's last
            // line), the new last line must not grow one.
            let terminator = if is_last && !ends_with_eol { "" } else { eol };
            new_lines.push(format!("{line}{terminator}"));
        }
        lines.splice(start..end, new_lines);
    }
}

/// Monotonic counter making concurrent temp-file names unique per process.
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Write `contents` to `path` atomically: a temp file in the same directory
/// is written and renamed over the target.
async fn atomic_write(path: &Path, contents: &str) -> Result<(), EditError> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_owned());
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(".{}.{}.{}.tmp", file_name, std::process::id(), seq));

    tokio::fs::write(&temp, contents).await?;
    if let Err(e) = tokio::fs::rename(&temp, path).await {
        // Best-effort cleanup of the orphaned temp file.
        let _ = tokio::fs::remove_file(&temp).await;
        return Err(e.into());
    }
    Ok(())
}

/// Apply a context patch to the file at `path` (see module docs).
///
/// The pipeline is: parse → resolve → overlap check → splice → atomic write.
/// On success returns hunk/addition/deletion counts.
pub async fn apply_context_patch(path: &Path, patch: &str) -> Result<EditResult, EditError> {
    let hunks = parse_patch(patch)?;

    let content = tokio::fs::read_to_string(path).await?;
    // Lines with their terminators preserved; matching uses logical content.
    let lines: Vec<String> = content.split_inclusive('\n').map(str::to_owned).collect();
    let logical: Vec<&str> = lines.iter().map(|l| logical_line(l)).collect();

    let spans = resolve_all_hunks(&hunks, &logical)?;
    ensure_non_overlapping(&spans)?;

    let additions: usize = hunks.iter().map(|h| h.additions).sum();
    let deletions: usize = hunks.iter().map(|h| h.deletions).sum();
    let hunk_count = hunks.len();

    let mut new_lines = lines;
    apply_hunks(&mut new_lines, &hunks, &spans);
    let new_content: String = new_lines.concat();

    atomic_write(path, &new_content).await?;

    Ok(EditResult {
        hunks: hunk_count,
        additions,
        deletions,
    })
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Write `content` into a fresh temp file and return its path.
    fn temp_file(content: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("target.txt");
        std::fs::write(&path, content).unwrap();
        (dir, path)
    }

    async fn applied(original: &str, patch: &str) -> String {
        let (_dir, path) = temp_file(original);
        apply_context_patch(&path, patch)
            .await
            .expect("patch should apply");
        std::fs::read_to_string(&path).unwrap()
    }

    // ── happy paths ──

    #[tokio::test]
    async fn simple_replacement() {
        let out = applied("alpha\nbeta\ngamma\n", " alpha\n-beta\n+BETA\n gamma\n").await;
        assert_eq!(out, "alpha\nBETA\ngamma\n");
    }

    #[tokio::test]
    async fn pure_addition_with_context() {
        let out = applied("one\ntwo\n", " one\n+one-and-a-half\n two\n").await;
        assert_eq!(out, "one\none-and-a-half\ntwo\n");
    }

    #[tokio::test]
    async fn pure_deletion() {
        let out = applied("keep\ndrop\nkeep2\n", " keep\n-drop\n keep2\n").await;
        assert_eq!(out, "keep\nkeep2\n");
    }

    #[tokio::test]
    async fn mixed_add_and_delete_in_one_hunk() {
        let out = applied("a\nb\nc\n", " a\n-b\n+B2\n+B3\n c\n").await;
        assert_eq!(out, "a\nB2\nB3\nc\n");
    }

    #[tokio::test]
    async fn multiple_hunks_any_order() {
        let original = "l1\nl2\nl3\nl4\nl5\n";
        // First hunk targets l4, second targets l2 — reverse document order.
        let patch = " l3\n-l4\n+L4\n\n l1\n-l2\n+L2\n";
        let out = applied(original, patch).await;
        assert_eq!(out, "l1\nL2\nl3\nL4\nl5\n");
    }

    #[tokio::test]
    async fn splice_order_survives_net_line_growth_before_later_hunk() {
        // Hunk 1 grows the file (1 line → 2), hunk 2 edits a later position.
        // Without descending-order splicing, hunk 1's growth shifts hunk 2's
        // span and corrupts the file — the full-content assertion catches it.
        let original = "a\nb\nc\nd\ne\nf\n";
        let patch = "-b\n+B1\n+B2\n\n-e\n+E\n";
        let out = applied(original, patch).await;
        assert_eq!(out, "a\nB1\nB2\nc\nd\nE\nf\n");
    }

    #[tokio::test]
    async fn result_counts() {
        let (_dir, path) = temp_file("a\nb\nc\nd\n");
        let res = apply_context_patch(&path, "-a\n+A1\n+A2\n\n-d\n+D1\n")
            .await
            .unwrap();
        assert_eq!(
            res,
            EditResult {
                hunks: 2,
                additions: 3,
                deletions: 2
            }
        );
        // Content must match the counts: net +1 across the whole file.
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "A1\nA2\nb\nc\nD1\n"
        );
    }

    // ── anchors ──

    #[tokio::test]
    async fn anchor_disambiguates_identical_matches() {
        // "value" appears twice; the anchor "section-2" appears only above
        // the second occurrence.
        let original = "section-1\nheader\nvalue\nsection-2\nheader\nvalue\n";
        let patch = "@@ section-2\n header\n-value\n+patched\n";
        let out = applied(original, patch).await;
        assert_eq!(
            out,
            "section-1\nheader\nvalue\nsection-2\nheader\npatched\n"
        );
    }

    #[tokio::test]
    async fn anchor_missing_from_window_is_no_match() {
        let (_dir, path) = temp_file("far-away-anchor\nheader\nvalue\n");
        // Anchor exists in the file but far above the 50-line window.
        let mut original = String::from("far-away-anchor\n");
        for i in 0..60 {
            original.push_str(&format!("filler-{i}\n"));
        }
        original.push_str("header\nvalue\n");
        std::fs::write(&path, original).unwrap();

        let err = apply_context_patch(&path, "@@ far-away-anchor\n header\n-value\n+x\n")
            .await
            .unwrap_err();
        assert!(matches!(err, EditError::NoMatch { hunk: 1 }), "{err:?}");
    }

    #[tokio::test]
    async fn anchor_within_50_lines_applies() {
        let mut original = String::new();
        for i in 0..49 {
            original.push_str(&format!("filler-{i}\n"));
        }
        original.push_str("the-anchor\nheader\nvalue\n");
        let (_dir, path) = temp_file(&original);

        apply_context_patch(&path, "@@ the-anchor\n header\n-value\n+patched\n")
            .await
            .expect("anchor within window should resolve");
        assert!(std::fs::read_to_string(&path).unwrap().contains("patched"));
    }

    /// Build a file where the anchor sits at line 0 and the search text
    /// (`header`/`value`) starts at line `distance` (so the anchor is
    /// exactly `distance` lines above the match start).
    fn file_with_anchor_distance(distance: usize) -> String {
        let mut original = String::from("the-anchor\n");
        for _ in 1..distance {
            original.push_str("filler\n");
        }
        original.push_str("header\nvalue\n");
        original
    }

    #[tokio::test]
    async fn anchor_exactly_50_lines_above_matches() {
        // Window is [start-50, end]: a line-distance of exactly 50 is the
        // inclusive boundary (anchor at line 0, match start at line 50).
        let (_dir, path) = temp_file(&file_with_anchor_distance(50));
        apply_context_patch(&path, "@@ the-anchor\n header\n-value\n+patched\n")
            .await
            .expect("anchor at exactly 50 lines must resolve");
    }

    #[tokio::test]
    async fn anchor_51_lines_above_is_no_match() {
        // One line beyond the window: the sole candidate is filtered away.
        let (_dir, path) = temp_file(&file_with_anchor_distance(51));
        let err = apply_context_patch(&path, "@@ the-anchor\n header\n-value\n+patched\n")
            .await
            .unwrap_err();
        assert!(matches!(err, EditError::NoMatch { hunk: 1 }), "{err:?}");
    }

    #[tokio::test]
    async fn anchor_matching_first_context_line_resolves() {
        // The most natural anchor spelling: a substring of the hunk's own
        // first context line — the window covers the matched span itself.
        let (_dir, path) = temp_file("fn main() {\n    let x = 1;\n}\n");
        apply_context_patch(&path, "@@ let x\n     let x = 1;\n+    let y = 2;\n }\n")
            .await
            .expect("anchor inside the matched span must resolve");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "fn main() {\n    let x = 1;\n    let y = 2;\n}\n"
        );
    }

    #[tokio::test]
    async fn anchor_not_present_in_file_is_anchor_not_found() {
        // Without the anchor the search text is unique; an anchor that exists
        // nowhere is reported as its own error (diff-header confusion) rather
        // than a bare NO_MATCH.
        let (_dir, path) = temp_file("only\nmatch\nhere\n");
        let err = apply_context_patch(&path, "@@ nowhere-to-be-found\n only\n-match\n+MATCH\n")
            .await
            .unwrap_err();
        assert!(
            matches!(err, EditError::AnchorNotFound { hunk: 1 }),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn valid_anchor_with_wrong_body_is_no_match() {
        // The anchor exists in the file; only the hunk body is wrong — that
        // stays a plain NO_MATCH.
        let (_dir, path) = temp_file("header\naaa\nbbb\n");
        let err = apply_context_patch(&path, "@@ header\n zzz\n-zzz-ghost\n+x\n")
            .await
            .unwrap_err();
        assert!(matches!(err, EditError::NoMatch { hunk: 1 }), "{err:?}");
    }

    // ── unified-diff header tolerance ──

    #[tokio::test]
    async fn unified_diff_header_is_ignored_and_body_applies() {
        // The classic LLM mistake: a `@@ -1,5 +1,5 @@` header where we
        // expect an anchor. The header must act as a plain separator.
        let (_dir, path) = temp_file("Config {\n    timeout: 30,\n    retries: 3,\n}\n");
        apply_context_patch(
            &path,
            "@@ -1,4 +1,4 @@\n-    timeout: 30,\n+    timeout: 60,\n     retries: 3,",
        )
        .await
        .expect("unified header should be tolerated");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "Config {\n    timeout: 60,\n    retries: 3,\n}\n"
        );
    }

    #[tokio::test]
    async fn unified_header_with_trailing_junk_is_ignored() {
        // `@@ -1,3 +1,3 @@ Config {` — trailing context text after the
        // second @@ is junk to us and must not become an anchor.
        let (_dir, path) = temp_file("Config {\n    timeout: 30,\n}\n");
        apply_context_patch(
            &path,
            "@@ -1,3 +1,3 @@ Config {\n-    timeout: 30,\n+    timeout: 60,\n }",
        )
        .await
        .expect("trailing junk after unified header should be tolerated");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "Config {\n    timeout: 60,\n}\n"
        );
    }

    #[test]
    fn unified_headers_parse_with_no_anchor() {
        // Parse-level check: unified headers (plain and with junk) produce
        // hunks without anchors; a verbatim substring still produces one.
        let hunks =
            parse_patch("@@ -1,5 +1,5 @@\n-a\n+A\n\n@@ -3,7 +3,7 @@ fn main\n-b\n+B\n").unwrap();
        assert_eq!(hunks.len(), 2);
        assert!(hunks[0].anchor.is_none());
        assert!(hunks[1].anchor.is_none());

        let hunks = parse_patch("@@ Config {\n-a\n+A\n").unwrap();
        assert_eq!(hunks[0].anchor.as_deref(), Some("Config {"));
    }

    #[test]
    fn anchor_looking_almost_unified_stays_an_anchor() {
        // Anchors that merely start with '-' or '+' but don't carry the
        // `-\d+(,\d+)?\s+\+\d+(,\d+)?` shape remain verbatim anchors.
        assert!(!is_unified_diff_header("-timeout: 30,"));
        assert!(!is_unified_diff_header("+1 row"));
        assert!(!is_unified_diff_header("1,5 +1,5")); // missing leading '-'
        assert!(!is_unified_diff_header("-1,")); // dangling comma, no count
        assert!(is_unified_diff_header("-1,5 +1,5"));
        assert!(is_unified_diff_header("-1 +1")); // single counts, no ranges
        assert!(is_unified_diff_header("-7 +7 @@ ctx"));
        assert!(is_unified_diff_header("-1,5 +1,5 @@"));
        assert!(is_unified_diff_header("-0,0 +1,3"));
    }

    // ── errors: every variant ──

    #[tokio::test]
    async fn no_match_error() {
        let (_dir, path) = temp_file("aaa\nbbb\n");
        let err = apply_context_patch(&path, " zzz\n-zzz-ghost\n+x\n")
            .await
            .unwrap_err();
        assert!(matches!(err, EditError::NoMatch { hunk: 1 }), "{err:?}");
    }

    #[tokio::test]
    async fn ambiguous_match_error_counts_locations() {
        let (_dir, path) = temp_file("dup\nmid\ndup\n");
        // Single-line search text "dup" matches twice, no anchor.
        let err = apply_context_patch(&path, "-dup\n").await.unwrap_err();
        assert!(
            matches!(
                err,
                EditError::AmbiguousMatch {
                    hunk: 1,
                    matches: 2
                }
            ),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn errors_report_second_hunk_number() {
        // Hunk 1 applies cleanly; hunk 2 is the broken one — its 1-based
        // index must show up in the error, not the hunk that succeeded.
        let (_dir, path) = temp_file("aaa\nbbb\nccc\nddd\n");
        let err = apply_context_patch(&path, "-aaa\n+AAA\n\n zzz\n-zzz-ghost\n+x\n")
            .await
            .unwrap_err();
        assert!(
            matches!(err, EditError::NoMatch { hunk: 2 }),
            "expected NoMatch on hunk 2, got {err:?}"
        );

        let (_dir, path) = temp_file("aaa\nbbb\ndup\nmid\ndup\n");
        let err = apply_context_patch(&path, "-aaa\n+AAA\n\n-dup\n")
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                EditError::AmbiguousMatch {
                    hunk: 2,
                    matches: 2
                }
            ),
            "expected AmbiguousMatch on hunk 2, got {err:?}"
        );
    }

    #[tokio::test]
    async fn overlapping_hunks_error() {
        let (_dir, path) = temp_file("a\nb\nc\n");
        // Two hunks both touch line "b".
        let err = apply_context_patch(&path, " a\n-b\n+B\n\n b\n-c\n+C\n")
            .await
            .unwrap_err();
        assert!(matches!(err, EditError::OverlappingHunks), "{err:?}");
    }

    #[tokio::test]
    async fn adjacent_hunks_do_not_overlap() {
        let out = applied("a\nb\nc\n", "-a\n+A\n\n-b\n+B\n").await;
        assert_eq!(out, "A\nB\nc\n");
    }

    #[tokio::test]
    async fn invalid_patch_unrecognized_line() {
        let (_dir, path) = temp_file("a\n");
        let err = apply_context_patch(&path, "no-prefix-here\n")
            .await
            .unwrap_err();
        assert!(matches!(err, EditError::InvalidPatch(_)), "{err:?}");
    }

    #[tokio::test]
    async fn invalid_patch_empty() {
        let (_dir, path) = temp_file("a\n");
        let err = apply_context_patch(&path, "").await.unwrap_err();
        assert!(
            matches!(err, EditError::InvalidPatch(ref m) if m.contains("no hunks")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn invalid_patch_blank_only() {
        let (_dir, path) = temp_file("a\n");
        let err = apply_context_patch(&path, "\n\n\n").await.unwrap_err();
        assert!(matches!(err, EditError::InvalidPatch(_)), "{err:?}");
    }

    #[tokio::test]
    async fn invalid_patch_pure_addition_without_context() {
        let (_dir, path) = temp_file("a\n");
        // Only a '+' line: search text is empty.
        let err = apply_context_patch(&path, "+brand-new-line\n")
            .await
            .unwrap_err();
        assert!(
            matches!(err, EditError::InvalidPatch(ref m) if m.contains("empty search")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn io_error_on_missing_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let err = apply_context_patch(&dir.path().join("nope.txt"), " a\n-a\n+b\n")
            .await
            .unwrap_err();
        assert!(matches!(err, EditError::Io(_)), "{err:?}");
    }

    // ── line endings / trailing newline ──

    #[tokio::test]
    async fn trailing_newline_preserved_when_present() {
        let out = applied("x\ny\n", " x\n-y\n+Y\n").await;
        assert_eq!(out, "x\nY\n");
        assert!(out.ends_with('\n'));
    }

    #[tokio::test]
    async fn missing_trailing_newline_not_introduced() {
        let out = applied("x\ny", " x\n-y\n+Y\n").await;
        assert_eq!(out, "x\nY");
        assert!(!out.ends_with('\n'));
    }

    #[tokio::test]
    async fn addition_after_final_line_without_newline() {
        // The new last line inherits the "no trailing newline" state.
        let out = applied("x\ny", " x\n-y\n+Y1\n+Y2\n").await;
        assert_eq!(out, "x\nY1\nY2");
    }

    #[tokio::test]
    async fn crlf_style_preserved() {
        let (_dir, path) = temp_file("one\r\ntwo\r\n");
        apply_context_patch(&path, " one\n-two\n+TWO\n")
            .await
            .expect("crlf file should patch");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\r\nTWO\r\n");
    }

    #[tokio::test]
    async fn empty_lines_added_and_deleted() {
        // Blank lines in the *output* are expressed as bare '+'/'-' lines.
        let out = applied("a\nb\nc\n", " a\n+blank-after-a\n b\n-c\n+\n").await;
        assert_eq!(out, "a\nblank-after-a\nb\n\n");
    }

    #[tokio::test]
    async fn crlf_terminated_patch_matches_lf_file() {
        // A patch whose lines end with \r\n (e.g. authored on Windows)
        // against an LF file: patch parsing strips the \r (str::lines
        // semantics), matching the file's logical lines; additions follow
        // the file's LF style.
        let patch = " alpha\r\n-beta\r\n+BETA\r\n gamma\r\n";
        let out = applied("alpha\nbeta\ngamma\n", patch).await;
        assert_eq!(out, "alpha\nBETA\ngamma\n");
    }

    #[tokio::test]
    async fn crlf_terminated_patch_matches_crlf_file() {
        // Same CRLF patch against a CRLF file: still matches, and the
        // additions inherit the file's CRLF style.
        let patch = " alpha\r\n-beta\r\n+BETA\r\n gamma\r\n";
        let out = applied("alpha\r\nbeta\r\ngamma\r\n", patch).await;
        assert_eq!(out, "alpha\r\nBETA\r\ngamma\r\n");
    }

    // ── parse-semantics pinning ──

    #[tokio::test]
    async fn bare_at_at_header_is_invalid_patch() {
        // "@@" with no trailing space/content is not a hunk header.
        let (_dir, path) = temp_file("a\n");
        let err = apply_context_patch(&path, "@@\n-a\n+A\n")
            .await
            .unwrap_err();
        assert!(matches!(err, EditError::InvalidPatch(_)), "{err:?}");
    }

    #[tokio::test]
    async fn blank_line_inside_patch_splits_two_hunks() {
        // " a" + blank + " b" is TWO hunks (blank = separator), not one hunk
        // with two context lines — pinned via the reported hunk count.
        let (_dir, path) = temp_file("a\nb\n");
        let res = apply_context_patch(&path, " a\n\n b\n").await.unwrap();
        assert_eq!(res.hunks, 2);
        // Both context-only hunks apply; content unchanged.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nb\n");
    }

    #[tokio::test]
    async fn bare_minus_deletes_empty_line() {
        // A single "-" line (no content) deletes a blank line.
        let out = applied("a\n\nb\n", "-\n").await;
        assert_eq!(out, "a\nb\n");
    }

    #[tokio::test]
    async fn no_temp_files_left_behind() {
        let (dir, path) = temp_file("a\n");
        apply_context_patch(&path, "-a\n+A\n").await.unwrap();
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["target.txt".to_string()]);
    }
}
