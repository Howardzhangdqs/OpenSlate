//! Status line — run-state machine + spinner + segment row
//! (restyle-1: minimax-code chrome).
//!
//! The state machine lives here as a pure function ([`transition`]) over
//! [`crate::event::TuiEvent`]s; the App applies it to its `run_state` (the
//! single source of truth, snapshotted into
//! [`RunInfo`](crate::components::RunInfo)) and renders through this
//! component.
//!
//! Visuals (restyle-1 spec): the REVERSED bar is retired — the line
//! renders PLAIN text spans on the terminal background. Groups join
//! with a dim ` │ `, sub-parts with a dim ` · `:
//!
//! ```text
//! {state} │ {cwd} │ ✦ {model} │ ◐ {n} agents │ ▕████░░▏ N% left
//! ```
//!
//! | state     | segment                          |
//! |-----------|----------------------------------|
//! | idle      | (hidden — no state segment)      |
//! | thinking  | `{spinner} thinking` (orbit)     |
//! | tool      | `{spinner} {name}…` (orbit)      |
//! | delegated | `◐ {agent}` (accent)             |
//! | approval  | `◉ approve?` (warning, blinking) |
//! | compacting| `◌ compacting…` (orbit)          |
//! | error     | `× {摘要≤36字}` (error)           |
//!
//! The spinner frames come from the theme's icon tier
//! (`IconSet::spinner`: the braille 10-frame table on unicode/nerd,
//! `|/-\` on ascii) in the orbit color (#1CCDD2). Every built-in tier
//! renders the spinner for Thinking (icons-5 retired the built-in
//! static glyph); the [`IconSet::thinking`] Option slot remains for
//! `[tui.icons] overrides` to configure a static glyph (rendered
//! verbatim, no frames). The context meter
//! renders only when the App has a remaining-context percentage
//! (thresholds: ≤10% error, ≤25% warning, else signal fill on a
//! line-colored ground).

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::{AppCtx, Component};
use crate::action::Action;
use crate::event::TuiEvent;
use crate::theme::Theme;

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

/// The one-row status line. Render-only: state comes in via
/// [`AppCtx::run`].
#[derive(Debug, Default)]
pub struct StatusComponent;

impl StatusComponent {
    pub fn new() -> Self {
        Self
    }
}

/// Left state group per the restyle-1 icon table (`◌` starting, `◐`
/// agents running, `◉` waiting, `×` error). `None` while Idle — a
/// ready session shows no state segment.
fn state_group(
    state: &RunState,
    frame: usize,
    approval_blink_on: bool,
    theme: &Theme,
) -> Option<Vec<Span<'static>>> {
    let g = theme.icons.set();
    let spinner = g
        .spinner
        .chars()
        .nth(frame % g.spinner.chars().count())
        .unwrap_or(' ');
    match state {
        RunState::Idle => None,
        RunState::Thinking => {
            // icons-5: every built-in tier ships `thinking = None`, so
            // the spinner frames render (the ordinary path — a
            // terminal cannot rotate a glyph and fonts carry no frame
            // pair). The Option SLOT stays for `[tui.icons] overrides`
            // to configure a static glyph, which renders here.
            let glyph = g
                .thinking
                .map(str::to_owned)
                .unwrap_or_else(|| spinner.to_string());
            Some(vec![
                Span::styled(glyph, theme.orbit),
                Span::styled(" thinking".to_owned(), theme.muted),
            ])
        }
        RunState::ToolRunning { name } => Some(vec![
            Span::styled(spinner.to_string(), theme.orbit),
            Span::styled(format!(" {name}{}", g.ellipsis), theme.muted),
        ]),
        RunState::Delegated { agent } => Some(vec![
            Span::styled(format!("{} ", g.agents_running), theme.delegate),
            Span::styled(agent.clone(), theme.muted),
        ]),
        RunState::ApprovalPending => {
            // 2-frame blink: the waiting glyph alternates.
            let glyph = if approval_blink_on {
                g.waiting
            } else {
                g.pending
            };
            Some(vec![
                Span::styled(format!("{glyph} "), theme.approval),
                Span::styled("approve?".to_owned(), theme.approval),
            ])
        }
        RunState::Compacting => Some(vec![
            Span::styled(format!("{} ", g.starting), theme.orbit),
            Span::styled(format!("compacting{}", g.ellipsis), theme.muted),
        ]),
        RunState::Error(msg) => {
            // Surface the failure reason (e.g. missing api key,
            // provider error); full detail goes to the log file.
            // Char-boundary truncation keeps CJK safe.
            let mut summary: String = msg.chars().take(36).collect();
            if msg.chars().count() > 36 {
                summary.push_str(g.ellipsis);
            }
            Some(vec![
                Span::styled(format!("{} ", g.cross), theme.error),
                Span::styled(summary, theme.error),
            ])
        }
    }
}

/// The `✦ model` group (accent).
fn model_group(alias: &str, theme: &Theme) -> Vec<Span<'static>> {
    vec![
        Span::styled(format!("{} ", theme.icons.set().brand), theme.tool_running),
        Span::styled(alias.to_owned(), theme.tool_running),
    ]
}

/// The `◐ N agents` group — delegation live only.
fn agents_group(depth: u32, theme: &Theme) -> Option<Vec<Span<'static>>> {
    (depth > 0).then(|| {
        vec![
            Span::styled(
                format!("{} ", theme.icons.set().agents_running),
                theme.delegate,
            ),
            Span::styled(
                format!("{depth} agent{}", if depth == 1 { "" } else { "s" }),
                theme.delegate,
            ),
        ]
    })
}

/// The context-remaining meter `▕████░░▏ N% left` (data-gated). Fill
/// blocks color by threshold (≤10% error / ≤25% warning / else
/// signal); the ground uses the line color.
fn meter_group(remaining: u8, theme: &Theme) -> Vec<Span<'static>> {
    const CELLS: usize = 8;
    let g = theme.icons.set();
    let tone = if remaining <= 10 {
        theme.error
    } else if remaining <= 25 {
        theme.warning
    } else {
        theme.tool_running
    };
    let filled = ((remaining as usize * CELLS + 50) / 100).min(CELLS);
    vec![
        Span::styled(g.meter_left.to_owned(), tone),
        Span::styled(g.meter_fill.repeat(filled), tone),
        Span::styled(g.meter_ground.repeat(CELLS - filled), theme.line),
        Span::styled(format!("{} {remaining}% left", g.meter_right), tone),
    ]
}

/// The current working directory's base name (`openslate`), muted —
/// the status line's anchor group. `None` when the cwd is unreadable.
pub(crate) fn cwd_dir_name() -> Option<String> {
    let dir = std::env::current_dir().ok()?;
    let base = dir.file_name()?.to_string_lossy().to_string();
    (!base.is_empty()).then_some(base)
}

/// The git branch of the working directory (P1, restyle-1): `⎇ branch`
/// renders only inside a git repo. Reads `.git/HEAD` directly (no
/// process spawn); a `ref: refs/heads/X` yields `X`, anything else
/// (detached) yields the first line trimmed to 12 chars.
pub(crate) fn git_branch() -> Option<String> {
    git_branch_in(&std::env::current_dir().ok()?)
}

/// [`git_branch`] against an explicit directory (test seam).
fn git_branch_in(dir: &std::path::Path) -> Option<String> {
    let head = std::fs::read_to_string(dir.join(".git").join("HEAD")).ok()?;
    let head = head.trim();
    if let Some(short) = head.strip_prefix("ref: refs/heads/") {
        Some(short.to_owned())
    } else {
        let detached: String = head.chars().take(12).collect();
        (!detached.is_empty()).then_some(detached)
    }
}

/// Assemble the full status line spans (groups joined by dim ` │ `).
/// Crate-visible for unit tests.
pub(crate) fn status_spans(ctx: &AppCtx) -> Vec<Span<'static>> {
    let theme = &ctx.theme;
    let mut groups: Vec<Vec<Span<'static>>> = Vec::new();
    if let Some(state) = state_group(
        &ctx.run.state,
        ctx.run.spinner_frame,
        ctx.run.spinner_frame.is_multiple_of(2),
        theme,
    ) {
        groups.push(state);
    }
    if let Some(cwd) = cwd_dir_name() {
        groups.push(vec![Span::styled(cwd, theme.muted)]);
    }
    // P1: git branch `⎇ name` — repo-gated (accent glyph, muted name).
    if let Some(branch) = git_branch() {
        groups.push(vec![
            Span::styled(format!("{} ", theme.icons.set().branch), theme.tool_running),
            Span::styled(branch, theme.muted),
        ]);
    }
    groups.push(model_group(&ctx.config.model_alias, theme));
    if let Some(agents) = agents_group(ctx.run.depth_cur, theme) {
        groups.push(agents);
    }
    if let Some(remaining) = ctx.run.context_remaining {
        groups.push(meter_group(remaining, theme));
    }
    let g = theme.icons.set();
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (i, group) in groups.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(format!(" {} ", g.vertical), theme.line));
        }
        spans.extend(group);
    }
    spans
}

impl Component for StatusComponent {
    fn handle(&mut self, _action: &Action, _ctx: &mut AppCtx) -> Option<Action> {
        // Purely presentational: state/frame/elapsed arrive via AppCtx.
        None
    }

    fn render(&self, f: &mut Frame, area: Rect, ctx: &AppCtx) {
        let line = Line::from(status_spans(ctx));
        f.render_widget(Paragraph::new(line), area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openslate_core::run_manager::RunManager;
    use openslate_core::types::RunStatus;
    use ratatui::style::{Modifier, Style};

    use crate::components::{ConfigSummary, Focus, RunInfo};
    use crate::theme::Theme;

    fn ctx(state: RunState, depth: u32, remaining: Option<u8>) -> AppCtx {
        AppCtx {
            theme: Theme::new(),
            focus: Focus::Input,
            run: RunInfo {
                state,
                spinner_frame: 0,
                model_label: "main@mock".into(),
                tokens_in: 0,
                tokens_out: 0,
                cost_usd: 0.0,
                elapsed: None,
                tool_calls_cur: 0,
                depth_cur: depth,
                context_remaining: remaining,
            },
            config: ConfigSummary {
                model_alias: "main".into(),
                model_id: "mock-model".into(),
                provider_name: "mock".into(),
                max_depth: 4,
                max_tool_calls: 20,
                run_id: None,
                model_aliases: Vec::new(),
            },
            size: (60, 1),
            notice: None,
        }
    }

    fn text(spans: &[Span<'static>]) -> String {
        spans.iter().map(|s| s.content.clone()).collect()
    }

    /// The plain status line: segments joined by dim ` │ `, the model
    /// group `✦ main` in accent — and NO REVERSED anywhere.
    #[test]
    fn status_line_renders_plain_segments() {
        let theme = Theme::new();
        let c = ctx(RunState::Idle, 0, None);
        let spans = status_spans(&c);
        let joined = text(&spans);
        // Expected assembly mirrors the group order: [cwd] [⎇ branch]
        // ✦ main — the environment-derived segments use the same
        // helpers the renderer does.
        let mut expected = String::new();
        if let Some(cwd) = cwd_dir_name() {
            expected.push_str(&cwd);
            expected.push_str(" │ ");
        }
        if let Some(branch) = git_branch() {
            expected.push_str(&format!("⎇ {branch} │ "));
        }
        expected.push_str("✦ main");
        assert_eq!(joined, expected);
        // Separator in the line color; the model glyph/alias accent.
        assert!(spans
            .iter()
            .any(|s| s.content == " │ " && s.style == theme.line));
        assert!(spans
            .iter()
            .any(|s| s.content == "✦ " && s.style == theme.tool_running));
        assert!(spans
            .iter()
            .all(|s| !s.style.add_modifier.contains(Modifier::REVERSED)));
    }

    /// Running state leads with the orbit spinner; the label is muted.
    #[test]
    fn thinking_leads_with_orbit_spinner() {
        let theme = Theme::new();
        let spans = status_spans(&ctx(RunState::Thinking, 0, None));
        assert_eq!(spans[0].content, "⠋");
        assert_eq!(spans[0].style, theme.orbit);
        assert_eq!(spans[1].content, " thinking");
        assert_eq!(spans[1].style, theme.muted);
    }

    /// Nerd tier (icons-5): Thinking renders the spinner frames like
    /// every built-in tier (the static glyph was retired); ToolRunning
    /// keeps the inherited braille spinner; a `[tui.icons] overrides`
    /// static glyph (via a Custom tier) still renders verbatim.
    #[test]
    fn nerd_thinking_uses_spinner_unless_overridden() {
        let theme = crate::theme::Theme::from_palette(
            crate::theme::ThemeMode::Dark.palette(),
            crate::icons::Icons::Nerd,
        );
        let orbit = theme.orbit;
        let mut c = ctx(RunState::Thinking, 0, None);
        c.theme = theme;
        let spans = status_spans(&c);
        assert_eq!(
            spans[0].content, "⠋",
            "default = spinner frame, no static glyph"
        );
        assert_eq!(spans[0].style, orbit);
        assert_eq!(spans[1].content, " thinking");
        // ToolRunning stays on the braille frame table (nerd inherits
        // it from unicode).
        c.run.state = RunState::ToolRunning {
            name: "read_file".into(),
        };
        let spans = status_spans(&c);
        assert_eq!(spans[0].content, "⠋", "tool running keeps the spinner");
        assert_eq!(spans[1].content, " read_file…");
        // The override path: a Custom tier with `thinking` patched
        // renders the static glyph instead of the frames.
        let mut base = crate::icons::Icons::Nerd.set();
        assert!(base.patch_field("thinking", "\u{F0EB}"));
        let custom = crate::theme::Theme::from_palette(
            crate::theme::ThemeMode::Dark.palette(),
            crate::icons::Icons::Custom(Box::leak(Box::new(base))),
        );
        c.run.state = RunState::Thinking;
        c.theme = custom;
        let bulb = char::from_u32(0xF0EB).expect("valid PUA").to_string();
        let spans = status_spans(&c);
        assert_eq!(spans[0].content, bulb, "overridden static glyph renders");
        assert_eq!(spans[1].content, " thinking");
    }

    /// Idle hides the state segment; the other states map to the
    /// restyle icon table.
    #[test]
    fn state_icons_follow_the_restyle_table() {
        // Delegated → `◐ agent`.
        let spans = status_spans(&ctx(
            RunState::Delegated {
                agent: "researcher".into(),
            },
            1,
            None,
        ));
        assert_eq!(&text(&spans[..2]), "◐ researcher");
        // ApprovalPending → `◉ approve?` blinking (frame 0 = on).
        let spans = status_spans(&ctx(RunState::ApprovalPending, 0, None));
        assert_eq!(&text(&spans[..2]), "◉ approve?");
        // Compacting → `◌ compacting…`.
        let spans = status_spans(&ctx(RunState::Compacting, 0, None));
        assert_eq!(&text(&spans[..2]), "◌ compacting…");
        // Error → `× {summary}` in error color.
        let spans = status_spans(&ctx(RunState::Error("boom".into()), 0, None));
        assert_eq!(&text(&spans[..2]), "× boom");
        assert_eq!(spans[1].style, theme_error());
    }

    fn theme_error() -> Style {
        Theme::new().error
    }

    /// The error summary clamps to ≤36 chars with an ellipsis
    /// (char-boundary safe for CJK).
    #[test]
    fn error_summary_clamps_to_36_chars() {
        let long = "错".repeat(50);
        let spans = status_spans(&ctx(RunState::Error(long), 0, None));
        let summary = spans
            .iter()
            .find(|s| s.content.starts_with('错'))
            .expect("summary span");
        assert!(summary.content.chars().count() <= 37, "{}", summary.content);
        assert!(summary.content.ends_with('…'));
    }

    /// `◐ N agents` renders only while delegation is live (accent).
    #[test]
    fn agents_segment_is_delegation_gated() {
        let theme = Theme::new();
        let spans = status_spans(&ctx(RunState::Idle, 0, None));
        assert!(!text(&spans).contains("agent"), "hidden at depth 0");
        let spans = status_spans(&ctx(RunState::Idle, 2, None));
        assert!(text(&spans).contains("◐ 2 agents"));
        assert!(spans
            .iter()
            .any(|s| s.content == "◐ " && s.style == theme.delegate));
    }

    /// The context meter: `▕██░░▏ N% left` with threshold coloring
    /// (signal fill above 25%, warning ≤25%, error ≤10%; ground = line).
    #[test]
    fn context_meter_thresholds_and_glyphs() {
        let theme = Theme::new();
        let spans = status_spans(&ctx(RunState::Idle, 0, Some(80)));
        let meter: String = spans
            .iter()
            .skip_while(|s| s.content != "▕")
            .map(|s| s.content.clone())
            .collect();
        assert!(meter.starts_with("▕█"), "{meter}");
        assert!(meter.contains("░"), "{meter}");
        assert!(meter.ends_with("▏ 80% left"), "{meter}");
        let fill = spans.iter().find(|s| s.content.starts_with('█')).unwrap();
        assert_eq!(fill.style, theme.tool_running);

        let warning_spans = status_spans(&ctx(RunState::Idle, 0, Some(20)));
        assert!(text(&warning_spans).contains("▏ 20% left"));
        let fill = warning_spans
            .iter()
            .find(|s| s.content.starts_with('█'))
            .unwrap();
        assert_eq!(fill.style, theme.warning);

        let error_spans = status_spans(&ctx(RunState::Idle, 0, Some(8)));
        assert!(text(&error_spans).contains("▏ 8% left"));
        let fill = error_spans
            .iter()
            .find(|s| s.content.starts_with('█'))
            .unwrap();
        assert_eq!(fill.style, theme.error);

        // No data → no meter.
        assert!(!text(&status_spans(&ctx(RunState::Idle, 0, None))).contains("% left"));
    }

    /// The git branch helper (P1): a repo dir parses `ref: refs/heads/X`
    /// into `X`; a detached HEAD degrades to the short hash; outside a
    /// repo nothing renders.
    #[test]
    fn git_branch_reads_head_or_hides() {
        let tmp = tempfile::tempdir().unwrap();
        // No .git → None.
        assert_eq!(git_branch_in(tmp.path()), None);
        // Branch ref → the branch name.
        let git_dir = tmp.path().join(".git");
        std::fs::create_dir(&git_dir).unwrap();
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/feat/restyle\n").unwrap();
        assert_eq!(git_branch_in(tmp.path()), Some("feat/restyle".to_owned()));
        // Detached HEAD → short hash.
        std::fs::write(git_dir.join("HEAD"), "0123456789abcdef0123\n").unwrap();
        assert_eq!(git_branch_in(tmp.path()), Some("0123456789ab".to_owned()));
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
        // theme-1: the frames live in the icon tier now — pull them
        // from the theme like `App::on_tick` does (the default unicode
        // tier keeps the frozen braille 10-frame table).
        let frames: Vec<char> = Theme::new().icons.set().spinner.chars().collect();
        assert_eq!(frames.len(), 10);
        assert_eq!(frames[0], '⠋');
        assert_eq!(frames[9], '⠏');
    }

    #[test]
    fn compacting_animates_but_is_not_running() {
        // Compacting shows a live badge but no engine task exists yet —
        // it must not gate /new or the tick cadence as "running".
        let state = RunState::Compacting;
        assert!(state.is_animated());
        assert!(!state.is_running());
        // The segment text carries the review-mandated label.
        let spans = state_group(&state, 0, true, &Theme::new()).expect("compacting segment");
        assert_eq!(text(&spans), "◌ compacting…");
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
