//! Markdown rendering for model output.
//!
//! - Prose (headings, bold/italic, lists, tables, inline code) is rendered by
//!   [`termimad`](https://crates.io/crates/termimad) to ANSI stdout.
//! - Fenced code blocks are extracted and syntax-highlighted by
//!   [`syntect`](https://crates.io/crates/syntect) (the engine `bat` uses),
//!   using the bundled `base16-ocean.dark` theme, foreground-only.
//!
//! When stdout is piped or redirected, the raw markdown is emitted verbatim —
//! no ANSI codes, no wrapping — so `openslate run ... > file` stays clean.
//!
//! This module deliberately avoids Ratatui: OpenSlate's CLI is a line-oriented
//! stdout application (println + rustyline + indicatif).

use std::io::{self, IsTerminal};
use std::sync::OnceLock;

use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::{as_24_bit_terminal_escaped, LinesWithEndings};
use termimad::MadSkin;

/// ANSI reset escape. `as_24_bit_terminal_escaped` does NOT append a reset, so
/// we emit it ourselves to avoid color leaking into subsequent output.
const RESET: &str = "\x1b[0m";

/// Print a markdown string to stdout.
///
/// - Interactive TTY: prose via termimad, fenced code blocks via syntect.
/// - Piped / redirected / file: printed verbatim (no styling, no wrapping).
pub fn print_markdown(content: &str) {
    if io::stdout().is_terminal() {
        print_rendered(content);
    } else {
        println!("{}", content);
    }
}

fn print_rendered(content: &str) {
    let skin = MadSkin::default();
    for segment in split_segments(content) {
        match segment {
            Segment::Prose(text) => {
                // termimad renders the prose chunk (with no fenced code in it,
                // since we extracted those) directly to stdout.
                let _ = skin.write_text(&text);
            }
            Segment::Code { lang, code } => {
                print_highlighted(lang.as_deref(), &code);
            }
        }
    }
    let _ = io::stdout().flush();
}

/// Syntax-highlight a fenced code block and print it to stdout.
///
/// Uses one stateful [`HighlightLines`] for the whole block (parsing carries
/// context across lines, e.g. multi-line strings/comments). On the first
/// highlighting error it resets ANSI state and emits the remainder raw, since
/// parsing is not documented as transactional.
fn print_highlighted(lang: Option<&str>, code: &str) {
    let syntax_set = syntax_set();
    let theme = theme();
    let syntax = lang
        .and_then(|l| syntax_set.find_syntax_by_token(l))
        .unwrap_or_else(|| syntax_set.find_syntax_plain_text());

    let mut highlighter = HighlightLines::new(syntax, theme);
    let mut raw_fallback = false;
    for line in LinesWithEndings::from(code) {
        if raw_fallback {
            print!("{}", line);
            continue;
        }
        match highlighter.highlight_line(line, syntax_set) {
            Ok(regions) => {
                // bg=false: foreground colors only (no block background).
                print!("{}", as_24_bit_terminal_escaped(&regions[..], false));
            }
            Err(_) => {
                // Reset any partial color, then raw-emit this line and fall
                // back for the rest of the block.
                print!("{}{}", RESET, line);
                raw_fallback = true;
            }
        }
    }
    // Always reset so color does not leak into the next prose/spinner line.
    print!("{}", RESET);
    let _ = io::stdout().flush();
}

fn syntax_set() -> &'static SyntaxSet {
    static SS: OnceLock<SyntaxSet> = OnceLock::new();
    SS.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme() -> &'static Theme {
    static TS: OnceLock<ThemeSet> = OnceLock::new();
    let ts = TS.get_or_init(ThemeSet::load_defaults);
    // base16-ocean.dark: balanced foreground-only palette for dark terminals;
    // bundled in default-themes.
    ts.themes
        .get("base16-ocean.dark")
        .expect("base16-ocean.dark is a bundled theme")
}

// --- fenced-code-block splitting -------------------------------------------

enum Segment {
    Prose(String),
    Code {
        lang: Option<String>,
        code: String,
    },
}

/// Split markdown into alternating prose / fenced-code-block segments.
///
/// A simple line-based scanner is sufficient for model output: a fenced block
/// opens with a line whose first non-whitespace token is ```` ``` ```` (the rest
/// of that line is the info string, e.g. `rust`) and closes with another such
/// line. Indented code blocks and nested fences are not specially handled.
fn split_segments(md: &str) -> Vec<Segment> {
    let mut segments = Vec::new();
    let mut prose = String::new();
    // When inside a fence: (language token, accumulated code).
    let mut code_buf: Option<(Option<String>, String)> = None;

    for line in md.split_inclusive('\n') {
        // `line` retains its trailing '\n' (or is the final line without one).
        let line_no_nl = line.trim_end_matches('\n');
        let fence_open = line_no_nl.trim_start().starts_with("```");

        if let Some((lang, buf)) = code_buf.as_mut() {
            if fence_open {
                // Close fence.
                let code = std::mem::take(buf);
                segments.push(Segment::Code {
                    lang: lang.take(),
                    code,
                });
                code_buf = None;
            } else {
                buf.push_str(line);
            }
        } else if fence_open {
            // Open fence — flush accumulated prose first.
            if !prose.is_empty() {
                segments.push(Segment::Prose(std::mem::take(&mut prose)));
            }
            let lang = line_no_nl
                .trim_start()
                .trim_start_matches("```")
                .split_whitespace()
                .next()
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            code_buf = Some((lang, String::new()));
        } else {
            prose.push_str(line);
        }
    }

    // Flush whatever remains (handles an unclosed fence gracefully).
    if let Some((lang, code)) = code_buf {
        segments.push(Segment::Code { lang, code });
    } else if !prose.is_empty() {
        segments.push(Segment::Prose(prose));
    }
    segments
}

use std::io::Write;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn print_markdown_handles_plain_text() {
        print_markdown("hello world");
    }

    #[test]
    fn print_markdown_handles_markdown_and_code() {
        print_markdown("# Title\n\nSome **bold** and `code`.\n\n- a\n- b\n");
    }

    #[test]
    fn split_extracts_code_block_with_language() {
        let md = "intro\n\n```rust\nfn main() {}\n```\n\noutro\n";
        let segs = split_segments(md);
        assert_eq!(segs.len(), 3, "prose, code, prose");
        match &segs[0] {
            Segment::Prose(t) => assert!(t.contains("intro")),
            _ => panic!("expected prose"),
        }
        match &segs[1] {
            Segment::Code { lang, code } => {
                assert_eq!(lang.as_deref(), Some("rust"));
                assert!(code.contains("fn main()"));
            }
            _ => panic!("expected code"),
        }
        match &segs[2] {
            Segment::Prose(t) => assert!(t.contains("outro")),
            _ => panic!("expected prose"),
        }
    }

    #[test]
    fn split_handles_plain_fence_no_language() {
        let md = "```\nplain code\n```\n";
        let segs = split_segments(md);
        assert_eq!(segs.len(), 1, "no prose around this block");
        match &segs[0] {
            Segment::Code { lang, code } => {
                assert!(lang.is_none(), "no language token");
                assert!(code.contains("plain code"));
            }
            _ => panic!("expected code"),
        }
    }

    #[test]
    fn split_handles_info_string_with_extras() {
        // ```rust title="main.rs"  → language is "rust"
        let md = "```rust title=\"main.rs\"\ncode\n```\n";
        let segs = split_segments(md);
        match &segs[0] {
            Segment::Code { lang, .. } => assert_eq!(lang.as_deref(), Some("rust")),
            _ => panic!("expected code"),
        }
    }
}
