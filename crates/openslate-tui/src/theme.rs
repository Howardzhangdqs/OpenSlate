//! Semantic theme — the design brief v1 color palette, frozen.
//!
//! Principles (locked in the brief):
//! * body text is ALWAYS the default foreground (readable on light and
//!   dark terminals — no `White`/`Black` hard-coded backgrounds);
//! * color is reserved for state / icons / labels;
//! * terminal 256-color only, no 24-bit truecolor;
//! * the status bar uses `Modifier::REVERSED` instead of painting a
//!   background color.

use ratatui::style::{Color, Modifier, Style};

/// Semantic style constants. `Copy` so it can be embedded in
/// [`crate::components::AppCtx`] cheaply.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    /// User `┃` bar on user messages — Cyan + BOLD, body stays default.
    pub user_label: Style,
    /// Assistant message body — default foreground, no background.
    pub assistant: Style,
    /// Reasoning/thinking text — DarkGray + DIM, `┆` prefix.
    pub reasoning: Style,
    /// Tool running — Yellow (`` + spinner).
    pub tool_running: Style,
    /// Tool completed — Green (`` + duration).
    pub tool_success: Style,
    /// Tool failed — Red (`` + error summary).
    pub tool_failure: Style,
    /// Delegation marker ` agent` — Magenta.
    pub delegate: Style,
    /// Agent-tree active node — Cyan + BOLD `` (root anchor).
    pub agent_active: Style,
    /// Agent-tree running depth-1 child — Cyan `` (plain; BOLD `` is
    /// reserved for the root anchor). Added in P3 lane-b.
    pub agent_running: Style,
    /// Agent-tree finished node — DIM ``.
    pub agent_done: Style,
    /// Agent-tree failed node — Red `` (brief: "完成转 ``，失败 ``").
    /// Added in P3 lane-b (distinct from `error` so panel semantics stay
    /// independent of the status bar's).
    pub agent_failed: Style,
    /// Fine print (session panel's model-id detail row, `…` unknowable
    /// placeholders in the agents tree) — DarkGray + DIM. Added in P3
    /// lane-b.
    pub fine: Style,
    /// Borders and block titles — DarkGray.
    pub border: Style,
    /// Status bar — REVERSED (never a painted background).
    pub status_bar: Style,
    /// Approval banner — Yellow + BOLD.
    pub approval: Style,
    /// Error state text — Red.
    pub error: Style,
    /// Muted/hint text (key hints, empty states) — DarkGray.
    pub muted: Style,
    /// ── Borderless redesign (Wave 1) additions ─────────────────────────
    /// Region separator language: the main/sidebar vertical `┃` divider
    /// column AND the input area's unfocused `┃` bar — DarkGray.
    pub bar_divider: Style,
    /// Panel headers (`agents` / `session` sidebar titles) — BOLD on the
    /// default foreground. Consumed by lane-b (Wave 2).
    pub header: Style,
    /// Panel header while the sidebar holds focus — Cyan + BOLD (the
    /// borderless focus expression). Consumed by lane-b (Wave 2).
    pub header_focused: Style,
    /// Overlay title bars (reversed whole-row ` 帮助 ` etc.) — REVERSED.
    /// Consumed by lane-c (Wave 2).
    pub overlay_title: Style,
    /// Turn-end marker ` 模型别名 · Ns` — Cyan. Consumed by lane-a
    /// (Wave 2).
    pub turn_marker: Style,
    /// ── Markdown rendering batch (additive) ───────────────────────────
    /// Markdown headings `#`~`###` in assistant output — accent (Cyan) +
    /// BOLD. See [`crate::md`].
    pub md_heading: Style,
    /// Markdown inline/fenced code in assistant output — a distinct
    /// color (Yellow). See [`crate::md`].
    pub md_code: Style,
}

impl Theme {
    /// Build the frozen palette.
    pub fn new() -> Self {
        Self {
            user_label: Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            assistant: Style::new(),
            reasoning: Style::new().fg(Color::DarkGray).add_modifier(Modifier::DIM),
            tool_running: Style::new().fg(Color::Yellow),
            tool_success: Style::new().fg(Color::Green),
            tool_failure: Style::new().fg(Color::Red),
            delegate: Style::new().fg(Color::Magenta),
            agent_active: Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            agent_running: Style::new().fg(Color::Cyan),
            agent_done: Style::new().fg(Color::DarkGray).add_modifier(Modifier::DIM),
            agent_failed: Style::new().fg(Color::Red),
            fine: Style::new().fg(Color::DarkGray).add_modifier(Modifier::DIM),
            border: Style::new().fg(Color::DarkGray),
            status_bar: Style::new().add_modifier(Modifier::REVERSED),
            approval: Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            error: Style::new().fg(Color::Red),
            muted: Style::new().fg(Color::DarkGray),
            // Borderless redesign (Wave 1) palette.
            bar_divider: Style::new().fg(Color::DarkGray),
            header: Style::new().add_modifier(Modifier::BOLD),
            header_focused: Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            overlay_title: Style::new().add_modifier(Modifier::REVERSED),
            turn_marker: Style::new().fg(Color::Cyan),
            // Markdown rendering batch palette.
            md_heading: Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            md_code: Style::new().fg(Color::Yellow),
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The design-brief v1 palette, constant by constant. Colors are the
    /// contract; the DIM/BOLD/REVERSED modifiers are checked where the
    /// brief names them.
    #[test]
    fn palette_matches_design_brief() {
        let t = Theme::new();
        assert_eq!(t.user_label.fg, Some(Color::Cyan));
        assert!(t.user_label.add_modifier.contains(Modifier::BOLD));
        assert_eq!(t.assistant.fg, None);
        assert_eq!(t.reasoning.fg, Some(Color::DarkGray));
        assert!(t.reasoning.add_modifier.contains(Modifier::DIM));
        assert_eq!(t.tool_running.fg, Some(Color::Yellow));
        assert_eq!(t.tool_success.fg, Some(Color::Green));
        assert_eq!(t.tool_failure.fg, Some(Color::Red));
        assert_eq!(t.delegate.fg, Some(Color::Magenta));
        assert_eq!(t.agent_active.fg, Some(Color::Cyan));
        assert!(t.agent_active.add_modifier.contains(Modifier::BOLD));
        assert_eq!(t.agent_running.fg, Some(Color::Cyan));
        assert!(t.agent_running.add_modifier.is_empty());
        assert_eq!(t.agent_done.fg, Some(Color::DarkGray));
        assert!(t.agent_done.add_modifier.contains(Modifier::DIM));
        assert_eq!(t.agent_failed.fg, Some(Color::Red));
        assert_eq!(t.fine.fg, Some(Color::DarkGray));
        assert!(t.fine.add_modifier.contains(Modifier::DIM));
        assert_eq!(t.border.fg, Some(Color::DarkGray));
        assert!(t.status_bar.add_modifier.contains(Modifier::REVERSED));
        assert_eq!(t.approval.fg, Some(Color::Yellow));
        assert!(t.approval.add_modifier.contains(Modifier::BOLD));
        assert_eq!(t.error.fg, Some(Color::Red));
        assert_eq!(t.muted.fg, Some(Color::DarkGray));
    }

    /// Borderless-redesign (Wave 1) additions, constant by constant.
    #[test]
    fn palette_borderless_additions() {
        let t = Theme::new();
        // Divider/bar language: DarkGray, no modifiers.
        assert_eq!(t.bar_divider.fg, Some(Color::DarkGray));
        assert!(t.bar_divider.add_modifier.is_empty());
        // Panel headers: BOLD on the DEFAULT foreground (no fg painted —
        // light/dark terminal compatibility).
        assert_eq!(t.header.fg, None);
        assert!(t.header.add_modifier.contains(Modifier::BOLD));
        // Focused header: Cyan + BOLD.
        assert_eq!(t.header_focused.fg, Some(Color::Cyan));
        assert!(t.header_focused.add_modifier.contains(Modifier::BOLD));
        // Overlay title bar: REVERSED, no painted fg/bg.
        assert_eq!(t.overlay_title.fg, None);
        assert!(t.overlay_title.add_modifier.contains(Modifier::REVERSED));
        // Turn marker: Cyan, plain.
        assert_eq!(t.turn_marker.fg, Some(Color::Cyan));
        assert!(t.turn_marker.add_modifier.is_empty());
    }

    /// Markdown-batch additions: heading accent (Cyan + BOLD) and the
    /// distinct code color (Yellow).
    #[test]
    fn palette_markdown_additions() {
        let t = Theme::new();
        assert_eq!(t.md_heading.fg, Some(Color::Cyan));
        assert!(t.md_heading.add_modifier.contains(Modifier::BOLD));
        assert_eq!(t.md_code.fg, Some(Color::Yellow));
        assert!(t.md_code.add_modifier.is_empty());
    }
}
