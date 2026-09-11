//! Session panel — model/limits/usage summary (P3 lane-b).
//!
//! Right sidebar bottom half, borderless: a lowercase BOLD `session`
//! title (Cyan + BOLD while the sidebar holds focus) with content
//! indented one column below. Shows the effective model
//! (`alias@provider` plus the resolved model id in fine print), live
//! run counters against the configured limits, session-accumulated
//! tokens/cost, turn elapsed time and the backing run id (truncated).
//! Two-column layout per the design brief: labels left-aligned
//! (muted), values right-aligned against the content width (panel
//! width minus the indent — the released border columns widened it).
//!
//! Empty state: before the first turn there is no backing run — the
//! config rows stay and a muted `no active run` hint replaces the
//! counters.
//!
//! All data arrives via [`AppCtx`](super::AppCtx) (render-only); the
//! formatting helpers are pure and unit-tested below.

use std::time::Duration;

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::{AppCtx, Component, Focus};
use crate::action::Action;
use crate::theme::Theme;

/// Display length of the run id (spec: truncated to 8 chars).
pub const RUN_ID_CHARS: usize = 8;

/// The session/config sidebar panel.
#[derive(Debug, Default)]
pub struct SessionComponent;

impl SessionComponent {
    pub fn new() -> Self {
        Self
    }
}

impl Component for SessionComponent {
    fn handle(&mut self, _action: &Action, _ctx: &mut AppCtx) -> Option<Action> {
        // Presentational panel: sidebar input is swallowed by the App.
        None
    }

    fn render(&self, f: &mut Frame, area: Rect, ctx: &AppCtx) {
        // Borderless redesign: no block frame — a lowercase BOLD title
        // carries the panel identity; Cyan + BOLD while the sidebar
        // holds focus. Content sits one row below, indented one column;
        // the two released border columns widen the two-column rows
        // (28 → area.width − 1 usable columns).
        let theme = &ctx.theme;
        let header_style = if ctx.focus == Focus::Sidebar {
            theme.header_focused
        } else {
            theme.header
        };
        let inner = area.width.saturating_sub(1);
        let mut lines = vec![
            Line::from(Span::styled("session", header_style)),
            row(
                "model",
                &format!("{}@{}", ctx.config.model_alias, ctx.config.provider_name),
                inner,
                theme,
                theme.assistant,
            ),
            row("id", &ctx.config.model_id, inner, theme, theme.fine),
        ];
        match &ctx.config.run_id {
            Some(run_id) => {
                lines.push(row(
                    "depth",
                    &format!("{}/{}", ctx.run.depth_cur, ctx.config.max_depth),
                    inner,
                    theme,
                    theme.assistant,
                ));
                lines.push(row(
                    "tools",
                    &format!("{}/{}", ctx.run.tool_calls_cur, ctx.config.max_tool_calls),
                    inner,
                    theme,
                    theme.assistant,
                ));
                lines.push(row(
                    "in/out",
                    &format!(
                        "{}/{}",
                        format_tokens(ctx.run.tokens_in),
                        format_tokens(ctx.run.tokens_out)
                    ),
                    inner,
                    theme,
                    theme.assistant,
                ));
                lines.push(row(
                    "cost",
                    &format!("${:.4}", ctx.run.cost_usd),
                    inner,
                    theme,
                    theme.assistant,
                ));
                lines.push(row(
                    "elapsed",
                    &ctx.run
                        .elapsed
                        .map(format_mmss)
                        .unwrap_or_else(|| "-".to_owned()),
                    inner,
                    theme,
                    theme.assistant,
                ));
                lines.push(row(
                    "run",
                    &truncate_run_id(run_id),
                    inner,
                    theme,
                    theme.assistant,
                ));
            }
            None => lines.push(Line::from(vec![
                Span::raw(" "),
                Span::styled("no active run", theme.muted),
            ])),
        }
        f.render_widget(Paragraph::new(lines), area);
    }
}

/// One two-column row: 1-column content indent, muted label left,
/// styled value right-aligned to `inner` display columns (the panel
/// width minus the indent). Values wider than the remaining space drop
/// the padding and let the panel clip the tail.
fn row(
    label: &str,
    value: &str,
    inner: u16,
    theme: &Theme,
    value_style: ratatui::style::Style,
) -> Line<'static> {
    let label_w = label.chars().count() as u16;
    let value_w = value.chars().count() as u16;
    let pad = inner.saturating_sub(label_w + value_w) as usize;
    Line::from(vec![
        Span::raw(" "),
        Span::styled(label.to_owned(), theme.muted),
        Span::raw(" ".repeat(pad)),
        Span::styled(value.to_owned(), value_style),
    ])
}

/// Elapsed time as `mm:ss` (spec). Minutes grow past 59 rather than
/// switching units — the status bar already owns the compact form.
fn format_mmss(d: Duration) -> String {
    let secs = d.as_secs();
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

/// Compact token count: `1.2k` above a thousand (status-bar parity).
fn format_tokens(n: u64) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// Run id truncated to [`RUN_ID_CHARS`] chars (shorter ids pass through).
fn truncate_run_id(id: &str) -> String {
    id.chars().take(RUN_ID_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mmss_formatting() {
        assert_eq!(format_mmss(Duration::from_secs(0)), "00:00");
        assert_eq!(format_mmss(Duration::from_secs(9)), "00:09");
        assert_eq!(format_mmss(Duration::from_secs(59)), "00:59");
        assert_eq!(format_mmss(Duration::from_secs(64)), "01:04");
        assert_eq!(format_mmss(Duration::from_secs(83)), "01:23");
        assert_eq!(format_mmss(Duration::from_secs(3661)), "61:01");
    }

    #[test]
    fn token_compaction() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(56), "56");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1000), "1.0k");
        assert_eq!(format_tokens(1234), "1.2k");
        assert_eq!(format_tokens(12345), "12.3k");
    }

    #[test]
    fn run_id_truncates_to_eight_chars() {
        assert_eq!(truncate_run_id("ses_f76d4822fffefe"), "ses_f76d");
        assert_eq!(truncate_run_id("abc"), "abc");
        assert_eq!(truncate_run_id(""), "");
    }

    #[test]
    fn row_right_aligns_values() {
        let theme = Theme::new();
        let line = row("cost", "$0.0012", 28, &theme, theme.assistant);
        assert_eq!(line.spans.len(), 4);
        // Leading 1-column content indent (borderless design).
        assert_eq!(line.spans[0].content.to_string(), " ");
        assert_eq!(line.spans[1].content.to_string(), "cost");
        // 28 - 4 (label) - 7 (value) = 17 padding columns.
        assert_eq!(line.spans[2].content.to_string(), " ".repeat(17));
        assert_eq!(line.spans[3].content.to_string(), "$0.0012");
    }

    #[test]
    fn row_padding_collapses_on_overflow() {
        let theme = Theme::new();
        let line = row(
            "model",
            "main@some-very-long-provider",
            12,
            &theme,
            theme.assistant,
        );
        assert_eq!(line.spans[0].content.to_string(), " ");
        assert_eq!(line.spans[2].content.to_string(), "");
    }
}
