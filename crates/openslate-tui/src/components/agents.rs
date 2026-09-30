//! Agents panel — delegation tree (P3 lane-b; web-1 client edition).
//!
//! Per spec D2 (frozen capability statement): while a turn runs, only the
//! root agent and its depth-1 children are observable at the root —
//! grandchildren run through core's `ChildProgress` and never surface as
//! events. The panel shows:
//!
//! | rows shown             | source                                           |
//! |------------------------|--------------------------------------------------|
//! | root + live depth-1    | root seeded at construction ([`Self::set_root`]) |
//! | children (`◐`→`✓`)     | `call_agent` ToolStart/ToolEnd (App-wired)       |
//!
//! web-1: execution trees no longer cross the wire (the server's
//! `TurnDone` carries none), so the old post-turn `calibrate` view is
//! gone — the live delegation history lingers (completed) until the
//! next turn's first request or a snapshot reset, matching the
//! delegate-entry semantics the wire transcript mirrors.
//!
//! # Glyphs (restyle-1)
//!
//! | row                  | glyph | style              |
//! |----------------------|-------|--------------------|
//! | root                 | `◆`   | signal + BOLD      |
//! | running depth-1      | `◐`   | accent             |
//! | completed            | `✓`   | muted              |
//!
//! Pure Unicode, no Nerd Font PUA.

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::{AppCtx, Component, Focus};
use crate::action::Action;

/// Display status of one tree row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    /// A depth-1 child whose `call_agent` call is in flight (`◐`).
    Running,
    /// Finished successfully (`✓`).
    Completed,
}

/// One visible tree row (root or a delegated execution).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    /// Agent id.
    pub id: String,
    /// Nesting depth (0 = root).
    pub depth: u32,
    /// Terminal/live display status.
    pub status: AgentStatus,
}

/// The agents sidebar panel.
#[derive(Debug, Default)]
pub struct AgentsComponent {
    /// Root agent id (seeded from the snapshot's agent tree at App
    /// start; re-seeded on config swaps).
    root: Option<String>,
    /// Live depth-1 children of the CURRENT turn: pushed by `call_agent`
    /// ToolStart, optimistically completed by ToolEnd. Kept (not
    /// removed) when a delegation ends so the panel shows the turn's
    /// delegation history; cleared by the next turn's first request or
    /// a snapshot reset.
    live: Vec<AgentRow>,
}

impl AgentsComponent {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the root row (App start / snapshot config swap).
    pub fn set_root(&mut self, root_id: &str) {
        self.root = Some(root_id.to_owned());
    }

    /// The seeded root agent id, if any.
    pub fn root(&self) -> Option<&str> {
        self.root.as_deref()
    }

    /// A `call_agent` tool started: push a live depth-1 child (`◐`).
    pub fn on_delegate_start(&mut self, agent: &str) {
        self.live.push(AgentRow {
            id: agent.to_owned(),
            depth: 1,
            status: AgentStatus::Running,
        });
    }

    /// A tool ended: complete the oldest still-running live child (`◐`→`✓`).
    ///
    /// The App forwards EVERY tool's end here (frozen call site) and
    /// `ToolEnd` carries no agent id — so non-`call_agent` ends are
    /// ignored. Core executes tool batches concurrently
    /// (`FuturesUnordered`) and drains ends in tool-call order, so the
    /// completion target is the FIRST still-`Running` row (FIFO) to
    /// match the event order; with parallel `call_agent` calls the
    /// pairing can still be transiently wrong.
    pub fn on_delegate_end(&mut self, tool_name: &str) {
        if tool_name != "call_agent" {
            return;
        }
        if let Some(row) = self
            .live
            .iter_mut()
            .find(|r| r.status == AgentStatus::Running)
        {
            row.status = AgentStatus::Completed;
        }
    }

    /// A new turn started (`RequestStart`): the previous turn's live
    /// delegation history gives way to the fresh turn's view.
    pub fn turn_reset(&mut self) {
        self.live.clear();
    }

    /// Rows of the current view (besides the separately-rendered root).
    pub fn rows(&self) -> &[AgentRow] {
        &self.live
    }
}

impl Component for AgentsComponent {
    fn handle(&mut self, _action: &Action, _ctx: &mut AppCtx) -> Option<Action> {
        // Presentational panel: sidebar input is swallowed by the App.
        None
    }

    fn render(&self, f: &mut Frame, area: Rect, ctx: &AppCtx) {
        // Borderless redesign: no block frame — a lowercase BOLD title
        // carries the panel identity; while the sidebar holds focus the
        // title switches to Cyan + BOLD (the focus expression borders
        // used to provide). Content starts one row below the title,
        // indented one column; the two released border columns widen
        // every row to the full area width.
        let header_style = if ctx.focus == Focus::Sidebar {
            ctx.theme.header_focused
        } else {
            ctx.theme.header
        };
        let g = ctx.theme.icons.set();
        let mut lines: Vec<Line> = vec![Line::from(Span::styled("agents", header_style))];
        match &self.root {
            Some(root) => lines.push(Line::from(vec![
                Span::raw(" "),
                Span::styled(format!("{} {root}", g.diamond), ctx.theme.agent_active),
            ])),
            None => lines.push(Line::from(vec![
                Span::raw(" "),
                Span::styled("no agents", ctx.theme.muted),
            ])),
        }
        let rows: Vec<&AgentRow> = self.rows().iter().filter(|r| r.depth > 0).collect();
        for (i, row) in rows.iter().enumerate() {
            // Flat-tree connector: the elbow when the NEXT row is not a
            // descendant (strictly shallower or absent) — i.e. this row
            // closed its parent's child list; the tee otherwise.
            let is_last_sibling = rows.get(i + 1).is_none_or(|next| next.depth < row.depth);
            let connector = if is_last_sibling { g.elbow } else { g.tee };
            let indent = "  ".repeat(row.depth as usize);
            let (glyph, style) = match row.status {
                AgentStatus::Running => (g.agents_running, ctx.theme.agent_running),
                AgentStatus::Completed => (g.check, ctx.theme.agent_done),
            };
            lines.push(Line::from(vec![
                Span::raw(" "),
                Span::styled(format!("{indent}{connector} {glyph} {}", row.id), style),
            ]));
            // D2: a running child may itself have delegated —
            // grandchildren are invisible at the root, so hint at the
            // unknowable tail with one ellipsis row.
            if row.status == AgentStatus::Running {
                let deeper = "  ".repeat(row.depth as usize + 1);
                lines.push(Line::from(vec![
                    Span::raw(" "),
                    Span::styled(
                        format!("{deeper}{} {}", g.elbow, g.ellipsis),
                        ctx.theme.fine,
                    ),
                ]));
            }
        }
        // One blank row after the tree: the App stacks this panel
        // directly above the session panel in the sidebar column, so
        // this trailing blank IS the inter-panel separator (the
        // borderless spacing rule — no frame to divide them).
        lines.push(Line::from(""));
        f.render_widget(Paragraph::new(lines), area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_children_push_and_lifo_complete() {
        let mut agents = AgentsComponent::new();
        agents.set_root("root");
        agents.on_delegate_start("researcher");
        agents.on_delegate_start("verifier");
        assert_eq!(
            agents.rows(),
            [
                AgentRow {
                    id: "researcher".into(),
                    depth: 1,
                    status: AgentStatus::Running
                },
                AgentRow {
                    id: "verifier".into(),
                    depth: 1,
                    status: AgentStatus::Running
                }
            ]
        );
        // Non-delegation tool ends must not complete anything.
        agents.on_delegate_end("read_file");
        assert_eq!(agents.rows()[0].status, AgentStatus::Running);
        // A delegation end completes the OLDEST running row (FIFO — core
        // drains parallel tool batches in tool-call order) and the row
        // lingers as completed.
        agents.on_delegate_end("call_agent");
        assert_eq!(agents.rows()[0].status, AgentStatus::Completed);
        assert_eq!(agents.rows()[1].status, AgentStatus::Running);
        assert_eq!(agents.rows().len(), 2);
        // A second end completes the remaining running row.
        agents.on_delegate_end("call_agent");
        assert_eq!(agents.rows()[1].status, AgentStatus::Completed);
        // Extra ends with nothing running are a no-op.
        agents.on_delegate_end("call_agent");
        assert_eq!(agents.rows().len(), 2);
    }

    #[test]
    fn repeated_delegation_of_the_same_agent_keeps_both_rows() {
        let mut agents = AgentsComponent::new();
        agents.on_delegate_start("researcher");
        agents.on_delegate_end("call_agent");
        agents.on_delegate_start("researcher");
        assert_eq!(agents.rows().len(), 2);
        assert_eq!(agents.rows()[0].status, AgentStatus::Completed);
        assert_eq!(agents.rows()[1].status, AgentStatus::Running);
    }

    #[test]
    fn turn_reset_clears_the_live_history() {
        let mut agents = AgentsComponent::new();
        agents.set_root("root");
        agents.on_delegate_start("researcher");
        agents.on_delegate_end("call_agent");
        assert_eq!(agents.rows().len(), 1);
        agents.turn_reset();
        assert!(agents.rows().is_empty());
        assert_eq!(agents.root(), Some("root"), "root survives the reset");
    }

    #[test]
    fn no_root_renders_empty_state() {
        let agents = AgentsComponent::new();
        assert_eq!(agents.root(), None);
        assert!(agents.rows().is_empty());
    }
}
