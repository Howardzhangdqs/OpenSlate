//! Prompt input — a full multiline editor (P2b-complete), borderless
//! since the Wave 1 redesign (opencode-style `┃` bar language).
//!
//! Semantics (design brief + borderless spec):
//! * char-boundary cursor (CJK-safe insert/delete/move);
//! * `Enter` submits, `Alt+Enter`/`Ctrl+J` inserts a newline;
//! * `↑`/`↓` walk input history ONLY on the first/last row — otherwise
//!   they move the cursor within the text;
//! * `Ctrl+W` deletes the word before the cursor;
//! * bracketed paste inserts verbatim (newlines included);
//! * rendering (borderless, SINGLE-LINE since the layout rework): NO
//!   frame and NO `›`/`>` prompt prefix — a `┃` bar column (Cyan while
//!   focused — the `user_label` color; DarkGray otherwise) + 1 blank
//!   column + the text; the empty state shows a muted placeholder.
//!   Key hints live on the STATUS BAR now — the old dedicated bottom
//!   hint row is gone;
//! * the block is ONE row tall: the cursor's current line renders with
//!   horizontal scroll (the cursor rides the last visible column once
//!   the line overflows);
//! * the cursor is drawn via `Frame::set_cursor_position` (the App
//!   calls [`InputComponent::cursor_position`] after rendering; the x
//!   offset = bar column + 1 blank column, minus the scroll offset).

use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::{AppCtx, Component, Focus};
use crate::action::Action;

/// Placeholder shown in the text area's empty state (muted).
const PLACEHOLDER: &str = "输入 prompt，Enter 发送";

/// The multiline prompt editor.
#[derive(Debug, Default)]
pub struct InputComponent {
    /// Editor content as char vectors (char-boundary safety by
    /// construction). Never empty — at least one (possibly empty) line.
    lines: Vec<Vec<char>>,
    /// Cursor row index into `lines`.
    row: usize,
    /// Cursor column as a CHAR index into `lines[row]`.
    col: usize,
    /// Submitted history (oldest first).
    history: Vec<String>,
    /// History cursor; `history.len()` means "live draft" (not navigating).
    history_pos: usize,
    /// Draft saved when navigating into history, restored when navigating
    /// past the newest entry.
    draft: String,
}

impl InputComponent {
    pub fn new() -> Self {
        Self {
            lines: vec![Vec::new()],
            row: 0,
            col: 0,
            history: Vec::new(),
            history_pos: 0,
            draft: String::new(),
        }
    }

    /// Current text with lines joined by `\n`.
    pub fn text(&self) -> String {
        self.lines
            .iter()
            .map(|l| l.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Replace the whole buffer (used when restoring a failed turn's
    /// prompt for editing). Cursor moves to the end and history navigation
    /// state resets to "draft".
    pub fn set_text(&mut self, text: &str) {
        self.load(text);
        self.history_pos = self.history.len();
        self.draft.clear();
    }

    /// Replace the buffer contents WITHOUT touching history navigation
    /// state (used by history walking itself).
    fn load(&mut self, text: &str) {
        self.lines = text
            .split('\n')
            .map(|l| l.chars().collect())
            .collect::<Vec<_>>();
        if self.lines.is_empty() {
            self.lines.push(Vec::new());
        }
        self.row = self.lines.len() - 1;
        self.col = self.lines[self.row].len();
    }

    /// True when the editor holds nothing but an empty line.
    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    /// Take the current text as the submission: resets the editor, records
    /// history. Returns the joined text.
    pub fn take(&mut self) -> String {
        let text = self.text();
        if !text.trim().is_empty() {
            self.history.push(text.clone());
        }
        self.lines = vec![Vec::new()];
        self.row = 0;
        self.col = 0;
        self.history_pos = self.history.len();
        self.draft.clear();
        text
    }

    /// Desired block height: ONE row — the input is single-line (layout
    /// rework). The buffer may still hold newlines (paste, Alt+Enter);
    /// render shows the cursor's current line with horizontal scroll.
    pub fn desired_height(&self) -> u16 {
        1
    }

    /// Horizontal scroll offset (display columns) keeping the cursor
    /// inside a `text_width`-wide single-line view: the cursor rides the
    /// last visible column once the line overflows.
    fn col_offset(&self, text_width: u16) -> u16 {
        let prefix: String = self.lines[self.row][..self.col].iter().collect();
        let col_w = Span::from(prefix).width() as u16;
        col_w.saturating_sub(text_width.saturating_sub(1))
    }

    /// Screen position of the text cursor when the block is drawn at
    /// `area`, or `None` when the area is too small to show content.
    /// x = the `┃` bar column + 1 blank column, minus the horizontal
    /// scroll offset; y = the single input row.
    pub fn cursor_position(&self, area: Rect) -> Option<Position> {
        if area.width < 3 || area.height == 0 {
            return None;
        }
        let text_width = area.width.saturating_sub(2);
        let off = self.col_offset(text_width);
        let prefix: String = self.lines[self.row][..self.col].iter().collect();
        // Display width via ratatui's unicode-width-backed Span::width.
        let col_width = Span::from(prefix).width() as u16;
        Some(Position::new(
            area.x + 2 + col_width.saturating_sub(off),
            area.y,
        ))
    }

    // ── editing primitives (all char-safe) ──────────────────────────────

    fn insert_char(&mut self, c: char) {
        self.lines[self.row].insert(self.col, c);
        self.col += 1;
    }

    fn insert_newline(&mut self) {
        let rest = self.lines[self.row].split_off(self.col);
        self.row += 1;
        self.col = 0;
        self.lines.insert(self.row, rest);
    }

    fn insert_raw(&mut self, text: &str) {
        for c in text.chars() {
            if c == '\n' {
                self.insert_newline();
            } else if c == '\r' {
                // Normalize CRLF pastes.
            } else {
                self.insert_char(c);
            }
        }
    }

    fn backspace(&mut self) {
        if self.col > 0 {
            self.lines[self.row].remove(self.col - 1);
            self.col -= 1;
        } else if self.row > 0 {
            let line = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.lines[self.row].len();
            self.lines[self.row].extend(line);
        }
    }

    /// Ctrl+W: delete back to the start of the previous word (emacs
    /// semantics: trailing spaces first, then the word; stops at the space
    /// before it). At BOL joins with the previous line.
    fn delete_word(&mut self) {
        let mut seen_word_char = false;
        loop {
            if self.col == 0 {
                if !seen_word_char && self.row > 0 {
                    self.backspace(); // join with the previous line
                }
                break;
            }
            let prev = self.lines[self.row][self.col - 1];
            if prev == ' ' && seen_word_char {
                break; // reached the start of the word
            }
            if prev != ' ' {
                seen_word_char = true;
            }
            self.col -= 1;
            self.lines[self.row].remove(self.col);
        }
    }

    fn cursor_left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.lines[self.row].len();
        }
    }

    fn cursor_right(&mut self) {
        if self.col < self.lines[self.row].len() {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    fn cursor_up(&mut self) {
        if self.row > 0 {
            self.row -= 1;
            self.col = self.col.min(self.lines[self.row].len());
        }
    }

    fn cursor_down(&mut self) {
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = self.col.min(self.lines[self.row].len());
        }
    }

    fn history_prev(&mut self) {
        if self.row == 0 && self.history_pos > 0 {
            if self.history_pos == self.history.len() {
                self.draft = self.text();
            }
            self.history_pos -= 1;
            let text = self.history[self.history_pos].clone();
            self.load(&text);
        } else {
            self.cursor_up();
        }
    }

    fn history_next(&mut self) {
        if self.row + 1 == self.lines.len() && self.history_pos < self.history.len() {
            self.history_pos += 1;
            let text = if self.history_pos == self.history.len() {
                self.draft.clone()
            } else {
                self.history[self.history_pos].clone()
            };
            self.load(&text);
        } else {
            self.cursor_down();
        }
    }
}

impl Component for InputComponent {
    fn handle(&mut self, action: &Action, ctx: &mut AppCtx) -> Option<Action> {
        if ctx.focus != Focus::Input {
            return None;
        }
        match action {
            Action::InputChar(c) => self.insert_char(*c),
            Action::InputNewline => self.insert_newline(),
            Action::InputBackspace => self.backspace(),
            Action::InputDeleteWord => self.delete_word(),
            Action::InputCursorLeft => self.cursor_left(),
            Action::InputCursorRight => self.cursor_right(),
            Action::InputCursorUp => self.cursor_up(),
            Action::InputCursorDown => self.cursor_down(),
            Action::InputHome => self.col = 0,
            Action::InputEnd => self.col = self.lines[self.row].len(),
            Action::InputHistoryPrev => self.history_prev(),
            Action::InputHistoryNext => self.history_next(),
            Action::PasteText(text) => self.insert_raw(text),
            Action::SubmitInput => {
                let text = self.take();
                if text.trim().is_empty() {
                    return None;
                }
                return Some(Action::StartTurn(text));
            }
            _ => return None,
        }
        None // handled, no follow-up action
    }

    fn render(&self, f: &mut Frame, area: Rect, ctx: &AppCtx) {
        if area.width < 3 || area.height == 0 {
            return; // need bar + blank + ≥1 text column, ≥1 row
        }
        let focused = ctx.focus == Focus::Input;
        let bar_style = if focused {
            ctx.theme.user_label // Cyan + BOLD (the focus color)
        } else {
            ctx.theme.bar_divider // DarkGray
        };

        // ┃ bar column on the single input row.
        let bar_area = Rect {
            x: area.x,
            y: area.y,
            width: 1,
            height: 1,
        };
        f.render_widget(Paragraph::new(Line::from("┃")).style(bar_style), bar_area);

        // Text: the cursor's current line only, horizontally scrolled so
        // the cursor stays visible (single-line input). Key hints live
        // on the status bar; the old dedicated hint row is gone.
        let text_width = area.width.saturating_sub(2);
        let text: String = if self.is_empty() {
            PLACEHOLDER.to_owned()
        } else {
            let off = self.col_offset(text_width);
            slice_by_width(&self.lines[self.row], off, text_width)
        };
        let text_style = if self.is_empty() {
            ctx.theme.muted
        } else {
            ctx.theme.assistant
        };
        let text_area = Rect {
            x: area.x + 2,
            y: area.y,
            width: text_width,
            height: 1,
        };
        f.render_widget(
            Paragraph::new(Line::from(text)).style(text_style),
            text_area,
        );
    }
}

/// Window `start_w..start_w+max_w` of `line` by DISPLAY columns (CJK
/// wide chars are never split mid-glyph; a wide char straddling the
/// window start is skipped whole).
fn slice_by_width(line: &[char], start_w: u16, max_w: u16) -> String {
    let mut w = 0u16;
    let mut out = String::new();
    for &c in line {
        let cw = Span::from(c.to_string()).width() as u16;
        if w >= start_w {
            if w + cw > start_w.saturating_add(max_w) {
                break;
            }
            out.push(c);
        }
        w = w.saturating_add(cw);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::{ConfigSummary, RunInfo, RunState};
    use crate::theme::Theme;

    fn ctx() -> AppCtx {
        AppCtx {
            theme: Theme::new(),
            focus: Focus::Input,
            run: RunInfo {
                state: RunState::Idle,
                spinner_frame: 0,
                model_label: "main@mock".into(),
                tokens_in: 0,
                tokens_out: 0,
                cost_usd: 0.0,
                elapsed: None,
                tool_calls_cur: 0,
                depth_cur: 0,
            },
            config: ConfigSummary {
                model_alias: "main".into(),
                model_id: "m".into(),
                provider_name: "mock".into(),
                max_depth: 4,
                max_tool_calls: 20,
                run_id: None,
            },
            size: (100, 30),
            notice: None,
        }
    }

    fn input() -> InputComponent {
        InputComponent::new()
    }

    fn type_str(inp: &mut InputComponent, s: &str) {
        for c in s.chars() {
            inp.handle(&Action::InputChar(c), &mut ctx());
        }
    }

    #[test]
    fn typing_and_submit_roundtrip() {
        let mut inp = input();
        type_str(&mut inp, "hello 世界");
        assert_eq!(inp.text(), "hello 世界");
        let action = inp.handle(&Action::SubmitInput, &mut ctx());
        assert_eq!(action, Some(Action::StartTurn("hello 世界".into())));
        assert!(inp.is_empty());
    }

    #[test]
    fn empty_submit_returns_none() {
        let mut inp = input();
        assert_eq!(inp.handle(&Action::SubmitInput, &mut ctx()), None);
        // whitespace-only also counts as empty
        type_str(&mut inp, "   ");
        assert_eq!(inp.handle(&Action::SubmitInput, &mut ctx()), None);
    }

    #[test]
    fn newline_split_and_join() {
        let mut inp = input();
        type_str(&mut inp, "ab");
        inp.handle(&Action::InputNewline, &mut ctx());
        type_str(&mut inp, "cd");
        assert_eq!(inp.text(), "ab\ncd");
        // Backspace at BOL (Home first) joins the lines.
        inp.handle(&Action::InputHome, &mut ctx());
        inp.handle(&Action::InputBackspace, &mut ctx());
        assert_eq!(inp.text(), "abcd");
    }

    #[test]
    fn backspace_at_bol_first_line_is_noop() {
        let mut inp = input();
        type_str(&mut inp, "x");
        inp.handle(&Action::InputHome, &mut ctx());
        inp.handle(&Action::InputBackspace, &mut ctx());
        assert_eq!(inp.text(), "x");
    }

    #[test]
    fn cjk_backspace_removes_whole_char() {
        let mut inp = input();
        type_str(&mut inp, "你好");
        inp.handle(&Action::InputBackspace, &mut ctx());
        assert_eq!(inp.text(), "你");
    }

    #[test]
    fn delete_word_removes_trailing_word() {
        let mut inp = input();
        type_str(&mut inp, "one two three");
        inp.handle(&Action::InputDeleteWord, &mut ctx());
        assert_eq!(inp.text(), "one two ");
        inp.handle(&Action::InputDeleteWord, &mut ctx());
        assert_eq!(inp.text(), "one ");
    }

    #[test]
    fn delete_word_joins_previous_line_at_bol() {
        let mut inp = input();
        type_str(&mut inp, "word");
        inp.handle(&Action::InputNewline, &mut ctx());
        inp.handle(&Action::InputDeleteWord, &mut ctx());
        assert_eq!(inp.text(), "word");
    }

    #[test]
    fn cursor_moves_across_line_boundaries() {
        let mut inp = input();
        type_str(&mut inp, "ab");
        inp.handle(&Action::InputNewline, &mut ctx());
        type_str(&mut inp, "cd");
        // cursor at end of "cd"; move left 3 times → lands inside "ab"
        inp.handle(&Action::InputCursorLeft, &mut ctx());
        inp.handle(&Action::InputCursorLeft, &mut ctx());
        inp.handle(&Action::InputCursorLeft, &mut ctx());
        // now insert 'X' → "abX" + "cd"? cursor after 'b'
        inp.handle(&Action::InputChar('X'), &mut ctx());
        assert_eq!(inp.text(), "abX\ncd");
        // right three times → end of "cd" (row0-end → row1 col0 → col1 → col2)
        inp.handle(&Action::InputCursorRight, &mut ctx());
        inp.handle(&Action::InputCursorRight, &mut ctx());
        inp.handle(&Action::InputCursorRight, &mut ctx());
        inp.handle(&Action::InputChar('!'), &mut ctx());
        assert_eq!(inp.text(), "abX\ncd!");
    }

    #[test]
    fn home_end_on_current_line() {
        let mut inp = input();
        type_str(&mut inp, "abc");
        inp.handle(&Action::InputHome, &mut ctx());
        inp.handle(&Action::InputChar('X'), &mut ctx());
        assert_eq!(inp.text(), "Xabc");
        inp.handle(&Action::InputEnd, &mut ctx());
        inp.handle(&Action::InputChar('Y'), &mut ctx());
        assert_eq!(inp.text(), "XabcY");
    }

    #[test]
    fn history_navigation_only_at_boundaries() {
        let mut inp = input();
        type_str(&mut inp, "first");
        let _ = inp.handle(&Action::SubmitInput, &mut ctx());
        type_str(&mut inp, "second");
        let _ = inp.handle(&Action::SubmitInput, &mut ctx());
        assert!(inp.is_empty());

        // On the (single, first and last) line: ↑ walks history.
        inp.handle(&Action::InputHistoryPrev, &mut ctx());
        assert_eq!(inp.text(), "second");
        inp.handle(&Action::InputHistoryPrev, &mut ctx());
        assert_eq!(inp.text(), "first");
        // ↓ walks forward, past newest → empty draft.
        inp.handle(&Action::InputHistoryNext, &mut ctx());
        assert_eq!(inp.text(), "second");
        inp.handle(&Action::InputHistoryNext, &mut ctx());
        assert!(inp.is_empty());
    }

    #[test]
    fn history_saves_draft_and_restores() {
        let mut inp = input();
        type_str(&mut inp, "sent");
        let _ = inp.handle(&Action::SubmitInput, &mut ctx());
        type_str(&mut inp, "half-typ");
        inp.handle(&Action::InputHistoryPrev, &mut ctx());
        assert_eq!(inp.text(), "sent");
        inp.handle(&Action::InputHistoryNext, &mut ctx());
        assert_eq!(inp.text(), "half-typ");
    }

    #[test]
    fn multiline_arrows_move_cursor_not_history() {
        let mut inp = input();
        type_str(&mut inp, "l1");
        inp.handle(&Action::InputNewline, &mut ctx());
        type_str(&mut inp, "l2");
        // cursor on last row → ↑ moves within text (to row 0), history
        // must NOT replace the buffer.
        inp.handle(&Action::InputHistoryPrev, &mut ctx());
        assert_eq!(inp.text(), "l1\nl2");
        inp.handle(&Action::InputHistoryNext, &mut ctx());
        assert_eq!(inp.text(), "l1\nl2");
    }

    #[test]
    fn paste_inserts_multiline_verbatim() {
        let mut inp = input();
        inp.handle(&Action::PasteText("a\nb\r\nc".into()), &mut ctx());
        assert_eq!(inp.text(), "a\nb\nc");
    }

    #[test]
    fn set_text_places_cursor_at_end() {
        let mut inp = input();
        inp.set_text("one\ntwo");
        inp.handle(&Action::InputChar('!'), &mut ctx());
        assert_eq!(inp.text(), "one\ntwo!");
    }

    #[test]
    fn desired_height_is_always_one() {
        // Single-line input (layout rework): the block is one row tall
        // regardless of buffer content — even after newlines, render
        // shows the cursor's line with horizontal scroll instead.
        let mut inp = input();
        assert_eq!(inp.desired_height(), 1);
        assert!(inp.is_empty());
        for _ in 0..20 {
            inp.handle(&Action::InputNewline, &mut ctx());
        }
        assert_eq!(inp.desired_height(), 1);
    }

    #[test]
    fn ignores_actions_when_not_focused() {
        let mut inp = input();
        let mut c = ctx();
        c.focus = Focus::Transcript;
        assert_eq!(inp.handle(&Action::InputChar('x'), &mut c), None);
        assert!(inp.is_empty());
    }

    // ── borderless rendering (Wave 1) ──────────────────────────────────

    use ratatui::backend::TestBackend;
    use ratatui::style::Color;
    use ratatui::Terminal;

    /// Render the component at `w x h` and return the buffer clone.
    fn render_buf(inp: &InputComponent, c: &AppCtx, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("test terminal");
        terminal.draw(|f| inp.render(f, f.area(), c)).expect("draw");
        terminal.backend().buffer().clone()
    }

    /// No frame vocabulary (`┌─┐│└┘…`) anywhere in the buffer.
    fn assert_no_frame_chars(buf: &ratatui::buffer::Buffer) {
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                let s = buf[(x, y)].symbol();
                assert!(
                    !matches!(s, "┌" | "┐" | "└" | "┘" | "─" | "│" | "├" | "┤" | "┬" | "┴"),
                    "frame char {s:?} at ({x},{y})"
                );
            }
        }
    }

    #[test]
    fn borderless_empty_state_placeholder_single_row() {
        let inp = input();
        // Height 2 proves the single-row semantics: the placeholder
        // paints ONLY on the input row and the row below stays blank
        // (the old hint row is gone — hints live on the status bar).
        let buf = render_buf(&inp, &ctx(), 40, 2);

        // ┃ bar at x=0 (Cyan + BOLD while focused), 1 blank column, then
        // the muted placeholder — NO frame, NO `›` prefix.
        assert_eq!(buf[(0, 0)].symbol(), "┃");
        assert_eq!(buf[(0, 0)].style().fg, Some(Color::Cyan));
        assert!(buf[(0, 0)]
            .style()
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD));
        assert_eq!(buf[(1, 0)].symbol(), " ");
        assert_eq!(buf[(2, 0)].symbol(), "输");
        assert_eq!(buf[(2, 0)].style().fg, Some(Color::DarkGray));
        assert_ne!(buf[(2, 0)].symbol(), "›");
        // The row below the input row is completely blank.
        for x in 0..40u16 {
            assert_eq!(buf[(x, 1)].symbol(), " ", "row below input at {x}");
        }
        assert_no_frame_chars(&buf);
    }

    #[test]
    fn borderless_unfocused_bar_is_dim() {
        let inp = input();
        let mut c = ctx();
        c.focus = Focus::Transcript;
        let buf = render_buf(&inp, &c, 40, 3);
        assert_eq!(buf[(0, 0)].symbol(), "┃");
        assert_eq!(buf[(0, 0)].style().fg, Some(Color::DarkGray));
        assert!(buf[(0, 0)].style().add_modifier.is_empty());
    }

    #[test]
    fn borderless_shows_only_the_cursor_line() {
        // The buffer may hold newlines (paste / Alt+Enter); the
        // single-line view renders the CURSOR's current row only.
        let mut inp = input();
        type_str(&mut inp, "ab");
        inp.handle(&Action::InputNewline, &mut ctx());
        type_str(&mut inp, "cd");
        let buf = render_buf(&inp, &ctx(), 40, 2);

        // The single input row carries the bar + the cursor's line
        // ("cd") at x=2 (default foreground — `Color::Reset`, i.e. the
        // terminal default, never painted).
        assert_eq!(buf[(0, 0)].symbol(), "┃");
        assert_eq!(buf[(2, 0)].symbol(), "c");
        assert_eq!(buf[(3, 0)].symbol(), "d");
        assert!(matches!(buf[(2, 0)].style().fg, None | Some(Color::Reset)));
        // The first line is NOT rendered, and no prompt prefix char
        // appears before the text.
        let row: String = (0..10).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(
            !row.contains('a'),
            "row 0 must not leak other lines: {row:?}"
        );
        assert_ne!(buf[(2, 0)].symbol(), "›");
        assert_ne!(buf[(2, 0)].symbol(), ">");
        // Row below the input row: blank (no hint row anymore).
        for x in 0..40u16 {
            assert_eq!(buf[(x, 1)].symbol(), " ");
        }
        assert_no_frame_chars(&buf);

        // Moving the cursor up to row 0 flips the view to "ab".
        inp.handle(&Action::InputCursorUp, &mut ctx());
        let buf = render_buf(&inp, &ctx(), 40, 2);
        assert_eq!(buf[(2, 0)].symbol(), "a");
        assert_eq!(buf[(3, 0)].symbol(), "b");
    }

    #[test]
    fn long_line_horizontally_scrolls_cursor_to_last_column() {
        // 40-col area → 38 text columns. Type 40 chars: the cursor
        // (display column 40) overflows, so the view scrolls to keep
        // the cursor on the LAST visible column.
        let mut inp = input();
        type_str(&mut inp, "abcdef0123456789abcdef0123456789abcdef01"); // 40 chars
        let buf = render_buf(&inp, &ctx(), 40, 1);
        let off = 40 - (38 - 1); // col_offset = 3
                                 // First visible char: index `off` (the 4th), cursor cell (col
                                 // 39) is past the last char of the 37-visible-char window.
        assert_eq!(buf[(2, 0)].symbol(), "d", "window starts at char {off}");
        assert_eq!(buf[(38, 0)].symbol(), "1", "window ends at char index 39");
        assert_eq!(buf[(39, 0)].symbol(), " ", "cursor rides the last column");
        // The scrolled-out prefix "abc" is not visible anywhere.
        let row: String = (0..40).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(!row.starts_with("┃ abc"), "prefix scrolled out: {row:?}");
    }

    /// `slice_by_width` — ASCII windowing basics.
    #[test]
    fn slice_by_width_ascii_windows() {
        let chars = |s: &str| s.chars().collect::<Vec<char>>();
        assert_eq!(slice_by_width(&chars("abcdef"), 0, 3), "abc");
        assert_eq!(slice_by_width(&chars("abcdef"), 2, 2), "cd");
        assert_eq!(slice_by_width(&chars("abcdef"), 4, 10), "ef");
    }

    /// A wide glyph straddling the window END is never cut in half —
    /// it is dropped whole and the window ends narrow.
    #[test]
    fn slice_by_width_never_splits_wide_glyph_at_end() {
        let line: Vec<char> = "ab你c".chars().collect();
        // Window 0..3: 'a' (0..1) and 'b' (1..2) fit; '你' would span
        // 2..4 → exceeds → break (its first column is NOT rendered).
        assert_eq!(slice_by_width(&line, 0, 3), "ab");
        // Window wide enough for the full glyph keeps it intact.
        assert_eq!(slice_by_width(&line, 0, 4), "ab你");
        assert_eq!(slice_by_width(&line, 0, 5), "ab你c");
    }

    /// A wide glyph straddling the window START is skipped whole — the
    /// window opens at the next narrow char instead of slicing the
    /// glyph's second cell.
    #[test]
    fn slice_by_width_skips_wide_glyph_straddling_start() {
        let line: Vec<char> = "a你c".chars().collect();
        // '你' occupies display columns 1..3; a window starting at 2
        // lands mid-glyph → skip the whole glyph, start at 'c'.
        assert_eq!(slice_by_width(&line, 2, 5), "c");
        // Window starting exactly at the glyph's first column keeps it.
        assert_eq!(slice_by_width(&line, 1, 5), "你c");
    }

    #[test]
    fn cursor_position_offsets_by_bar_plus_blank_column() {
        let mut inp = input();
        type_str(&mut inp, "ab");
        let area = Rect::new(10, 5, 40, 1);
        // Cursor after "ab": x = area.x + bar(1) + blank(1) + width(2).
        assert_eq!(inp.cursor_position(area), Some(Position::new(14, 5)));
        // The buffer may hold more lines, but the view is single-row:
        // the cursor stays on y = area.y (the cursor's line renders
        // there) and x tracks that line's prefix width.
        inp.handle(&Action::InputNewline, &mut ctx());
        type_str(&mut inp, "c");
        assert_eq!(inp.cursor_position(area), Some(Position::new(13, 5)));
        // CJK display width counts double.
        let mut inp = input();
        type_str(&mut inp, "你");
        assert_eq!(inp.cursor_position(area), Some(Position::new(14, 5)));
        // Too-small areas return None.
        assert_eq!(inp.cursor_position(Rect::new(0, 0, 2, 1)), None);
        assert_eq!(inp.cursor_position(Rect::new(0, 0, 40, 0)), None);
    }

    #[test]
    fn cursor_position_rides_last_visible_column_when_scrolled() {
        // 40-col area → 38 text columns. With 40 chars typed the
        // cursor's prefix width (40) overflows: offset = 40-(38-1)=3,
        // x = area.x + 2 + (40-3) = area.x + 39 — the LAST column.
        let mut inp = input();
        type_str(&mut inp, "abcdef0123456789abcdef0123456789abcdef01");
        let area = Rect::new(10, 5, 40, 1);
        assert_eq!(inp.cursor_position(area), Some(Position::new(49, 5)));
        // Moving left within the overflow region keeps the cursor
        // pinned on the last column (the window scrolls back in
        // lockstep: col_width-1 and off-1 cancel out).
        inp.handle(&Action::InputCursorLeft, &mut ctx());
        assert_eq!(inp.cursor_position(area), Some(Position::new(49, 5)));
        inp.handle(&Action::InputCursorLeft, &mut ctx());
        assert_eq!(inp.cursor_position(area), Some(Position::new(49, 5)));
        // Home releases the pin: off collapses to 0 and the cursor
        // lands right after the bar + blank columns.
        inp.handle(&Action::InputHome, &mut ctx());
        assert_eq!(inp.cursor_position(area), Some(Position::new(12, 5)));
    }
}
