//! Lightweight markdown → styled [`Line`] renderer (no dependencies —
//! termimad paints straight to stdout and cannot enter a ratatui
//! buffer, and full parsers outweigh the subset an assistant actually
//! emits in a chat transcript).
//!
//! Two-stage design:
//!
//! * [`parse`] turns markdown text into block-level [`MdLine`]s —
//!   logical soft lines carrying inline-styled spans. It knows nothing
//!   about the viewport width (except that fenced-code lines are
//!   marked [`MdLine::Code`] so the caller clips instead of wraps).
//! * [`wrap_spans`] (and [`clip`]) adapt those lines to the available
//!   width at render time, re-using the transcript's wrap vocabulary:
//!   word boundaries preserved, CJK breaks per character, spaces
//!   dropped at break points — but over styled characters so inline
//!   markup survives wrapping — plus CJK kinsoku (禁則) at break
//!   points ([`is_no_line_start`] / [`is_no_line_end`]) and a lone-CJK
//!   orphan guard on the final row.
//!
//! Supported subset (anything else renders verbatim):
//!
//! | syntax                          | rendering                                    |
//! |---------------------------------|----------------------------------------------|
//! | `#`/`##`/`###` headings         | `#` prefix stripped, BOLD + accent color      |
//! | `**bold**`, `*italic*`, `***b+i***` | style modifiers on the body text         |
//! | `` `code` ``                    | distinct code color (content literal)        |
//! | ```` ``` ```` fenced blocks     | whole block indented, fixed color, verbatim  |
//! | `- `/`* `/`1. ` lists           | `• ` / `N. ` prefix + inline body            |
//! | `> ` blockquote                 | dim `> ` prefix, dim body                    |
//! | `---` / `***` / `___` hr        | [`MdLine::Hr`] — dim rule expanded at width  |
//! | `[text](url)`                   | link text (URL dropped)                      |
//! | `\| a \|` + `\|---\|` tables    | [`MdLine::Table`] — pipes dropped, columns   |
//! |                                 | padded (display-width aware), header BOLD,   |
//! |                                 | separator row dropped, widest column shrunk  |
//! |                                 | to fit                                      |
//!
//! Deliberate simplifications: `####`+ headings render as plain text;
//! list nesting is flattened (one bullet level); blockquotes are
//! per-line (no lazy continuation); setext headings/HTML are not
//! recognized; `\` escapes are not processed; table cells render
//! literally (no inline markup, no per-column alignment flags).

use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

/// Style inputs for markdown rendering. Kept as a small standalone
/// struct (not a [`crate::theme::Theme`] reference) so the parser
/// stays theme-agnostic and unit-testable.
#[derive(Debug, Clone, Copy)]
pub struct MdStyles {
    /// Body text (paragraphs, list bodies).
    pub text: Style,
    /// `#`~`###` headings — BOLD + accent color.
    pub heading: Style,
    /// Inline code and fenced code blocks — distinct color.
    pub code: Style,
    /// Dim structural elements (blockquote prefix+body, bullets, hr).
    pub dim: Style,
}

impl MdStyles {
    /// `text` + BOLD (the `**` marker composes onto any base).
    fn bold_over(base: Style) -> Style {
        base.add_modifier(Modifier::BOLD)
    }

    /// `text` + ITALIC (the `*` marker composes onto any base).
    fn italic_over(base: Style) -> Style {
        base.add_modifier(Modifier::ITALIC)
    }
}

/// One parsed logical line. `Flow` lines still wrap at render time;
/// `Code` lines are verbatim (clip, never wrap); `Hr` expands to the
/// full available width as a dim rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MdLine {
    /// A wrappable styled line (paragraph / heading / list / quote).
    Flow(Vec<Span<'static>>),
    /// A fenced-code line — kept verbatim, rendered in the code style,
    /// clipped when over-wide (never word-wrapped).
    Code(String),
    /// A horizontal rule (`---` / `***` / `___`) — dim full-width line;
    /// the glyph repetition happens at render time (width is unknown
    /// during parsing).
    Hr,
    /// A GFM-style pipe table: `rows[0]` is the header, the rest are
    /// data rows (the `|---|` separator row is consumed by the
    /// parser). Cells hold literal trimmed text; column widths and
    /// padding are computed at render time ([`table_lines`]) from the
    /// available width, so the variant stays width-agnostic.
    Table(Vec<Vec<String>>),
}

/// Parse markdown `text` into logical [`MdLine`]s.
pub fn parse(text: &str, styles: &MdStyles) -> Vec<MdLine> {
    let mut out = Vec::new();
    let mut in_fence = false;
    let lines: Vec<&str> = text.split('\n').collect();
    let mut i = 0usize;
    while i < lines.len() {
        let line = lines[i].trim_end();
        let trimmed = line.trim();

        if in_fence {
            if trimmed.starts_with("```") {
                in_fence = false; // closing fence
            } else {
                out.push(MdLine::Code(line.to_owned()));
            }
            i += 1;
            continue;
        }
        if trimmed.starts_with("```") {
            in_fence = true; // opening fence (info string ignored)
            i += 1;
            continue;
        }

        if trimmed.is_empty() {
            out.push(MdLine::Flow(Vec::new()));
            i += 1;
            continue;
        }

        // Horizontal rule: 3+ of the same rule char, nothing else.
        if is_hr(trimmed) {
            out.push(MdLine::Hr);
            i += 1;
            continue;
        }

        // Pipe table: a `|…|` header row IMMEDIATELY followed by a
        // `|---|` separator row, then any run of `|…|` data rows. A
        // pipe row with no separator after it is NOT a table (verbatim
        // paragraph — single-line pipe prose must not be mangled).
        if is_pipe_row(trimmed)
            && lines
                .get(i + 1)
                .is_some_and(|next| is_separator_row(next.trim()))
        {
            let mut rows = vec![split_pipe_row(trimmed)];
            i += 2;
            while i < lines.len() && is_pipe_row(lines[i].trim()) {
                rows.push(split_pipe_row(lines[i].trim()));
                i += 1;
            }
            out.push(MdLine::Table(rows));
            continue;
        }

        // Heading: 1-3 `#` followed by a space (or end of line) —
        // accent+BOLD base, inline markup composes onto it.
        if let Some(rest) = heading_body(trimmed) {
            out.push(MdLine::Flow(parse_inline(
                rest.trim(),
                styles.heading,
                styles,
            )));
            i += 1;
            continue;
        }

        // Blockquote: dim `> ` prefix + dim inline body.
        if let Some(body) = trimmed.strip_prefix('>') {
            let body = body.strip_prefix(' ').unwrap_or(body);
            let mut spans = vec![Span::styled("> ".to_owned(), styles.dim)];
            spans.extend(parse_inline(body, styles.dim, styles));
            out.push(MdLine::Flow(spans));
            i += 1;
            continue;
        }

        // Unordered list: `- ` / `* ` → `• ` prefix.
        if let Some(body) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            let mut spans = vec![Span::styled("• ".to_owned(), styles.dim)];
            spans.extend(parse_inline(body, styles.text, styles));
            out.push(MdLine::Flow(spans));
            i += 1;
            continue;
        }

        // Ordered list: `1. ` → keep the number.
        if let Some((num, body)) = ordered_list_item(trimmed) {
            let mut spans = vec![Span::styled(format!("{num}. "), styles.dim)];
            spans.extend(parse_inline(body, styles.text, styles));
            out.push(MdLine::Flow(spans));
            i += 1;
            continue;
        }

        // Plain paragraph line.
        out.push(MdLine::Flow(parse_inline(line.trim(), styles.text, styles)));
        i += 1;
    }
    out
}

/// A `|…|`-delimited row (leading AND trailing pipe; the common GFM
/// shape). Cell contents may be anything — the separator check below
/// is what actually gates table recognition.
fn is_pipe_row(s: &str) -> bool {
    s.len() >= 2 && s.starts_with('|') && s.ends_with('|')
}

/// Split a pipe row into trimmed cells (outer pipes dropped).
fn split_pipe_row(s: &str) -> Vec<String> {
    s[1..s.len() - 1]
        .split('|')
        .map(|cell| cell.trim().to_owned())
        .collect()
}

/// A separator row: a pipe row whose every cell is an alignment spec
/// (`---`, `:---`, `---:`, `:---:` — at least one dash each). The
/// alignment markers are accepted but not honored (columns stay
/// left-aligned — the simple subset).
fn is_separator_row(s: &str) -> bool {
    is_pipe_row(s)
        && split_pipe_row(s).iter().all(|cell| {
            let core = cell.trim_start_matches(':').trim_end_matches(':');
            !core.is_empty() && core.chars().all(|c| c == '-')
        })
}

/// `---` / `***` / `___` (3+ of one rule character, whitespace aside).
fn is_hr(s: &str) -> bool {
    for rule in ['-', '*', '_'] {
        let n = s.chars().filter(|c| *c == rule).count();
        if n >= 3 && s.chars().all(|c| c == rule || c.is_whitespace()) {
            return true;
        }
    }
    false
}

/// Heading body for `#`~`###` headings (`None` for anything else —
/// including `####`+ and `#nospace`).
fn heading_body(s: &str) -> Option<&str> {
    let level = s.chars().take_while(|c| *c == '#').count();
    if (1..=3).contains(&level) {
        let rest = &s[level..];
        // A heading needs a space after the markers (or nothing at all).
        if rest.is_empty() || rest.starts_with(' ') {
            Some(rest)
        } else {
            None
        }
    } else {
        None
    }
}

/// `1. item` → `("1", "item")` (single- or multi-digit counters).
fn ordered_list_item(s: &str) -> Option<(&str, &str)> {
    let digits: usize = s.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    s.get(digits..)
        .and_then(|rest| rest.strip_prefix(". "))
        .map(|body| (&s[..digits], body))
}

// ── Inline parsing ─────────────────────────────────────────────────────

/// Parse inline markup (`**bold**`, `*italic*`, `` `code` ``,
/// `[text](url)`) over `s` with the given base style. Unmatched
/// markers render literally.
fn parse_inline(s: &str, base: Style, styles: &MdStyles) -> Vec<Span<'static>> {
    // Fast path: no inline markers at all → one span, no per-char
    // machinery. Most lines of real output are marker-free, and the
    // streaming renderer re-parses the whole buffer every frame
    // (fix-18 perf).
    if !s.is_empty()
        && !s
            .as_bytes()
            .iter()
            .any(|&b| b == b'`' || b == b'*' || b == b'[')
    {
        return vec![Span::styled(s.to_owned(), base)];
    }
    let chars: Vec<char> = s.chars().collect();
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        match chars[i] {
            '`' => {
                if let Some(j) = find_char(&chars, b'`', i + 1) {
                    push_str(&mut out, &chars[i + 1..j], styles.code);
                    i = j + 1;
                } else {
                    push_str(&mut out, &chars[i..i + 1], base);
                    i += 1;
                }
            }
            '*' => {
                let run = star_run(&chars, i).min(4);
                if run == 4 {
                    // `****...` — not a recognized marker: literal star.
                    push_str(&mut out, &chars[i..i + 1], base);
                    i += 1;
                    continue;
                }
                match find_exact_star_run(&chars, i + run, run) {
                    Some(j) => {
                        let inner_base = match run {
                            1 => MdStyles::italic_over(base),
                            3 => MdStyles::italic_over(MdStyles::bold_over(base)),
                            _ => MdStyles::bold_over(base),
                        };
                        let inner: String = chars[i + run..j].iter().collect();
                        out.extend(parse_inline(&inner, inner_base, styles));
                        i = j + run;
                    }
                    None => {
                        // No closing marker: verbatim.
                        push_str(&mut out, &chars[i..i + 1], base);
                        i += 1;
                    }
                }
            }
            '[' => match parse_link(&chars, i) {
                Some((text, end)) => {
                    out.extend(parse_inline(&text, base, styles));
                    i = end;
                }
                None => {
                    push_str(&mut out, &chars[i..i + 1], base);
                    i += 1;
                }
            },
            _ => {
                // Run of plain characters up to the next marker — one
                // push (one String) per run, not per character: the
                // streaming renderer re-parses every frame, and the
                // per-char path dominated the frame budget (fix-18
                // perf).
                let start = i;
                i += 1;
                while i < chars.len() && !matches!(chars[i], '`' | '*' | '[') {
                    i += 1;
                }
                push_str(&mut out, &chars[start..i], base);
            }
        }
    }
    merge_adjacent(out)
}

/// `[text](url)` starting at `chars[i] == '['` → `(text, index past ')')`.
fn parse_link(chars: &[char], i: usize) -> Option<(String, usize)> {
    let close = find_char(chars, b']', i + 1)?;
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let paren = find_char(chars, b')', close + 2)?;
    let text: String = chars[i + 1..close].iter().collect();
    if text.is_empty() {
        return None;
    }
    Some((text, paren + 1))
}

fn find_char(chars: &[char], needle: u8, from: usize) -> Option<usize> {
    (from..chars.len()).find(|&k| chars[k] as u32 == needle as u32)
}

/// Length of the consecutive `*` run starting at `i` (capped at 4 —
/// longer runs are not markers).
fn star_run(chars: &[char], i: usize) -> usize {
    (i..chars.len())
        .take(4)
        .take_while(|&k| chars[k] == '*')
        .count()
}

/// Index of the next `*` run of EXACTLY `len` stars at or after
/// `from` — the closing marker for an exact-length open.
fn find_exact_star_run(chars: &[char], from: usize, len: usize) -> Option<usize> {
    let mut k = from;
    while k < chars.len() {
        if chars[k] == '*' {
            let run = (k..chars.len()).take_while(|&j| chars[j] == '*').count();
            if run == len {
                return Some(k);
            }
            k += run; // skip runs of other lengths entirely
        } else {
            k += 1;
        }
    }
    None
}

/// Push `chars` as one span with `style`, merging into the previous
/// span when the style matches (avoids span-per-char for plain runs).
fn push_str(out: &mut Vec<Span<'static>>, chars: &[char], style: Style) {
    if chars.is_empty() {
        return;
    }
    let text: String = chars.iter().collect();
    if let Some(last) = out.last_mut() {
        if last.style == style {
            last.content.to_mut().push_str(&text);
            return;
        }
    }
    out.push(Span::styled(text, style));
}

/// Merge adjacent same-style spans (recursion can produce neighbors).
fn merge_adjacent(spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::with_capacity(spans.len());
    for span in spans {
        if let Some(last) = out.last_mut() {
            if last.style == span.style {
                last.content.to_mut().push_str(&span.content);
                continue;
            }
        }
        out.push(span);
    }
    out
}

// ── Width adaptation ───────────────────────────────────────────────────

/// Display width of one character (unicode-width via ratatui).
/// Printable ASCII is exactly width 1 in unicode-width, so the
/// per-character wrap loops take an allocation-free fast path —
/// `Span::width()` allocates a String per call, which dominated the
/// per-frame re-parse cost of the streaming renderer (fix-18 perf).
fn char_width(ch: char) -> usize {
    if ('\x20'..='\x7e').contains(&ch) {
        1
    } else {
        Span::from(ch.to_string()).width()
    }
}

/// Display width of a string (unicode-width via ratatui).
fn str_width(s: &str) -> usize {
    Span::from(s).width()
}

// ── CJK kinsoku (禁則) ──────────────────────────────────────────────────

/// 行首禁則 — characters that must not OPEN a wrapped line (closing
/// punctuation and postfix symbols): a break is pulled back so the
/// char lands against text instead. Shared by the plain wrap
/// (transcript) and the styled wrap ([`wrap_spans`]).
pub fn is_no_line_start(ch: char) -> bool {
    matches!(
        ch,
        // CJK closing punctuation
        '，' | '。' | '、' | '．' | '：' | '；' | '！' | '？'
        // CJK closing brackets / quotes
        | '）' | '〉' | '》' | '」' | '』' | '】' | '〕'
        // ellipsis / percent
        | '…' | '‥' | '％' | '‰'
        // ASCII twins (effective right after CJK text; the plain wrap
        // only pulls a break when the char above is wide, so latin
        // words never split over these)
        | ',' | '.' | ';' | ':' | '!' | '?' | ')' | ']' | '}'
    )
}

/// 行尾禁則 — characters that must not CLOSE a wrapped line (opening
/// brackets / quotes): they are pushed down so they stay glued to the
/// text that follows.
pub fn is_no_line_end(ch: char) -> bool {
    matches!(
        ch,
        '（' | '〈' | '《' | '「' | '『' | '【' | '〔' | '“' | '‘' | '(' | '[' | '{'
    )
}

/// Display width of a leading list/quote prefix span (`• `, `N. `,
/// `> `) when the line starts with one — the hanging-indent anchor
/// for wrapped continuation rows. `0` when the line has no such
/// prefix (continuation rows then align at the block indent).
pub fn hang_width(spans: &[Span<'static>]) -> usize {
    let Some(first) = spans.first() else { return 0 };
    let t = first.content.as_ref();
    let numbered = t.len() > 2
        && t.ends_with(". ")
        && t[..t.len() - 2].chars().all(|c| c.is_ascii_digit())
        && !t[..t.len() - 2].is_empty();
    if t == "• " || t == "> " || numbered {
        first.width()
    } else {
        0
    }
}

/// Render a parsed pipe table into display rows: pipes dropped, cells
/// padded to their column widths (display-width aware — CJK safe),
/// columns joined by two spaces, header row BOLD, separator row
/// already consumed by the parser. When the natural column widths
/// overflow `width`, the widest column is shrunk one column at a time
/// (cells clip, no ellipsis) until the table fits or every column is
/// degenerate. Each returned row is a single span.
pub fn table_lines(
    rows: &[Vec<String>],
    width: usize,
    styles: &MdStyles,
) -> Vec<Vec<Span<'static>>> {
    let Some(header) = rows.first() else {
        return Vec::new();
    };
    let ncols = header.len().max(1);
    let mut widths: Vec<usize> = (0..ncols)
        .map(|j| {
            rows.iter()
                .map(|r| r.get(j).map(|c| str_width(c)).unwrap_or(0))
                .max()
                .unwrap_or(0)
                .max(1)
        })
        .collect();
    // Shrink-to-fit: trim the widest column until the joined row fits.
    let gap = 2 * widths.len().saturating_sub(1);
    while widths.iter().sum::<usize>() + gap > width && widths.iter().any(|w| *w > 1) {
        let widest = widths.iter_mut().max().unwrap();
        *widest -= 1;
    }
    let render_row = |cells: &[String], style: Style| -> Vec<Span<'static>> {
        let mut text = String::new();
        for (j, cell) in cells.iter().enumerate() {
            if j > 0 {
                text.push_str("  ");
            }
            let clipped = clip(cell, widths[j]);
            text.push_str(&clipped);
            // Pad every cell but the last (no trailing spaces), then
            // trim the tail so an empty final cell leaves no gap.
            if j + 1 < cells.len().min(widths.len()) {
                let pad = widths[j].saturating_sub(str_width(&clipped));
                text.push_str(&" ".repeat(pad));
            }
        }
        vec![Span::styled(text.trim_end().to_owned(), style)]
    };
    let mut out = vec![render_row(header, MdStyles::bold_over(styles.text))];
    for row in &rows[1..] {
        let normalized: Vec<String> = (0..ncols)
            .map(|j| row.get(j).cloned().unwrap_or_default())
            .collect();
        out.push(render_row(&normalized, styles.text));
    }
    out
}

/// Flush the pending word into the current row, wrapping (and
/// char-splitting over-long words) — the styled twin of the
/// transcript's `wrap_to_width` helper. Break points honor CJK
/// kinsoku (禁則): a word opening with a [`is_no_line_start`] char
/// pulls the preceding WIDE char down with it (never a narrow one —
/// that would split latin words), and a row ending in an
/// [`is_no_line_end`] opener pushes the opener down.
fn flush_styled_word(
    width: usize,
    rows: &mut Vec<Vec<(char, Style)>>,
    row: &mut Vec<(char, Style)>,
    row_w: &mut usize,
    word: &mut Vec<(char, Style)>,
    word_w: &mut usize,
    pending: &mut usize,
) {
    if word.is_empty() {
        return;
    }
    if *row_w > 0 {
        if *row_w + *pending + *word_w > width {
            let last = row.last().map(|(c, _)| *c);
            let pull_down = is_no_line_start(word[0].0) && last.is_some_and(|c| char_width(c) >= 2);
            let opener_down = last.is_some_and(is_no_line_end);
            if (pull_down || opener_down) && row.len() > 1 {
                // Kinsoku: break one char earlier so the punctuation
                // stays glued to text.
                let moved = row.pop().unwrap();
                rows.push(std::mem::take(row));
                row.push(moved);
                *row_w = char_width(moved.0);
            } else {
                rows.push(std::mem::take(row));
                *row_w = 0;
            }
        } else {
            for _ in 0..*pending {
                row.push((' ', word[0].1));
            }
            *row_w += *pending;
        }
    }
    *pending = 0;
    if *word_w > width {
        for (ch, style) in word.drain(..) {
            let cw = char_width(ch);
            if *row_w > 0 && *row_w + cw > width {
                rows.push(std::mem::take(row));
                *row_w = 0;
            }
            row.push((ch, style));
            *row_w += cw;
        }
    } else if *row_w + *word_w <= width {
        row.append(word);
        *row_w += *word_w;
    } else {
        // The kinsoku-moved prefix leaves no room for the word — wrap
        // the word whole onto the next row.
        rows.push(std::mem::take(row));
        row.append(word);
        *row_w = *word_w;
    }
    word.clear();
    *word_w = 0;
}

/// Greedy word-wrap of one styled soft line to `width` display
/// columns. Same vocabulary as the transcript's plain wrap: space-
/// delimited words stay intact when they fit, single over-long words
/// hard-split by characters, wide (CJK) characters are breakable units
/// of their own, spaces at break points are dropped, kinsoku honored
/// at break points. Always returns at least one (possibly empty) row.
pub fn wrap_spans(spans: &[Span<'static>], width: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    let mut rows: Vec<Vec<(char, Style)>> = Vec::new();
    let mut row: Vec<(char, Style)> = Vec::new();
    let mut row_w = 0usize;
    let mut word: Vec<(char, Style)> = Vec::new();
    let mut word_w = 0usize;
    let mut pending = 0usize;

    for span in spans {
        for ch in span.content.chars() {
            let cw = char_width(ch);
            if ch == ' ' {
                flush_styled_word(
                    width,
                    &mut rows,
                    &mut row,
                    &mut row_w,
                    &mut word,
                    &mut word_w,
                    &mut pending,
                );
                pending += 1;
            } else if cw >= 2 {
                flush_styled_word(
                    width,
                    &mut rows,
                    &mut row,
                    &mut row_w,
                    &mut word,
                    &mut word_w,
                    &mut pending,
                );
                word.push((ch, span.style));
                word_w += cw;
                flush_styled_word(
                    width,
                    &mut rows,
                    &mut row,
                    &mut row_w,
                    &mut word,
                    &mut word_w,
                    &mut pending,
                );
            } else {
                word.push((ch, span.style));
                word_w += cw;
            }
        }
    }
    flush_styled_word(
        width,
        &mut rows,
        &mut row,
        &mut row_w,
        &mut word,
        &mut word_w,
        &mut pending,
    );
    // 孤字行防护 (orphan guard): a lone WIDE char on the final row
    // borrows one char from the row above (wide chars only — never
    // splits a latin word, and never moves a 行首禁則 char down to
    // open the last row).
    if row.len() == 1 && char_width(row[0].0) >= 2 {
        if let Some(prev) = rows.last_mut() {
            if prev.len() > 1 {
                if let Some(&(pch, pst)) = prev.last() {
                    if char_width(pch) >= 2 && !is_no_line_start(pch) {
                        prev.pop();
                        row.insert(0, (pch, pst));
                    }
                }
            }
        }
    }
    rows.push(row);

    rows.into_iter().map(merge_row).collect()
}

/// Re-assemble one row of `(char, style)` pairs into spans (merging
/// adjacent same-style runs).
fn merge_row(row: Vec<(char, Style)>) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    for (ch, style) in row {
        if let Some(last) = out.last_mut() {
            if last.style == style {
                last.content.to_mut().push(ch);
                continue;
            }
        }
        out.push(Span::styled(ch.to_string(), style));
    }
    out
}

/// Clip a verbatim (code) line to `width` display columns — no wrap,
/// no ellipsis; CJK boundaries respected.
pub fn clip(s: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        let cw = char_width(ch);
        if used + cw > width {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    fn styles() -> MdStyles {
        MdStyles {
            text: Style::new(),
            heading: Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            code: Style::new().fg(Color::Yellow),
            dim: Style::new().fg(Color::DarkGray),
        }
    }

    fn flow_texts(lines: &[MdLine]) -> Vec<String> {
        lines
            .iter()
            .map(|l| match l {
                MdLine::Flow(spans) => spans.iter().map(|s| s.content.clone()).collect(),
                other => panic!("expected Flow, got {other:?}"),
            })
            .collect()
    }

    // ── Block level ─────────────────────────────────────────────────

    #[test]
    fn headings_level_1_to_3_strip_prefix_and_use_heading_style() {
        for (src, want) in [
            ("# Title", "Title"),
            ("## Sub", "Sub"),
            ("### Deep", "Deep"),
        ] {
            let lines = parse(src, &styles());
            assert_eq!(flow_texts(&lines), vec![want]);
            match &lines[0] {
                MdLine::Flow(spans) => {
                    assert_eq!(spans.len(), 1);
                    assert_eq!(spans[0].style, styles().heading);
                }
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn heading_level_4_and_nospace_hash_stay_verbatim() {
        // `####` is beyond the subset → plain paragraph.
        let lines = parse("#### Not a heading", &styles());
        assert_eq!(flow_texts(&lines), vec!["#### Not a heading"]);
        match &lines[0] {
            MdLine::Flow(spans) => assert_eq!(spans[0].style, styles().text),
            other => panic!("{other:?}"),
        }
        // `#nospace` is not a heading either.
        assert_eq!(flow_texts(&parse("#nospace", &styles())), vec!["#nospace"]);
    }

    #[test]
    fn fenced_blocks_are_verbatim_code_lines() {
        let md = "before\n```rust\nfn main() {}\n  indent kept\n```\nafter";
        let lines = parse(md, &styles());
        assert_eq!(
            lines,
            vec![
                MdLine::Flow(parse_inline("before", Style::new(), &styles())),
                MdLine::Code("fn main() {}".to_owned()),
                MdLine::Code("  indent kept".to_owned()),
                MdLine::Flow(parse_inline("after", Style::new(), &styles())),
            ]
        );
    }

    #[test]
    fn unterminated_fence_swallows_the_rest_as_code() {
        let lines = parse("```\nline", &styles());
        assert_eq!(lines, vec![MdLine::Code("line".to_owned())]);
    }

    #[test]
    fn hr_variants_map_to_hr_marker() {
        for src in ["---", "***", "___", " - - - "] {
            assert_eq!(parse(src, &styles()), vec![MdLine::Hr], "{src:?}");
        }
        // Not hr: prose with dashes.
        assert_eq!(
            flow_texts(&parse("a - b - c", &styles())),
            vec!["a - b - c"]
        );
        // `***` inside inline context is bold+italic, not hr — but a
        // line consisting ONLY of *** is hr by definition.
    }

    #[test]
    fn blockquote_dim_prefix_and_body() {
        let lines = parse("> quoted **strong**", &styles());
        match &lines[0] {
            MdLine::Flow(spans) => {
                assert_eq!(spans[0].content, "> ");
                assert_eq!(spans[0].style, styles().dim);
                // Body carries the dim base with BOLD composed on top.
                assert!(spans.iter().any(|s| s.content == "strong"
                    && s.style == styles().dim.add_modifier(Modifier::BOLD)));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unordered_lists_use_bullet_prefix() {
        for src in ["- one", "* two"] {
            let lines = parse(src, &styles());
            match &lines[0] {
                MdLine::Flow(spans) => {
                    assert_eq!(spans[0].content, "• ");
                    let body: String = spans[1..].iter().map(|s| s.content.clone()).collect();
                    assert!(body == "one" || body == "two");
                }
                other => panic!("{other:?}"),
            }
        }
        // A bare `-` (no space) is prose, not a list.
        assert_eq!(flow_texts(&parse("-", &styles())), vec!["-"]);
    }

    #[test]
    fn ordered_lists_keep_the_number() {
        let lines = parse("1. first", &styles());
        match &lines[0] {
            MdLine::Flow(spans) => {
                assert_eq!(spans[0].content, "1. ");
                assert_eq!(spans[1].content, "first");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            flow_texts(&parse("12. twelve", &styles())),
            vec!["12. twelve"]
        );
    }

    #[test]
    fn blank_lines_become_empty_flow_lines() {
        let lines = parse("a\n\nb", &styles());
        assert_eq!(lines.len(), 3);
        assert_eq!(flow_texts(&lines), vec!["a", "", "b"]);
    }

    // ── Inline level ────────────────────────────────────────────────

    #[test]
    fn bold_italic_and_code_spans() {
        let spans = parse_inline("a **b** c *d* e `f` g", Style::new(), &styles());
        let texts: Vec<&str> = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(texts, vec!["a ", "b", " c ", "d", " e ", "f", " g"]);
        assert_eq!(spans[1].style, Style::new().add_modifier(Modifier::BOLD));
        assert_eq!(spans[3].style, Style::new().add_modifier(Modifier::ITALIC));
        assert_eq!(spans[5].style, styles().code);
    }

    #[test]
    fn triple_star_is_bold_plus_italic() {
        let spans = parse_inline("***both***", Style::new(), &styles());
        assert_eq!(spans[0].content, "both");
        assert_eq!(
            spans[0].style,
            Style::new()
                .add_modifier(Modifier::BOLD)
                .add_modifier(Modifier::ITALIC)
        );
    }

    #[test]
    fn nested_markers_compose() {
        // `**a *b* c**` — italic inside bold.
        let spans = parse_inline("**a *b* c**", Style::new(), &styles());
        let texts: Vec<&str> = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(texts, vec!["a ", "b", " c"]);
        assert_eq!(spans[0].style, Style::new().add_modifier(Modifier::BOLD));
        assert_eq!(
            spans[1].style,
            Style::new()
                .add_modifier(Modifier::BOLD)
                .add_modifier(Modifier::ITALIC)
        );
    }

    #[test]
    fn unmatched_markers_render_literally() {
        let spans = parse_inline("2 * 3 = 6 and a `tick", Style::new(), &styles());
        let text: String = spans.iter().map(|s| s.content.clone()).collect();
        assert_eq!(text, "2 * 3 = 6 and a `tick");
        assert!(spans.iter().all(|s| s.style == Style::new()));
    }

    #[test]
    fn code_span_content_is_literal() {
        // Markers inside code spans do not nest.
        let spans = parse_inline("a `*x*` b", Style::new(), &styles());
        let text: String = spans.iter().map(|s| s.content.clone()).collect();
        assert_eq!(text, "a *x* b");
        assert!(spans
            .iter()
            .any(|s| s.content == "*x*" && s.style == styles().code));
    }

    #[test]
    fn links_render_text_and_drop_url() {
        let spans = parse_inline("see [docs](https://x.y/z) here", Style::new(), &styles());
        let text: String = spans.iter().map(|s| s.content.clone()).collect();
        assert_eq!(text, "see docs here");
        assert!(!text.contains("https"));
        // Malformed links stay literal.
        let spans = parse_inline("[unclosed(]", Style::new(), &styles());
        let text: String = spans.iter().map(|s| s.content.clone()).collect();
        assert_eq!(text, "[unclosed(]");
    }

    #[test]
    fn inline_cjk_passes_through() {
        let spans = parse_inline("中文**加粗**测试", Style::new(), &styles());
        let text: String = spans.iter().map(|s| s.content.clone()).collect();
        assert_eq!(text, "中文加粗测试");
    }

    // ── wrap_spans ──────────────────────────────────────────────────

    fn s(text: &str, style: Style) -> Span<'static> {
        Span::styled(text.to_owned(), style)
    }

    #[test]
    fn wrap_preserves_word_boundaries_and_styles() {
        let spans = vec![
            s("aaa ", Style::new()),
            s("bbb", Style::new().add_modifier(Modifier::BOLD)),
            s(" ccc", Style::new()),
        ];
        let rows = wrap_spans(&spans, 7);
        let texts: Vec<String> = rows
            .iter()
            .map(|r| r.iter().map(|sp| sp.content.clone()).collect())
            .collect();
        assert_eq!(texts, vec!["aaa bbb", "ccc"]);
        // The bold marker survives the wrap into row 0.
        assert_eq!(rows[0][1].style, Style::new().add_modifier(Modifier::BOLD));
    }

    #[test]
    fn wrap_hard_splits_over_long_words() {
        let spans = vec![s("abcdefghij", Style::new())];
        let rows = wrap_spans(&spans, 4);
        let texts: Vec<String> = rows
            .iter()
            .map(|r| r.iter().map(|sp| sp.content.clone()).collect())
            .collect();
        assert_eq!(texts, vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn wrap_breaks_cjk_per_character() {
        let spans = vec![s("世界世界", Style::new())];
        let rows = wrap_spans(&spans, 4);
        let texts: Vec<String> = rows
            .iter()
            .map(|r| r.iter().map(|sp| sp.content.clone()).collect())
            .collect();
        assert_eq!(texts, vec!["世界", "世界"]);
    }

    #[test]
    fn wrap_drops_spaces_at_break_points() {
        let spans = vec![s("aa bb  cc", Style::new())];
        let rows = wrap_spans(&spans, 4);
        let texts: Vec<String> = rows
            .iter()
            .map(|r| r.iter().map(|sp| sp.content.clone()).collect())
            .collect();
        assert_eq!(texts, vec!["aa", "bb", "cc"]);
    }

    #[test]
    fn wrap_empty_input_yields_one_empty_row() {
        let rows = wrap_spans(&[], 10);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].is_empty());
    }

    // ── kinsoku (禁則) ─────────────────────────────────────────────

    fn rows_text(rows: &[Vec<Span<'static>>]) -> Vec<String> {
        rows.iter()
            .map(|r| r.iter().map(|sp| sp.content.clone()).collect())
            .collect()
    }

    /// `，` must never open a wrapped line — the break pulls back so
    /// the preceding CJK char moves down with it.
    #[test]
    fn wrap_kinsoku_closing_punct_never_starts_a_row() {
        let spans = vec![s("第一行满行，第二行", Style::new())];
        assert_eq!(
            rows_text(&wrap_spans(&spans, 8)),
            vec!["第一行满", "行，第", "二行"]
        );
        // Content conserved through the moves.
        assert_eq!(
            rows_text(&wrap_spans(&spans, 8)).join(""),
            "第一行满行，第二行"
        );
    }

    /// `（` must never close a wrapped line — it is pushed down to
    /// stay glued to the text that follows.
    #[test]
    fn wrap_kinsoku_opener_never_ends_a_row() {
        let spans = vec![s("说明（注解", Style::new())];
        assert_eq!(rows_text(&wrap_spans(&spans, 6)), vec!["说明", "（注解"]);
    }

    /// A lone CJK char on the last row borrows one char from above
    /// (7 wide chars at width 6: greedy 3+3+1 → guarded 3+2+2).
    #[test]
    fn wrap_orphan_guard_moves_a_char_down() {
        let spans = vec![s("世界世界世世世", Style::new())];
        assert_eq!(
            rows_text(&wrap_spans(&spans, 6)),
            vec!["世界世", "界世", "世世"]
        );
    }

    /// Styles survive kinsoku moves: a bold CJK run rebalanced by the
    /// orphan guard keeps BOLD on every row.
    #[test]
    fn wrap_kinsoku_moves_preserve_styles() {
        let bold = Style::new().add_modifier(Modifier::BOLD);
        let spans = vec![s("世界世界世世世", bold)];
        let rows = wrap_spans(&spans, 6);
        assert_eq!(rows_text(&rows), vec!["世界世", "界世", "世世"]);
        for row in &rows {
            assert!(row.iter().all(|sp| sp.style == bold), "{row:?}");
        }
    }

    /// ASCII punctuation right after CJK text also pulls the break
    /// back (latin words themselves are never split — see below).
    #[test]
    fn wrap_kinsoku_ascii_punct_after_cjk() {
        let spans = vec![s("如下:然后", Style::new())];
        assert_eq!(rows_text(&wrap_spans(&spans, 4)), vec!["如", "下:", "然后"]);
    }

    /// Latin words never split mid-word regardless of kinsoku (词中拆
    /// 不复现): the word wraps whole; only over-long single words
    /// hard-split.
    #[test]
    fn wrap_never_splits_latin_words() {
        let spans = vec![s("中文 configuration 续行", Style::new())];
        assert_eq!(
            rows_text(&wrap_spans(&spans, 10)),
            vec!["中文", "configurat", "ion 续行"]
        );
        // A word that fits is never broken even when CJK surrounds it.
        let spans = vec![s("中文 word 续", Style::new())];
        assert_eq!(rows_text(&wrap_spans(&spans, 8)), vec!["中文", "word 续"]);
    }

    // ── tables ─────────────────────────────────────────────────────

    #[test]
    fn parses_pipe_table_with_separator() {
        let md = "| a | b |\n|---|---|\n| 1 | 2 |";
        assert_eq!(
            parse(md, &styles()),
            vec![MdLine::Table(vec![
                vec!["a".to_owned(), "b".to_owned()],
                vec!["1".to_owned(), "2".to_owned()],
            ])]
        );
    }

    #[test]
    fn separator_alignment_variants_are_accepted() {
        let md = "| a | b | c |\n|:---|---:|:-:|\n| 1 | 2 | 3 |";
        assert_eq!(
            parse(md, &styles()),
            vec![MdLine::Table(vec![
                vec!["a".into(), "b".into(), "c".into()],
                vec!["1".into(), "2".into(), "3".into()],
            ])]
        );
    }

    /// A pipe row with NO separator row after it is NOT a table —
    /// verbatim paragraph lines.
    #[test]
    fn pipe_row_without_separator_stays_verbatim() {
        let lines = parse("| a | b |\n| 1 | 2 |", &styles());
        assert_eq!(flow_texts(&lines), vec!["| a | b |", "| 1 | 2 |"]);
    }

    /// Table recognition needs the header form on the first line: a
    /// separator-shaped row alone is prose (its `|` bars keep it off
    /// the hr check).
    #[test]
    fn separator_row_alone_is_not_a_table() {
        assert_eq!(
            flow_texts(&parse("|---|---|", &styles())),
            vec!["|---|---|"]
        );
    }

    /// A table interrupts and resumes around plain lines: the data run
    /// stops at the first non-pipe line.
    #[test]
    fn table_data_run_stops_at_non_pipe_line() {
        let md = "| h |\n|---|\n| a |\nplain\n| b |";
        let lines = parse(md, &styles());
        assert_eq!(
            lines,
            vec![
                MdLine::Table(vec![vec!["h".to_owned()], vec!["a".to_owned()]]),
                MdLine::Flow(parse_inline("plain", Style::new(), &styles())),
                // Trailing lone pipe row: no separator follows → prose.
                MdLine::Flow(parse_inline("| b |", Style::new(), &styles())),
            ]
        );
    }

    /// Cells render pipes-dropped, columns padded by display width
    /// (CJK safe), two-space gutters, header BOLD + body plain.
    #[test]
    fn table_lines_pad_columns_display_width_aware() {
        let rows = vec![
            vec!["类别".to_owned(), "要点".to_owned()],
            vec!["代码".to_owned(), "说明内容".to_owned()],
        ];
        let lines = table_lines(&rows, 20, &styles());
        assert_eq!(lines.len(), 2);
        let header: String = lines[0].iter().map(|s| s.content.clone()).collect();
        let body: String = lines[1].iter().map(|s| s.content.clone()).collect();
        // col0 width 4 (类别/代码 both 4), col1 width 8 (说明内容).
        assert_eq!(header, "类别  要点");
        assert_eq!(body, "代码  说明内容");
        assert_eq!(
            lines[0][0].style,
            styles().text.add_modifier(Modifier::BOLD)
        );
        assert_eq!(lines[1][0].style, styles().text);
    }

    /// Over-wide tables shrink the widest column until the row fits.
    #[test]
    fn table_lines_shrink_widest_column_to_fit() {
        let rows = vec![
            vec!["a".to_owned(), "bbbbbbbbbb".to_owned()],
            vec!["x".to_owned(), "yy".to_owned()],
        ];
        // Width 10: natural = 1 + 2 + 10 = 13 → shrink col1 to 7.
        let lines = table_lines(&rows, 10, &styles());
        let header: String = lines[0].iter().map(|s| s.content.clone()).collect();
        let body: String = lines[1].iter().map(|s| s.content.clone()).collect();
        assert_eq!(header, "a  bbbbbbb");
        assert_eq!(body, "x  yy");
        assert!(header.chars().count() <= 10);
    }

    /// Ragged data rows are padded with empty cells (never panic).
    #[test]
    fn table_lines_tolerate_ragged_rows() {
        let rows = vec![
            vec!["a".to_owned(), "b".to_owned()],
            vec!["only-one".to_owned()],
        ];
        let lines = table_lines(&rows, 20, &styles());
        let body: String = lines[1].iter().map(|s| s.content.clone()).collect();
        assert_eq!(body, "only-one");
    }

    // ── hang_width ─────────────────────────────────────────────────

    #[test]
    fn hang_width_detects_list_and_quote_prefixes() {
        let bullet = parse("- item", &styles());
        assert_eq!(
            hang_width(match &bullet[0] {
                MdLine::Flow(spans) => spans,
                other => panic!("{other:?}"),
            }),
            2
        );
        let ordered = parse("12. item", &styles());
        assert_eq!(
            hang_width(match &ordered[0] {
                MdLine::Flow(spans) => spans,
                other => panic!("{other:?}"),
            }),
            4
        );
        let quote = parse("> quoted", &styles());
        assert_eq!(
            hang_width(match &quote[0] {
                MdLine::Flow(spans) => spans,
                other => panic!("{other:?}"),
            }),
            2
        );
        // Plain paragraphs: no hanging indent.
        let plain_line = parse("plain text", &styles());
        assert_eq!(
            hang_width(match &plain_line[0] {
                MdLine::Flow(spans) => spans,
                other => panic!("{other:?}"),
            }),
            0
        );
    }

    #[test]
    fn clip_truncates_ascii_and_cjk_by_display_width() {
        assert_eq!(clip("abcdef", 4), "abcd");
        assert_eq!(clip("世界世界", 5), "世界");
        assert_eq!(clip("short", 10), "short");
        assert_eq!(clip("", 10), "");
    }

    // ── perf smoke (fix-18: streaming re-parses every frame) ────────

    /// PERF SMOKE (non-asserting, run with --nocapture): parse and
    /// wrap a ~50KB realistic markdown document — the streaming
    /// renderer pays this every frame, so it bounds the per-frame
    /// budget at 10fps. Numbers go in the batch report; if the cost
    /// ever becomes significant, cache parsed lines up to the last
    /// delta's line boundary.
    #[test]
    fn parse_smoke_50kb_timing() {
        let mut doc = String::new();
        while doc.len() < 50 * 1024 {
            doc.push_str(
                "# 标题 Heading\n\n一段中文正文，mixed with english words and \
                 `inline code`, **bold spans** and [links](https://example.com/long).\n\n\
                 - 列表项 one with a fairly long body that wraps\n- 列表项 two\n\n\
                 ```\nfn main() {\n    println!(\"hello, world\");\n}\n```\n\n\
                 | 列一 | col two |\n|---|---|\n| 数据 | data row |\n\n",
            );
        }
        let s = styles();
        let iters = 50u32;

        let start = std::time::Instant::now();
        let mut parsed = Vec::new();
        for _ in 0..iters {
            parsed = parse(&doc, &s);
        }
        let parse_per = start.elapsed() / iters;
        let md_lines = parsed.len();

        let start = std::time::Instant::now();
        let mut rows = 0usize;
        for _ in 0..iters {
            for line in parse(&doc, &s) {
                if let MdLine::Flow(spans) = line {
                    rows += wrap_spans(&spans, 78).len();
                } else {
                    rows += 1;
                }
            }
        }
        let parse_wrap_per = start.elapsed() / iters;

        println!(
            "md parse ~{}B doc: {parse_per:?} ({md_lines} lines); \
             parse+wrap: {parse_wrap_per:?} ({} rows)",
            doc.len(),
            rows / iters as usize
        );
    }
}
