//! Agents panel — delegation tree (P3 lane-b).
//!
//! Per spec D2 (frozen capability statement): while a turn runs, only the
//! root agent and its depth-1 children are observable at the root —
//! grandchildren run through core's `ChildProgress` and never surface as
//! events. The panel therefore runs a two-phase state machine:
//!
//! | phase           | source                        | rows shown                    |
//! |-----------------|-------------------------------|-------------------------------|
//! | live (turn      | `call_agent` ToolStart/ToolEnd | root + depth-1 children; an   |
//! | running)        | push/complete (App-wired)     | ACTIVE child additionally     |
//! |                 |                               | renders a `…` row for its     |
//! |                 |                               | unknowable grand-children    |
//! | calibrated      | `TurnDone` → [`Self::calibr   | full DFS-ordered tree with    |
//! | (after TurnDone)| ate`] with the execution tree | terminal statuses             |
//!
//! The App seeds only the root id at construction ([`Self::set_root`]) —
//! the static agent-tree topology is NOT handed to this component, so the
//! first calibration doubles as the topology source (P3 lane-b task
//! brief; the frozen `App::new` call site cannot be extended).
//!
//! # Glyphs (restyle-1)
//!
//! | row                  | glyph | style              |
//! |----------------------|-------|--------------------|
//! | root                 | `◆`   | signal + BOLD      |
//! | running depth-1      | `◐`   | accent             |
//! | completed            | `✓`   | muted              |
//! | failed               | `×`   | error              |
//! | interrupted          | `○`   | muted              |
//!
//! Pure Unicode, no Nerd Font PUA. `Interrupted` rows only exist
//! post-calibration: the execution-tree snapshot is final when
//! `TurnDone` fires, so any node still `Running` in it was abandoned
//! mid-flight (the turn was cancelled) — its true outcome is
//! unknowable and it renders as `○`.

use std::collections::HashMap;

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::{AppCtx, Component, Focus};
use crate::action::Action;
use openslate_core::execution::{ExecutionNode, ExecutionStatus, ExecutionTree};
use openslate_core::types::ExecutionNodeId;

/// Display status of one tree row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    /// A depth-1 child whose `call_agent` call is in flight (`◐`).
    Running,
    /// Finished successfully (`✓`).
    Completed,
    /// Finished with an error (`×`).
    Failed,
    /// Still `Running` in a FINAL tree snapshot — the turn was cancelled
    /// above it; the real outcome is unknowable (`…`).
    Interrupted,
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
    /// Root agent id (seeded from the static agent tree at App start).
    root: Option<String>,
    /// Live depth-1 children of the CURRENT turn: pushed by `call_agent`
    /// ToolStart, optimistically completed by ToolEnd, replaced whole at
    /// the next calibration. Kept (not removed) when a delegation ends so
    /// the panel shows the turn's delegation history until TurnDone.
    live: Vec<AgentRow>,
    /// Calibrated full-tree snapshot from the last `TurnDone` (DFS order,
    /// root first).
    calibrated: Vec<AgentRow>,
}

impl AgentsComponent {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the root row from the static agent tree (App start).
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
    /// pairing can still be transiently wrong — the `TurnDone`
    /// calibration is authoritative for real statuses.
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

    /// Turn finished: replace the view with the full execution-tree
    /// snapshot (depth ≥2 children become visible here). Siblings render
    /// in agent-id order — `all_nodes()` is HashMap-random and nodes
    /// carry no creation timestamp, so a stable sort is the only
    /// deterministic option. Nodes still `Running` map to
    /// [`AgentStatus::Interrupted`] (the snapshot is final post-turn).
    pub fn calibrate(&mut self, tree: &ExecutionTree) {
        let mut by_parent: HashMap<Option<ExecutionNodeId>, Vec<&ExecutionNode>> = HashMap::new();
        for node in tree.all_nodes() {
            by_parent
                .entry(node.parent_execution_id.clone())
                .or_default()
                .push(node);
        }
        for bucket in by_parent.values_mut() {
            bucket.sort_by(|a, b| a.agent_id.0.cmp(&b.agent_id.0));
        }
        let mut rows = Vec::new();
        walk(tree.root(), &by_parent, &mut rows);
        self.calibrated = rows;
        self.live.clear();
    }

    /// Rows of the current view (besides the separately-rendered root):
    /// live children while a delegation happened this turn, otherwise the
    /// last calibrated full tree (which includes its own depth-0 root
    /// row — render skips it to avoid duplicating the anchor row).
    pub fn rows(&self) -> &[AgentRow] {
        if self.live.is_empty() {
            &self.calibrated
        } else {
            &self.live
        }
    }
}

/// Depth-first walk from a node, emitting display rows in tree order.
fn walk(
    node: &ExecutionNode,
    by_parent: &HashMap<Option<ExecutionNodeId>, Vec<&ExecutionNode>>,
    out: &mut Vec<AgentRow>,
) {
    out.push(AgentRow {
        id: node.agent_id.0.clone(),
        depth: node.depth,
        status: match node.status {
            ExecutionStatus::Running => AgentStatus::Interrupted,
            ExecutionStatus::Completed => AgentStatus::Completed,
            ExecutionStatus::Failed => AgentStatus::Failed,
        },
    });
    if let Some(children) = by_parent.get(&Some(node.id.clone())) {
        for child in children {
            walk(child, by_parent, out);
        }
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
                AgentStatus::Failed => (g.cross, ctx.theme.agent_failed),
                AgentStatus::Interrupted => (g.pending, ctx.theme.agent_done),
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
    use openslate_core::types::{AgentId, RunId};

    fn tree() -> ExecutionTree {
        ExecutionTree::new(RunId("r".into()), AgentId("root".into()))
    }

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
    fn calibrate_orders_dfs_and_maps_terminal_statuses() {
        let mut agents = AgentsComponent::new();
        agents.set_root("root");
        let run_id = RunId("r".into());
        let mut tree = tree();
        let root_id = tree.root_id().clone();
        let researcher = tree.create_child(
            run_id.clone(),
            AgentId("researcher".into()),
            root_id.clone(),
            None,
        );
        let verifier = tree.create_child(
            run_id.clone(),
            AgentId("verifier".into()),
            tree.root_id().clone(),
            None,
        );
        let writer = tree.create_child(run_id, AgentId("writer".into()), researcher.clone(), None);
        tree.update_status(&root_id, ExecutionStatus::Completed);
        tree.update_status(&researcher, ExecutionStatus::Completed);
        tree.update_status(&verifier, ExecutionStatus::Failed);
        tree.update_status(&writer, ExecutionStatus::Failed);

        agents.on_delegate_start("researcher"); // live view active pre-calibration
        agents.calibrate(&tree);

        // DFS order: root, then researcher (with writer nested), then
        // verifier — siblings sorted by agent id.
        let got: Vec<(String, u32, AgentStatus)> = agents
            .rows()
            .iter()
            .map(|r| (r.id.clone(), r.depth, r.status))
            .collect();
        assert_eq!(
            got,
            vec![
                ("root".into(), 0, AgentStatus::Completed),
                ("researcher".into(), 1, AgentStatus::Completed),
                ("writer".into(), 2, AgentStatus::Failed),
                ("verifier".into(), 1, AgentStatus::Failed),
            ]
        );
    }

    #[test]
    fn calibrate_maps_running_to_interrupted_and_clears_live() {
        let mut agents = AgentsComponent::new();
        agents.on_delegate_start("researcher");
        assert!(!agents.rows().is_empty());
        let mut tree = tree();
        let run_id = RunId("r".into());
        let root_id = tree.root_id().clone();
        tree.create_child(run_id, AgentId("researcher".into()), root_id, None);
        // Leave everything Running — the snapshot is final post-turn, so
        // these were interrupted.
        agents.calibrate(&tree);
        assert_eq!(agents.rows()[0].status, AgentStatus::Interrupted);
        assert_eq!(agents.rows()[1].status, AgentStatus::Interrupted);
        // Live rows were replaced by the calibrated view (root + child).
        assert_eq!(agents.rows().len(), 2);
    }

    #[test]
    fn calibrate_sibling_order_is_deterministic() {
        // Two executions of the same agent + one sibling: HashMap-random
        // input must yield a stable, sorted output.
        let mut agents = AgentsComponent::new();
        let run_id = RunId("r".into());
        let mut tree = tree();
        let root_id = tree.root_id().clone();
        tree.create_child(
            run_id.clone(),
            AgentId("zeta".into()),
            root_id.clone(),
            None,
        );
        tree.create_child(
            run_id.clone(),
            AgentId("alpha".into()),
            root_id.clone(),
            None,
        );
        tree.create_child(run_id, AgentId("alpha".into()), root_id, None);
        agents.calibrate(&tree);
        let ids: Vec<&str> = agents.rows().iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["root", "alpha", "alpha", "zeta"]);
    }

    #[test]
    fn no_root_renders_empty_state() {
        let agents = AgentsComponent::new();
        assert_eq!(agents.root(), None);
        assert!(agents.rows().is_empty());
    }
}
