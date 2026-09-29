//! Help overlay — keymap reference (restyle-1: minimax panel frame).
//!
//! Content is the design brief's shortcut table in full — global keys,
//! input-box keys, transcript-scroll keys, approval keys and the slash
//! subset — in two columns: key = BOLD text color, description = body
//! text. The App opens/closes the modal, clears the background and
//! computes the centered 60%×70% area (see `App::render`); this
//! component fills whatever area it receives.
//!
//! Container (restyle-1): the rounded panel frame
//! `╭─ Help ─ Esc 关闭 ─╮` / `│ body │` / `├───┤` / `│ footer │` /
//! `╰───╯` — line-color strokes, signal+BOLD title, a muted meta in
//! the header's right slot (panel-meta-1: the close-key hint), a
//! muted right-aligned footer (`Esc / ? 关闭`) riding the row above
//! the bottom border. Degenerate areas degrade via the shared
//! [`crate::panel`] helper.
//!
//! Degradation: the body is 23 lines (fix-13 added the Ctrl+Y row,
//! copy-1 the `/copy` forms line, select-1 the drag-select row),
//! which fits the 70% overlay from ~34 terminal rows up; on shorter
//! terminals the tail simply clips (Paragraph never panics) and the
//! footer stays visible because the body area stops two rows above
//! the frame bottom.

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::{AppCtx, Component};
use crate::action::Action;
use crate::icons::localize;
use crate::panel;
use crate::theme::Theme;

/// Key-column width in display columns. Every key string is ASCII
/// (arrows are width-1), so `format!` char padding equals display width.
/// Longest key: `Alt+Enter / Ctrl+J` (18) + a 1-column gap. The body
/// budget is 44 columns (60% overlay on an 80-col terminal − 4 frame
/// columns).
const KEY_COLS: usize = 19;

/// Content budget of the 60%-wide overlay on an 80-col terminal
/// (48 − 4 frame cols): the registry-derived slash command line wraps
/// greedily to this width (model-mgmt-2 — the model-management command
/// pushed the single line past it).
const SLASH_LINE_MAX_COLS: usize = 44;

/// Greedy word wrap on ASCII spaces (the slash line is space-joined
/// `/names`; no CJK inside). A single word wider than `max` rides its
/// own line (Paragraph clips — never panics).
fn wrap_words(s: &str, max: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in s.split(' ') {
        let candidate = if cur.is_empty() {
            word.to_owned()
        } else {
            format!("{cur} {word}")
        };
        if candidate.chars().count() > max && !cur.is_empty() {
            lines.push(std::mem::take(&mut cur));
            cur = word.to_owned();
        } else {
            cur = candidate;
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

/// The help overlay.
#[derive(Debug, Default)]
pub struct HelpComponent;

impl HelpComponent {
    pub fn new() -> Self {
        Self
    }

    /// The keymap sections (design brief shortcut table, full set;
    /// restyle-1 rewords the Ctrl+T/Tab rows for the agents panel):
    /// `(section header, [(key, description)])`.
    fn sections() -> Vec<(&'static str, Vec<(&'static str, &'static str)>)> {
        vec![
            (
                "全局",
                vec![
                    ("Tab", "焦点循环 输入/转录/浮层"),
                    ("?", "帮助浮层（输入框为空时）"),
                    ("Ctrl+T", "agents 浮层显隐"),
                    ("Ctrl+M /mouse", "鼠标捕获·关=终端原生选择"),
                    ("Ctrl+Y", "复制最后输出（文件兜底）"),
                    ("Ctrl+L", "强制重绘"),
                    ("Ctrl+C", "取消本轮 / 退出确认"),
                    ("Esc", "关闭浮层 / 取消选区/滚动"),
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
                "转录区滚动与选择",
                vec![
                    ("滚轮 ↑↓ PgUp PgDn", "滚动·g/G/Home/End 跳顶底"),
                    ("拖拽选中文本", "松开自动复制"),
                ],
            ),
            (
                "审批激活时",
                vec![
                    ("y / n / a", "允许 / 拒绝 / 全部允许"),
                    ("Ctrl+C", "拒绝并取消本轮"),
                ],
            ),
        ]
    }

    /// The slash-command line (full-width, no key column), derived
    /// from the registry (slash-1 single truth). `/help` is
    /// deliberately absent — this line renders INSIDE the help overlay
    /// (self-referential; `?` and the footer already document it).
    /// `/exit` was dropped when `/copy` joined (fix-13) to hold the
    /// 44-column budget — quitting stays discoverable through the
    /// Ctrl+C row. model-mgmt-2: the model-management command (`/provider`)
    /// pushed the line past that
    /// budget, so the RENDER wraps it (see `SLASH_LINE_MAX_COLS`);
    /// this derived string stays the byte-exact registry truth.
    fn slash_commands() -> String {
        crate::slash::registry()
            .iter()
            .map(|s| format!("/{}", s.name))
            .filter(|cmd| cmd != "/help" && cmd != "/exit")
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The `/copy` argument forms (copy-1): no arg = last assistant
    /// output; `all` = whole transcript; `tool` = last tool output;
    /// every form also lands in a copy file (`last-copy.md`) as the
    /// guaranteed-available fallback. The forms come from the
    /// registry's `args_hint` (slash-1 single truth).
    fn copy_forms() -> String {
        let copy = crate::slash::registry()
            .iter()
            .find(|s| s.name == "copy")
            .expect("copy is in the registry");
        format!("/copy {}（兜底存文件）", copy.args_hint)
    }

    /// The overlay body (inside the frame), 23 lines. Crate-local so
    /// unit tests can assert content and styles without a terminal.
    pub(crate) fn overlay_lines(theme: &Theme) -> Vec<Line<'static>> {
        let set = theme.icons.set();
        let h = set.horizontal;
        let mut lines = Vec::new();
        for (header, rows) in Self::sections() {
            lines.push(Line::styled(format!("{h}{h} {header} {h}{h}"), theme.muted));
            for (key, desc) in rows {
                // Key/desc copy carries symbol glyphs (`↑↓`, `·`) —
                // chrome, localize for the tier (identity outside
                // ascii).
                let key = localize(key, &set);
                let desc = localize(desc, &set);
                // Display-width-aware padding: `{key:<KEY_COLS$}` pads by
                // CHAR count, which misaligns CJK keys (滚轮 = 2 chars but
                // 4 columns). Pad to KEY_COLS display columns instead
                // (post-localize — the ascii tier swaps ↑↓ for ^v).
                let pad = KEY_COLS.saturating_sub(ratatui::text::Span::raw(key.as_str()).width());
                let key_col = format!("{key}{}", " ".repeat(pad));
                lines.push(Line::from(vec![
                    Span::styled(key_col, theme.header),
                    Span::styled(desc, theme.assistant),
                ]));
            }
        }
        lines.push(Line::styled(
            format!("{h}{h} slash 命令 {h}{h}"),
            theme.muted,
        ));
        // model-mgmt-2: the derived line is width-wrapped to the
        // overlay's content budget (greedy word wrap; the registry
        // string itself stays byte-exact — see the verbatim test).
        for chunk in wrap_words(&Self::slash_commands(), SLASH_LINE_MAX_COLS) {
            lines.push(Line::from(Span::styled(chunk, theme.assistant)));
        }
        lines.push(Line::from(Span::styled(
            Self::copy_forms(),
            theme.assistant,
        )));
        lines
    }
}

impl Component for HelpComponent {
    fn handle(&mut self, _action: &Action, _ctx: &mut AppCtx) -> Option<Action> {
        None
    }

    fn render(&self, f: &mut Frame, area: Rect, ctx: &AppCtx) {
        // The App clears the background and computes the centered area;
        // this component fills it. restyle-1: the rounded panel frame;
        // body + divider + footer live inside the content rect.
        if area.width < 2 || area.height < 2 {
            return; // degenerate overlay — nothing legible to paint
        }
        let theme = &ctx.theme;
        let content = panel::render_frame(
            f,
            area,
            "Help",
            theme.overlay_title,
            theme,
            // panel-meta-1: the close-key hint in the header.
            Some("Esc 关闭"),
        );
        if content.width < 2 || content.height < 2 {
            return;
        }
        // Body: everything except the divider + footer rows.
        let body_height = content.height.saturating_sub(2);
        if body_height > 0 {
            f.render_widget(
                Paragraph::new(Self::overlay_lines(theme)),
                Rect {
                    x: content.x,
                    y: content.y,
                    width: content.width,
                    height: body_height,
                },
            );
        }
        // Divider row, then the footer on the last content row
        // (right-aligned, muted — the close affordance never clips).
        // The divider spans the panel's FULL width (├───┤ joins the
        // outer frame — panel-frame.ts renderPanelDivider).
        let divider_row = Rect {
            x: area.x,
            y: content.y + body_height,
            width: area.width,
            height: 1,
        };
        panel::render_divider(f, divider_row, theme);
        f.render_widget(
            Paragraph::new(Line::styled("Esc / ? 关闭", theme.muted))
                .alignment(ratatui::layout::Alignment::Right),
            Rect {
                x: content.x,
                y: content.y + content.height - 1,
                width: content.width,
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
            "agents 浮层显隐",
            "Ctrl+M /mouse",
            "鼠标捕获",
            "关=终端原生选择",
            "Ctrl+Y",
            "复制最后输出（文件兜底）",
            "Ctrl+L",
            "Ctrl+C",
            "Esc",
            "输入框",
            "Enter",
            "Alt+Enter / Ctrl+J",
            "↑ / ↓",
            "Ctrl+W",
            "转录区滚动与选择",
            "滚轮 ↑↓ PgUp PgDn",
            "g/G/Home/End",
            "拖拽选中文本",
            "松开自动复制",
            "审批激活时",
            "y / n / a",
            "允许 / 拒绝 / 全部允许",
            "slash 命令",
            "/new /status /agents /model /copy /mouse",
            "/provider",
            "/copy [all|tool]（兜底存文件）",
        ] {
            assert!(all.contains(needle), "help must cover {needle}");
        }
    }

    #[test]
    fn slash_lines_derive_from_the_registry_verbatim() {
        // slash-1: the registry is the single truth — the derived
        // lines must equal the historical literals byte-for-byte
        // (model-mgmt-2 appended the model-management command, since
        // renamed to `/provider`; the RENDER wraps this string to the
        // overlay budget, the string itself is exact).
        assert_eq!(
            HelpComponent::slash_commands(),
            "/new /status /agents /model /copy /mouse /provider"
        );
        assert_eq!(
            HelpComponent::copy_forms(),
            "/copy [all|tool]（兜底存文件）"
        );
    }

    #[test]
    fn slash_line_wraps_within_the_overlay_budget() {
        // model-mgmt-2: the model-management command (`/provider`)
        // outgrew the 44-col content budget — the renderer wraps
        // greedily so every line fits, and the pre-provider prefix
        // stays on one line (the keymap needle keeps matching).
        let wrapped = wrap_words(&HelpComponent::slash_commands(), SLASH_LINE_MAX_COLS);
        assert_eq!(
            wrapped,
            vec![
                "/new /status /agents /model /copy /mouse".to_owned(),
                "/provider".to_owned()
            ]
        );
        assert!(wrapped
            .iter()
            .all(|l| l.chars().count() <= SLASH_LINE_MAX_COLS));
    }

    #[test]
    fn body_line_count_matches_the_70pct_overlay() {
        // fix-13 added the Ctrl+Y row (21); copy-1 added the /copy
        // forms line (22); select-1 added the drag-select row (23);
        // model-mgmt-2 wrapped the slash line past the budget (24).
        // 70% of ~34 rows = 23 → frame (2) + divider (1) + footer
        // (1) + 20 body rows fits; the tail clips on shorter
        // terminals (Paragraph never panics).
        let lines = HelpComponent::overlay_lines(&theme());
        assert_eq!(lines.len(), 24, "24 lines: {lines:?}");
    }

    #[test]
    fn rows_fit_an_80_col_terminal_overlay() {
        // 60% of 80 cols = 48 → panel content = 48 − 4 frame = 44 cols.
        let lines = HelpComponent::overlay_lines(&theme());
        for line in &lines {
            let rendered = text(line);
            let width = line.width();
            assert!(width <= 44, "row too wide ({width}): {rendered}");
        }
    }

    #[test]
    fn two_column_styling_keys_bold_text_desc_body() {
        let lines = HelpComponent::overlay_lines(&theme());
        let theme = theme();
        // First data row: key span BOLD text color, description body.
        let first_row = &lines[1];
        assert_eq!(first_row.spans[0].style, theme.header);
        assert_eq!(first_row.spans[1].style, theme.assistant);
        // Section headers are muted (`Line::styled` carries the style on
        // the line itself).
        assert_eq!(lines[0].style, theme.muted);
    }

    #[test]
    fn key_column_padding_aligns_descriptions() {
        let lines = HelpComponent::overlay_lines(&theme());
        // Every key/desc row starts its description at column KEY_COLS.
        for line in lines.iter().skip(1) {
            if line.spans.len() == 2 && line.spans[0].style == theme().header {
                assert_eq!(
                    line.spans[0].width(),
                    KEY_COLS,
                    "key column padded: {}",
                    text(line)
                );
            }
        }
    }

    // ── panel container (restyle-1) ────────────────────────────────────

    /// Render through a TestBackend and hand back the screen rows.
    fn draw_rows(width: u16, height: u16, paint: impl FnOnce(&mut Frame)) -> Vec<String> {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                .expect("test terminal");
        terminal
            .draw(|f| paint(f))
            .expect("painting must not panic");
        terminal
            .backend()
            .to_string()
            .lines()
            .map(|l| l.split('"').nth(1).unwrap_or("").to_owned())
            .collect()
    }

    /// The title row is the rounded panel header `╭─ Help ─...─╮` —
    /// line-colored strokes with the signal+BOLD title; no REVERSED
    /// cell anywhere in the overlay.
    #[test]
    fn title_is_the_rounded_panel_header() {
        let buf = {
            let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 25))
                .expect("test terminal");
            terminal
                .draw(|f| HelpComponent::new().render(f, f.area(), &test_ctx()))
                .expect("painting must not panic");
            terminal.backend().buffer().clone()
        };
        let mut title_rows = 0;
        for y in 0..buf.area.height {
            let cells: Vec<_> = (0..buf.area.width)
                .map(|x| buf.cell((x, y)).expect("cell in bounds"))
                .collect();
            if !cells.iter().any(|c| c.symbol() == "╭") {
                continue;
            }
            title_rows += 1;
            assert_eq!(title_rows, 1, "one frame header row: row {y}");
            // Signal title on the line-color frame.
            let title = cells
                .iter()
                .find(|c| c.symbol() == "H")
                .expect("title text");
            assert_eq!(
                title.fg,
                ratatui::style::Color::Rgb(0x67, 0xE8, 0xF9),
                "signal title"
            );
            assert!(title.modifier.contains(ratatui::style::Modifier::BOLD));
            let corner = cells.first().expect("corner");
            assert_eq!(
                corner.fg,
                ratatui::style::Color::Rgb(0x66, 0x66, 0x66),
                "line-color stroke"
            );
            // panel-meta-1: the close-key meta rides the header's
            // right slot — the muted-fg run of the row is exactly the
            // meta text (slot-wise compare: buffer cells carry Reset
            // defaults, hidden cells under wide glyphs stay Reset).
            let muted: String = cells
                .iter()
                .filter(|c| c.style().fg == theme().muted.fg)
                .map(|c| c.symbol())
                .collect();
            assert_eq!(muted, "Esc 关闭", "header meta right slot");
        }
        assert_eq!(title_rows, 1, "the frame header rendered");
        // REVERSED is retired.
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                assert!(
                    !buf.cell((x, y))
                        .expect("cell")
                        .modifier
                        .contains(ratatui::style::Modifier::REVERSED),
                    "REVERSED retired at ({x},{y})"
                );
            }
        }
    }

    /// Footer: right-aligned on the last content row (above the bottom
    /// border) with a 1-column margin to the frame; muted style. The
    /// divider row separates body from footer.
    #[test]
    fn footer_is_right_aligned_muted_above_the_bottom_border() {
        let rows = draw_rows(50, 12, |f| {
            HelpComponent::new().render(f, f.area(), &test_ctx());
        });
        let last = rows.len() - 1;
        assert!(rows[last].starts_with('╰'), "bottom border: {}", rows[last]);
        let footer_row = &rows[last - 1];
        assert!(
            footer_row.contains("Esc / ? 关闭"),
            "footer above the border: {footer_row:?}"
        );
        assert!(
            footer_row.trim_end().ends_with("闭 │"),
            "footer right-aligned inside the frame: {footer_row:?}"
        );
        // The divider row spans the full panel width (joins the frame).
        assert!(
            rows[last - 2].starts_with('├') && rows[last - 2].ends_with('┤'),
            "divider above the footer: {}",
            rows[last - 2]
        );
    }
}
