//! Help overlay — keymap reference (P3 lane-c: full table).
//!
//! Content is the design brief's shortcut table in full — global keys,
//! input-box keys, transcript-scroll keys, approval keys and the slash
//! subset — in two columns: key = Cyan (BOLD), description = default
//! foreground (body-text principle). The App opens/closes the modal,
//! clears the background and computes the centered 60%×70% area (see
//! `App::render`); this component fills whatever area it receives.
//!
//! Borderless container (Wave 2 lane-c): row 0 is a REVERSED title bar
//! — ` Help ` padded out to the overlay's full width, all
//! reversed, so it reads as a solid color bar; the body sits 1 column
//! inside each edge; a right-aligned muted footer (`Esc / ? 关闭`)
//! rides the last row.
//!
//! Degradation: the body is 21 lines (fix-13 added the Ctrl+Y row),
//! which fits the 70% overlay from ~33 terminal rows up; on shorter
//! terminals the tail simply clips (Paragraph never panics) and the
//! footer stays visible because the body area stops one row above the
//! footer row.

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::{AppCtx, Component};
use crate::action::Action;
use crate::theme::Theme;

/// Key-column width in display columns. Every key string is ASCII
/// (arrows are width-1), so `format!` char padding equals display width.
/// Longest key: `Alt+Enter / Ctrl+J` (18) + a 2-column gap.
const KEY_COLS: usize = 20;

/// The help overlay.
#[derive(Debug, Default)]
pub struct HelpComponent;

impl HelpComponent {
    pub fn new() -> Self {
        Self
    }

    /// The keymap sections (design brief shortcut table, full set):
    /// `(section header, [(key, description)])`.
    fn sections() -> Vec<(&'static str, Vec<(&'static str, &'static str)>)> {
        vec![
            (
                "全局",
                vec![
                    ("Tab", "焦点循环（输入↔转录↔侧栏）"),
                    ("?", "帮助浮层（输入框为空时）"),
                    ("Ctrl+T", "侧栏显隐（窄终端自动隐藏）"),
                    ("Ctrl+M", "鼠标捕获（关=可选中复制）"),
                    ("Ctrl+Y", "复制最后输出"),
                    ("Ctrl+L", "强制重绘"),
                    ("Ctrl+C", "取消本轮 / 退出确认"),
                    ("Esc", "关闭浮层 / 取消滚动钉住"),
                ],
            ),
            (
                "输入框",
                vec![
                    ("Enter", "提交 prompt 开始新一轮"),
                    ("Alt+Enter / Ctrl+J", "插入换行"),
                    ("↑ / ↓", "输入历史（首/末行时）"),
                    ("Ctrl+W", "删除光标前一个词"),
                ],
            ),
            (
                "转录区滚动",
                vec![("滚轮 ↑↓ PgUp PgDn", "滚动 (g/G/Home/End 跳顶底)")],
            ),
            (
                "审批激活时",
                vec![
                    ("y / n / a", "允许 / 拒绝 / 本轮全部允许"),
                    ("Ctrl+C", "拒绝并取消本轮"),
                ],
            ),
        ]
    }

    /// The slash-command line (full-width, no key column). `/help` is
    /// deliberately absent — this line renders INSIDE the help overlay
    /// (self-referential; `?` and the footer already document it).
    /// `/exit` was dropped when `/copy` joined (fix-13) to hold the
    /// 46-column budget — quitting stays discoverable through the
    /// Ctrl+C row. Width budget: ≤46 display columns (60% overlay on
    /// an 80-col terminal − 2 inset).
    fn slash_commands() -> &'static str {
        "/new /status /agents /model 别名 /copy /mouse"
    }

    /// The overlay body (inside the borders), 20 lines. Crate-local so
    /// unit tests can assert content and styles without a terminal.
    pub(crate) fn overlay_lines(theme: &Theme) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for (header, rows) in Self::sections() {
            lines.push(Line::styled(format!("── {header} ──"), theme.muted));
            for (key, desc) in rows {
                // Display-width-aware padding: `{key:<KEY_COLS$}` pads by
                // CHAR count, which misaligns CJK keys (滚轮 = 2 chars but
                // 4 columns). Pad to KEY_COLS display columns instead.
                let pad = KEY_COLS.saturating_sub(ratatui::text::Span::raw(key).width());
                let key_col = format!("{key}{}", " ".repeat(pad));
                lines.push(Line::from(vec![
                    Span::styled(key_col, theme.user_label),
                    Span::raw(desc),
                ]));
            }
        }
        lines.push(Line::styled("── slash 命令 ──", theme.muted));
        lines.push(Line::from(Self::slash_commands()));
        lines
    }
}

impl Component for HelpComponent {
    fn handle(&mut self, _action: &Action, _ctx: &mut AppCtx) -> Option<Action> {
        None
    }

    fn render(&self, f: &mut Frame, area: Rect, ctx: &AppCtx) {
        // The App clears the background and computes the centered area;
        // this component fills it. Borderless (Wave 2 lane-c): REVERSED
        // full-width title bar instead of a frame; body inset 1 column
        // each side; muted footer on the last row, right-aligned, safe
        // from body clipping (the body area stops one row above it).
        if area.width < 2 || area.height < 2 {
            return; // degenerate overlay — nothing legible to paint
        }
        let theme = &ctx.theme;
        // Row 0 — REVERSED title bar: the title text (with its framing
        // spaces) plus reversed-space padding out to the full width, so
        // the whole row reads as a color bar. Rendered as a bare Line
        // widget — its render sets the line style on EVERY cell of the
        // row (a Paragraph would skip the padding cells after wide
        // glyphs). CJK-aware fill: pad by display columns, not chars.
        // ASCII-only title: CJK in a REVERSED row leaves wide-glyph tail
        // cells that the frame diff can never repaint (stale cells).
        let title = " Help ";
        let fill = " ".repeat((area.width as usize).saturating_sub(Span::raw(title).width()));
        let title_rect = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: 1,
        };
        f.render_widget(
            Line::styled(format!("{title}{fill}"), theme.overlay_title),
            title_rect,
        );
        // Post-pass: CJK in the title resets the style of the skip
        // cells after wide glyphs (same mechanism as the status bar's
        // black-gaps bug) — re-apply the treatment so the color bar
        // renders continuously.
        f.buffer_mut().set_style(title_rect, theme.overlay_title);
        // Rows 1..h-1 — the keymap, 1 column inside each edge (long
        // rows clip; Paragraph never wraps here, matching the framed
        // behavior).
        f.render_widget(
            Paragraph::new(Self::overlay_lines(theme)),
            Rect {
                x: area.x + 1,
                y: area.y + 1,
                width: area.width - 2,
                height: area.height - 2,
            },
        );
        // Row h-1 — right-aligned muted close affordance.
        f.render_widget(
            Paragraph::new(Line::styled("Esc / ? 关闭", theme.muted))
                .alignment(ratatui::layout::Alignment::Right),
            Rect {
                x: area.x + 1,
                y: area.y + area.height - 1,
                width: area.width - 2,
                height: 1,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::{ConfigSummary, Focus, RunInfo, RunState};

    fn theme() -> Theme {
        Theme::new()
    }

    fn test_ctx() -> AppCtx {
        AppCtx {
            theme: Theme::new(),
            focus: Focus::Input,
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

    fn text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    }

    #[test]
    fn body_covers_the_full_keymap() {
        let lines = HelpComponent::overlay_lines(&theme());
        let all = lines.iter().map(text).collect::<Vec<_>>().join("\n");
        for needle in [
            "全局",
            "Tab",
            "Ctrl+T",
            "Ctrl+M",
            "鼠标捕获",
            "Ctrl+Y",
            "复制最后输出",
            "Ctrl+L",
            "Ctrl+C",
            "Esc",
            "输入框",
            "Enter",
            "Alt+Enter / Ctrl+J",
            "↑ / ↓",
            "Ctrl+W",
            "转录区滚动",
            "滚轮 ↑↓ PgUp PgDn",
            "g/G/Home/End",
            "审批激活时",
            "y / n / a",
            "允许 / 拒绝 / 本轮全部允许",
            "slash 命令",
            "/new /status /agents /model 别名 /copy /mouse",
        ] {
            assert!(all.contains(needle), "help must cover {needle}");
        }
    }

    #[test]
    fn body_line_count_matches_the_70pct_overlay() {
        // fix-13 added the Ctrl+Y row: 21 lines. 70% of ~33 rows = 23 →
        // title bar (1) + 21 content lines + footer (1) fits; shorter
        // terminals clip only the tail (Paragraph never panics) and the
        // footer stays visible (the body area stops one row above it).
        let lines = HelpComponent::overlay_lines(&theme());
        assert_eq!(lines.len(), 21, "21 lines: {lines:?}");
    }

    #[test]
    fn rows_fit_an_80_col_terminal_overlay() {
        // 60% of 80 cols = 48 → borderless content area = 48 − 2 pad
        // (1 column inside each edge) = 46 columns.
        let lines = HelpComponent::overlay_lines(&theme());
        for line in &lines {
            let rendered = text(line);
            let width = line.width();
            assert!(width <= 46, "row too wide ({width}): {rendered}");
        }
    }

    #[test]
    fn two_column_styling_keys_cyan_desc_default() {
        let lines = HelpComponent::overlay_lines(&theme());
        let theme = theme();
        // First data row: key span Cyan+BOLD, description default.
        let first_row = &lines[1];
        assert_eq!(first_row.spans[0].style, theme.user_label);
        assert_eq!(first_row.spans[1].style, ratatui::style::Style::new());
        // Section headers are muted (`Line::styled` carries the style on
        // the line itself).
        assert_eq!(lines[0].style, theme.muted);
    }

    #[test]
    fn key_column_padding_aligns_descriptions() {
        let lines = HelpComponent::overlay_lines(&theme());
        // Every key/desc row starts its description at column KEY_COLS.
        for line in lines.iter().skip(1) {
            if line.spans.len() == 2 && line.spans[0].style == theme().user_label {
                assert_eq!(
                    line.spans[0].width(),
                    KEY_COLS,
                    "key column padded: {}",
                    text(line)
                );
            }
        }
    }

    // ── borderless container (Wave 2 lane-c) ───────────────────────────

    /// Render through a TestBackend and hand back the buffer (style
    /// assertions need cells, not the string view).
    fn draw_buffer(
        width: u16,
        height: u16,
        paint: impl FnOnce(&mut Frame),
    ) -> ratatui::buffer::Buffer {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                .expect("test terminal");
        terminal
            .draw(|f| paint(f))
            .expect("painting must not panic");
        terminal.backend().buffer().clone()
    }

    /// The title row carries the REVERSED bar style across its full
    /// width. Ratatui's buffer semantics reset the "skip" cell after
    /// each wide glyph (CJK), so those cells stay unstyled holes —
    /// every OTHER cell of the title row (and no other row) is
    /// reversed.
    #[test]
    fn title_bar_is_reversed_across_the_full_width() {
        let buf = draw_buffer(60, 25, |f| {
            HelpComponent::new().render(f, f.area(), &test_ctx());
        });
        let title = " Help ";
        let wide = title
            .chars()
            .filter(|c| Span::raw(c.to_string()).width() > 1)
            .count();
        let mut title_rows = 0;
        for y in 0..buf.area.height {
            let cells: Vec<_> = (0..buf.area.width)
                .map(|x| buf.cell((x, y)).expect("cell in bounds"))
                .collect();
            let is_rev = |c: &&ratatui::buffer::Cell| {
                c.modifier.contains(ratatui::style::Modifier::REVERSED)
            };
            if !cells.iter().any(&is_rev) {
                continue; // not the title row
            }
            title_rows += 1;
            assert_eq!(title_rows, 1, "only one reversed row: row {y}");
            let reversed = cells.iter().filter(|c| is_rev(c)).count();
            assert_eq!(
                reversed,
                buf.area.width as usize - wide,
                "full-width bar minus one skip hole per wide glyph"
            );
            for c in &cells {
                if !is_rev(c) {
                    assert_eq!(c.symbol(), " ", "the only holes are wide-glyph skips");
                }
            }
        }
        assert_eq!(title_rows, 1, "exactly the title row is reversed");
    }

    /// Footer: right-aligned on the last row with a 1-column margin to
    /// the overlay's right edge; muted style.
    #[test]
    fn footer_is_right_aligned_muted_on_the_last_row() {
        let buf = draw_buffer(50, 12, |f| {
            HelpComponent::new().render(f, f.area(), &test_ctx());
        });
        let last = buf.area.height - 1;
        // Last non-blank cell of the row = the footer's trailing 闭.
        let mut last_text_x = None;
        for x in 0..buf.area.width {
            let sym = buf.cell((x, last)).map(|c| c.symbol()).unwrap_or(" ");
            if sym != " " {
                last_text_x = Some(x);
            }
        }
        let x = last_text_x.expect("footer text on the last row");
        let cell = buf.cell((x, last)).expect("footer cell");
        assert_eq!(cell.symbol(), "闭", "footer ends with 闭");
        assert_eq!(cell.fg, ratatui::style::Color::DarkGray, "footer is muted");
        // Right-aligned against the borderless 1-column inset: 闭 spans
        // 2 columns (x, x+1) and exactly one blank column remains.
        assert_eq!(
            x + 3,
            buf.area.width,
            "one blank inset column after the footer"
        );
    }
}
