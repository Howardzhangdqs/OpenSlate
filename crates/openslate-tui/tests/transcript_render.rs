//! Transcript render tests — TestBackend + Buffer assertions (the
//! ratatui `test_case` convention). The borderless redesign (Wave 2
//! lane-a) renders straight into the given area: no frame, no title —
//! user blocks carry the Cyan `┃` bar, assistant text indents 2, tool
//! rows use the opencode icon table, and spacing follows the block/
//! single-line rules. These tests assert the rendered TEXT layer
//! row-by-row and spot-check the semantic colors on specific glyphs;
//! the pin state machine, wrap math, and rebuild heuristics live as
//! inline unit tests in `src/components/transcript.rs`.

use openslate_core::types::{Message, MessageRole, ToolCall, ToolCallId};
use openslate_tui::action::Action;
use openslate_tui::components::transcript::TranscriptComponent;
use openslate_tui::components::{AppCtx, Component, ConfigSummary, Focus, RunInfo, RunState};
use openslate_tui::theme::Theme;
use ratatui::backend::TestBackend;
use ratatui::buffer::{Buffer, CellWidth};
use ratatui::style::Color;
use ratatui::Terminal;
use serde_json::json;

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
        },
        config: ConfigSummary {
            model_alias: String::new(),
            model_id: String::new(),
            provider_name: String::new(),
            max_depth: 4,
            max_tool_calls: 20,
            run_id: None,
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
fn renders_user_bar_and_assistant_body() {
    let mut t = TranscriptComponent::new();
    t.rebuild(&[user_msg("hi"), assistant_msg("hello")]);
    let buf = render_once(&t, 20, 6);
    // Block spacing: one blank line between user and assistant blocks.
    assert_rows(
        &buf,
        &[
            row(20, "┃ hi"),
            blank(20),
            row(20, "  hello"),
            blank(20),
            blank(20),
            blank(20),
        ],
    );
    // The ┃ bar is Cyan (user identity); the body keeps the default fg.
    let (bx, by) = find_cell(&buf, "┃");
    assert_eq!(buf.cell((bx, by)).unwrap().style().fg, Some(Color::Cyan));
    // 'l' occurs only in the assistant body ("hello"): default
    // foreground (Reset in buffer cells).
    let (ax, ay) = find_cell(&buf, "l");
    assert_eq!(buf.cell((ax, ay)).unwrap().style().fg, Some(Color::Reset));
}

#[test]
fn wraps_long_text_at_word_boundaries() {
    let mut t = TranscriptComponent::new();
    t.push_user("one two three four five");
    let buf = render_once(&t, 22, 6);
    // Width 22: `┃ ` gutter (2) + body avail 20 → wrap at the word
    // boundary; the bar covers EVERY display line (hanging indent 2).
    assert_rows(
        &buf,
        &[
            row(22, "┃ one two three four"),
            row(22, "┃ five"),
            blank(22),
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
    // Width 8: user `┃ go`; assistant indent 2, avail 6 → 世世世 / 世世.
    let buf = render_once(&t, 8, 5);
    assert_rows(
        &buf,
        &[
            row(8, "┃ go"),
            blank(8),
            "  世世世".to_owned(), // 2 + 6 = 8 cols, no padding
            "  世世  ".to_owned(), // 2 + 4 = 6 cols, +2 padding
            blank(8),
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
    let buf = render_once(&t, 24, 5);
    // Icon table: echo  (other), grep  (glob/grep). Tool rows
    // are single-liners: NO blank between them (compact run) and none
    // between the user block and the first row.
    assert_rows(
        &buf,
        &[
            row(24, "┃ go"),
            row(24, " echo({})  2B"),
            row(24, " grep({})  Error: x"),
            blank(24),
            blank(24),
        ],
    );
    // Done = the whole line DarkGray muted…
    let (dx, dy) = find_cell(&buf, "");
    assert_eq!(
        buf.cell((dx, dy)).unwrap().style().fg,
        Some(Color::DarkGray)
    );
    let (hx, hy) = find_cell(&buf, "h"); // 'h' only in "echo" (row 1)
    assert_eq!(
        buf.cell((hx, hy)).unwrap().style().fg,
        Some(Color::DarkGray)
    );
    // …with the  keeping its Green accent,  Red on the failure.
    let (sx, sy) = find_cell(&buf, "");
    assert_eq!(buf.cell((sx, sy)).unwrap().style().fg, Some(Color::Green));
    let (fx, fy) = find_cell(&buf, "");
    assert_eq!(buf.cell((fx, fy)).unwrap().style().fg, Some(Color::Red));
}

#[test]
fn renders_running_tool_line_yellow_with_spinner() {
    let mut t = TranscriptComponent::new();
    t.push_user("go");
    t.tool_start("read_file", r#"{"path":"a.rs"}"#);
    let buf = render_once(&t, 40, 4);
    // Running: icon + name(args) all Yellow, spinner frame 0 = ⠋, live
    // elapsed ticking (do not assert the exact ms — timing-dependent).
    let lines = rows(&buf);
    assert_eq!(lines[0], row(40, "┃ go"));
    assert!(
        lines[1].starts_with(r#" read_file({"path":"a.rs"}) ⠋ "#),
        "{}",
        lines[1]
    );
    assert_eq!(lines[2], blank(40));
    assert_eq!(lines[3], blank(40));
    let (ix, iy) = find_cell(&buf, "");
    assert_eq!(buf.cell((ix, iy)).unwrap().style().fg, Some(Color::Yellow));
    let (rx, ry) = find_cell(&buf, "r"); // 'r' only in read_file (row 1)
    assert_eq!(buf.cell((rx, ry)).unwrap().style().fg, Some(Color::Yellow));
    let (sx, sy) = find_cell(&buf, "⠋");
    assert_eq!(buf.cell((sx, sy)).unwrap().style().fg, Some(Color::Yellow));
}

#[test]
fn renders_delegate_marker_and_approval_outcome() {
    let mut t = TranscriptComponent::new();
    t.tool_start("call_agent", r#"{"agent_id":"researcher"}"#);
    t.push_approval("shell", "denied");
    let buf = render_once(&t, 26, 5);
    // PUA icons are width-1: the row is 16 display cols of content
    // → 10 spaces of padding.
    let approval_row = format!(" shell — denied{}", " ".repeat(10));
    assert_rows(
        &buf,
        &[
            row(26, " researcher"),
            approval_row,
            blank(26),
            blank(26),
            blank(26),
        ],
    );
    let (dx, dy) = find_cell(&buf, "");
    assert_eq!(buf.cell((dx, dy)).unwrap().style().fg, Some(Color::Magenta));
    let (ax, ay) = find_cell(&buf, "");
    assert_eq!(buf.cell((ax, ay)).unwrap().style().fg, Some(Color::Yellow));
    // denied → the decision text is red ('d' only in "denied").
    let (ex, ey) = find_cell(&buf, "d");
    assert_eq!(buf.cell((ex, ey)).unwrap().style().fg, Some(Color::Red));
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
    let buf = render_once(&t, 22, 5);
    // The borderless spec retired the full-width `─` rule: a step
    // separator renders as one blank line after the tool row.
    assert_rows(
        &buf,
        &[
            row(22, "┃ go"),
            row(22, " echo({})  2B"),
            blank(22),
            blank(22),
            blank(22),
        ],
    );
}

#[test]
fn renders_end_of_turn_marker() {
    let mut t = TranscriptComponent::new();
    t.rebuild(&[user_msg("go"), assistant_msg("ok")]);
    t.set_turn_meta("main".to_owned(), 9, None);
    let buf = render_once(&t, 24, 6);
    // Block spacing: one blank line before the marker; cube icon Cyan, the
    // rest muted, `·` (U+00B7) separator. Hand-padded by display width
    // (`秒` is a wide char — `row()`'s char-count padding would lie).
    let marker_row = format!(" main · 9秒{}", " ".repeat(24 - 12));
    assert_rows(
        &buf,
        &[
            row(24, "┃ go"),
            blank(24),
            row(24, "  ok"),
            blank(24),
            marker_row,
            blank(24),
        ],
    );
    let (mx, my) = find_cell(&buf, "");
    assert_eq!(buf.cell((mx, my)).unwrap().style().fg, Some(Color::Cyan));
    // 'a' occurs only in "main" (the muted span).
    let (ax, ay) = find_cell(&buf, "a");
    assert_eq!(
        buf.cell((ax, ay)).unwrap().style().fg,
        Some(Color::DarkGray)
    );
    // Without turn_meta there is no marker anywhere.
    let mut t2 = TranscriptComponent::new();
    t2.rebuild(&[user_msg("go"), assistant_msg("ok")]);
    let buf2 = render_once(&t2, 24, 5);
    assert!(rows(&buf2).iter().all(|r| !r.contains("")));
}

/// fix-18: the streaming answer renders markdown live through the
/// same pipeline as committed blocks — no `▍` full-height gutter; the
/// live signal is a `▍` tail cursor on the answer's last row
/// (running-yellow), while reasoning keeps its dim `┆` gutter.
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
            row(24, "┆ thinking…"),
            blank(24),
            row(24, "  partial answer▍"),
            blank(24),
            blank(24),
        ],
    );
    // Tail cursor keeps the running color (Yellow); reasoning dim.
    let (cx, cy) = find_cell(&buf, "▍");
    assert_eq!(buf.cell((cx, cy)).unwrap().style().fg, Some(Color::Yellow));
    let (rx, ry) = find_cell(&buf, "┆");
    assert_eq!(
        buf.cell((rx, ry)).unwrap().style().fg,
        Some(Color::DarkGray)
    );
}

/// Markdown styles land LIVE in the frame buffer: a heading delta
/// paints Cyan+BOLD into the cells the reader is watching, before any
/// flush boundary (fix-18).
#[test]
fn streaming_heading_paints_accent_cells_before_flush() {
    let mut t = TranscriptComponent::new();
    t.begin_streaming();
    t.push_delta("# Result heading");
    let buf = render_once(&t, 30, 4);
    assert_rows(
        &buf,
        &[
            row(30, "  Result heading▍"),
            blank(30),
            blank(30),
            blank(30),
        ],
    );
    // 'R' occurs only in the heading text: accent + BOLD, mid-stream.
    let (hx, hy) = find_cell(&buf, "R");
    let cell = buf.cell((hx, hy)).unwrap();
    assert_eq!(cell.style().fg, Some(Color::Cyan));
    assert!(cell
        .style()
        .add_modifier
        .contains(ratatui::style::Modifier::BOLD));
    // The tail cursor cell is the running yellow.
    let (cx, cy) = find_cell(&buf, "▍");
    assert_eq!(buf.cell((cx, cy)).unwrap().style().fg, Some(Color::Yellow));
}

#[test]
fn shows_empty_state_hint() {
    let t = TranscriptComponent::new();
    let buf = render_once(&t, 44, 5);
    let text = "空会话 — 输入 prompt 开始,Enter 发送"; // display width 36
    let expected_row = format!("{text}{}", " ".repeat(44 - 36));
    assert_rows(
        &buf,
        &[expected_row, blank(44), blank(44), blank(44), blank(44)],
    );
    let (mx, my) = find_cell(&buf, "空");
    assert_eq!(
        buf.cell((mx, my)).unwrap().style().fg,
        Some(Color::DarkGray)
    );
}

#[test]
fn pinned_view_shows_new_content_hint() {
    let mut t = TranscriptComponent::new();
    for i in 0..10 {
        t.push_user(&format!("line{i}"));
    }
    // First render establishes the viewport (26x6 → 10 lines + 9 block
    // gaps = 19 total, 6 visible → max scroll 13).
    let mut terminal = Terminal::new(TestBackend::new(40, 6)).unwrap();
    let mut ctx = test_ctx();
    terminal.draw(|f| t.render(f, f.area(), &ctx)).unwrap();
    // ScrollUp unpins one line above the bottom (scroll 12 of max 13).
    t.handle(&Action::ScrollUp, &mut ctx);
    assert!(t.is_pinned());
    terminal.draw(|f| t.render(f, f.area(), &ctx)).unwrap();

    // Lines 12..18 visible: line6/line7/line8 blocks with their gaps.
    // The hint Paragraph writes only its own cells, right-aligned in
    // its (x+1, width-2) sub-rect: the hint (11 display cols) starts
    // at col 28 and col 39 stays blank. below = 1.
    assert_rows(
        terminal.backend().buffer(),
        &[
            row(40, "┃ line6"),
            blank(40),
            row(40, "┃ line7"),
            blank(40),
            row(40, "┃ line8"),
            format!("{}↓ 新内容 +1 ", " ".repeat(28)),
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
        Some(Color::Cyan)
    );
}

#[test]
fn following_view_sticks_to_bottom_without_hint() {
    let mut t = TranscriptComponent::new();
    for i in 0..10 {
        t.push_user(&format!("line{i}"));
    }
    let buf = render_once(&t, 40, 6);
    // Following: last 6 of the 19 lines visible (13..19), no hint.
    assert_rows(
        &buf,
        &[
            blank(40),
            row(40, "┃ line7"),
            blank(40),
            row(40, "┃ line8"),
            blank(40),
            row(40, "┃ line9"),
        ],
    );
    assert!(rows(&buf).iter().all(|r| !r.contains("新内容")));
}

/// Assistant text renders through the markdown subset in BOTH states:
/// a heading row carries the accent+BOLD style, inline code the
/// distinct code color, fenced code stays verbatim — and the SAME
/// text while STILL streaming renders the SAME rows (fix-18), the
/// only live-vs-committed difference being the tail cursor.
#[test]
fn renders_markdown_in_finalized_assistant_blocks() {
    let mut t = TranscriptComponent::new();
    t.rebuild(&[
        user_msg("go"),
        assistant_msg("# Result\n\nanswer with `code`"),
    ]);
    let buf = render_once(&t, 30, 6);
    assert_rows(
        &buf,
        &[
            row(30, "┃ go"),
            blank(30),
            row(30, "  Result"),
            blank(30),
            row(30, "  answer with code"),
            blank(30),
        ],
    );
    // Heading row: accent (Cyan) + BOLD on the text cells.
    let (hx, hy) = find_cell(&buf, "R"); // 'R' only in "Result"
    let heading_cell = buf.cell((hx, hy)).unwrap();
    assert_eq!(heading_cell.style().fg, Some(Color::Cyan));
    assert!(heading_cell
        .style()
        .add_modifier
        .contains(ratatui::style::Modifier::BOLD));
    // Inline code: distinct color (Yellow).
    let (cx, cy) = find_cell(&buf, "c"); // 'c' only in "code"
    assert_eq!(buf.cell((cx, cy)).unwrap().style().fg, Some(Color::Yellow));

    // The same text while STILL streaming: identical rows (markdown
    // applied live) plus the tail cursor on the last row.
    let mut live = TranscriptComponent::new();
    live.push_user("go");
    live.begin_streaming();
    live.push_delta("# Result\n\nanswer with `code`");
    let buf = render_once(&live, 30, 6);
    assert_rows(
        &buf,
        &[
            row(30, "┃ go"),
            blank(30),
            row(30, "  Result"),
            blank(30),
            row(30, "  answer with code▍"),
            blank(30),
        ],
    );
    // The live heading carries the same accent + BOLD as the
    // committed one — no raw `#`/backtick markers anywhere.
    let (hx, hy) = find_cell(&buf, "R");
    let live_heading = buf.cell((hx, hy)).unwrap();
    assert_eq!(live_heading.style().fg, Some(Color::Cyan));
    assert!(live_heading
        .style()
        .add_modifier
        .contains(ratatui::style::Modifier::BOLD));
    let (cx, cy) = find_cell(&buf, "c");
    assert_eq!(buf.cell((cx, cy)).unwrap().style().fg, Some(Color::Yellow));
}

/// Pin geometry under live markdown re-rendering (fix-18): the
/// `↓ 新内容 +N` count tracks the RENDERED line count, so when a
/// closing marker collapses the literal wrap (an unclosed `` ` `` pair
/// keeps the backtick in the literal, wrapping one char onto a second
/// row; closed, the marker pair vanishes and the row count shrinks),
/// N shrinks with it — the pin offset itself stays put (clamped, no
/// drift), and follow mode is unaffected (still at the bottom).
#[test]
fn new_content_hint_tracks_rendered_streaming_rows() {
    let mut t = TranscriptComponent::new();
    for i in 0..10 {
        t.push_user(&format!("line{i}")); // 19 layout rows at width 40
    }
    let mut terminal = Terminal::new(TestBackend::new(40, 6)).unwrap();
    let mut ctx = test_ctx();
    terminal.draw(|f| t.render(f, f.area(), &ctx)).unwrap();
    t.handle(&Action::ScrollUp, &mut ctx); // pinned at 12 of max 13 → below 1
    terminal.draw(|f| t.render(f, f.area(), &ctx)).unwrap();
    assert!(rows(terminal.backend().buffer())[5].contains("↓ 新内容 +1"));

    // Unclosed inline code, one char over the available width (38):
    // the literal (backtick included) wraps onto a second row →
    // gap + 2 rows below the pin.
    t.begin_streaming();
    t.push_delta(&format!("`{}", "b".repeat(38)));
    terminal.draw(|f| t.render(f, f.area(), &ctx)).unwrap();
    let raw = rows(terminal.backend().buffer());
    assert!(
        raw[5].contains("↓ 新内容 +4"),
        "literal wrap counted: {}",
        raw[5]
    );

    // The closing backtick arrives: the marker pair vanishes, the
    // code span fits the row → the hint count shrinks by one.
    t.push_delta("`");
    terminal.draw(|f| t.render(f, f.area(), &ctx)).unwrap();
    let styled = rows(terminal.backend().buffer());
    assert!(
        styled[5].contains("↓ 新内容 +3"),
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
    assert_rows(&buf, &["  abcd".to_owned(), blank(6), blank(6)]);
    let (fx, fy) = find_cell(&buf, "a");
    assert_eq!(buf.cell((fx, fy)).unwrap().style().fg, Some(Color::Yellow));
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
    let buf = render_once(&t, 30, 7);
    let screen = rows(&buf).join("\n");
    assert!(screen.contains("~4tok"), "estimate line present"); // 9 chars / 2.2 ≈ 4
    assert!(
        screen.contains("↑50 ↓10 · ttft 0.8s · 5tok/s"),
        "exact line present:\n{screen}"
    );
    // Meta cells are DarkGray (muted) — check the usage line's arrow.
    let all_rows = rows(&buf);
    let y = all_rows
        .iter()
        .position(|r| r.contains("↑50"))
        .expect("usage row") as u16;
    let row = &all_rows[y as usize];
    let x = row.find('↑').expect("arrow position") as u16;
    assert_eq!(buf.cell((x, y)).unwrap().style().fg, Some(Color::DarkGray));
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
    let buf = render_once(&t, 46, 5);
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
            row(24, "┆ thinking"),
            blank(24),
            row(24, "  partial answer▍"),
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
    let header = format!("  类别  要点{}", " ".repeat(24 - 12));
    let body = format!("  代码  说明内容{}", " ".repeat(24 - 16));
    assert_rows(&buf, &[header, body, blank(24), blank(24)]);
    // Header row BOLD; body row default.
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
    assert_rows(&buf, &[row(20, "  | a | b |▍"), blank(20), blank(20)]);

    // Separator (and a data row) arrive → the table layout, live; the
    // tail cursor rides the LAST row (the data row).
    let mut t2 = TranscriptComponent::new();
    t2.begin_streaming();
    t2.push_delta("| a | b |\n|---|---|\n| 1 | 2 |");
    let buf = render_once(&t2, 20, 3);
    assert_rows(&buf, &[row(20, "  a  b"), row(20, "  1  2▍"), blank(20)]);
    let (hx, hy) = find_cell(&buf, "a"); // header cell
    assert!(buf
        .cell((hx, hy))
        .unwrap()
        .style()
        .add_modifier
        .contains(ratatui::style::Modifier::BOLD));
}
