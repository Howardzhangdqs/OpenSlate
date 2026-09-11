//! Approval banner — pending tool-approval UI (P3 lane-c: full banner).
//!
//! The App owns the modal precedence (ApprovalActive preempts everything;
//! y/n/a are intercepted at the dispatcher). This component holds the
//! queue of blocked requests (serial display: front of queue first) and
//! renders the yellow banner over the bottom of the transcript area
//! (placement decided in P2b `App::render`). The engine side of the
//! handshake lives in [`crate::event::ApprovalBridge`].
//!
//! Visual contract (design brief + lane-c spec, borderless Wave 2):
//!
//! ```text
//! ┃   approve? [researcher] shell({"cmd":"rm -rf /tmp/x"…})
//! ┃  risk high        +2 pending
//! ┃  [y] 允许  [n] 拒绝  [a] 本轮全允   Ctrl+C 拒绝并取消
//! ```
//!
//! * borderless: `Clear` exposes the default background; column 0 is a
//!   full-height `┃` accent line in Yellow+BOLD (approval style); the
//!   content lines sit one blank column in (the input area's
//!   bar+gap language); the hand icon is a Nerd Font PUA glyph
//!   (nf-fa-hand_paper) — width exactly 1 column, no emoji
//!   presentation variant (U+270B ✋ rendered 2-wide on emoji-capable
//!   terminals and misaligned the request line);
//! * the request line is Yellow+BOLD (approval style); the source
//!   agent is Magenta (delegation semantics — child-agent approvals
//!   bubble up to the root layer, so the banner must show WHERE the
//!   request came from);
//! * the args preview is truncated to ≤60 display columns (and further
//!   to whatever the banner width allows), CJK-boundary safe;
//! * queued requests surface as `+N pending` (English status words per
//!   the design brief's copy rules; the key hints stay Chinese).

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

use super::{AppCtx, Component};
use crate::action::Action;
use crate::event::ApprovalSummary;
use crate::theme::Theme;

/// Args-preview cap in the request line (spec: ≤60 display columns).
const ARGS_MAX_COLS: usize = 60;

/// One queued approval request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApproval {
    /// Correlation id (see [`crate::event::TuiEvent::ApprovalRequested`]).
    pub id: u64,
    /// Display summary.
    pub summary: ApprovalSummary,
}

/// Queue of approval requests waiting for an answer.
#[derive(Debug, Default)]
pub struct ApprovalComponent {
    queue: Vec<PendingApproval>,
}

impl ApprovalComponent {
    pub fn new() -> Self {
        Self::default()
    }

    /// Enqueue a newly blocked request.
    pub fn enqueue(&mut self, id: u64, summary: ApprovalSummary) {
        self.queue.push(PendingApproval { id, summary });
    }

    /// The request the banner currently shows (front of the queue).
    pub fn current(&self) -> Option<&PendingApproval> {
        self.queue.first()
    }

    /// Whether the approval modal is active (queue non-empty).
    pub fn has_pending(&self) -> bool {
        !self.queue.is_empty()
    }

    /// Number of queued requests.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Whether no request is queued (`len() == 0`).
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Pop the front request (called after the App delivered the answer).
    pub fn pop_answered(&mut self) -> Option<PendingApproval> {
        if self.queue.is_empty() {
            None
        } else {
            Some(self.queue.remove(0))
        }
    }

    /// Drop everything (shutdown / deny-all).
    pub fn clear(&mut self) {
        self.queue.clear();
    }

    /// The banner body (three lines, drawn one blank column right of
    /// the accent bar).
    ///
    /// `inner_width` is the banner area minus the `┃` column and the
    /// gap column; the args preview shrinks below [`ARGS_MAX_COLS`]
    /// when the banner is narrower than the request line. Crate-local
    /// so unit tests can assert content and styles without a terminal.
    pub(crate) fn banner_lines(&self, inner_width: usize, theme: &Theme) -> Vec<Line<'static>> {
        let mut lines = Vec::with_capacity(3);
        let Some(pending) = self.current() else {
            return lines;
        };
        let s = &pending.summary;

        // Line 1 — ` approve? [agent] tool(args≤60)` (Nerd hand icon).
        let head = format!(" approve? [{}] {}(", s.agent_id, s.tool_name);
        let budget = ARGS_MAX_COLS.min(inner_width.saturating_sub(display_cols(&head) + 1));
        let args = truncate_cols(&s.arguments, budget);
        lines.push(Line::from(vec![
            Span::styled(" approve? ", theme.approval),
            Span::styled(format!("[{}]", s.agent_id), theme.delegate),
            Span::raw(" "),
            Span::styled(format!("{}(", s.tool_name), theme.approval),
            Span::raw(format!("{args})")),
        ]));

        // Line 2 — risk + queue depth (`+N pending` only when queued).
        let mut meta = vec![Span::styled(format!("risk {}", s.risk_level), theme.muted)];
        let queued = self.queue.len().saturating_sub(1);
        if queued > 0 {
            meta.push(Span::raw("   "));
            meta.push(Span::styled(format!("+{queued} pending"), theme.approval));
        }
        lines.push(Line::from(meta));

        // Line 3 — key hints (Chinese per the design brief's copy rules).
        lines.push(Line::from(vec![
            Span::styled("[y] ", theme.approval),
            Span::raw("允许   "),
            Span::styled("[n] ", theme.approval),
            Span::raw("拒绝   "),
            Span::styled("[a] ", theme.approval),
            Span::raw("本轮全允"),
            Span::styled("   Ctrl+C ", theme.muted),
            Span::raw("拒绝并取消"),
        ]));

        lines
    }
}

/// Display width of `s` in terminal columns (CJK-aware through ratatui's
/// unicode-width handling — no direct dependency needed).
fn display_cols(s: &str) -> usize {
    Span::from(s).width()
}

/// Truncate `s` to at most `max_cols` display columns, appending `…`
/// when cut. Walks whole chars, so multi-byte boundaries are safe.
fn truncate_cols(s: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    if display_cols(s) <= max_cols {
        return s.to_owned();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        // Reserve one column for the ellipsis once we must cut.
        if used + display_cols(&ch.to_string()) > max_cols - 1 {
            break;
        }
        out.push(ch);
        used += display_cols(&ch.to_string());
    }
    out.push('…');
    out
}

impl Component for ApprovalComponent {
    fn handle(&mut self, _action: &Action, _ctx: &mut AppCtx) -> Option<Action> {
        // y/n/a input is intercepted by the App's modal layer before
        // components ever see it.
        None
    }

    fn render(&self, f: &mut Frame, area: Rect, ctx: &AppCtx) {
        if !self.has_pending() || area.width < 2 || area.height == 0 {
            return;
        }
        // Borderless banner (Wave 2 lane-c): Clear wipes whatever the
        // transcript painted underneath; a full-height `┃` accent
        // column in approval Yellow claims column 0; the three content
        // lines sit one blank column in.
        f.render_widget(Clear, area);
        let bar = vec![Line::from("┃"); area.height as usize];
        f.render_widget(
            Paragraph::new(bar).style(ctx.theme.approval),
            Rect {
                x: area.x,
                y: area.y,
                width: 1,
                height: area.height,
            },
        );
        let body = Rect {
            x: area.x + 2,
            y: area.y,
            width: area.width - 2,
            height: area.height,
        };
        f.render_widget(
            Paragraph::new(self.banner_lines(body.width as usize, &ctx.theme)),
            body,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(tool: &str, args: &str) -> ApprovalSummary {
        ApprovalSummary {
            tool_name: tool.into(),
            arguments: args.into(),
            agent_id: "root".into(),
            risk_level: "high".into(),
        }
    }

    fn theme() -> Theme {
        Theme::new()
    }

    /// Concatenate a line's spans (content-level assertions).
    fn text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    }

    #[test]
    fn queue_is_fifo() {
        let mut approval = ApprovalComponent::new();
        assert!(!approval.has_pending());
        approval.enqueue(1, summary("shell", "{}"));
        approval.enqueue(2, summary("write_file", "{}"));
        assert_eq!(approval.len(), 2);
        assert_eq!(approval.current().unwrap().id, 1);
        let answered = approval.pop_answered();
        assert_eq!(answered.unwrap().summary.tool_name, "shell");
        assert_eq!(approval.current().unwrap().id, 2);
        approval.clear();
        assert!(!approval.has_pending());
    }

    #[test]
    fn empty_queue_renders_nothing() {
        let approval = ApprovalComponent::new();
        assert!(approval.banner_lines(78, &theme()).is_empty());
    }

    #[test]
    fn request_line_shows_agent_tool_and_args() {
        let mut approval = ApprovalComponent::new();
        approval.enqueue(7, summary("shell", r#"{"cmd":"ls -la"}"#));
        let lines = approval.banner_lines(78, &theme());
        assert_eq!(lines.len(), 3);
        let request = text(&lines[0]);
        assert!(
            request.contains(" approve? [root] shell("),
            "request line: {request}"
        );
        assert!(request.contains(r#""cmd":"ls -la""#));
        assert!(request.ends_with(')'), "args closed: {request}");
    }

    #[test]
    fn args_truncated_to_60_display_cols() {
        let mut approval = ApprovalComponent::new();
        approval.enqueue(1, summary("shell", &"x".repeat(200)));
        let lines = approval.banner_lines(200, &theme()); // wide: cap is ARGS_MAX_COLS
        let request = text(&lines[0]);
        let xs = request.matches('x').count();
        assert!(xs <= ARGS_MAX_COLS, "args ≤60 cols, got {xs}");
        assert!(request.ends_with("…)"), "cut marked with ellipsis");
    }

    #[test]
    fn args_truncate_cjk_boundary_safe() {
        let mut approval = ApprovalComponent::new();
        approval.enqueue(1, summary("shell", &"世".repeat(100)));
        let lines = approval.banner_lines(200, &theme());
        let request = text(&lines[0]);
        assert!(request.ends_with("…)"));
        let worlds = request.matches('世').count();
        // each 世 is 2 cols + 1 col ellipsis ⇒ ≤ 60 total.
        assert!(worlds * 2 < ARGS_MAX_COLS, "worlds={worlds}");
    }

    #[test]
    fn args_shrink_further_on_narrow_banner() {
        let mut approval = ApprovalComponent::new();
        approval.enqueue(1, summary("shell", &"x".repeat(200)));
        // Head " approve? [root] shell(" = 24 cols (icon is 1 col now)
        // → only ~5 left.
        let lines = approval.banner_lines(30, &theme());
        let request = text(&lines[0]);
        assert!(
            display_cols(&request) <= 30,
            "request fits the narrow banner: {request}"
        );
        assert!(request.ends_with("…)"));
    }

    #[test]
    fn pending_suffix_counts_only_extra_requests() {
        let mut approval = ApprovalComponent::new();
        approval.enqueue(1, summary("shell", "{}"));
        let single = text(&approval.banner_lines(78, &theme())[1]);
        assert_eq!(single, "risk high");

        approval.enqueue(2, summary("write_file", "{}"));
        approval.enqueue(3, summary("run_code", "{}"));
        let queued = text(&approval.banner_lines(78, &theme())[1]);
        assert!(queued.contains("+2 pending"), "meta line: {queued}");
    }

    #[test]
    fn hint_line_is_the_spec_copy() {
        let mut approval = ApprovalComponent::new();
        approval.enqueue(1, summary("shell", "{}"));
        let hints = text(&approval.banner_lines(78, &theme())[2]);
        assert!(hints.contains("[y]"));
        assert!(hints.contains("允许"));
        assert!(hints.contains("[n]"));
        assert!(hints.contains("拒绝"));
        assert!(hints.contains("[a]"));
        assert!(hints.contains("本轮全允"));
    }

    #[test]
    fn styles_match_the_brief() {
        let mut approval = ApprovalComponent::new();
        approval.enqueue(1, summary("shell", "{}"));
        let lines = approval.banner_lines(78, &theme());
        let theme = theme();
        // Request base is Yellow+BOLD; the source agent is Magenta.
        assert_eq!(lines[0].spans[0].style, theme.approval);
        assert_eq!(lines[0].spans[1].style, theme.delegate);
        // Args stay default-foreground (body-text principle).
        assert_eq!(lines[0].spans[4].style, ratatui::style::Style::new());
        // Key brackets pop; labels stay default.
        assert_eq!(lines[2].spans[0].style, theme.approval);
        assert_eq!(lines[2].spans[1].style, ratatui::style::Style::new());
    }

    #[test]
    fn truncate_cols_edge_cases() {
        assert_eq!(truncate_cols("abc", 0), "");
        assert_eq!(truncate_cols("abc", 5), "abc");
        assert_eq!(truncate_cols("abcde", 4), "abc…");
        assert_eq!(truncate_cols("ab", 1), "…");
        // A wide char that would overflow the reserved ellipsis column.
        assert_eq!(truncate_cols("a世", 2), "a…");
        assert_eq!(truncate_cols("世", 2), "世");
    }
}
