//! Sidebar render tests — agents + session panels at the frozen
//! 30-column sidebar width (`app.rs::SIDEBAR_WIDTH`), exact-buffer
//! assertions (TestBackend + `assert_buffer`, ratatui test convention).
//!
//! Borderless redesign (Wave 2 lane-b): no block frames — each panel is
//! a lowercase BOLD title row (`agents` / `session`; Cyan + BOLD while
//! the sidebar holds focus) with content indented one column below; the
//! two released border columns widen every row (28 inner → 30 total).
//!
//! Covers: title focus styling (buffer-exact via `theme.header*` plus
//! cell-level BOLD/Cyan spot-checks), glyph vocabulary and indentation
//! of the delegation tree (live view with the D2 `…`
//! unknowable-grandchildren row, calibrated terminal statuses), the
//! agents panel's trailing blank separator row, the session panel's
//! two-column alignment, the `no active run` empty state, and
//! 30-column tail clipping (no wrap — the panels are non-wrapping
//! Paragraphs; long ids/values truncate at the panel edge).

use std::time::Duration;

use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::Terminal;

use openslate_core::execution::{ExecutionStatus, ExecutionTree};
use openslate_core::types::{AgentId, RunId};
use openslate_tui::components::{
    AgentsComponent, AppCtx, Component, ConfigSummary, Focus, RunInfo, RunState, SessionComponent,
};
use openslate_tui::theme::Theme;

/// Frozen sidebar width (must match `app.rs::SIDEBAR_WIDTH`).
const W: u16 = 30;
/// Content width under the borderless title: panel width minus the
/// 1-column content indent.
const CONTENT: usize = 29;

fn test_ctx(run_id: Option<&str>) -> AppCtx {
    AppCtx {
        theme: Theme::new(),
        focus: Focus::Sidebar,
        run: RunInfo {
            state: RunState::Idle,
            spinner_frame: 0,
            model_label: "main@intern_genai".into(),
            tokens_in: 1234,
            tokens_out: 56,
            cost_usd: 0.0012,
            elapsed: Some(Duration::from_secs(83)),
            tool_calls_cur: 3,
            depth_cur: 1,
        },
        config: ConfigSummary {
            model_alias: "main".into(),
            model_id: "intern-latest".into(),
            provider_name: "intern_genai".into(),
            max_depth: 4,
            max_tool_calls: 20,
            run_id: run_id.map(str::to_owned),
        },
        size: (W, 12),
        notice: None,
    }
}

/// Draw one component full-area and return the backend for assertions.
fn draw<F>(height: u16, ctx: &AppCtx, render: F) -> TestBackend
where
    F: FnOnce(&mut ratatui::Frame, ratatui::layout::Rect, &AppCtx),
{
    let mut terminal = Terminal::new(TestBackend::new(W, height)).expect("test terminal");
    terminal.draw(|f| render(f, f.area(), ctx)).expect("draw");
    terminal.backend().clone()
}

/// Panel title row: lowercase word at column 0 — `theme.header` (BOLD
/// on the default foreground) or `theme.header_focused` (Cyan + BOLD)
/// while the sidebar holds focus (the borderless focus expression).
fn title(text: &str, style: Style) -> Line<'static> {
    let pad = W as usize - text.chars().count();
    Line::from(vec![
        Span::styled(text.to_owned(), style),
        Span::raw(" ".repeat(pad)),
    ])
}

/// Content row: 1-column indent + styled content, padded to the full
/// panel width. Clipped rows pass pre-truncated content so the spans
/// fill exactly W columns.
fn content(text: &str, style: Style) -> Line<'static> {
    let pad = (W as usize - 1).saturating_sub(text.chars().count());
    Line::from(vec![
        Span::raw(" "),
        Span::styled(text.to_owned(), style),
        Span::raw(" ".repeat(pad)),
    ])
}

/// Full-width blank row (the agents panel's trailing separator).
fn blank() -> Line<'static> {
    Line::from(Span::raw(" ".repeat(W as usize)))
}

/// Two-column session row: 1-column indent + muted label + padding +
/// value right-aligned to the content width (spans total exactly W
/// columns; clipped values pass pre-truncated with zero padding).
fn session_row(label: &str, value: &str, value_style: Style, t: &Theme) -> Line<'static> {
    let pad = CONTENT.saturating_sub(label.chars().count() + value.chars().count());
    Line::from(vec![
        Span::raw(" "),
        Span::styled(label.to_owned(), t.muted),
        Span::raw(" ".repeat(pad)),
        Span::styled(value.to_owned(), value_style),
    ])
}

fn execution_tree() -> ExecutionTree {
    ExecutionTree::new(RunId("r".into()), AgentId("root".into()))
}

// ─── agents panel ──────────────────────────────────────────────────────────

#[test]
fn agents_live_view_renders_running_child_and_unknowable_tail() {
    let t = Theme::new();
    // test_ctx focus = Sidebar → focused (Cyan + BOLD) title.
    let ctx = test_ctx(None);
    let mut agents = AgentsComponent::new();
    agents.set_root("root");
    agents.on_delegate_start("researcher");
    agents.on_delegate_start("verifier");
    // Parallel batch drain order: the OLDEST delegation (researcher)
    // ends first (FIFO pairing, per core's tool-call drain order);
    // verifier stays running with an unknowable-grandchild tail.
    agents.on_delegate_end("call_agent");

    let backend = draw(6, &ctx, |f, area, ctx| agents.render(f, area, ctx));
    let expected = Buffer::with_lines(vec![
        title("agents", t.header_focused),
        content(" root", t.agent_active),
        content("  ├  researcher", t.agent_done),
        content("  └  verifier", t.agent_running),
        content("    └ …", t.fine),
        blank(),
    ]);
    backend.assert_buffer(&expected);
}

#[test]
fn agents_calibrated_view_renders_full_tree_with_terminal_glyphs() {
    let t = Theme::new();
    let ctx = test_ctx(None);
    let mut agents = AgentsComponent::new();
    agents.set_root("root");

    // root → researcher(Completed) → writer(Failed, depth 2)
    // root → verifier(left Running → Interrupted)
    let run_id = RunId("r".into());
    let mut tree = execution_tree();
    let root_id = tree.root_id().clone();
    let researcher = tree.create_child(
        run_id.clone(),
        AgentId("researcher".into()),
        root_id.clone(),
        None,
    );
    let writer = tree.create_child(
        run_id.clone(),
        AgentId("writer".into()),
        researcher.clone(),
        None,
    );
    tree.create_child(run_id, AgentId("verifier".into()), root_id.clone(), None);
    tree.update_status(&root_id, ExecutionStatus::Completed);
    tree.update_status(&researcher, ExecutionStatus::Completed);
    tree.update_status(&writer, ExecutionStatus::Failed);
    // root/verifier stay Running → Interrupted in the final snapshot.
    agents.calibrate(&tree);

    let backend = draw(6, &ctx, |f, area, ctx| agents.render(f, area, ctx));
    let expected = Buffer::with_lines(vec![
        title("agents", t.header_focused),
        content(" root", t.agent_active),
        content("  ├  researcher", t.agent_done),
        content("    └  writer", t.agent_failed),
        content("  └ … verifier", t.agent_done),
        blank(),
    ]);
    backend.assert_buffer(&expected);
}

#[test]
fn agents_empty_state_without_root() {
    let t = Theme::new();
    let ctx = test_ctx(None);
    let agents = AgentsComponent::new();
    let backend = draw(3, &ctx, |f, area, ctx| agents.render(f, area, ctx));
    let expected = Buffer::with_lines(vec![
        title("agents", t.header_focused),
        content("no agents", t.muted),
        blank(),
    ]);
    backend.assert_buffer(&expected);
}

#[test]
fn agents_long_id_clips_at_30_columns_without_wrap() {
    let t = Theme::new();
    let ctx = test_ctx(None);
    let mut agents = AgentsComponent::new();
    agents.set_root("root");
    let run_id = RunId("r".into());
    let mut tree = execution_tree();
    let root_id = tree.root_id().clone();
    let child = tree.create_child(
        run_id,
        AgentId("very-long-agent-identifier-123456".into()),
        root_id,
        None,
    );
    tree.update_status(&child, ExecutionStatus::Completed);
    agents.calibrate(&tree);

    let backend = draw(4, &ctx, |f, area, ctx| agents.render(f, area, ctx));
    // 1 indent + "  └  " prefix (6 cols) + 23 visible id chars = 30:
    // the tail clips at the panel edge instead of wrapping (the freed
    // border columns buy one more visible char than the framed panel).
    let expected = Buffer::with_lines(vec![
        title("agents", t.header_focused),
        content(" root", t.agent_active),
        content("  └  very-long-agent-identif", t.agent_done),
        blank(),
    ]);
    backend.assert_buffer(&expected);
}

#[test]
fn agents_title_is_bold_and_cyan_only_when_sidebar_focused() {
    let agents = AgentsComponent::new();
    let mut ctx = test_ctx(None);
    // Unfocused: BOLD on the DEFAULT foreground (light/dark neutral —
    // cells in an untouched buffer carry `Color::Reset`, ratatui's
    // "terminal default fg").
    ctx.focus = Focus::Input;
    let backend = draw(2, &ctx, |f, area, ctx| agents.render(f, area, ctx));
    let cell = backend.buffer().cell((0, 0)).expect("title cell");
    assert_eq!(cell.symbol(), "a");
    assert!(matches!(cell.style().fg, None | Some(Color::Reset)));
    assert!(cell.style().add_modifier.contains(Modifier::BOLD));
    // Sidebar-focused: the title switches to Cyan + BOLD.
    ctx.focus = Focus::Sidebar;
    let backend = draw(2, &ctx, |f, area, ctx| agents.render(f, area, ctx));
    let cell = backend.buffer().cell((0, 0)).expect("title cell");
    assert_eq!(cell.symbol(), "a");
    assert_eq!(cell.style().fg, Some(Color::Cyan));
    assert!(cell.style().add_modifier.contains(Modifier::BOLD));
}

// ─── session panel ─────────────────────────────────────────────────────────

#[test]
fn session_full_run_renders_two_column_rows() {
    let t = Theme::new();
    let ctx = test_ctx(Some("ses_f76d4822fffeFuK3"));
    let session = SessionComponent::new();
    let backend = draw(9, &ctx, |f, area, ctx| session.render(f, area, ctx));
    let expected = Buffer::with_lines(vec![
        title("session", t.header_focused),
        session_row("model", "main@intern_genai", t.assistant, &t),
        session_row("id", "intern-latest", t.fine, &t),
        session_row("depth", "1/4", t.assistant, &t),
        session_row("tools", "3/20", t.assistant, &t),
        session_row("in/out", "1.2k/56", t.assistant, &t),
        session_row("cost", "$0.0012", t.assistant, &t),
        session_row("elapsed", "01:23", t.assistant, &t),
        session_row("run", "ses_f76d", t.assistant, &t),
    ]);
    backend.assert_buffer(&expected);
}

#[test]
fn session_without_run_renders_graceful_empty_state() {
    let t = Theme::new();
    let ctx = test_ctx(None);
    let session = SessionComponent::new();
    let backend = draw(4, &ctx, |f, area, ctx| session.render(f, area, ctx));
    let expected = Buffer::with_lines(vec![
        title("session", t.header_focused),
        session_row("model", "main@intern_genai", t.assistant, &t),
        session_row("id", "intern-latest", t.fine, &t),
        content("no active run", t.muted),
    ]);
    backend.assert_buffer(&expected);
}

#[test]
fn session_long_value_clips_at_30_columns() {
    let t = Theme::new();
    let mut ctx = test_ctx(Some("ses_f76d4822fffeFuK3"));
    ctx.config.model_id = "intern-latest-ultra-long-model-name-xyz".into();
    let session = SessionComponent::new();
    let backend = draw(9, &ctx, |f, area, ctx| session.render(f, area, ctx));
    // Indent + label "id" + 27 visible value chars fill the row; the
    // tail clips at the panel edge (one more char than the framed
    // panel showed).
    let expected = Buffer::with_lines(vec![
        title("session", t.header_focused),
        session_row("model", "main@intern_genai", t.assistant, &t),
        session_row("id", "intern-latest-ultra-long-mo", t.fine, &t),
        session_row("depth", "1/4", t.assistant, &t),
        session_row("tools", "3/20", t.assistant, &t),
        session_row("in/out", "1.2k/56", t.assistant, &t),
        session_row("cost", "$0.0012", t.assistant, &t),
        session_row("elapsed", "01:23", t.assistant, &t),
        session_row("run", "ses_f76d", t.assistant, &t),
    ]);
    backend.assert_buffer(&expected);
}

#[test]
fn session_title_is_bold_and_cyan_only_when_sidebar_focused() {
    let session = SessionComponent::new();
    let mut ctx = test_ctx(None);
    // Unfocused: BOLD on the DEFAULT foreground (light/dark neutral —
    // untouched buffer cells carry `Color::Reset`).
    ctx.focus = Focus::Transcript;
    let backend = draw(2, &ctx, |f, area, ctx| session.render(f, area, ctx));
    let cell = backend.buffer().cell((0, 0)).expect("title cell");
    assert_eq!(cell.symbol(), "s");
    assert!(matches!(cell.style().fg, None | Some(Color::Reset)));
    assert!(cell.style().add_modifier.contains(Modifier::BOLD));
    // Sidebar-focused: the title switches to Cyan + BOLD.
    ctx.focus = Focus::Sidebar;
    let backend = draw(2, &ctx, |f, area, ctx| session.render(f, area, ctx));
    let cell = backend.buffer().cell((0, 0)).expect("title cell");
    assert_eq!(cell.symbol(), "s");
    assert_eq!(cell.style().fg, Some(Color::Cyan));
    assert!(cell.style().add_modifier.contains(Modifier::BOLD));
}
