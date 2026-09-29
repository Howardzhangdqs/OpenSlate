//! Prompt input — a full multiline editor (P2b-complete), restyle-1
//! minimax composer language.
//!
//! Semantics (design brief + restyle-1 spec):
//! * char-boundary cursor (CJK-safe insert/delete/move);
//! * `Enter` submits, `Alt+Enter`/`Ctrl+J` inserts a newline;
//! * `↑`/`↓` walk input history ONLY on the first/last row — otherwise
//!   they move the cursor within the text;
//! * `Ctrl+W` deletes the word before the cursor;
//! * bracketed paste inserts verbatim (newlines included);
//! * rendering (restyle-1): a `› ` prompt column — BOLD signal while
//!   focused, muted otherwise — then the text (explicit text color);
//!   the empty state shows a muted placeholder. Key hints live on the
//!   hint row above the input (minimax composer header);
//! * the block is ONE row tall: the cursor's current line renders with
//!   horizontal scroll (the cursor rides the last visible column once
//!   the line overflows);
//! * the cursor is drawn via `Frame::set_cursor_position` (the App
//!   calls [`InputComponent::cursor_position`] after rendering; the x
//!   offset = prompt column + 1 blank column, minus the scroll offset).

use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::{AppCtx, Component, Focus};
use crate::action::Action;
use crate::complete;
use crate::icons::localize;
use crate::slash;

/// Placeholder shown in the text area's empty state (muted).
const PLACEHOLDER: &str = "输入 prompt，Enter 发送";

/// Display columns before the input text: the `›` prompt glyph plus
/// one blank column. The completion list indents by the same amount
/// so its left edge aligns with the input TEXT column (slash-1).
const PROMPT_COLS: u16 = 2;

/// Commands whose argument opens a second-level completion list
/// (slash-1): `/copy [all|tool]` (static) and `/model <alias>`
/// (dynamic, from the config).
const ARGS_COMMANDS: [&str; 2] = ["copy", "model"];

/// Which list the open completion shows (slash-1).
#[derive(Debug, Clone, PartialEq, Eq)]
enum CompletionMode {
    /// `/na…` — filtering the command registry.
    Command,
    /// `/cmd ar…` — filtering the argument choices of `cmd`.
    Args { cmd: String },
}

/// One completion row (already resolved to display strings).
#[derive(Debug, Clone, PartialEq, Eq)]
struct CompletionRow {
    /// Main column: `/name` in command mode, the bare argument
    /// (`all`, `fast`) in args mode.
    label: String,
    /// Argument hint for the description column (command mode only).
    args_hint: String,
    /// Description column (empty for argument rows).
    description: String,
}

/// The open completion state (slash-1): the filtered rows plus the
/// selected index. The scroll window is DERIVED at render time (a
/// stored start would go stale on resize).
#[derive(Debug, Clone, PartialEq, Eq)]
struct CompletionState {
    mode: CompletionMode,
    rows: Vec<CompletionRow>,
    selected: usize,
}

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
    /// The open slash-completion list, if any (slash-1). Recomputed on
    /// every text change; closed by submit/paste/newline/modals.
    completion: Option<CompletionState>,
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
            completion: None,
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
    /// state resets to "draft". Closes any open completion list
    /// (slash-1: a programmatic replace is not completion driving).
    pub fn set_text(&mut self, text: &str) {
        self.load(text);
        self.history_pos = self.history.len();
        self.draft.clear();
        self.completion = None;
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
    /// history. Returns the joined text. Closing the completion list is
    /// part of the reset (slash-1: submit always closes).
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
        self.completion = None;
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
    /// x = the `›` prompt column + 1 blank column, minus the horizontal
    /// scroll offset; y = the single input row.
    pub fn cursor_position(&self, area: Rect) -> Option<Position> {
        if area.width < 3 || area.height == 0 {
            return None;
        }
        let text_width = area.width.saturating_sub(PROMPT_COLS);
        let off = self.col_offset(text_width);
        let prefix: String = self.lines[self.row][..self.col].iter().collect();
        // Display width via ratatui's unicode-width-backed Span::width.
        let col_width = Span::from(prefix).width() as u16;
        Some(Position::new(
            area.x + PROMPT_COLS + col_width.saturating_sub(off),
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

    // ── slash-1: the completion state machine ──────────────────────────

    /// Whether the slash-completion list is open.
    pub fn completion_open(&self) -> bool {
        self.completion.is_some()
    }

    /// Close the completion list without touching the text (Esc; the
    /// App's overlay-opening paths call this so no modal ever opens
    /// on top of a live list).
    pub fn close_completion(&mut self) {
        self.completion = None;
    }

    /// Recompute the completion list from the current text (`None` =
    /// closed). Command mode: `/query` (single `/`, no whitespace).
    /// Argument mode: `/cmd arg-query` for cmd ∈ {copy, model}. `//`
    /// (literal escape), a second space, unknown commands with args,
    /// multiline buffers and empty matches all close.
    fn compute_completion(&self, ctx: &AppCtx) -> Option<CompletionState> {
        if self.lines.len() != 1 {
            return None; // multiline buffer — not a command draft
        }
        let text = self.text();
        // Leading whitespace is tolerated ("starts with `/` after
        // trim" — trim_start); TRAILING whitespace is significant
        // (the space after `/cmd` switches to argument mode), so a
        // full trim() must not eat it.
        let trimmed = text.trim_start();
        let rest = trimmed.strip_prefix('/')?;
        match rest.find(char::is_whitespace) {
            None => {
                // Command mode. A `/` inside the query means the input
                // is a `//`-escape (or deeper) — not a command draft.
                if rest.contains('/') {
                    return None;
                }
                let registry = slash::registry();
                let names: Vec<&str> = registry.iter().map(|s| s.name).collect();
                let hits = complete::filter(rest, &names);
                if hits.is_empty() {
                    return None;
                }
                let rows = hits
                    .iter()
                    .map(|&i| {
                        let spec = &registry[i];
                        CompletionRow {
                            label: format!("/{}", spec.name),
                            args_hint: spec.args_hint.to_owned(),
                            description: spec.description.to_owned(),
                        }
                    })
                    .collect();
                let selected = complete::initial_selection(rest, &names, &hits);
                Some(CompletionState {
                    mode: CompletionMode::Command,
                    rows,
                    selected,
                })
            }
            Some(space) => {
                // Argument mode: `/cmd arg…` with exactly one space.
                let (cmd, query) = (&rest[..space], &rest[space + 1..]);
                // A second whitespace or a `/` in the query closes.
                if query.contains('/') || query.contains(char::is_whitespace) {
                    return None;
                }
                if !ARGS_COMMANDS.contains(&cmd) {
                    return None;
                }
                let choices = slash::arg_choices(cmd, &ctx.config);
                let refs: Vec<&str> = choices.iter().map(String::as_str).collect();
                let hits = complete::filter(query, &refs);
                if hits.is_empty() {
                    return None;
                }
                let rows = hits
                    .iter()
                    .map(|&i| CompletionRow {
                        label: choices[i].clone(),
                        args_hint: String::new(),
                        description: String::new(),
                    })
                    .collect();
                let selected = complete::initial_selection(query, &refs, &hits);
                Some(CompletionState {
                    mode: CompletionMode::Args {
                        cmd: cmd.to_owned(),
                    },
                    rows,
                    selected,
                })
            }
        }
    }

    /// Recompute the open/closed state after a text change.
    fn refresh_completion(&mut self, ctx: &AppCtx) {
        self.completion = self.compute_completion(ctx);
    }

    /// Move the selection by `delta` rows, WRAPPING at both ends
    /// (slash-1: ↑/↓ navigate the list, never the history).
    fn completion_move(&mut self, delta: i32) {
        if let Some(c) = self.completion.as_mut() {
            let len = c.rows.len();
            if len > 0 {
                c.selected = (c.selected as i32 + delta).rem_euclid(len as i32) as usize;
            }
        }
    }

    /// Apply the selected item to the buffer (Tab / Enter):
    /// a command completes to `/name ` (trailing space, cursor at the
    /// end — copy/model immediately reopen their argument list);
    /// an argument completes to `/cmd arg` (no trailing space).
    fn completion_apply(&mut self, ctx: &AppCtx) {
        let Some(c) = self.completion.as_ref() else {
            return;
        };
        let applied = match &c.mode {
            CompletionMode::Command => format!("{} ", c.rows[c.selected].label),
            CompletionMode::Args { cmd } => format!("/{cmd} {}", c.rows[c.selected].label),
        };
        self.load(&applied);
        self.refresh_completion(ctx);
    }

    /// Enter on an open list (slash-1), three branches:
    /// (a) a command WITHOUT argument completion — apply, then submit
    ///     through the standard take() → StartTurn path (handle_slash
    ///     parses the completed text);
    /// (b) copy/model — same as Tab (apply + reopen the argument
    ///     list, no submit);
    /// (c) an argument — apply; if the application changed nothing,
    ///     submit the buffer verbatim (the identity guard prevents
    ///     Enter from endlessly intercepting its own completion).
    fn completion_submit(&mut self, ctx: &AppCtx) -> Option<Action> {
        let c = self.completion.as_ref()?;
        match &c.mode {
            CompletionMode::Command => {
                let name = c.rows[c.selected].label.trim_start_matches('/');
                if ARGS_COMMANDS.contains(&name) {
                    self.completion_apply(ctx); // (b) — no submit
                    None
                } else {
                    self.completion_apply(ctx); // (a) → "/name "
                    let text = self.take();
                    if text.trim().is_empty() {
                        None
                    } else {
                        Some(Action::StartTurn(text))
                    }
                }
            }
            CompletionMode::Args { .. } => {
                let before = self.text();
                self.completion_apply(ctx);
                if self.text() == before {
                    let text = self.take();
                    if text.trim().is_empty() {
                        None
                    } else {
                        Some(Action::StartTurn(text))
                    }
                } else {
                    None // (c) — applied, waiting for the confirming Enter
                }
            }
        }
    }
}

impl Component for InputComponent {
    fn handle(&mut self, action: &Action, ctx: &mut AppCtx) -> Option<Action> {
        if ctx.focus != Focus::Input {
            return None;
        }
        // slash-1: while the completion list is open it PREEMPTS the
        // navigation/apply/dismiss keys (consumed — never fall
        // through to history walking or focus cycling). Editing keys
        // fall through to the normal arms below and recompute.
        if self.completion.is_some() {
            match action {
                Action::InputHistoryPrev => {
                    self.completion_move(-1);
                    return None;
                }
                Action::InputHistoryNext => {
                    self.completion_move(1);
                    return None;
                }
                // Tab (the App routes FocusNext here while the list is
                // open): apply, do not submit.
                Action::FocusNext => {
                    self.completion_apply(ctx);
                    return None;
                }
                // Esc: close the list, keep the text.
                Action::DismissOverlay => {
                    self.completion = None;
                    return None;
                }
                // Paste always closes the list (slash-1).
                Action::PasteText(text) => {
                    self.insert_raw(text);
                    self.completion = None;
                    return None;
                }
                Action::SubmitInput => return self.completion_submit(ctx),
                _ => {}
            }
        }
        match action {
            Action::InputChar(c) => {
                self.insert_char(*c);
                self.refresh_completion(ctx);
            }
            Action::InputNewline => {
                self.insert_newline();
                self.completion = None;
            }
            Action::InputBackspace => {
                self.backspace();
                self.refresh_completion(ctx);
            }
            Action::InputDeleteWord => {
                self.delete_word();
                self.refresh_completion(ctx);
            }
            Action::InputCursorLeft => self.cursor_left(),
            Action::InputCursorRight => self.cursor_right(),
            Action::InputCursorUp => self.cursor_up(),
            Action::InputCursorDown => self.cursor_down(),
            Action::InputHome => self.col = 0,
            Action::InputEnd => self.col = self.lines[self.row].len(),
            Action::InputHistoryPrev => {
                self.history_prev();
                self.refresh_completion(ctx);
            }
            Action::InputHistoryNext => {
                self.history_next();
                self.refresh_completion(ctx);
            }
            Action::PasteText(text) => {
                self.insert_raw(text);
                self.completion = None;
            }
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
            return; // need prompt + blank + ≥1 text column, ≥1 row
        }
        let focused = ctx.focus == Focus::Input;
        // restyle-1: `›` prompt — BOLD signal while focused, muted
        // otherwise (the old `┃` gutter is retired).
        let prompt_style = if focused {
            ctx.theme.user_label // signal + BOLD (the focus color)
        } else {
            ctx.theme.muted
        };

        // `›` prompt glyph on the single input row (column 0) — from
        // the theme's icon tier (`>` on ascii).
        let prompt_area = Rect {
            x: area.x,
            y: area.y,
            width: 1,
            height: 1,
        };
        f.render_widget(
            Paragraph::new(Line::from(ctx.theme.icons.set().prompt)).style(prompt_style),
            prompt_area,
        );

        // Text: the cursor's current line only, horizontally scrolled so
        // the cursor stays visible (single-line input). Key hints live
        // on the hint row above the input.
        let text_width = area.width.saturating_sub(PROMPT_COLS);
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
            x: area.x + PROMPT_COLS,
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

// ── slash-1..3: completion layout & rendering ────────────────────────

/// The completion overlay's max visible rows (slash-1 spec; the
/// painter's capacity cap — an `(i/n)` indicator claims the last row
/// when the items overflow it).
const COMPLETION_MAX_ROWS: u16 = 8;

impl InputComponent {
    /// The completion overlay's desired row count (slash-3): the item
    /// count capped at [`COMPLETION_MAX_ROWS`] (the painter turns the
    /// last row into the dim `(i/n)` indicator whenever the items
    /// overflow the given area). The App further clamps this against
    /// the height available above the hint row — the overlay only
    /// ever COVERS transcript rows, never reflows the layout.
    pub fn completion_rows(&self) -> u16 {
        self.completion
            .as_ref()
            .map_or(0, |c| (c.rows.len() as u16).min(COMPLETION_MAX_ROWS))
    }

    /// The completion list's view geometry for a given area capacity
    /// (the SINGLE SOURCE both `render_completion` and the
    /// interactive-1 hit-test accessors use): which rows are shown,
    /// from what start offset, and whether the `(i/n)` indicator
    /// claims the last row. `None` when the list is closed or the
    /// area cannot show a single item.
    fn completion_view(&self, capacity: usize) -> Option<(usize, usize, bool)> {
        let c = self.completion.as_ref()?;
        let total = c.rows.len();
        // Overflow: the indicator row claims one row of the budget.
        let (shown, overflow) = if total > capacity {
            (capacity.saturating_sub(1), true)
        } else {
            (total, false)
        };
        if shown == 0 {
            return None;
        }
        // Scroll window: selection at the center, clamped in-bounds.
        let start = c.selected.saturating_sub(shown / 2).min(total - shown);
        Some((shown, start, overflow))
    }

    /// interactive-1: map a terminal row inside the last-rendered
    /// overlay `rect` to the row INDEX it displays. Only ITEM rows
    /// map; the overflow `(i/n)` indicator row and rows outside the
    /// rect are dead space (`None` — the overlay swallows the gesture
    /// without a target). Shares [`Self::completion_view`] with the
    /// painter so the two can never disagree.
    pub fn completion_row_at(&self, rect: Rect, row: u16) -> Option<usize> {
        if row < rect.y || row >= rect.y.saturating_add(rect.height) {
            return None;
        }
        let (shown, start, _) = self.completion_view(rect.height as usize)?;
        let d = (row - rect.y) as usize;
        (d < shown).then_some(start + d)
    }

    /// interactive-1: whether row `index` still exists in the open
    /// list (the App's hover-validity check — a re-filter can shrink
    /// the row set between frames).
    pub fn completion_row_exists(&self, index: usize) -> bool {
        self.completion
            .as_ref()
            .is_some_and(|c| index < c.rows.len())
    }

    /// interactive-1: a click on visible row `index` — move the
    /// KEYBOARD selection there, then fire the exact Enter semantics
    /// ([`Self::completion_submit`]'s three branches — reused, not
    /// duplicated). Hover never calls this: hovering must not move
    /// the selection or re-center the scroll window.
    pub fn completion_click(&mut self, ctx: &AppCtx, index: usize) -> Option<Action> {
        if let Some(c) = self.completion.as_mut() {
            if index < c.rows.len() {
                c.selected = index;
            }
        }
        self.completion_submit(ctx)
    }

    /// Render the completion list into `area` (the OVERLAY rectangle
    /// the App reserved since slash-3: growing up from the hint row
    /// over the transcript's bottom rows, after `Clear` + the surface
    /// background; full terminal width). In-list document flow (mcode
    /// select-list isomorphic): no frame, no animation. Rows: `→ `
    /// marker (tier arrow) on the selected row — the whole row
    /// signal + BOLD — and a 2-space pad plus text/muted columns
    /// otherwise. The list's left edge aligns with the input TEXT
    /// column (PROMPT_COLS). When the rows overflow the area, the
    /// last row becomes a dim `(i/n)` indicator and a scroll window
    /// keeps the selection in view (centered-ish, clamped
    /// in-bounds). interactive-1: `hover` is the MOUSE-hovered row
    /// index — a light style (label span in `theme.hover` only, NO
    /// marker, NO row-bold: those belong to the keyboard selection)
    /// that LOSES to the selected row's style when the two coincide.
    pub fn render_completion(&self, f: &mut Frame, area: Rect, ctx: &AppCtx, hover: Option<usize>) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let Some(c) = &self.completion else {
            return;
        };
        let theme = &ctx.theme;
        let set = theme.icons.set();

        let total = c.rows.len();
        let Some((shown, start, overflow)) = self.completion_view(area.height as usize) else {
            return;
        };

        // Column geometry: main column = clamp(widest label + 2, 12, 32)
        // (the +2 is the gap to the description column).
        let widest = c
            .rows
            .iter()
            .map(|r| disp_width(&r.label))
            .max()
            .unwrap_or(0);
        let main_w = widest.saturating_add(2).clamp(12, 32);
        let show_desc = area.width as usize > 40;
        let desc_avail = (area.width as usize)
            .saturating_sub(PROMPT_COLS as usize)
            .saturating_sub(2)
            .saturating_sub(main_w);

        // Selected row: the WHOLE row signal + BOLD (user_label is
        // exactly that composed slot).
        let sel_style = theme.user_label;
        let marker = format!("{} ", set.right);
        let indent = " ".repeat(PROMPT_COLS as usize);

        let mut lines: Vec<Line> = Vec::with_capacity(shown + usize::from(overflow));
        for d in 0..shown {
            let idx = start + d;
            let row = &c.rows[idx];
            // Label: truncate into the main column (tier ellipsis),
            // then right-pad to the column width.
            let label = truncate_cols(&row.label, main_w, set.ellipsis);
            let pad = main_w.saturating_sub(disp_width(&label));
            let label_col = format!("{label}{}", " ".repeat(pad));
            // Description column: `args_hint — description` (hint
            // alone when there is no description); chrome-localized
            // per tier (`—` downgrades on ascii) and truncated with
            // the tier ellipsis.
            let desc_col = if show_desc {
                let text = if row.args_hint.is_empty() {
                    row.description.clone()
                } else if row.description.is_empty() {
                    row.args_hint.clone()
                } else {
                    format!("{} — {}", row.args_hint, row.description)
                };
                truncate_cols(&localize(&text, &set), desc_avail, set.ellipsis)
            } else {
                String::new()
            };
            if idx == c.selected {
                let mut spans = vec![
                    Span::raw(indent.clone()),
                    Span::styled(marker.clone(), sel_style),
                    Span::styled(label_col, sel_style),
                ];
                if !desc_col.is_empty() {
                    spans.push(Span::styled(desc_col, sel_style));
                }
                lines.push(Line::from(spans));
            } else {
                // interactive-1: the hovered row's LABEL span alone
                // takes the hover style (no marker, no row-bold —
                // those are the keyboard selection's); the selected
                // row above already wins the tie by construction.
                let label_style = if hover == Some(idx) {
                    theme.hover
                } else {
                    theme.assistant
                };
                let mut spans = vec![
                    Span::raw(indent.clone()),
                    Span::raw("  "),
                    Span::styled(label_col, label_style),
                ];
                if !desc_col.is_empty() {
                    spans.push(Span::styled(desc_col, theme.muted));
                }
                lines.push(Line::from(spans));
            }
        }
        if overflow {
            // Pure-ASCII indicator (slash-1 spec; never localized).
            lines.push(Line::from(vec![
                Span::raw(indent.clone()),
                Span::styled(format!("({}/{})", c.selected + 1, total), theme.fine),
            ]));
        }
        f.render_widget(Paragraph::new(lines), area);
    }
}

/// Display width of `s` (ratatui's unicode-width-backed measure).
fn disp_width(s: &str) -> usize {
    Span::raw(s).width()
}

/// Truncate `s` to at most `max` display columns, appending the
/// tier's `ellipsis` when cut (CJK wide glyphs never split
/// mid-glyph). Inputs already within budget pass through unchanged.
fn truncate_cols(s: &str, max: usize, ellipsis: &str) -> String {
    if max == 0 || disp_width(s) <= max {
        return s.to_owned();
    }
    let budget = max.saturating_sub(disp_width(ellipsis));
    let mut out = String::new();
    let mut w = 0;
    for ch in s.chars() {
        let cw = disp_width(&ch.to_string());
        if w + cw > budget {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push_str(ellipsis);
    out
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
                context_remaining: None,
            },
            config: ConfigSummary {
                model_alias: "main".into(),
                model_id: "m".into(),
                provider_name: "mock".into(),
                max_depth: 4,
                max_tool_calls: 20,
                run_id: None,
                model_aliases: vec!["fast".into(), "main".into()],
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
        // (hints live on the hint row above the input).
        let buf = render_buf(&inp, &ctx(), 40, 2);

        // `›` prompt at x=0 (signal #67E8F9 + BOLD while focused), 1
        // blank column, then the muted placeholder.
        let signal = Color::Rgb(0x67, 0xE8, 0xF9);
        let muted = Color::Rgb(0xAD, 0xAD, 0xAD);
        assert_eq!(buf[(0, 0)].symbol(), "›");
        assert_eq!(buf[(0, 0)].style().fg, Some(signal));
        assert!(buf[(0, 0)]
            .style()
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD));
        assert_eq!(buf[(1, 0)].symbol(), " ");
        assert_eq!(buf[(2, 0)].symbol(), "输");
        assert_eq!(buf[(2, 0)].style().fg, Some(muted));
        // The row below the input row is completely blank.
        for x in 0..40u16 {
            assert_eq!(buf[(x, 1)].symbol(), " ", "row below input at {x}");
        }
        assert_no_frame_chars(&buf);
    }

    #[test]
    fn borderless_unfocused_prompt_is_muted() {
        let inp = input();
        let mut c = ctx();
        c.focus = Focus::Transcript;
        let buf = render_buf(&inp, &c, 40, 3);
        assert_eq!(buf[(0, 0)].symbol(), "›");
        assert_eq!(buf[(0, 0)].style().fg, Some(Color::Rgb(0xAD, 0xAD, 0xAD)));
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

        // The single input row carries the prompt + the cursor's line
        // ("cd") at x=2 (explicit text color #D6D6D6).
        assert_eq!(buf[(0, 0)].symbol(), "›");
        assert_eq!(buf[(2, 0)].symbol(), "c");
        assert_eq!(buf[(3, 0)].symbol(), "d");
        assert_eq!(buf[(2, 0)].style().fg, Some(Color::Rgb(0xD6, 0xD6, 0xD6)));
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
        assert!(!row.starts_with("› abc"), "prefix scrolled out: {row:?}");
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

    // ── slash-1: completion state machine ─────────────────────────────

    /// Labels of the open list (test observer via the private field).
    fn labels(inp: &InputComponent) -> Vec<String> {
        inp.completion
            .as_ref()
            .map(|c| c.rows.iter().map(|r| r.label.clone()).collect())
            .unwrap_or_default()
    }

    fn mode_of(inp: &InputComponent) -> Option<CompletionMode> {
        inp.completion.as_ref().map(|c| c.mode.clone())
    }

    #[test]
    fn slash_opens_the_full_list_and_filters() {
        let mut inp = input();
        type_str(&mut inp, "/");
        assert!(inp.completion_open());
        assert_eq!(
            labels(&inp),
            vec![
                "/help",
                "/exit",
                "/new",
                "/status",
                "/agents",
                "/model",
                "/copy",
                "/mouse",
                "/provider",
            ]
        );
        // Registry order, selection at the head (no exact/prefix for "").
        assert_eq!(inp.completion.as_ref().unwrap().selected, 0);

        type_str(&mut inp, "mo");
        // Prefix class first, registry tiebreak: model before mouse
        // (the model-management command is `/provider` since the
        // model-mgmt-2 rename — it left the `/mo` class entirely).
        assert_eq!(labels(&inp), vec!["/model", "/mouse"]);
        assert_eq!(inp.completion.as_ref().unwrap().selected, 0);

        // Subsequence "del": only /model hits now (`provider` has no
        // `l`); the exact-ish shorter /model is selected.
        type_str(&mut inp, "del");
        assert_eq!(labels(&inp), vec!["/model"]);
        assert_eq!(inp.completion.as_ref().unwrap().selected, 0);

        // Subsequence-only + case-insensitivity: "He" prefix-matches help.
        let mut inp = input();
        type_str(&mut inp, "/He");
        assert_eq!(labels(&inp), vec!["/help"]);

        // No match → closed.
        let mut inp = input();
        type_str(&mut inp, "/zz");
        assert!(!inp.completion_open());
    }

    #[test]
    fn double_slash_escape_never_opens() {
        let mut inp = input();
        type_str(&mut inp, "//");
        assert!(!inp.completion_open(), "`//` is the literal escape");
        // A `/` inside the query also closes.
        let mut inp = input();
        type_str(&mut inp, "/mo/");
        assert!(!inp.completion_open());
    }

    #[test]
    fn space_closes_every_command_but_copy_and_model() {
        let mut inp = input();
        type_str(&mut inp, "/help ");
        assert!(!inp.completion_open(), "help takes no completion args");
        let mut inp = input();
        type_str(&mut inp, "/new ");
        assert!(!inp.completion_open());
        // A word after a non-args command stays closed.
        let mut inp = input();
        type_str(&mut inp, "/mouse on");
        assert!(!inp.completion_open());
    }

    #[test]
    fn args_mode_opens_for_copy_and_model() {
        let mut inp = input();
        type_str(&mut inp, "/copy ");
        assert!(inp.completion_open());
        assert_eq!(
            mode_of(&inp),
            Some(CompletionMode::Args { cmd: "copy".into() })
        );
        assert_eq!(labels(&inp), vec!["all", "tool"]);

        // model: choices come from the ctx config (dynamic).
        let mut inp = input();
        type_str(&mut inp, "/model ");
        assert_eq!(
            mode_of(&inp),
            Some(CompletionMode::Args {
                cmd: "model".into()
            })
        );
        assert_eq!(labels(&inp), vec!["fast", "main"]);

        // Fuzzy filtering inside the args list.
        type_str(&mut inp, "fa");
        assert_eq!(labels(&inp), vec!["fast"]);
        // No matching alias → closed.
        let mut inp = input();
        type_str(&mut inp, "/model zz");
        assert!(!inp.completion_open());
    }

    #[test]
    fn second_space_or_slash_in_args_closes() {
        let mut inp = input();
        type_str(&mut inp, "/copy all ");
        assert!(!inp.completion_open(), "second space closes");
        let mut inp = input();
        type_str(&mut inp, "/copy a/b");
        assert!(!inp.completion_open(), "`/` in the arg query closes");
    }

    #[test]
    fn paste_closes_the_list() {
        let mut inp = input();
        type_str(&mut inp, "/mo");
        assert!(inp.completion_open());
        inp.handle(&Action::PasteText("x".into()), &mut ctx());
        assert!(!inp.completion_open());
        assert_eq!(inp.text(), "/mox");
    }

    #[test]
    fn newline_closes_and_multiline_never_opens() {
        let mut inp = input();
        type_str(&mut inp, "/mo");
        inp.handle(&Action::InputNewline, &mut ctx());
        assert!(!inp.completion_open());
        assert_eq!(inp.text(), "/mo\n");
    }

    #[test]
    fn backspace_and_delete_word_recompute() {
        // Deleting back to a wider matching prefix widens the list.
        let mut inp = input();
        type_str(&mut inp, "/model");
        // `/model` is an exact hit now (the old `/models` prefix twin
        // is gone since the `/provider` rename).
        assert_eq!(labels(&inp), vec!["/model"]);
        inp.handle(&Action::InputBackspace, &mut ctx()); // "/mode"
        assert_eq!(labels(&inp), vec!["/model"]);
        inp.handle(&Action::InputBackspace, &mut ctx()); // "/mod"
        inp.handle(&Action::InputBackspace, &mut ctx()); // "/mo"
        assert_eq!(labels(&inp), vec!["/model", "/mouse"]);
        // Deleting back to bare `/` reopens the FULL list; deleting
        // the `/` itself closes entirely.
        let mut inp = input();
        type_str(&mut inp, "/h");
        assert_eq!(labels(&inp), vec!["/help"]);
        inp.handle(&Action::InputBackspace, &mut ctx()); // → "/"
        assert_eq!(labels(&inp).len(), 9, "bare `/` lists everything");
        inp.handle(&Action::InputBackspace, &mut ctx()); // → ""
        assert!(!inp.completion_open(), "empty input is not a command");
        assert!(inp.is_empty());
        // DeleteWord on `/model fa` falls back to the full args list.
        let mut inp = input();
        type_str(&mut inp, "/model fa");
        assert_eq!(labels(&inp), vec!["fast"]);
        inp.handle(&Action::InputDeleteWord, &mut ctx());
        assert_eq!(inp.text(), "/model ");
        assert_eq!(labels(&inp), vec!["fast", "main"]);
    }

    #[test]
    fn arrows_navigate_with_wrap_and_never_touch_history() {
        let mut inp = input();
        // Seed history so a history walk would be observable.
        type_str(&mut inp, "old");
        let _ = inp.handle(&Action::SubmitInput, &mut ctx());
        type_str(&mut inp, "/mo");
        assert_eq!(inp.completion.as_ref().unwrap().selected, 0);
        // ↑ from the head wraps to the tail (2 `/mo` hits → index 1,
        // since `/provider` left the class in the model-mgmt rename).
        inp.handle(&Action::InputHistoryPrev, &mut ctx());
        assert_eq!(inp.completion.as_ref().unwrap().selected, 1);
        inp.handle(&Action::InputHistoryPrev, &mut ctx());
        assert_eq!(inp.completion.as_ref().unwrap().selected, 0, "wrap");
        // ↓ wraps forward too; the buffer is never replaced.
        inp.handle(&Action::InputHistoryNext, &mut ctx());
        assert_eq!(inp.completion.as_ref().unwrap().selected, 1);
        assert_eq!(inp.text(), "/mo", "history walking is suppressed");
    }

    #[test]
    fn tab_applies_command_with_trailing_space() {
        let mut inp = input();
        type_str(&mut inp, "/mo");
        inp.handle(&Action::FocusNext, &mut ctx()); // Tab
        assert_eq!(inp.text(), "/model ");
        // copy/model immediately reopen their argument list.
        assert_eq!(
            mode_of(&inp),
            Some(CompletionMode::Args {
                cmd: "model".into()
            })
        );
        // A non-args command completes and the list closes (refresh:
        // "/help " carries a space).
        let mut inp = input();
        type_str(&mut inp, "/he");
        inp.handle(&Action::FocusNext, &mut ctx());
        assert_eq!(inp.text(), "/help ");
        assert!(!inp.completion_open());
    }

    #[test]
    fn tab_applies_arg_without_trailing_space() {
        let mut inp = input();
        type_str(&mut inp, "/model fa");
        inp.handle(&Action::FocusNext, &mut ctx());
        assert_eq!(inp.text(), "/model fast");
        assert_eq!(
            mode_of(&inp),
            Some(CompletionMode::Args {
                cmd: "model".into()
            }),
            "the args list stays open for the confirming Enter"
        );
    }

    #[test]
    fn enter_submits_a_simple_command_through_start_turn() {
        let mut inp = input();
        type_str(&mut inp, "/he");
        let action = inp.handle(&Action::SubmitInput, &mut ctx());
        assert_eq!(action, Some(Action::StartTurn("/help ".into())));
        assert!(inp.is_empty(), "submit took the buffer");
        assert!(!inp.completion_open());
    }

    #[test]
    fn enter_on_copy_and_model_behaves_like_tab() {
        let mut inp = input();
        type_str(&mut inp, "/copy");
        assert_eq!(
            inp.handle(&Action::SubmitInput, &mut ctx()),
            None,
            "no submit — the arg list opens instead"
        );
        assert_eq!(inp.text(), "/copy ");
        assert!(inp.completion_open());

        let mut inp = input();
        type_str(&mut inp, "/model");
        assert_eq!(inp.handle(&Action::SubmitInput, &mut ctx()), None);
        assert_eq!(inp.text(), "/model ");
        assert!(inp.completion_open());
    }

    #[test]
    fn enter_on_an_arg_applies_then_confirms() {
        let mut inp = input();
        type_str(&mut inp, "/model fa");
        // First Enter applies (text changes) — no submit.
        assert_eq!(inp.handle(&Action::SubmitInput, &mut ctx()), None);
        assert_eq!(inp.text(), "/model fast");
        // Second Enter: application is a no-op → submit verbatim.
        assert_eq!(
            inp.handle(&Action::SubmitInput, &mut ctx()),
            Some(Action::StartTurn("/model fast".into()))
        );
        assert!(inp.is_empty());
    }

    #[test]
    fn esc_closes_but_keeps_the_text() {
        let mut inp = input();
        type_str(&mut inp, "/mo");
        inp.handle(&Action::DismissOverlay, &mut ctx());
        assert!(!inp.completion_open());
        assert_eq!(inp.text(), "/mo");
        // Typing resumes normally (the closed list no longer
        // intercepts anything).
        type_str(&mut inp, "re");
        assert_eq!(inp.text(), "/more");
    }

    #[test]
    fn submit_without_a_list_takes_the_normal_path() {
        let mut inp = input();
        type_str(&mut inp, "hello");
        assert_eq!(
            inp.handle(&Action::SubmitInput, &mut ctx()),
            Some(Action::StartTurn("hello".into()))
        );
        // A closed list + Enter never re-opens (no completion state).
        assert!(!inp.completion_open());
    }

    #[test]
    fn set_text_closes_the_list() {
        let mut inp = input();
        type_str(&mut inp, "/mo");
        assert!(inp.completion_open());
        inp.set_text("/model fast");
        assert!(!inp.completion_open(), "programmatic replace closes");
    }

    // ── slash-1: completion layout & rendering ─────────────────────────

    use ratatui::style::Modifier;

    /// Render the completion list alone at `w x h` and return the
    /// buffer (the draw() pattern from tests/overlays_render.rs).
    /// `hover` feeds interactive-1's mouse-hover row.
    fn draw_completion(
        inp: &InputComponent,
        c: &AppCtx,
        w: u16,
        h: u16,
        hover: Option<usize>,
    ) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("test terminal");
        terminal
            .draw(|f| inp.render_completion(f, f.area(), c, hover))
            .expect("draw");
        terminal.backend().buffer().clone()
    }

    #[test]
    fn completion_rows_capped_at_max_visible() {
        let mut inp = input();
        type_str(&mut inp, "/");
        // Closed → 0.
        let closed = input();
        assert_eq!(closed.completion_rows(), 0);
        // 9 commands → capped at 8 (the 9th joins since model-mgmt-2;
        // the display then spends its last row on the (i/n) indicator).
        assert_eq!(inp.completion_rows(), 8);
        // Fewer items → the item count (`/mo` hits {model, mouse}).
        let mut inp = input();
        type_str(&mut inp, "/mo");
        assert_eq!(inp.completion_rows(), 2);
        // More items than the cap → capped at 8 (the painter shows
        // 7 + the `(i/n)` indicator; the avail clamp is the App's).
        let mut c = ctx();
        c.config.model_aliases = (1..=12).map(|i| format!("a{i:02}")).collect();
        let mut inp = input();
        for ch in "/model ".chars() {
            inp.handle(&Action::InputChar(ch), &mut c);
        }
        assert_eq!(inp.completion_rows(), 8);
    }

    #[test]
    fn completion_renders_marker_columns_and_styles() {
        let mut inp = input();
        type_str(&mut inp, "/mo"); // [model, mouse], model selected
        let buf = draw_completion(&inp, &ctx(), 60, 8, None);
        // Row 0 = selected: `→ ` marker at x=2 (PROMPT_COLS), label at
        // x=4 — signal + BOLD on the whole row.
        assert_eq!(buf[(2, 0)].symbol(), "→");
        assert_eq!(buf[(4, 0)].symbol(), "/");
        assert_eq!(buf[(5, 0)].symbol(), "m");
        let signal = Color::Rgb(0x67, 0xE8, 0xF9);
        for x in [2u16, 4, 5] {
            assert_eq!(buf[(x, 0)].style().fg, Some(signal), "signal at {x}");
            assert!(
                buf[(x, 0)].style().add_modifier.contains(Modifier::BOLD),
                "BOLD at {x}"
            );
        }
        // Row 1 = unselected: 2-space pad at x=2, text-colored label,
        // muted description (mouse has no args_hint → desc only:
        // `鼠标捕获开关`).
        assert_eq!(buf[(2, 1)].symbol(), " ");
        assert_eq!(buf[(3, 1)].symbol(), " ");
        assert_eq!(buf[(4, 1)].symbol(), "/");
        assert_eq!(buf[(4, 1)].style().fg, Some(Color::Rgb(0xD6, 0xD6, 0xD6)));
        // Main column = clamp(widest(`/model`)=6 + 2, 12, 32) = 12 →
        // the description starts at x = 2 + 2 + 12 = 16.
        assert_eq!(buf[(16, 1)].symbol(), "鼠");
        assert_eq!(buf[(16, 1)].style().fg, Some(Color::Rgb(0xAD, 0xAD, 0xAD)));
        // The SELECTED row's description (`<alias> — 切换模型别名`)
        // rides the same signal + BOLD (the whole row).
        assert_eq!(buf[(16, 0)].symbol(), "<");
        assert_eq!(buf[(16, 0)].style().fg, Some(signal));
        let desc: String = (16..28u16).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(desc.contains("alias"), "desc column: {desc:?}");
        // No overflow indicator (3 items, 8 rows).
        assert_eq!(
            buf[(2, 7)].symbol(),
            " ",
            "no (i/n) row when everything fits"
        );
    }

    #[test]
    fn completion_hides_the_description_on_narrow_terminals() {
        let mut inp = input();
        type_str(&mut inp, "/mo");
        let buf = draw_completion(&inp, &ctx(), 40, 8, None);
        // Width 40: description column hidden; the main column and
        // marker still render.
        assert_eq!(buf[(2, 0)].symbol(), "→");
        let row: String = (0..40u16).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(row.contains("/model"), "label present: {row:?}");
        assert!(!row.contains("alias"), "desc hidden at width 40: {row:?}");
    }

    #[test]
    fn completion_overflow_shows_index_and_scrolls() {
        let mut inp = input();
        type_str(&mut inp, "/"); // 9 rows, budget 3 → 2 items + (i/n)
        let buf = draw_completion(&inp, &ctx(), 60, 3, None);
        // Items: rows 0-1 (help, exit); the dim ASCII indicator on the
        // last row: selected=1-based index over the total.
        assert_eq!(buf[(4, 0)].symbol(), "/");
        let head: String = (4..9u16).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(head.contains("help"), "first visible: {head:?}");
        let tail: String = (4..9u16).map(|x| buf[(x, 1)].symbol()).collect();
        assert!(tail.contains("exit"), "second visible: {tail:?}");
        let indicator: String = (2..8u16).map(|x| buf[(x, 2)].symbol()).collect();
        assert_eq!(indicator.trim(), "(1/9)", "indicator row: {indicator:?}");
        assert_eq!(
            buf[(2, 2)].style().fg,
            Some(Color::Rgb(0x66, 0x66, 0x66)),
            "dim indicator"
        );

        // Move the selection down: the window scrolls to keep it
        // centered-ish (selected 3 → start 3-1=2 → new/status).
        inp.handle(&Action::InputHistoryNext, &mut ctx());
        inp.handle(&Action::InputHistoryNext, &mut ctx());
        inp.handle(&Action::InputHistoryNext, &mut ctx());
        let buf = draw_completion(&inp, &ctx(), 60, 3, None);
        let head: String = (4..10u16).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(head.contains("new"), "window starts at `new`: {head:?}");
        let indicator: String = (2..8u16).map(|x| buf[(x, 2)].symbol()).collect();
        assert_eq!(indicator.trim(), "(4/9)");
        // The selected row (`status`, index 3 → display row 1) is the
        // BOLD signal one.
        assert_eq!(buf[(2, 1)].symbol(), "→");
    }

    #[test]
    fn completion_truncates_long_descriptions_with_the_tier_ellipsis() {
        // Width 41 (> 40 → desc shown): avail = 41-2-2-12 = 25; the
        // copy row's desc `[all|tool] — 复制输出·兜底存文件` (32 cols)
        // truncates with `…`.
        let mut inp = input();
        type_str(&mut inp, "/copy");
        let buf = draw_completion(&inp, &ctx(), 41, 8, None);
        // Selected row 0 = /copy (exact match): find the desc span
        // after the main column (x=16).
        let row: String = (16..41u16).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(row.contains('…'), "truncated with ellipsis: {row:?}");
        assert!(row.contains("[all|tool]"), "hint lead kept: {row:?}");
        assert!(!row.contains("文件"), "the tail was cut: {row:?}");
    }

    #[test]
    fn completion_renders_ascii_tier_without_unicode_chrome() {
        // Ascii tier: the marker degrades to `>` (the right-arrow
        // slot), the ellipsis to `...`.
        let mut c = ctx();
        c.theme = crate::theme::Theme::dark().with_icons(crate::icons::Icons::Ascii);
        let mut inp = input();
        type_str(&mut inp, "/mo");
        let buf = draw_completion(&inp, &c, 60, 8, None);
        assert_eq!(buf[(2, 0)].symbol(), ">", "ascii marker");
        let row: String = (0..30u16).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(row.contains("/model"));
        assert!(!row.contains('→'), "no unicode arrow on ascii");
        // The selected row's desc `—` chrome downgrades to `-` (the
        // model row carries the args_hint separator).
        let desc: String = (16..28u16).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(desc.contains('-'), "em-dash localized: {desc:?}");
        assert!(!desc.contains('—'), "no raw em-dash: {desc:?}");
    }

    #[test]
    fn completion_colors_come_from_theme_slots_on_all_boards() {
        // The list must ride the semantic slots on every board —
        // spot-check the ANSI degraded board (indexed colors, no Rgb
        // leak) and the light board against their palette values.
        for (theme, signal, text, muted) in [
            (
                crate::theme::Theme::ansi(),
                Color::Indexed(81),  // ANSI signal
                Color::Indexed(252), // ANSI text
                Color::Indexed(245), // ANSI muted
            ),
            (
                crate::theme::Theme::light(),
                Color::Rgb(0x06, 0xB6, 0xD4), // light signal
                Color::Rgb(0x30, 0x30, 0x30), // light text
                Color::Rgb(0x66, 0x66, 0x66), // light muted
            ),
        ] {
            let mut c = ctx();
            c.theme = theme;
            let mut inp = input();
            type_str(&mut inp, "/mo");
            let buf = draw_completion(&inp, &c, 60, 8, None);
            // Selected row (model): signal + BOLD.
            assert_eq!(buf[(2, 0)].style().fg, Some(signal));
            assert!(buf[(2, 0)].style().add_modifier.contains(Modifier::BOLD));
            // Unselected label: text; its description: muted.
            assert_eq!(buf[(4, 1)].style().fg, Some(text));
            assert_eq!(buf[(16, 1)].style().fg, Some(muted));
        }
        // The overflow indicator is the dim slot (dark board probe).
        let mut inp = input();
        type_str(&mut inp, "/");
        let buf = draw_completion(&inp, &ctx(), 60, 3, None);
        assert_eq!(buf[(2, 2)].style().fg, Some(Color::Rgb(0x66, 0x66, 0x66)));
    }

    // ── interactive-1: completion hover & click ─────────────────────────

    #[test]
    fn completion_row_at_maps_item_rows_only() {
        let mut inp = input();
        type_str(&mut inp, "/"); // 9 commands; a 3-row area shows 2 + (i/n)
        let rect = Rect {
            x: 0,
            y: 10,
            width: 60,
            height: 3,
        };
        // selected=0 → window [0..2]: row 0 ↦ 0, row 1 ↦ 1 …
        assert_eq!(inp.completion_row_at(rect, 10), Some(0));
        assert_eq!(inp.completion_row_at(rect, 11), Some(1));
        // … the indicator row and anything outside the rect are dead.
        assert_eq!(inp.completion_row_at(rect, 12), None, "indicator row");
        assert_eq!(inp.completion_row_at(rect, 9), None, "above the rect");
        assert_eq!(inp.completion_row_at(rect, 13), None, "below the rect");
        // Taller area: every row is an item row (9 commands, 9 rows).
        let tall = Rect {
            x: 0,
            y: 0,
            width: 60,
            height: 9,
        };
        assert_eq!(inp.completion_row_at(tall, 8), Some(8));
        // Closed list maps nothing.
        let closed = input();
        assert_eq!(closed.completion_row_at(tall, 0), None);
        // Row-existence check (the App's hover-validity probe).
        assert!(inp.completion_row_exists(8));
        assert!(!inp.completion_row_exists(9));
        assert!(!closed.completion_row_exists(0));
    }

    #[test]
    fn completion_row_at_tracks_the_scroll_window() {
        // With 12 model aliases the window recenters on the selection:
        // selected 5 → start 5-1=4 → screen row 0 displays index 4.
        let mut c = ctx();
        c.config.model_aliases = (1..=12).map(|i| format!("a{i:02}")).collect();
        let mut inp = input();
        for ch in "/model ".chars() {
            inp.handle(&Action::InputChar(ch), &mut c);
        }
        for _ in 0..5 {
            inp.handle(&Action::InputHistoryNext, &mut c);
        }
        let rect = Rect {
            x: 0,
            y: 0,
            width: 60,
            height: 4,
        };
        assert_eq!(inp.completion_row_at(rect, 0), Some(4), "window start");
        assert_eq!(inp.completion_row_at(rect, 2), Some(6), "mid row");
        assert_eq!(inp.completion_row_at(rect, 3), None, "indicator row");
    }

    #[test]
    fn hover_styles_only_the_label_span() {
        let mut inp = input();
        type_str(&mut inp, "/mo"); // [model, mouse]; hover row 1 = /mouse
        let buf = draw_completion(&inp, &ctx(), 60, 8, Some(1));
        let signal = Color::Rgb(0x67, 0xE8, 0xF9);
        // Label span: hover slot (signal + UNDERLINED + BOLD).
        assert_eq!(buf[(4, 1)].style().fg, Some(signal));
        assert!(buf[(4, 1)]
            .style()
            .add_modifier
            .contains(Modifier::UNDERLINED));
        assert!(buf[(4, 1)].style().add_modifier.contains(Modifier::BOLD));
        // NO marker, NO row-bold: the pad stays blank and the desc
        // stays muted without underline.
        assert_eq!(buf[(2, 1)].symbol(), " ", "no marker on hover");
        assert_eq!(buf[(16, 1)].style().fg, Some(Color::Rgb(0xAD, 0xAD, 0xAD)));
        assert!(!buf[(16, 1)]
            .style()
            .add_modifier
            .contains(Modifier::UNDERLINED));
        // The other rows keep the plain styles.
        assert_eq!(buf[(4, 0)].style().fg, Some(signal), "selected row intact");
        assert!(
            !buf[(4, 0)]
                .style()
                .add_modifier
                .contains(Modifier::UNDERLINED),
            "selected row is not underlined (its style wins)"
        );
    }

    #[test]
    fn hover_on_the_selected_row_keeps_the_selection_style() {
        let mut inp = input();
        type_str(&mut inp, "/mo"); // row 0 selected
        let buf = draw_completion(&inp, &ctx(), 60, 8, Some(0));
        assert_eq!(buf[(2, 0)].symbol(), "→", "marker stays");
        assert!(
            !buf[(4, 0)]
                .style()
                .add_modifier
                .contains(Modifier::UNDERLINED),
            "keyboard-selection style wins over hover"
        );
    }

    #[test]
    fn completion_click_selects_then_runs_the_enter_branches() {
        // /mo click on row 1 (/mouse — a plain command): Enter's branch
        // (a) — apply + submit → StartTurn("/mouse ").
        let mut inp = input();
        type_str(&mut inp, "/mo");
        let follow = inp.completion_click(&ctx(), 1);
        assert_eq!(follow, Some(Action::StartTurn("/mouse ".to_owned())));
        assert!(inp.text().is_empty(), "buffer taken by the submit");

        // /model click on an alias row (argument mode): Enter's branch
        // (c) — apply only, no submit while the text changes.
        let mut c = ctx();
        c.config.model_aliases = ["main", "fast"].iter().map(|s| s.to_string()).collect();
        let mut inp = input();
        for ch in "/model ".chars() {
            inp.handle(&Action::InputChar(ch), &mut c);
        }
        let follow = inp.completion_click(&c, 1); // fast
        assert_eq!(follow, None, "arg apply does not submit");
        assert_eq!(inp.text(), "/model fast");

        // A second click (same row, nothing changes): branch (c)'s
        // identity guard — the buffer submits verbatim.
        let follow = inp.completion_click(&c, 1);
        assert_eq!(
            follow,
            Some(Action::StartTurn("/model fast".to_owned())),
            "identity submit on re-click"
        );
    }
}
