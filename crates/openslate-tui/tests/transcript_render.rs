//! Transcript render tests — TestBackend + Buffer assertions (the
//! ratatui `test_case` convention). The restyle-1 rendering draws
//! straight into the given area: no frame, no title — user blocks are
//! full-width #262626 bands with a BOLD signal `›` anchor, assistant
//! text carries the `●` text-color anchor and indents 2, tool rows use
//! the connector/marker/verb language, and spacing follows the
//! block/single-line rules. These tests assert the rendered TEXT layer
//! row-by-row and spot-check the semantic colors on specific glyphs;
//! the pin state machine, wrap math, and rebuild heuristics live as
//! inline unit tests in `src/components/transcript.rs`.

use openslate_core::types::{Message, MessageRole, ToolCall, ToolCallId};
use openslate_tui::action::Action;
use openslate_tui::components::transcript::TranscriptComponent;
use openslate_tui::components::{AppCtx, Component, ConfigSummary, Focus, RunInfo, RunState};
use openslate_tui::icons::Icons;
use openslate_tui::theme::Theme;
use ratatui::backend::TestBackend;
use ratatui::buffer::{Buffer, CellWidth};
use ratatui::Terminal;
use serde_json::json;

/// Theme slot accessor for color assertions (theme-1: no literal hex
/// in component tests — future boards keep these green).
fn theme() -> Theme {
    Theme::new()
}

fn test_ctx() -> AppCtx {
    AppCtx {
        theme: Theme::new(),
        focus: Focus::Transcript,
        run: RunInfo {
            state: RunState::Idle,
            spinner_frame: 0,
            model_label: String::new(),
            tokens_in: 0,
            tokens_out: 0,
            cost_usd: 0.0,
            elapsed: None,
            tool_calls_cur: 0,
            depth_cur: 0,
            context_remaining: None,
        },
        config: ConfigSummary {
            model_alias: String::new(),
            model_id: String::new(),
            provider_name: String::new(),
            max_depth: 4,
            max_tool_calls: 20,
            run_id: None,
            model_aliases: Vec::new(),
        },
        size: (80, 24),
        notice: None,
    }
}

/// Draw once and return a snapshot of the rendered buffer.
fn render_once(comp: &TranscriptComponent, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let ctx = test_ctx();
    terminal.draw(|f| comp.render(f, f.area(), &ctx)).unwrap();
    terminal.backend().buffer().clone()
}

/// Rendered text rows (symbols joined, wide-char hidden cells skipped).
fn rows(buf: &Buffer) -> Vec<String> {
    (0..buf.area.height)
        .map(|y| {
            let mut row = String::new();
            let mut x = 0u16;
            while x < buf.area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    row.push_str(cell.symbol());
                    x += cell.cell_width().max(1);
                } else {
                    x += 1;
                }
            }
            row
        })
        .collect()
}

/// Assert the rendered text layer against expected rows.
#[track_caller]
fn assert_rows(buf: &Buffer, expected: &[String]) {
    assert_eq!(rows(buf), expected);
}

/// Find the first cell carrying `symbol`; panics when absent.
#[track_caller]
fn find_cell(buf: &Buffer, symbol: &str) -> (u16, u16) {
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            if buf.cell((x, y)).is_some_and(|c| c.symbol() == symbol) {
                return (x, y);
            }
        }
    }
    panic!("symbol {symbol:?} not found in buffer");
}

/// A full-width content row: `text` padded with trailing spaces to the
/// area width (ASCII/narrow text — char count equals display width).
fn row(width: usize, text: &str) -> String {
    format!("{text:<width$}", width = width)
}

/// A blank full-width row.
fn blank(width: usize) -> String {
    " ".repeat(width)
}

fn user_msg(content: &str) -> Message {
    Message {
        role: MessageRole::User,
        content: content.to_owned(),
        tool_call_id: None,
        name: None,
        tool_calls: None,
    }
}

fn assistant_msg(content: &str) -> Message {
    Message {
        role: MessageRole::Assistant,
        content: content.to_owned(),
        tool_call_id: None,
        name: None,
        tool_calls: None,
    }
}

fn assistant_call(id: &str, name: &str) -> Message {
    Message {
        role: MessageRole::Assistant,
        content: String::new(),
        tool_call_id: None,
        name: None,
        tool_calls: Some(vec![ToolCall {
            id: ToolCallId(id.to_owned()),
            name: name.to_owned(),
            arguments: json!({}),
        }]),
    }
}

fn tool_result(id: &str, name: &str, content: &str) -> Message {
    Message {
        role: MessageRole::Tool,
        content: content.to_owned(),
        tool_call_id: Some(ToolCallId(id.to_owned())),
        name: Some(name.to_owned()),
        tool_calls: None,
    }
}

#[test]
fn renders_user_band_and_assistant_anchor() {
    let mut t = TranscriptComponent::new();
    t.rebuild(&[user_msg("hi"), assistant_msg("hello")]);
    let buf = render_once(&t, 20, 7);
    // restyle-1: user = #262626 band (blank + `›` row + blank); the
    // assistant opens with the `●` text-color anchor, block gap between.
    assert_rows(
        &buf,
        &[
            blank(20),
            row(20, "  › hi"),
            blank(20),
            blank(20),
            row(20, "● hello"),
            blank(20),
            blank(20),
        ],
    );
    // The `›` anchor is BOLD signal on the band; the band rows carry
    // the band background.
    let (bx, by) = find_cell(&buf, "›");
    let anchor = buf.cell((bx, by)).unwrap();
    assert_eq!(anchor.style().fg, theme().user_label.fg);
    assert!(anchor
        .style()
        .add_modifier
        .contains(ratatui::style::Modifier::BOLD));
    assert_eq!(
        buf.cell((0, 0)).unwrap().style().bg,
        theme().user_message_bg.bg,
        "band background covers the blank band row"
    );
    // 'l' occurs only in the assistant body ("hello"): text color.
    let (ax, ay) = find_cell(&buf, "l");
    assert_eq!(buf.cell((ax, ay)).unwrap().style().fg, theme().assistant.fg);
    // The `●` anchor carries the text color too.
    let (ux, uy) = find_cell(&buf, "●");
    assert_eq!(buf.cell((ux, uy)).unwrap().style().fg, theme().assistant.fg);
}

#[test]
fn wraps_long_text_at_word_boundaries() {
    let mut t = TranscriptComponent::new();
    t.push_user("one two three four five");
    let buf = render_once(&t, 22, 6);
    // Band content width = 22 - 4 = 18: "one two three four" (18) then
    // "five" — every band row padded to the full width.
    assert_rows(
        &buf,
        &[
            blank(22),
            row(22, "  › one two three four"),
            row(22, "    five"),
            blank(22),
            blank(22),
            blank(22),
        ],
    );
}

#[test]
fn wraps_cjk_per_character() {
    let mut t = TranscriptComponent::new();
    t.rebuild(&[user_msg("go"), assistant_msg("世世世世世")]);
    // Width 8: user band (`  › go`); assistant anchor + avail 6 →
    // 世世世 / 世世.
    let buf = render_once(&t, 8, 6);
    assert_rows(
        &buf,
        &[
            blank(8),
            row(8, "  › go"),
            blank(8),
            blank(8),
            "● 世世世".to_owned(), // 2 + 6 = 8 cols, no padding
            "  世世  ".to_owned(), // 2 + 4 = 6 cols, +2 padding
        ],
    );
}

#[test]
fn renders_tool_entries_success_and_failure() {
    let mut t = TranscriptComponent::new();
    t.rebuild(&[
        user_msg("go"),
        assistant_call("t1", "echo"),
        tool_result("t1", "echo", "ok"),
        assistant_call("t2", "grep"),
        tool_result("t2", "grep", "Error: x"),
    ]);
    let buf = render_once(&t, 40, 5);
    // restyle-1: connectors (`├` connects to the next tool row, `└`
    // closes), status markers (✓ success / × error), BOLD English
    // verbs, muted ` · N output lines` suffix (folded output text
    // known) and the error summary in error color.
    assert_rows(
        &buf,
        &[
            blank(40),
            row(40, "  › go"),
            blank(40),
            row(40, "├ ✓ Used echo {} · 1 output line"),
            row(40, "└ × Searched {} · Error: x"),
        ],
    );
    // The ✓ marker keeps its success green…
    let (sx, sy) = find_cell(&buf, "✓");
    assert_eq!(
        buf.cell((sx, sy)).unwrap().style().fg,
        theme().tool_success.fg
    );
    // …the × marker is error red; the failing verb is BOLD error.
    let (fx, fy) = find_cell(&buf, "×");
    assert_eq!(
        buf.cell((fx, fy)).unwrap().style().fg,
        theme().tool_failure.fg
    );
    let (vx, vy) = find_cell(&buf, "S");
    let verb = buf.cell((vx, vy)).unwrap();
    assert_eq!(verb.style().fg, theme().tool_failure.fg);
    assert!(verb
        .style()
        .add_modifier
        .contains(ratatui::style::Modifier::BOLD));
    // The succeeding verb is BOLD text; the args summary muted.
    let (ex, ey) = find_cell(&buf, "U");
    let verb = buf.cell((ex, ey)).unwrap();
    assert_eq!(verb.style().fg, theme().assistant.fg);
    assert!(verb
        .style()
        .add_modifier
        .contains(ratatui::style::Modifier::BOLD));
    let (ox, oy) = find_cell(&buf, "{");
    assert_eq!(buf.cell((ox, oy)).unwrap().style().fg, theme().muted.fg);
    // Connectors in the line color.
    let (cx, cy) = find_cell(&buf, "├");
    assert_eq!(buf.cell((cx, cy)).unwrap().style().fg, theme().line.fg);
}

#[test]
fn renders_running_tool_row_with_accent_marker() {
    let mut t = TranscriptComponent::new();
    t.push_user("go");
    t.tool_start("read_file", r#"{"path":"a.rs"}"#);
    let buf = render_once(&t, 40, 4);
    // restyle-1: running rows carry the accent `•` marker + the BOLD
    // present-participle verb (the spinner lives on the status line
    // now); the muted suffix reports the live elapsed time.
    let lines = rows(&buf);
    assert_eq!(lines[0], blank(40), "band blank above");
    assert_eq!(lines[1], row(40, "  › go"));
    assert_eq!(lines[2], blank(40), "band blank below");
    assert!(
        lines[3].starts_with(r#"└ • Reading {"path":"a.rs"} · "#),
        "{}",
        lines[3]
    );
    let (ix, iy) = find_cell(&buf, "•");
    assert_eq!(
        buf.cell((ix, iy)).unwrap().style().fg,
        theme().user_label.fg
    );
    let (rx, ry) = find_cell(&buf, "R"); // 'R' only in Reading (row 1)
    let verb = buf.cell((rx, ry)).unwrap();
    assert_eq!(verb.style().fg, theme().assistant.fg);
    assert!(verb
        .style()
        .add_modifier
        .contains(ratatui::style::Modifier::BOLD));
}

#[test]
fn renders_delegate_marker_and_approval_outcome() {
    let mut t = TranscriptComponent::new();
    t.tool_start("call_agent", r#"{"agent_id":"researcher"}"#);
    t.push_approval("shell", "denied");
    let buf = render_once(&t, 26, 5);
    // restyle-1: `● agent Running` (accent marker, BOLD agent, muted
    // label); the denied approval outcome is `× …` in error color.
    let approval_row = format!("× shell — denied{}", " ".repeat(26 - 16));
    assert_rows(
        &buf,
        &[
            row(26, "● researcher Running"),
            approval_row,
            blank(26),
            blank(26),
            blank(26),
        ],
    );
    let (dx, dy) = find_cell(&buf, "●");
    assert_eq!(
        buf.cell((dx, dy)).unwrap().style().fg,
        theme().user_label.fg
    );
    let (ax, ay) = find_cell(&buf, "×");
    assert_eq!(
        buf.cell((ax, ay)).unwrap().style().fg,
        theme().tool_failure.fg
    );
    // denied → the decision text is red ('d' only in "denied").
    let (ex, ey) = find_cell(&buf, "d");
    assert_eq!(
        buf.cell((ex, ey)).unwrap().style().fg,
        theme().tool_failure.fg
    );
}

#[test]
fn renders_step_separator_as_blank_line() {
    let mut t = TranscriptComponent::new();
    t.rebuild(&[
        user_msg("go"),
        assistant_call("t", "echo"),
        tool_result("t", "echo", "ok"),
    ]);
    t.step_end();
    let buf = render_once(&t, 40, 5);
    // A step separator renders as one blank line after the tool row.
    assert_rows(
        &buf,
        &[
            blank(40),
            row(40, "  › go"),
            blank(40),
            row(40, "└ ✓ Used echo {} · 1 output line"),
            blank(40),
        ],
    );
}

#[test]
fn renders_end_of_turn_marker() {
    let mut t = TranscriptComponent::new();
    t.rebuild(&[user_msg("go"), assistant_msg("ok")]);
    t.set_turn_meta("main".to_owned(), 9, None);
    let buf = render_once(&t, 24, 7);
    // restyle-1: `└ main · 9s` — the connector and body muted. Hand-
    // padded by display width (CJK defeats `row()`'s char-count pad).
    let marker_row = format!("└ main · 9s{}", " ".repeat(24 - 11));
    assert_rows(
        &buf,
        &[
            blank(24),
            row(24, "  › go"),
            blank(24),
            blank(24),
            row(24, "● ok"),
            blank(24),
            marker_row,
        ],
    );
    let (mx, my) = find_cell(&buf, "└");
    assert_eq!(buf.cell((mx, my)).unwrap().style().fg, theme().muted.fg);
    // 'a' occurs only in "main" (the muted body span).
    let (ax, ay) = find_cell(&buf, "a");
    assert_eq!(buf.cell((ax, ay)).unwrap().style().fg, theme().muted.fg);
    // Without turn_meta there is no marker anywhere.
    let mut t2 = TranscriptComponent::new();
    t2.rebuild(&[user_msg("go"), assistant_msg("ok")]);
    let buf2 = render_once(&t2, 24, 6);
    assert!(rows(&buf2).iter().all(|r| !r.contains("· 9s")));
}

/// fix-18: the streaming answer renders markdown live through the
/// same pipeline as committed blocks — the live signal is a `▍` tail
/// cursor on the answer's last row (accent), while reasoning collapses
/// to the `•` summary row.
#[test]
fn streams_markdown_with_tail_cursor() {
    let mut t = TranscriptComponent::new();
    t.begin_streaming();
    t.push_reasoning("thinking…");
    t.push_delta("partial answer");
    let buf = render_once(&t, 24, 5);
    assert_rows(
        &buf,
        &[
            row(24, "• thinking…"),
            blank(24),
            row(24, "● partial answer▍"),
            blank(24),
            blank(24),
        ],
    );
    // Tail cursor keeps the accent color; the reasoning marker accent,
    // its summary BOLD muted.
    let (cx, cy) = find_cell(&buf, "▍");
    assert_eq!(
        buf.cell((cx, cy)).unwrap().style().fg,
        theme().user_label.fg
    );
    let (rx, ry) = find_cell(&buf, "•");
    assert_eq!(
        buf.cell((rx, ry)).unwrap().style().fg,
        theme().user_label.fg
    );
    let (tx, ty) = find_cell(&buf, "t"); // 't' only in "thinking…"
    let summary = buf.cell((tx, ty)).unwrap();
    assert_eq!(summary.style().fg, theme().muted.fg);
    assert!(summary
        .style()
        .add_modifier
        .contains(ratatui::style::Modifier::BOLD));
}

/// Markdown styles land LIVE in the frame buffer: a heading delta
/// paints #CBA6F7+BOLD (+UNDERLINED for H1) into the cells the reader
/// is watching, before any flush boundary (fix-18).
#[test]
fn streaming_heading_paints_accent_cells_before_flush() {
    let mut t = TranscriptComponent::new();
    t.begin_streaming();
    t.push_delta("# Result heading");
    let buf = render_once(&t, 30, 4);
    assert_rows(
        &buf,
        &[
            row(30, "● Result heading▍"),
            blank(30),
            blank(30),
            blank(30),
        ],
    );
    // 'R' occurs only in the heading text: markdownHeading accent +
    // BOLD + UNDERLINED, mid-stream.
    let (hx, hy) = find_cell(&buf, "R");
    let cell = buf.cell((hx, hy)).unwrap();
    assert_eq!(cell.style().fg, theme().md_heading.fg);
    assert!(cell
        .style()
        .add_modifier
        .contains(ratatui::style::Modifier::BOLD));
    assert!(cell
        .style()
        .add_modifier
        .contains(ratatui::style::Modifier::UNDERLINED));
    // The tail cursor cell is the running accent.
    let (cx, cy) = find_cell(&buf, "▍");
    assert_eq!(
        buf.cell((cx, cy)).unwrap().style().fg,
        theme().user_label.fg
    );
}

#[test]
fn shows_empty_state_wordmark_and_hint() {
    let t = TranscriptComponent::new();
    let buf = render_once(&t, 44, 5);
    // P2 (restyle-1): the welcome wordmark `✦ OpenSlate` (gradient
    // #A5F3FC→#67E8F9→#22D3EE, two letters per tone) over the muted
    // empty-session hint.
    let hint = "空会话 — 输入 prompt 开始,Enter 发送"; // display width 36
    let hint_row = format!("{hint}{}", " ".repeat(44 - 36));
    assert_rows(
        &buf,
        &[
            row(44, "✦ OpenSlate"),
            hint_row,
            blank(44),
            blank(44),
            blank(44),
        ],
    );
    // Gradient tones: O/p = highlight, e/n = brand (the running accent
    // slot the renderer uses as the middle tone), S/= shadow.
    let cases = [
        ("O", theme().wordmark_highlight.fg),
        ("p", theme().wordmark_highlight.fg),
        ("e", theme().tool_running.fg),
        ("S", theme().wordmark_shadow.fg),
    ];
    for (glyph, tone) in cases {
        let (x, y) = find_cell(&buf, glyph);
        assert_eq!(
            buf.cell((x, y)).unwrap().style().fg,
            tone,
            "gradient tone for {glyph}"
        );
    }
    let (mx, my) = find_cell(&buf, "空");
    assert_eq!(buf.cell((mx, my)).unwrap().style().fg, theme().muted.fg);
}

// ─── splash-1: the empty-session ANSI-Shadow splash ────────────────────────

/// The full tier's six frozen rows (an independent copy for the
/// render assertions — the consts themselves are pinned in the
/// component's unit tests). Every glyph is a width-1 BMP box/block
/// character, so display width == char count.
const ART_FULL: [&str; 6] = [
    " ██████╗ ██████╗ ███████╗███╗   ██╗███████╗██╗      █████╗ ████████╗███████╗",
    "██╔═══██╗██╔══██╗██╔════╝████╗  ██║██╔════╝██║     ██╔══██╗╚══██╔══╝██╔════╝",
    "██║   ██║██████╔╝█████╗  ██╔██╗ ██║███████╗██║     ███████║   ██║   █████╗",
    "██║   ██║██╔═══╝ ██╔══╝  ██║╚██╗██║╚════██║██║     ██╔══██║   ██║   ██╔══╝",
    "╚██████╔╝██║     ███████╗██║ ╚████║███████║███████╗██║  ██║   ██║   ███████╗",
    " ╚═════╝ ╚═╝     ╚══════╝╚═╝  ╚═══╝╚══════╝╚══════╝╚═╝  ╚═╝   ╚═╝   ╚══════╝",
];

/// The medium tier's six frozen rows (`Slate`).
const ART_MEDIUM: [&str; 6] = [
    "███████╗██╗      █████╗ ████████╗███████╗",
    "██╔════╝██║     ██╔══██╗╚══██╔══╝██╔════╝",
    "███████╗██║     ███████║   ██║   █████╗",
    "╚════██║██║     ██╔══██║   ██║   ██╔══╝",
    "███████║███████╗██║  ██║   ██║   ███████╗",
    "╚══════╝╚══════╝╚═╝  ╚═╝   ╚═╝   ╚══════╝",
];

/// Pad an art row to its block width by CHAR count (== display
/// width — every art glyph is width-1): the hero.ts canvasLine
/// treatment, whose trailing spaces ride the row's band tone.
fn pad76(line: &str) -> String {
    format!("{line:<width$}", width = 76)
}
fn pad41(line: &str) -> String {
    format!("{line:<width$}", width = 41)
}

/// Render once with a theme override (the three-board color tests).
fn render_once_theme(comp: &TranscriptComponent, width: u16, height: u16, theme: Theme) -> Buffer {
    let mut ctx = test_ctx();
    ctx.theme = theme;
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|f| comp.render(f, f.area(), &ctx)).unwrap();
    terminal.backend().buffer().clone()
}

/// Roomy empty session → the centered FULL splash: the six art rows
/// at top = (30−8)/2 = 11, left = (100−76)/2 = 12, four gradient
/// bands (rows 1-2 highlight, rows 3-4 signal, rows 5-6 shadow),
/// every row BOLD and padded to 76 (the padding spaces carry the
/// tone), blank row, then the muted hint centered on the same axis
/// (display width 36 → x = (100−36)/2 = 32).
#[test]
fn shows_empty_state_splash_centered() {
    let t = TranscriptComponent::new();
    let buf = render_once(&t, 100, 30);
    let expected: Vec<String> = (0..30)
        .map(|y| match y {
            11..=16 => format!(
                "{}{}{}",
                " ".repeat(12),
                pad76(ART_FULL[(y - 11) as usize]),
                " ".repeat(12)
            ),
            18 => format!(
                "{}空会话 — 输入 prompt 开始,Enter 发送{}",
                " ".repeat(32),
                " ".repeat(100 - 32 - 36)
            ),
            _ => blank(100),
        })
        .collect();
    assert_rows(&buf, &expected);

    // The four bands + BOLD: rows 1-2 highlight, rows 3-4 signal
    // (the running accent slot), rows 5-6 shadow — one solid glyph
    // per row, plus the row-3 canvasLine padding space at the
    // block's last column (raw row is 74 wide → col 75 is padding).
    let cases = [
        ((13, 11), theme().wordmark_highlight.fg), // `█` of row 1
        ((12, 12), theme().wordmark_highlight.fg), // `█` of row 2 (flush)
        ((12, 13), theme().tool_running.fg),       // `█` of row 3 (flush)
        ((12, 14), theme().tool_running.fg),       // `█` of row 4 (flush)
        ((12, 15), theme().wordmark_shadow.fg),    // `╚` of row 5 (flush)
        ((13, 16), theme().wordmark_shadow.fg),    // `╚` of row 6
        ((87, 13), theme().tool_running.fg),       // padded space, same tone
    ];
    for ((x, y), tone) in cases {
        let style = buf.cell((x, y)).unwrap().style();
        assert_eq!(style.fg, tone, "band tone at ({x},{y})");
        assert!(
            style.add_modifier.contains(ratatui::style::Modifier::BOLD),
            "row is BOLD at ({x},{y})"
        );
    }
    // The hint rides muted, centered under the block (same axis:
    // the shared (area−w)/2 rounding).
    let (hx, hy) = find_cell(&buf, "空");
    assert_eq!((hx, hy), (32, 18), "hint centered on the block axis");
    assert_eq!(buf.cell((hx, hy)).unwrap().style().fg, theme().muted.fg);
    // Nothing above or below the block.
    assert_eq!(rows(&buf)[10], blank(100));
    assert_eq!(rows(&buf)[19], blank(100));
}

/// The size ladder's exact boundaries: 80×10 paints the full art
/// (left (80−76)/2 = 2, top 1), 79×10 and 45×10 the medium art
/// (left 19 / 2), 44×10 and 80×9 the legacy wordmark.
#[test]
fn splash_ladder_boundaries_are_80_45_10() {
    // Full at the 80 boundary.
    let t = TranscriptComponent::new();
    let buf = render_once(&t, 80, 10);
    assert_eq!(
        rows(&buf)[1],
        format!("  {}  ", pad76(ART_FULL[0])),
        "80 wide takes the full art"
    );
    assert_eq!(rows(&buf)[0], blank(80));
    let (hx, hy) = find_cell(&buf, "空");
    assert_eq!((hx, hy), (22, 8), "hint at (80−36)/2, block row 8");

    // 79 wide → medium (left (79−41)/2 = 19).
    let t = TranscriptComponent::new();
    let buf = render_once(&t, 79, 10);
    assert_eq!(
        rows(&buf)[1],
        format!(
            "{}{}{}",
            " ".repeat(19),
            pad41(ART_MEDIUM[0]),
            " ".repeat(79 - 19 - 41)
        ),
        "79 wide takes the medium art"
    );

    // Medium at the 45 boundary (left 2).
    let t = TranscriptComponent::new();
    let buf = render_once(&t, 45, 10);
    assert_eq!(
        rows(&buf)[1],
        format!("  {}  ", pad41(ART_MEDIUM[0])),
        "45 wide still takes the medium art"
    );

    // One column less → legacy wordmark.
    let t = TranscriptComponent::new();
    let buf = render_once(&t, 44, 10);
    assert!(
        rows(&buf)[0].starts_with("✦ OpenSlate"),
        "width 44 falls back to the wordmark"
    );
    // One row less → legacy wordmark.
    let t = TranscriptComponent::new();
    let buf = render_once(&t, 80, 9);
    assert!(
        rows(&buf)[0].starts_with("✦ OpenSlate"),
        "height 9 falls back to the wordmark"
    );
}

/// The medium tier's own geometry: at 60×12 the block sits at
/// left (60−41)/2 = 9, top (12−8)/2 = 2 — full row pinning plus the
/// same four-band gradient (one solid glyph per band row, the
/// row-3 padding space included) and the hint at (60−36)/2 = 12.
#[test]
fn medium_splash_gradient_and_centering() {
    let t = TranscriptComponent::new();
    let buf = render_once(&t, 60, 12);
    let expected: Vec<String> = (0..12)
        .map(|y| match y {
            2..=7 => format!(
                "{}{}{}",
                " ".repeat(9),
                pad41(ART_MEDIUM[(y - 2) as usize]),
                " ".repeat(60 - 9 - 41)
            ),
            9 => format!(
                "{}空会话 — 输入 prompt 开始,Enter 发送{}",
                " ".repeat(12),
                " ".repeat(60 - 12 - 36)
            ),
            _ => blank(60),
        })
        .collect();
    assert_rows(&buf, &expected);

    let cases = [
        ((9, 2), theme().wordmark_highlight.fg),
        ((9, 3), theme().wordmark_highlight.fg),
        ((9, 4), theme().tool_running.fg),
        ((9, 5), theme().tool_running.fg),
        ((9, 6), theme().wordmark_shadow.fg),
        ((9, 7), theme().wordmark_shadow.fg),
        ((49, 4), theme().tool_running.fg), // row 3 is 39 wide → col 40 padded
    ];
    for ((x, y), tone) in cases {
        let style = buf.cell((x, y)).unwrap().style();
        assert_eq!(style.fg, tone, "band tone at ({x},{y})");
        assert!(
            style.add_modifier.contains(ratatui::style::Modifier::BOLD),
            "row is BOLD at ({x},{y})"
        );
    }
    let (hx, hy) = find_cell(&buf, "空");
    assert_eq!((hx, hy), (12, 9), "hint centered on the block axis");
}

/// The pure-ASCII icon tier never shows the box/block art: at a
/// roomy geometry it renders the legacy wordmark (brand `*`) and the
/// localized hint instead.
#[test]
fn ascii_icon_tier_falls_back_to_wordmark() {
    let t = TranscriptComponent::new();
    let buf = render_once_theme(&t, 100, 30, Theme::dark().with_icons(Icons::Ascii));
    assert!(
        rows(&buf)[0].starts_with("* OpenSlate"),
        "ascii tier falls back to the legacy wordmark"
    );
    let (_, hy) = find_cell(&buf, "空");
    assert_eq!(hy, 1, "the hint rides the second flow row");
    let screen = rows(&buf).join("\n");
    assert!(!screen.contains('█'), "no block glyph anywhere");
    assert!(!screen.contains('╗'), "no shadow corner anywhere");
    assert!(
        screen.contains("空会话 - 输入 prompt 开始,Enter 发送"),
        "the hint localizes the em-dash for the ascii tier"
    );
}

/// The fallback stays byte-identical below the ladder: at 44×12 the
/// empty session renders the legacy wordmark + left-aligned hint
/// exactly as before splash-1.
#[test]
fn empty_state_fallback_is_byte_identical_below_ladder() {
    let t = TranscriptComponent::new();
    let buf = render_once(&t, 44, 12);
    let hint = "空会话 — 输入 prompt 开始,Enter 发送"; // display width 36
    assert_eq!(rows(&buf)[0], row(44, "✦ OpenSlate"));
    assert_eq!(rows(&buf)[1], format!("{hint}{}", " ".repeat(44 - 36)));
    for y in 2..12 {
        assert_eq!(rows(&buf)[y], blank(44));
    }
}

/// Any content kills the splash: one user message → the document flow
/// renders and no art fragment survives anywhere on screen.
#[test]
fn non_empty_transcript_has_no_splash_art() {
    let mut t = TranscriptComponent::new();
    t.push_user("hello");
    let buf = render_once(&t, 100, 30);
    let screen = rows(&buf).join("\n");
    assert!(screen.contains("hello"), "the content renders");
    assert!(!screen.contains("███████╗"), "no art fragment (`███████╗`)");
    assert!(!screen.contains("╚══════╝"), "no art fragment (`╚══════╝`)");
    assert!(!screen.contains("空会话"), "the empty-state hint is gone");
}

/// The bands ride the theme slots on every board — the light board's
/// hexes and the ansi board's Indexed values (no Rgb leak in the
/// degraded board) flow through the same wordmark slots, every row
/// BOLD.
#[test]
fn splash_bands_ride_theme_slots_on_all_boards() {
    for board in [Theme::light(), Theme::ansi()] {
        let t = TranscriptComponent::new();
        let buf = render_once_theme(&t, 100, 30, board);
        let cases = [
            ((13, 11), board.wordmark_highlight.fg),
            ((12, 12), board.wordmark_highlight.fg),
            ((12, 13), board.tool_running.fg),
            ((12, 14), board.tool_running.fg),
            ((12, 15), board.wordmark_shadow.fg),
            ((13, 16), board.wordmark_shadow.fg),
            ((32, 18), board.muted.fg),
        ];
        for ((x, y), tone) in cases {
            let style = buf.cell((x, y)).unwrap().style();
            assert_eq!(style.fg, tone, "{:?} band tone at ({x},{y})", board.line.fg);
            if y <= 16 {
                assert!(
                    style.add_modifier.contains(ratatui::style::Modifier::BOLD),
                    "{:?} art row is BOLD at ({x},{y})",
                    board.line.fg
                );
            }
        }
    }
    // The ansi board must not leak a single Rgb cell on the art rows.
    let t = TranscriptComponent::new();
    let buf = render_once_theme(&t, 100, 30, Theme::ansi());
    for y in 11..=16u16 {
        for x in 0..100u16 {
            assert!(
                !matches!(
                    buf.cell((x, y)).unwrap().style().fg,
                    Some(ratatui::style::Color::Rgb(..))
                ),
                "Rgb leak at ({x},{y})"
            );
        }
    }
}

#[test]
fn pinned_view_shows_new_content_hint() {
    let mut t = TranscriptComponent::new();
    for i in 0..10 {
        t.push_user(&format!("line{i}"));
    }
    // First render establishes the viewport (40x6): restyle-1 bands →
    // 3 + 9×4 = 39 rows, 6 visible → max scroll 33.
    let mut terminal = Terminal::new(TestBackend::new(40, 6)).unwrap();
    let mut ctx = test_ctx();
    terminal.draw(|f| t.render(f, f.area(), &ctx)).unwrap();
    // ScrollUp unpins one line above the bottom (scroll 32 of max 33).
    t.handle(&Action::ScrollUp, &mut ctx);
    assert!(t.is_pinned());
    terminal.draw(|f| t.render(f, f.area(), &ctx)).unwrap();

    // Rows 32..38 visible: user8's band rows (blank, content, blank),
    // user9's gap, band blank — and the hint overwrites the last row
    // (the `+1` count = one rendered row below the pin: line9's body).
    assert_rows(
        terminal.backend().buffer(),
        &[
            blank(40),
            row(40, "  › line8"),
            blank(40),
            blank(40),
            blank(40),
            "  › line9                 +1 ↓ 回到底部 ".to_owned(),
        ],
    );
    let hint_row = 5;
    let (hx, hy) = find_cell(terminal.backend().buffer(), "↓");
    assert_eq!(hy, hint_row);
    assert_eq!(
        terminal
            .backend()
            .buffer()
            .cell((hx, hy))
            .unwrap()
            .style()
            .fg,
        theme().user_label.fg
    );
}

#[test]
fn following_view_sticks_to_bottom_without_hint() {
    let mut t = TranscriptComponent::new();
    for i in 0..10 {
        t.push_user(&format!("line{i}"));
    }
    let buf = render_once(&t, 40, 6);
    // Following: last 6 of the 39 rows visible (33..39), no hint.
    assert_rows(
        &buf,
        &[
            row(40, "  › line8"),
            blank(40),
            blank(40),
            blank(40),
            row(40, "  › line9"),
            blank(40),
        ],
    );
    assert!(rows(&buf).iter().all(|r| !r.contains("回到底部")));
}

/// Assistant text renders through the markdown subset in BOTH states:
/// a heading row carries the markdownHeading +BOLD style, inline code
/// the distinct code color, fenced code stays verbatim — and the SAME
/// text while STILL streaming renders the SAME rows (fix-18), the
/// only live-vs-committed difference being the tail cursor.
#[test]
fn renders_markdown_in_finalized_assistant_blocks() {
    let mut t = TranscriptComponent::new();
    t.rebuild(&[
        user_msg("go"),
        assistant_msg("# Result\n\nanswer with `code`"),
    ]);
    let buf = render_once(&t, 30, 7);
    assert_rows(
        &buf,
        &[
            blank(30),
            row(30, "  › go"),
            blank(30),
            blank(30),
            row(30, "● Result"),
            blank(30),
            row(30, "  answer with code"),
        ],
    );
    // Heading row: markdownHeading (#CBA6F7) + BOLD on the text cells.
    let (hx, hy) = find_cell(&buf, "R"); // 'R' only in "Result"
    let heading_cell = buf.cell((hx, hy)).unwrap();
    assert_eq!(heading_cell.style().fg, theme().md_heading.fg);
    assert!(heading_cell
        .style()
        .add_modifier
        .contains(ratatui::style::Modifier::BOLD));
    // Inline code: markdownCode green.
    let (cx, cy) = find_cell(&buf, "c"); // 'c' only in "code"
    assert_eq!(buf.cell((cx, cy)).unwrap().style().fg, theme().md_code.fg);

    // The same text while STILL streaming: identical rows (markdown
    // applied live) plus the tail cursor on the last row.
    let mut live = TranscriptComponent::new();
    live.push_user("go");
    live.begin_streaming();
    live.push_delta("# Result\n\nanswer with `code`");
    let buf = render_once(&live, 30, 7);
    assert_rows(
        &buf,
        &[
            blank(30),
            row(30, "  › go"),
            blank(30),
            blank(30),
            row(30, "● Result"),
            blank(30),
            row(30, "  answer with code▍"),
        ],
    );
    // The live heading carries the same accent + BOLD as the
    // committed one — no raw `#`/backtick markers anywhere.
    let (hx, hy) = find_cell(&buf, "R");
    let live_heading = buf.cell((hx, hy)).unwrap();
    assert_eq!(live_heading.style().fg, theme().md_heading.fg);
    assert!(live_heading
        .style()
        .add_modifier
        .contains(ratatui::style::Modifier::BOLD));
    let (cx, cy) = find_cell(&buf, "c");
    assert_eq!(buf.cell((cx, cy)).unwrap().style().fg, theme().md_code.fg);
}

/// Pin geometry under live markdown re-rendering (fix-18): while
/// pinned, the `+N ↓ 回到底部` hint stays up as newly streaming content
/// changes the rendered row count below the pin — and the `+N` count
/// (fix-25: a PREFIX now, the old fix-23 count restyled) TRACKS the
/// rendered rows: a closing marker that collapses the literal wrap
/// shrinks N with it. The pin offset itself stays put (clamped, no
/// drift), and follow mode is unaffected (still at the bottom, no
/// hint).
#[test]
fn pinned_hint_holds_while_streaming_rows_change() {
    let mut t = TranscriptComponent::new();
    for i in 0..10 {
        t.push_user(&format!("line{i}")); // 39 layout rows at width 40
    }
    let mut terminal = Terminal::new(TestBackend::new(40, 6)).unwrap();
    let mut ctx = test_ctx();
    terminal.draw(|f| t.render(f, f.area(), &ctx)).unwrap();
    t.handle(&Action::ScrollUp, &mut ctx); // pinned at 32 of max 33
    terminal.draw(|f| t.render(f, f.area(), &ctx)).unwrap();
    assert!(rows(terminal.backend().buffer())[5].contains("+1 ↓ 回到底部"));

    // Unclosed inline code, one char over the available width (38):
    // the literal (backtick included) wraps onto a second row → below
    // grows (band blank + gap + 2 wrapped rows below the pin).
    t.begin_streaming();
    t.push_delta(&format!("`{}", "b".repeat(38)));
    terminal.draw(|f| t.render(f, f.area(), &ctx)).unwrap();
    let raw = rows(terminal.backend().buffer());
    assert!(
        raw[5].contains("+4 ↓ 回到底部"),
        "literal wrap counted: {}",
        raw[5]
    );

    // The closing backtick arrives: the marker pair vanishes, the
    // code span fits the row → the count shrinks by one.
    t.push_delta("`");
    terminal.draw(|f| t.render(f, f.area(), &ctx)).unwrap();
    let styled = rows(terminal.backend().buffer());
    assert!(
        styled[5].contains("+3 ↓ 回到底部"),
        "rendered rows counted: {}",
        styled[5]
    );
}

/// Fenced code blocks render verbatim in the code color and CLIP (no
/// word wrap) when a line exceeds the available width.
#[test]
fn renders_fenced_code_verbatim_with_clip() {
    let mut t = TranscriptComponent::new();
    t.rebuild(&[assistant_msg("```\nabcdef\n```")]);
    let buf = render_once(&t, 6, 3); // avail = 6-2 = 4 → "abcd"
    assert_rows(&buf, &["● abcd".to_owned(), blank(6), blank(6)]);
    // The clipped content keeps the code color (the ● anchor leads the
    // row in text color).
    let (fx, fy) = find_cell(&buf, "a");
    assert_eq!(buf.cell((fx, fy)).unwrap().style().fg, theme().md_code.fg);
}

/// Per-request meta lines render dim, indented 2, below their block.
/// The usage line carries the ttft segment (FirstToken observed).
/// fix-19: the usage line is HELD at RequestEnd and lands via the
/// tool-less flush (`flush_pending_step_meta` — the App fires it at
/// the next RequestStart / TurnDone).
#[test]
fn renders_request_meta_lines_dim_and_indented() {
    let mut t = TranscriptComponent::new();
    t.push_user("go");
    t.begin_streaming();
    t.push_reasoning("thinking");
    t.push_delta("done");
    t.finish_request(
        Some(openslate_core::types::Usage {
            input_tokens: 50,
            output_tokens: 10,
            cached_input_tokens: None,
        }),
        Some(std::time::Duration::from_secs(2)),
        Some(std::time::Duration::from_millis(800)),
    );
    t.flush_pending_step_meta();
    let buf = render_once(&t, 30, 9);
    let screen = rows(&buf).join("\n");
    assert!(screen.contains("~4tok"), "estimate line present"); // 9 chars / 2.2 ≈ 4
    assert!(
        screen.contains("↑50 ↓10 · ttft 0.8s · 5tok/s"),
        "exact line present:\n{screen}"
    );
    // Meta cells are muted (#ADADAD) — check the usage line's arrow.
    let all_rows = rows(&buf);
    let y = all_rows
        .iter()
        .position(|r| r.contains("↑50"))
        .expect("usage row") as u16;
    let row = &all_rows[y as usize];
    let x = row.find('↑').expect("arrow position") as u16;
    assert_eq!(buf.cell((x, y)).unwrap().style().fg, theme().muted.fg);
    // Indent 2: the arrow sits in column 2.
    assert_eq!(x, 2);
}

/// Defect B (render level): a tool-call-only step (empty answer)
/// merges the reasoning estimate and the request usage into ONE dim
/// meta line — never two stacked metas with no output block between.
#[test]
fn renders_single_merged_meta_line_for_empty_answer() {
    let mut t = TranscriptComponent::new();
    t.push_user("go");
    t.begin_streaming();
    t.push_reasoning("thinking hard"); // 13 chars → ~6tok
    t.finish_request(
        Some(openslate_core::types::Usage {
            input_tokens: 1221,
            output_tokens: 96,
            cached_input_tokens: None,
        }),
        Some(std::time::Duration::from_secs_f64(2.6)),
        Some(std::time::Duration::from_millis(900)),
    );
    t.flush_pending_step_meta(); // fix-19: held line lands (no tools)
    let buf = render_once(&t, 46, 6);
    let all_rows = rows(&buf);
    let screen = all_rows.join("\n");
    assert!(
        screen.contains("~6tok · ↑1221 ↓96 · ttft 0.9s · 37tok/s"),
        "merged meta line:\n{screen}"
    );
    let meta_rows = all_rows.iter().filter(|r| r.contains("tok")).count();
    assert_eq!(meta_rows, 1, "exactly one meta line:\n{screen}");
}

/// Defect A (render level): model output opening/closing with `\n\n`
/// renders without blank gutter rows at the block edges.
#[test]
fn streaming_edge_blank_lines_do_not_render() {
    let mut t = TranscriptComponent::new();
    t.begin_streaming();
    t.push_reasoning("\n\nthinking\n");
    t.push_delta("\n\npartial answer\n\n");
    let buf = render_once(&t, 24, 4);
    assert_rows(
        &buf,
        &[
            row(24, "• thinking"),
            blank(24),
            row(24, "● partial answer▍"),
            blank(24),
        ],
    );
}

/// Defect C (render level): finalized tables render pipes-dropped,
/// columns padded by display width (CJK safe), header BOLD; the
/// `|---|` separator row is gone.
#[test]
fn renders_markdown_table_with_aligned_cjk_columns() {
    let mut t = TranscriptComponent::new();
    t.rebuild(&[assistant_msg(
        "| 类别 | 要点 |\n|---|---|\n| 代码 | 说明内容 |",
    )]);
    let buf = render_once(&t, 24, 4);
    // col0 width 4 (类别/代码), col1 width 8 (说明内容), 2-space gutter.
    // Hand-padded by display width (CJK rows defeat `row()`'s
    // char-count padding).
    let header = format!("● 类别  要点{}", " ".repeat(24 - 12));
    let body = format!("  代码  说明内容{}", " ".repeat(24 - 16));
    assert_rows(&buf, &[header, body, blank(24), blank(24)]);
    // Header row BOLD; body row not.
    let (hx, hy) = find_cell(&buf, "类");
    assert!(buf
        .cell((hx, hy))
        .unwrap()
        .style()
        .add_modifier
        .contains(ratatui::style::Modifier::BOLD));
    let (dx, dy) = find_cell(&buf, "代");
    assert!(!buf
        .cell((dx, dy))
        .unwrap()
        .style()
        .add_modifier
        .contains(ratatui::style::Modifier::BOLD));
}

/// Streaming tables (fix-18): the header row before its separator
/// arrives stays verbatim pipes; once the separator lands, the live
/// render flips to the aligned table layout (pipes dropped, header
/// BOLD, separator consumed).
#[test]
fn streaming_table_renders_live() {
    // Separator not yet arrived: raw pipe prose, body style.
    let mut t = TranscriptComponent::new();
    t.begin_streaming();
    t.push_delta("| a | b |");
    let buf = render_once(&t, 20, 3);
    assert_rows(&buf, &[row(20, "● | a | b |▍"), blank(20), blank(20)]);

    // Separator (and a data row) arrive → the table layout, live; the
    // tail cursor rides the LAST row (the data row).
    let mut t2 = TranscriptComponent::new();
    t2.begin_streaming();
    t2.push_delta("| a | b |\n|---|---|\n| 1 | 2 |");
    let buf = render_once(&t2, 20, 3);
    assert_rows(&buf, &[row(20, "● a  b"), row(20, "  1  2▍"), blank(20)]);
    let (hx, hy) = find_cell(&buf, "a"); // header cell
    assert!(buf
        .cell((hx, hy))
        .unwrap()
        .style()
        .add_modifier
        .contains(ratatui::style::Modifier::BOLD));
}

/// fix-23/fix-24: reasoning renders COLLAPSED to one summary row — the
/// block FLATTENED behind an accent `•` marker in BOLD muted; a click
/// on the row expands the full block, and clicking the expanded block
/// collapses it again — both through the real render pipeline.
#[test]
fn reasoning_renders_collapsed_and_click_toggles() {
    let mut t = TranscriptComponent::new();
    t.push_user("go");
    t.begin_streaming();
    t.push_reasoning("step one\nstep two");
    t.tool_start("echo", "{}"); // boundary flush → committed entry
    let mut terminal = Terminal::new(TestBackend::new(30, 9)).unwrap();
    let mut ctx = test_ctx();
    terminal.draw(|f| t.render(f, f.area(), &ctx)).unwrap();
    let screen = rows(terminal.backend().buffer());
    let summary_row = screen
        .iter()
        .position(|r| r.contains("• step one step two"))
        .expect("collapsed flattened summary row");
    assert_eq!(summary_row, 3, "below the user band: {screen:?}");
    assert!(
        !screen.iter().any(|r| r.trim() == "• step two"),
        "the full block does not render while collapsed: {screen:?}"
    );

    // Click the summary row → expanded full block (the second line
    // becomes its own indented row behind the marker).
    assert_eq!(
        t.handle(&Action::Click(4, summary_row as u16), &mut ctx),
        None
    );
    terminal.draw(|f| t.render(f, f.area(), &ctx)).unwrap();
    let screen = rows(terminal.backend().buffer());
    assert!(
        screen.iter().any(|r| r.trim() == "• step one"),
        "expanded renders the full block: {screen:?}"
    );
    assert!(screen.iter().any(|r| r.trim() == "step two"));

    // Click again (the same spot now lies inside the expanded
    // block) → collapsed again.
    assert_eq!(
        t.handle(&Action::Click(4, summary_row as u16), &mut ctx),
        None
    );
    terminal.draw(|f| t.render(f, f.area(), &ctx)).unwrap();
    let screen = rows(terminal.backend().buffer());
    assert!(screen.iter().any(|r| r.contains("• step one step two")));
    assert!(
        !screen.iter().any(|r| r.trim() == "• step two"),
        "re-collapsed: {screen:?}"
    );
}

/// fix-23/fix-24: the LIVE streaming reasoning block collapses to a
/// flattened summary that re-renders with every delta; once it
/// overflows the width, the TAIL survives behind a leading `…`.
#[test]
fn streaming_reasoning_summary_updates_per_delta() {
    let mut t = TranscriptComponent::new();
    t.begin_streaming();
    t.push_reasoning("first thought");
    let buf = render_once(&t, 30, 3);
    let screen = rows(&buf).join("\n");
    assert!(screen.contains("• first thought"), "{screen}");
    assert!(!screen.contains("second"));
    // A second line joins with a single space — English reads.
    t.push_reasoning("\nsecond thought");
    let buf = render_once(&t, 30, 3);
    let screen = rows(&buf).join("\n");
    assert!(
        screen.contains("• first thought second thought"),
        "{screen}"
    );
    // Overflow (avail 28): the tail cut keeps 27 cols behind a
    // leading ellipsis — the earliest text drops out.
    t.push_reasoning(&format!("\n{}", "z".repeat(28)));
    let buf = render_once(&t, 30, 3);
    let screen = rows(&buf).join("\n");
    assert!(
        screen.contains(&format!("• …{}", "z".repeat(27))),
        "{screen}"
    );
    assert!(!screen.contains("first thought"), "{screen}");
}

/// fix-25: a completed tool row click-expands into the dim call/output
/// detail block (full args + the output text folded in at merge), and
/// clicking the expanded block collapses it again. restyle-1: the
/// detail rows carry the 4-space rail prefix (`│   ` when connected).
#[test]
fn tool_row_expands_on_click_with_args_and_output() {
    let mut t = TranscriptComponent::new();
    t.tool_start("echo", r#"{"text":"hi"}"#);
    t.tool_end("echo", 2, false);
    // Turn ends: the Tool message folds the full output text in (and
    // resets any expansion — the merge rule).
    t.merge_turn(&[
        assistant_call("tc-1", "echo"),
        tool_result("tc-1", "echo", "merged output"),
    ]);

    let mut terminal = Terminal::new(TestBackend::new(30, 8)).unwrap();
    let mut ctx = test_ctx();
    terminal.draw(|f| t.render(f, f.area(), &ctx)).unwrap();
    let screen = rows(terminal.backend().buffer());
    let head_row = screen
        .iter()
        .position(|r| r.contains("Used echo"))
        .expect("tool head row");
    assert!(
        !screen.iter().any(|r| r.contains("调用")),
        "no detail while collapsed: {screen:?}"
    );

    // Click the head row → the detail block renders: 调用 + full
    // args + 输出 + the folded output text.
    assert_eq!(t.handle(&Action::Click(3, head_row as u16), &mut ctx), None);
    terminal.draw(|f| t.render(f, f.area(), &ctx)).unwrap();
    let screen = rows(terminal.backend().buffer()).join("\n");
    assert!(screen.contains("    调用 echo"), "{screen}");
    assert!(screen.contains(r#"    {"text":"hi"}"#), "{screen}");
    assert!(screen.contains("    输出"), "{screen}");
    assert!(screen.contains("    merged output"), "{screen}");

    // Click again → collapsed back to the single head row.
    assert_eq!(t.handle(&Action::Click(3, head_row as u16), &mut ctx), None);
    terminal.draw(|f| t.render(f, f.area(), &ctx)).unwrap();
    let screen = rows(terminal.backend().buffer()).join("\n");
    assert!(!screen.contains("调用"), "re-collapsed: {screen}");
}
