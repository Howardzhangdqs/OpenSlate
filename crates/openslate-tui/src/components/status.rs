//! Status bar — run-state machine + spinner + model/tokens/cost/elapsed
//! + key hints (P2b-complete).
//!
//! The state machine lives here as a pure function ([`transition`]) over
//! [`crate::event::TuiEvent`]s; the App applies it to its `run_state` (the
//! single source of truth, snapshotted into
//! [`RunInfo`](crate::components::RunInfo)) and renders through this
//! component. Visuals per the design brief — state icons are Nerd Font
//! PUA glyphs (nf-* set, width always exactly 1 column, no emoji
//! presentation variants that would punch holes in the REVERSED bar):
//!
//! | state     | left segment      | animation                        |
//! |-----------|-------------------|----------------------------------|
//! | idle      | ` idle`           | static                           |
//! | thinking  | `◐ thinking`      | braille frames, 1 frame / tick   |
//! | tool      | ` name...`          | braille frames + tool name       |
//! | delegated | ` agent`          | active child agent name          |
//! | approval  | ` approve?`       | 2-frame yellow blink             |
//! | error     | ` {摘要≤36字}`   | static red                       |

use std::time::Duration;

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::{AppCtx, Component};
use crate::action::Action;
use crate::event::TuiEvent;

/// Braille spinner frame sequence (10 frames ≈ 1 s at the 100 ms running
/// tick). Frozen visual vocabulary.
pub const SPINNER_FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// The run-state machine positions. Owned by the App, displayed here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunState {
    /// No turn running.
    Idle,
    /// A model request is streaming or pending.
    Thinking,
    /// A tool is executing.
    ToolRunning { name: String },
    /// A `call_agent` delegation is in flight (live depth-1 view; the
    /// child itself is invisible until `TurnDone` — see spec D2).
    Delegated { agent: String },
    /// A tool approval is blocking the engine.
    ApprovalPending,
    /// Auto-compact is running between turns (set by the App around the
    /// blocking summary call — a real provider may take up to the full
    /// `timeout_ms`). Falls back to `Thinking`/`Idle` when it completes.
    Compacting,
    /// The last turn ended in an error.
    Error(String),
}

impl RunState {
    /// Whether the state animates (spinner / blink) — drives both the
    /// App's tick cadence switch and the frame advance.
    pub fn is_animated(&self) -> bool {
        matches!(
            self,
            RunState::Thinking
                | RunState::ToolRunning { .. }
                | RunState::Delegated { .. }
                | RunState::Compacting
        ) || matches!(self, RunState::ApprovalPending)
    }

    /// True while a turn is executing (any of the running-ish states).
    /// `Compacting` is deliberately NOT "running": no engine task exists
    /// yet (the turn spawns right after the summary call).
    pub fn is_running(&self) -> bool {
        matches!(
            self,
            RunState::Thinking
                | RunState::ToolRunning { .. }
                | RunState::Delegated { .. }
                | RunState::ApprovalPending
        )
    }
}

/// Pure state-machine step: fold one engine event into the run state.
///
/// Note `ApprovalRespond` is not a `TuiEvent` — when the App sends the
/// answer it sets the state back to `Thinking` itself (the engine's next
/// event corrects it to the truth).
pub fn transition(state: &mut RunState, event: &TuiEvent) {
    match event {
        TuiEvent::RequestStart { .. } => *state = RunState::Thinking,
        TuiEvent::FirstToken | TuiEvent::Delta(_) | TuiEvent::Reasoning(_) => {
            // Deltas arrive only in the model phase.
            if !matches!(*state, RunState::Delegated { .. }) {
                *state = RunState::Thinking;
            }
        }
        TuiEvent::Usage(_) | TuiEvent::RequestEnd | TuiEvent::StepEnd => {
            // No state change: request end → maybe more steps.
        }
        TuiEvent::ToolStart { name, args } => {
            if name == "call_agent" {
                *state = RunState::Delegated {
                    agent: delegate_target(args),
                };
            } else {
                *state = RunState::ToolRunning { name: name.clone() };
            }
        }
        TuiEvent::ToolEnd { .. } => {
            // Tool finished → the model is about to be called again.
            *state = RunState::Thinking;
        }
        TuiEvent::ApprovalRequested { .. } => *state = RunState::ApprovalPending,
        TuiEvent::TurnDone(Ok((summary, _manager))) => {
            *state = if summary.status == openslate_core::types::RunStatus::Failed {
                RunState::Error("run failed".to_owned())
            } else {
                RunState::Idle
            };
        }
        TuiEvent::TurnDone(Err((msg, _))) => {
            *state = RunState::Error(msg.clone());
        }
    }
}

/// Best-effort extraction of the delegated child agent id from a
/// `call_agent` args display string (JSON). Falls back to `call_agent`.
/// Shared with the transcript's delegate marker.
pub(crate) fn delegate_target(args: &str) -> String {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(args) {
        for key in ["agent_id", "agent", "child"] {
            if let Some(id) = value.get(key).and_then(|v| v.as_str()) {
                return id.to_owned();
            }
        }
    }
    "call_agent".to_owned()
}

/// The one-row status bar. Render-only: state comes in via
/// [`AppCtx::run`].
#[derive(Debug, Default)]
pub struct StatusComponent;

impl StatusComponent {
    pub fn new() -> Self {
        Self
    }
}

/// Format a duration as compact `1m04s` / `12s` / `800ms`.
fn format_elapsed(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else if secs >= 1 {
        format!("{secs}s")
    } else {
        format!("{}ms", d.as_millis())
    }
}

/// Compact token/cost cell: `1.2k/0` + `$0.0012` (or `-` when zero).
fn format_usage(tokens_in: u64, tokens_out: u64, cost: f64) -> String {
    fn tok(n: u64) -> String {
        if n >= 1000 {
            format!("{:.1}k", n as f64 / 1000.0)
        } else {
            n.to_string()
        }
    }
    let cost = if cost > 0.0 {
        format!("${cost:.4}")
    } else {
        "-".to_owned()
    };
    format!("{}/{} {}", tok(tokens_in), tok(tokens_out), cost)
}

/// Left status segment: state glyph + label (per the brief's table).
/// Icons are Nerd Font PUA glyphs — width 1, no emoji variants.
fn state_segment(state: &RunState, frame: usize, approval_blink_on: bool) -> String {
    let spinner = SPINNER_FRAMES[frame % SPINNER_FRAMES.len()];
    match state {
        RunState::Idle => " idle".to_owned(),
        RunState::Thinking => format!("{spinner} thinking"),
        RunState::ToolRunning { name } => format!("{spinner}  {name}..."),
        RunState::Delegated { agent } => format!(" {agent}"),
        RunState::ApprovalPending => {
            // 2-frame blink: alternate the glyph.
            let glyph = if approval_blink_on { "" } else { " " };
            format!("{glyph} approve?")
        }
        RunState::Compacting => " compacting...".to_owned(),
        RunState::Error(msg) => {
            // Surface the failure reason (e.g. missing api key, provider
            // error) in the status bar itself; full detail goes to the
            // log file. Char-boundary truncation keeps CJK safe.
            let mut summary: String = msg.chars().take(36).collect();
            if msg.chars().count() > 36 {
                summary.push_str("...");
            }
            format!(" {summary}")
        }
    }
}

impl Component for StatusComponent {
    fn handle(&mut self, _action: &Action, _ctx: &mut AppCtx) -> Option<Action> {
        // Purely presentational: state/frame/elapsed arrive via AppCtx.
        None
    }

    fn render(&self, f: &mut Frame, area: Rect, ctx: &AppCtx) {
        let left = state_segment(
            &ctx.run.state,
            ctx.run.spinner_frame,
            ctx.run.spinner_frame.is_multiple_of(2),
        );
        // The state segment is tinted (error red / approval yellow) on top
        // of the bar's REVERSED treatment; everything else stays neutral
        // reversed so no background color is ever hard-coded.
        let left_style = match &ctx.run.state {
            RunState::Error(_) => ctx.theme.error.patch(ctx.theme.status_bar),
            RunState::ApprovalPending => ctx.theme.approval.patch(ctx.theme.status_bar),
            _ => ctx.theme.status_bar,
        };

        let usage = format_usage(ctx.run.tokens_in, ctx.run.tokens_out, ctx.run.cost_usd);
        // Elapsed chip: Nerd clock (U+23F1 ⏱ has an emoji variant that
        // renders 2-wide on some terminals — PUA glyphs never do).
        let elapsed = ctx
            .run
            .elapsed
            .map(|d| format!("  {}", format_elapsed(d)))
            .unwrap_or_default();

        // One reversed line: `state │ model@provider │ usage elapsed │
        // notice (transient, yellow) or key hints`.
        let right_segment = if let Some(notice) = &ctx.notice {
            Span::styled(
                format!("│ {notice} "),
                ctx.theme.tool_running.patch(ctx.theme.status_bar),
            )
        } else {
            // ASCII-only hint text: CJK glyphs in this REVERSED row leave
            // tail cells the frame diff can never repaint (stale black
            // half-cells on some terminal/tmux redraw paths).
            Span::styled(
                "│ ↑↓ history │ ? help │ Ctrl+C cancel",
                ctx.theme.status_bar,
            )
        };
        let line = Line::from(vec![
            Span::styled(format!(" {left} "), left_style),
            Span::styled(format!(" {} ", ctx.run.model_label), ctx.theme.status_bar),
            Span::styled(format!(" {usage}{elapsed} "), ctx.theme.status_bar),
            right_segment,
        ]);
        let paragraph = Paragraph::new(line).style(ctx.theme.status_bar);
        f.render_widget(paragraph, area);
        // Post-pass: re-apply the bar treatment to EVERY cell of the row.
        // Wide glyphs (CJK hints/notices/error text) make ratatui reset
        // the style of the cell following them (skip cells), and the
        // frame diff then repaints those as default-background spaces —
        // the "black gaps in the status bar" bug. Patching after the
        // render keeps the fg tints (red/yellow) and restores REVERSED
        // on the skip cells, so the bar is one continuous color strip.
        f.buffer_mut().set_style(area, ctx.theme.status_bar);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openslate_core::run_manager::RunManager;
    use openslate_core::types::RunStatus;

    #[test]
    fn status_row_keeps_reversed_across_cjk_skip_cells() {
        use ratatui::{backend::TestBackend, Terminal};

        use crate::components::{AppCtx, ConfigSummary, Focus, RunInfo};
        use crate::theme::Theme;

        let ctx = AppCtx {
            theme: Theme::new(),
            focus: Focus::Input,
            run: RunInfo {
                state: RunState::Idle,
                spinner_frame: 0,
                model_label: "main@intern_genai".into(),
                tokens_in: 1200,
                tokens_out: 78,
                cost_usd: 0.0001,
                elapsed: None,
                tool_calls_cur: 0,
                depth_cur: 0,
            },
            config: ConfigSummary {
                model_alias: "main".into(),
                model_id: "intern-latest".into(),
                provider_name: "intern_genai".into(),
                max_depth: 4,
                max_tool_calls: 20,
                run_id: None,
            },
            size: (60, 1),
            notice: None,
        };
        let component = StatusComponent::new();
        let backend = TestBackend::new(60, 1);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| component.render(f, f.area(), &ctx))
            .expect("draw");
        // Every PAINTED cell of the row must keep the REVERSED
        // treatment. The trailing halves of wide glyphs (the CJK hints
        // 历史/帮助/取消) are excluded from the frame diff BY DESIGN —
        // the diff never copies them into the backend buffer, so they
        // stay default cells there, and the terminal draws those
        // columns as part of the wide glyph, with ITS attributes. The
        // black-gaps bug was paintable cells (spaces next to CJK,
        // notice text) losing the modifier when content shifted each
        // frame; identify the excluded cells geometrically (directly
        // after a painted wide glyph) since the skip flag lives in the
        // front buffer, not this one.
        let buf = terminal.backend().buffer();
        let mut x = 0u16;
        while x < 60 {
            let cell = buf.cell((x, 0)).expect("cell");
            if cell.modifier.contains(ratatui::style::Modifier::REVERSED) {
                x += 1;
                continue;
            }
            let prev_is_painted_wide = x > 0
                && buf
                    .cell((x - 1, 0))
                    .map(|p| {
                        ratatui::text::Span::raw(p.symbol()).width() > 1
                            && p.modifier.contains(ratatui::style::Modifier::REVERSED)
                    })
                    .unwrap_or(false);
            assert!(
                prev_is_painted_wide,
                "cell {x} lost the bar treatment (not a wide-glyph tail)"
            );
            x += 1;
        }
    }

    fn dummy_summary(status: RunStatus) -> crate::event::TurnSummary {
        crate::event::TurnSummary {
            run_id: openslate_core::types::RunId("r".into()),
            status,
            messages: vec![],
            total_steps: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cost_usd: 0.0,
            execution_tree: openslate_core::execution::ExecutionTree::new(
                openslate_core::types::RunId("r".into()),
                openslate_core::types::AgentId("root".into()),
            ),
            model: "m".into(),
        }
    }

    #[test]
    fn request_and_stream_events_go_thinking() {
        let mut state = RunState::Idle;
        transition(
            &mut state,
            &TuiEvent::RequestStart {
                step: 1,
                model: "m".into(),
            },
        );
        assert_eq!(state, RunState::Thinking);
        transition(&mut state, &TuiEvent::FirstToken);
        transition(&mut state, &TuiEvent::Delta("hi".into()));
        assert_eq!(state, RunState::Thinking);
    }

    #[test]
    fn tool_events_cycle_running_then_thinking() {
        let mut state = RunState::Thinking;
        transition(
            &mut state,
            &TuiEvent::ToolStart {
                name: "read_file".into(),
                args: "{}".into(),
            },
        );
        assert_eq!(
            state,
            RunState::ToolRunning {
                name: "read_file".into()
            }
        );
        transition(
            &mut state,
            &TuiEvent::ToolEnd {
                name: "read_file".into(),
                bytes: 10,
                truncated: false,
            },
        );
        assert_eq!(state, RunState::Thinking);
    }

    #[test]
    fn call_agent_tool_start_is_delegation() {
        let mut state = RunState::Thinking;
        transition(
            &mut state,
            &TuiEvent::ToolStart {
                name: "call_agent".into(),
                args: r#"{"agent_id":"researcher"}"#.into(),
            },
        );
        assert_eq!(
            state,
            RunState::Delegated {
                agent: "researcher".into()
            }
        );
        // Deltas from the PARENT (after delegation ends) fall back to
        // thinking; delegation itself holds until ToolEnd.
        transition(&mut state, &TuiEvent::Delta("x".into()));
        assert_eq!(
            state,
            RunState::Delegated {
                agent: "researcher".into()
            }
        );
        transition(
            &mut state,
            &TuiEvent::ToolEnd {
                name: "call_agent".into(),
                bytes: 5,
                truncated: false,
            },
        );
        assert_eq!(state, RunState::Thinking);
    }

    #[test]
    fn approval_event_pends() {
        let mut state = RunState::ToolRunning {
            name: "shell".into(),
        };
        transition(
            &mut state,
            &TuiEvent::ApprovalRequested {
                id: 1,
                request: crate::event::ApprovalSummary {
                    tool_name: "shell".into(),
                    arguments: "{}".into(),
                    agent_id: "root".into(),
                    risk_level: "high".into(),
                },
            },
        );
        assert_eq!(state, RunState::ApprovalPending);
        assert!(state.is_animated());
    }

    #[test]
    fn turn_done_returns_to_idle_or_error() {
        let mut state = RunState::Thinking;
        let summary = dummy_summary(RunStatus::Completed);
        transition(
            &mut state,
            &TuiEvent::TurnDone(Ok((summary, manager_stub()))),
        );
        assert_eq!(state, RunState::Idle);
        assert!(!state.is_running());

        let mut state = RunState::Thinking;
        transition(&mut state, &TuiEvent::TurnDone(Err(("boom".into(), None))));
        assert_eq!(state, RunState::Error("boom".into()));
    }

    #[test]
    fn turn_done_failed_status_is_error() {
        let mut state = RunState::Thinking;
        let summary = dummy_summary(RunStatus::Failed);
        transition(
            &mut state,
            &TuiEvent::TurnDone(Ok((summary, manager_stub()))),
        );
        assert!(matches!(state, RunState::Error(_)));
    }

    #[test]
    fn delegate_target_parses_agent_id() {
        assert_eq!(
            delegate_target(r#"{"agent_id":"researcher"}"#),
            "researcher"
        );
        assert_eq!(delegate_target("not json"), "call_agent");
    }

    #[test]
    fn spinner_frames_sequence_is_frozen() {
        assert_eq!(SPINNER_FRAMES.len(), 10);
        assert_eq!(SPINNER_FRAMES[0], '⠋');
        assert_eq!(SPINNER_FRAMES[9], '⠏');
    }

    #[test]
    fn compacting_animates_but_is_not_running() {
        // Compacting shows a live badge but no engine task exists yet —
        // it must not gate /new or the tick cadence as "running".
        let state = RunState::Compacting;
        assert!(state.is_animated());
        assert!(!state.is_running());
        // The segment text is the review-mandated label.
        assert_eq!(state_segment(&state, 0, true), " compacting...");
    }

    #[test]
    fn usage_and_elapsed_formatting() {
        assert_eq!(format_usage(1200, 0, 0.0012), "1.2k/0 $0.0012");
        assert_eq!(format_usage(12, 3, 0.0), "12/3 -");
        assert_eq!(format_elapsed(Duration::from_millis(800)), "800ms");
        assert_eq!(format_elapsed(Duration::from_secs(12)), "12s");
        assert_eq!(format_elapsed(Duration::from_secs(64)), "1m04s");
    }

    // A minimal real RunManager (config parse + empty tree) as the opaque
    // TurnDone payload — constructing it inline keeps the test hermetic.
    fn manager_stub() -> RunManager {
        let toml = r#"
[providers.p]
base_url = "http://localhost"
api_key_env = "K"

[models.main]
provider = "p"
model = "m"
"#;
        let config = openslate_core::config::parse_openslate_toml(toml).expect("parse");
        let agents = vec![openslate_core::types::AgentConfig {
            id: openslate_core::types::AgentId("root".into()),
            name: "Root".into(),
            model: "main".into(),
            children: vec![],
            tools: vec![],
            default_prompt: "p".into(),
        }];
        let tree = openslate_core::agent_tree::AgentTree::from_configs(&agents).expect("tree");
        RunManager::new(
            config,
            tree,
            openslate_core::tool::ToolRegistry::new(),
            openslate_core::skills::SkillsCatalog::default(),
        )
    }
}
