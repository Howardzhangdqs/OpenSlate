//! App — aggregate state, main loop, modal stack, turn lifecycle,
//! shutdown. Interface-frozen as of P2b (see the spec's crate layout).
//!
//! # Main loop shape
//!
//! ```text
//! select! { tick │ crossterm EventStream │ engine channel }
//!   → all sources become Actions in one unbounded dispatcher channel
//!   → bounded drain (≤64 actions or 4 ms, whichever first)
//!   → adjacent Delta events coalesced
//!   → dispatch each → single terminal.draw
//! ```
//!
//! The tick cadence is dynamic: 250 ms idle, 100 ms while a turn runs
//! (spinner advances one frame per tick).
//!
//! # Modal stack (frozen precedence)
//!
//! `ApprovalActive > HelpOpen > ConfirmExit > Normal`. While the approval
//! layer is active, `y`/`n`/`a` preempt EVERYTHING (including input-box
//! characters). `Tab` (focus cycling) only works in the non-modal layer.
//!
//! # Engine-turn ownership (frozen)
//!
//! The App holds `Option<RunManager>`; submitting a turn `take()`s it,
//! moves it into [`crate::event::spawn_turn`]'s task, and
//! [`crate::event::TuiEvent::TurnDone`] carries it back (both arms). The
//! turn future is NEVER inlined into the UI `select!` — an approval's
//! blocking `decide()` would freeze the loop. The runtime MUST be
//! multi-thread (see main.rs).

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use openslate_app::wiring::AppContext;
use openslate_core::approval::ApprovalManager;
use openslate_core::config::OpenSlateConfig;
use openslate_core::context_manager::{compact, needs_compact};
use openslate_core::provider::{GenerateRequest, ModelProvider};
use openslate_core::run_manager::RunManager;
use openslate_core::runtime::{CancellationToken, CostSpec, MessageSink};
use openslate_core::types::{Message, MessageRole, RunId, Usage};
use openslate_store_sqlite::recorder::RunRecorder;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio_stream::StreamExt;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::action::{map_event, Action, ApprovalChoice};
use crate::clipboard;
use crate::components::{
    AgentsComponent, AppCtx, ApprovalComponent, Component, ConfigSummary, Focus, HelpComponent,
    InputComponent, RunInfo, RunState, SessionComponent, StatusComponent, TranscriptComponent,
};
use crate::event::{spawn_turn, ApprovalBridge, TuiEvent, TurnSummary};
use crate::slash::{self, SlashCommand};
use crate::theme::Theme;

/// Idle tick cadence (spec: 250 ms).
const IDLE_TICK: Duration = Duration::from_millis(250);
/// Running-turn tick cadence (spec: 100 ms → 10 spinner frames ≈ 1 s).
const RUN_TICK: Duration = Duration::from_millis(100);
/// Max actions drained per loop wake (spec: 64).
const DRAIN_BATCH: usize = 64;
/// Drain time budget (spec: 4 ms).
const DRAIN_BUDGET: Duration = Duration::from_millis(4);
/// Grace period for the engine task during shutdown (spec: 5 s).
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// Ticks a transient status-bar notice stays up (~5 s at the 250 ms idle
/// tick) before auto-clearing.
const NOTICE_TICKS: u8 = 20;
/// Below this width the layout degrades: sidebar hidden.
const SIDEBAR_MIN_COLS: u16 = 100;
/// Sidebar width when visible.
const SIDEBAR_WIDTH: u16 = 30;
/// Minimum usable terminal; below this a warning overlay replaces layout.
const MIN_COLS: u16 = 60;
const MIN_ROWS: u16 = 12;

/// System prompt for the compaction summarizer (same contract as the
/// REPL's — ported verbatim so both frontends summarize identically).
const SUMMARY_SYSTEM_PROMPT: &str = "You summarize agent conversation transcripts \
for continued work. Produce a concise summary that preserves: \
(1) key decisions made and their rationale, \
(2) important file paths, commands, and code artifacts touched, \
(3) unfinished tasks, open questions, and next steps. \
Drop pleasantries and verbose tool output details. Be brief — only what is \
needed to continue the work effectively.";

/// How the test seam builds the per-turn provider. Production wires
/// [`openslate_app::build_provider_for_model`]; the integration test
/// injects a scripted provider.
pub type ProviderFactory =
    Arc<dyn Fn(&OpenSlateConfig, &str) -> Result<Box<dyn ModelProvider>> + Send + Sync>;

/// The modal layer stack, computed (not stored): approval queue wins over
/// the stored `modal` field. See the module docs for precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    /// No overlay.
    Normal,
    /// Exit confirmation (`y`/`q`/second Ctrl+C confirm, Esc cancels).
    ConfirmExit,
    /// Help overlay.
    HelpOpen,
    /// A tool approval is pending — y/n/a preempt all input.
    ApprovalActive,
}

/// Overlays stored on the App (`ApprovalActive` is derived from the
/// approval queue instead).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Modal {
    None,
    HelpOpen,
    ConfirmExit,
}

/// Result of dispatching one action through the App.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchOutcome {
    /// Keep running.
    Continue,
    /// The session should exit now.
    Quit,
}

/// One-line session summary printed to stdout after terminal restore.
#[derive(Debug, Clone, Default)]
pub struct SessionSummary {
    pub run_id: Option<String>,
    pub turns: u32,
    pub total_cost_usd: f64,
}

/// Session-accumulated statistics (REPL-parity semantics).
#[derive(Debug, Clone)]
struct SessionStats {
    total_steps: u32,
    total_input_tokens: u64,
    total_output_tokens: u64,
    turns: u32,
    /// Total session cost: turns + compact summary calls.
    total_cost_usd: f64,
    /// The compact-summary subset (bills the session, not the run row).
    compact_cost_usd: f64,
}

impl SessionStats {
    fn new() -> Self {
        Self {
            total_steps: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            turns: 0,
            total_cost_usd: 0.0,
            compact_cost_usd: 0.0,
        }
    }

    /// Cost attributable to the persisted run row (session minus compact).
    fn run_cost_usd(&self) -> f64 {
        self.total_cost_usd - self.compact_cost_usd
    }
}

/// The session's backing persisted run (REPL `SessionRun` port): one run
/// row spans the whole session; every turn lands under the same `run_id`.
#[derive(Clone)]
struct SessionRun {
    run_id: RunId,
    recorder: Arc<RunRecorder>,
}

/// The application aggregate. Public surface frozen; internals evolve
/// only within the P3 write-domain rules.
pub struct App {
    // ── wiring pieces (kept alive / used per turn) ─────────────────────
    config: OpenSlateConfig,
    agent_tree: openslate_core::agent_tree::AgentTree,
    store: Option<openslate_store_sqlite::store::SqliteStore>,
    provider_factory: ProviderFactory,
    /// MCP connections must outlive the tool registry inside the manager.
    _mcp_connections: openslate_core::mcp::McpConnectionGuard,

    // ── engine bridge ──────────────────────────────────────────────────
    manager: Option<RunManager>,
    engine_task: Option<tokio::task::JoinHandle<()>>,
    cancel_token: CancellationToken,
    events_tx: UnboundedSender<TuiEvent>,
    events_rx: UnboundedReceiver<TuiEvent>,
    approval_bridge: Arc<ApprovalBridge>,

    // ── conversation ───────────────────────────────────────────────────
    history: Vec<Message>,
    session_run: Option<SessionRun>,
    /// Input submitted while a turn was running; auto-submitted after
    /// `TurnDone` (restored to the editor on error instead — no retry
    /// storms).
    pending_input: Option<String>,
    model_override: Option<String>,
    stats: SessionStats,
    /// Set by `Action::Redraw` (Ctrl+L): the run loop clears the
    /// terminal before the next draw — a TRUE full repaint. Diff-based
    /// rendering can never correct stale cells the diff excludes by
    /// design (wide-glyph tails), so this is the manual recovery path.
    needs_full_redraw: bool,
    /// Terminal mouse capture is ON (default — the wheel scrolls the
    /// chat). Ctrl+M / `/mouse` flip this; the run loop detects the
    /// flip and executes Enable/DisableMouseCapture on stdout (same
    /// flag→loop pattern as `needs_full_redraw`). OFF restores the
    /// terminal's native text selection at the cost of the wheel
    /// degrading into ↑/↓ keys.
    mouse_capture: bool,
    /// OSC 52 write channel (fix-13): production writes the clipboard
    /// escape to the REAL stdout immediately inside dispatch (the
    /// terminal consumes it at once; the next diff draw is unaffected);
    /// tests inject a capturing sink via [`App::set_clipboard_sink`].
    clipboard_out: clipboard::ClipboardSink,

    // ── UI state ───────────────────────────────────────────────────────
    theme: Theme,
    focus: Focus,
    modal: Modal,
    sidebar_visible: bool,
    run_state: RunState,
    spinner_frame: usize,
    turn_started: Option<Instant>,
    /// RequestStart instant of the CURRENT model request (reset per
    /// request; consumed at RequestEnd → the per-request meta line's
    /// rate denominator).
    request_started: Option<Instant>,
    /// FirstToken instant of the CURRENT model request (set when the
    /// `FirstToken` event arrives, reset on each `RequestStart`).
    /// Paired with `request_started` at RequestEnd → the per-request
    /// meta line's TTFT segment (`None` when no first token was
    /// observed — the segment is then omitted).
    first_token_at: Option<Instant>,
    /// Precise usage of the current request, stored when the `Usage`
    /// event arrives and consumed by `RequestEnd` (the meta line).
    /// Providers may deliver it before or after the deltas.
    pending_usage: Option<Usage>,
    size: (u16, u16),
    /// Tool calls issued in the current turn (`ToolStart` count; reset
    /// when the next turn starts). Kept after `TurnDone` for the session
    /// panel's `tool calls cur/max` cell.
    tool_calls_cur: u32,
    /// Live delegation depth (`call_agent` push/pop; zeroed on TurnDone —
    /// the execution tree is all-terminal post-turn).
    depth_cur: u32,
    /// Transient status-bar notice (yellow; e.g. a rejected `/new`).
    status_notice: Option<String>,
    /// Ticks remaining before `status_notice` auto-clears.
    notice_ticks: u8,
    /// Auto-compact deferred flag: set with `RunState::Compacting` so the
    /// badge PAINTS on the draw between dispatch rounds; the blocking
    /// summary call then runs at the top of the NEXT dispatched action
    /// (worst case one idle tick, 250 ms) — the UI is never frozen
    /// without the ` compacting…` feedback visible.
    compact_pending: bool,

    // ── components ─────────────────────────────────────────────────────
    input: InputComponent,
    transcript: TranscriptComponent,
    agents: AgentsComponent,
    session: SessionComponent,
    help: HelpComponent,
    approval: ApprovalComponent,
    status: StatusComponent,
}

impl App {
    /// Build the App with the production provider factory.
    pub fn new(ctx: AppContext) -> Self {
        Self::with_provider_factory(ctx, Arc::new(openslate_app::build_provider_for_model))
    }

    /// Build the App with a custom per-turn provider factory (test seam).
    #[allow(clippy::too_many_lines)]
    pub fn with_provider_factory(ctx: AppContext, provider_factory: ProviderFactory) -> Self {
        let AppContext {
            config,
            agent_tree,
            manager,
            store,
            mcp_connections,
            ..
        } = ctx;

        let (events_tx, events_rx) = unbounded_channel();
        let approval_bridge = Arc::new(ApprovalBridge::new(events_tx.clone()));

        // Interactive approval policy derivation (same priority as the
        // REPL): [approval].policy > interactive default
        // auto_except([shell, run_code]).
        let configured = config.approval.as_ref().map(|a| a.to_policy());
        let effective = openslate_app::wiring::derive_effective_policy(configured, true, false);
        let mut manager = manager;
        // Arc<ApprovalBridge> coerces into the callback trait object here.
        manager.approval = ApprovalManager::new(effective).with_callback(approval_bridge.clone());

        let mut agents = AgentsComponent::new();
        agents.set_root(&agent_tree.get_root().id.0);

        Self {
            config,
            agent_tree,
            store,
            provider_factory,
            _mcp_connections: mcp_connections,
            manager: Some(manager),
            engine_task: None,
            cancel_token: CancellationToken::new(),
            events_tx,
            events_rx,
            approval_bridge,
            history: Vec::new(),
            session_run: None,
            pending_input: None,
            model_override: None,
            stats: SessionStats::new(),
            needs_full_redraw: false,
            mouse_capture: true,
            clipboard_out: clipboard::stdout_clipboard_sink(),
            theme: Theme::new(),
            focus: Focus::Input,
            modal: Modal::None,
            sidebar_visible: true,
            run_state: RunState::Idle,
            spinner_frame: 0,
            turn_started: None,
            request_started: None,
            first_token_at: None,
            pending_usage: None,
            size: (0, 0),
            tool_calls_cur: 0,
            depth_cur: 0,
            status_notice: None,
            notice_ticks: 0,
            compact_pending: false,
            input: InputComponent::new(),
            transcript: TranscriptComponent::new(),
            agents,
            session: SessionComponent::new(),
            help: HelpComponent::new(),
            approval: ApprovalComponent::new(),
            status: StatusComponent::new(),
        }
    }

    // ── Observers (frozen pub surface for tests/panels) ────────────────

    /// Current run-state machine position.
    pub fn run_state(&self) -> &RunState {
        &self.run_state
    }

    /// Whether a turn is executing (engine task alive).
    pub fn is_running(&self) -> bool {
        self.engine_task.is_some()
    }

    /// Committed transcript entries (rebuild output / live view).
    pub fn transcript_entries(&self) -> &[crate::components::transcript::TranscriptEntry] {
        self.transcript.entries()
    }

    /// Read-only handle on the transcript component — the streaming
    /// buffers and the render surface for dispatch-level tests that
    /// must draw a frame exactly like the run loop does.
    pub fn transcript(&self) -> &crate::components::TranscriptComponent {
        &self.transcript
    }

    /// In-memory conversation history (last turn's full message list).
    pub fn history(&self) -> &[Message] {
        &self.history
    }

    /// Backing session run id, once a turn has been submitted.
    pub fn session_run_id(&self) -> Option<&str> {
        self.session_run.as_ref().map(|r| r.run_id.0.as_str())
    }

    /// Current input-editor text (dispatch-level test observer; also the
    /// Err-path pending-restore assertion point).
    pub fn input_text(&self) -> String {
        self.input.text()
    }

    /// Whether the current turn's cancel token has been flipped
    /// (dispatch-level test observer for the CancelTurn / Ctrl+C paths).
    pub fn is_cancelled(&self) -> bool {
        self.cancel_token.is_cancelled()
    }

    /// Whether the transcript is pinned away from the live tail
    /// (dispatch-level test observer for scroll/wheel routing).
    pub fn transcript_pinned(&self) -> bool {
        self.transcript.is_pinned()
    }

    /// Whether terminal mouse capture is currently ON (dispatch-level
    /// test observer for the Ctrl+M / `/mouse` toggle).
    pub fn mouse_capture(&self) -> bool {
        self.mouse_capture
    }

    /// The transient status-bar notice, if any (dispatch-level test
    /// observer).
    pub fn status_notice(&self) -> Option<&str> {
        self.status_notice.as_deref()
    }

    /// Replace the OSC 52 clipboard write channel (test seam):
    /// production writes to the real stdout; tests capture the payload.
    pub fn set_clipboard_sink(&mut self, sink: clipboard::ClipboardSink) {
        self.clipboard_out = sink;
    }

    // ── Main loop ──────────────────────────────────────────────────────

    /// Run until quit. Performs the ordered shutdown (cancel → deny
    /// pending approvals → await engine with timeout → finalize run row),
    /// restores NOTHING here (terminal restore is main's job, symmetric
    /// with init).
    pub async fn run(mut self, mut terminal: ratatui::DefaultTerminal) -> Result<SessionSummary> {
        let (action_tx, mut action_rx) = unbounded_channel::<Action>();
        let mut events = crossterm::event::EventStream::new();
        let mut tick = tokio::time::interval(IDLE_TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut tick_period = IDLE_TICK;

        let mut quit = false;
        // Mouse-capture mode currently applied to the terminal (init
        // enabled it in main.rs → starts true). Compared against
        // `self.mouse_capture` after each dispatch round; a mismatch
        // executes the Enable/Disable escape BEFORE the next draw.
        let mut mouse_capture_applied = true;
        while !quit {
            tokio::select! {
                _ = tick.tick() => {
                    let _ = action_tx.send(Action::Tick);
                }
                maybe = events.next() => {
                    match maybe {
                        Some(Ok(event)) => {
                            if let Some(action) = map_event(&event) {
                                let _ = action_tx.send(action);
                            }
                        }
                        // A broken terminal stream is unrecoverable → quit.
                        Some(Err(_)) | None => {
                            let _ = action_tx.send(Action::Quit);
                        }
                    }
                }
                maybe = self.events_rx.recv() => {
                    if let Some(event) = maybe {
                        let _ = action_tx.send(Action::Engine(event));
                    }
                }
            }

            // Bounded drain: ≤64 actions or DRAIN_BUDGET, whichever first.
            let mut batch: Vec<Action> = Vec::new();
            let deadline = Instant::now() + DRAIN_BUDGET;
            while batch.len() < DRAIN_BATCH {
                let now = Instant::now();
                if now < deadline {
                    match tokio::time::timeout_at(deadline.into(), action_rx.recv()).await {
                        Ok(Some(action)) => batch.push(action),
                        Ok(None) => break, // dispatcher channel closed
                        Err(_) => break,   // budget exhausted
                    }
                } else {
                    // Past the deadline: only opportunistically take what
                    // is already queued (no waiting).
                    match action_rx.try_recv() {
                        Ok(action) => batch.push(action),
                        Err(_) => break,
                    }
                }
            }
            coalesce_deltas(&mut batch);

            for action in batch {
                if self.dispatch(action).await == DispatchOutcome::Quit {
                    quit = true;
                    break;
                }
            }
            if quit {
                break;
            }

            // Dynamic tick cadence.
            let want = if self.run_state.is_running() {
                RUN_TICK
            } else {
                IDLE_TICK
            };
            if want != tick_period {
                tick_period = want;
                tick = tokio::time::interval(tick_period);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            }

            if self.needs_full_redraw {
                terminal.clear()?;
                self.needs_full_redraw = false;
            }
            // Ctrl+M / `/mouse` flip (flag→loop pattern): apply the
            // escape on the real stdout, not the ratatui backend —
            // same channel main.rs used at init. On write failure the
            // applied flag stays stale so the next loop retries.
            if self.mouse_capture != mouse_capture_applied {
                let applied = if self.mouse_capture {
                    crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture)
                } else {
                    crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture)
                };
                if applied.is_ok() {
                    mouse_capture_applied = self.mouse_capture;
                }
            }
            terminal.draw(|f| self.render(f))?;
        }

        self.shutdown().await;
        Ok(self.summary())
    }

    /// Feed one action through the dispatcher. Public so integration
    /// tests can drive the App without a terminal.
    pub async fn dispatch(&mut self, action: Action) -> DispatchOutcome {
        let mut current = Some(action);
        while let Some(action) = current.take() {
            match self.dispatch_one(action).await {
                (DispatchOutcome::Continue, follow) => {
                    current = follow;
                }
                (DispatchOutcome::Quit, _) => return DispatchOutcome::Quit,
            }
        }
        DispatchOutcome::Continue
    }

    /// Drain already-delivered engine events into the dispatcher. The
    /// run loop does this via `select!`; tests (which drive `dispatch`
    /// directly) call this to observe the engine task's output.
    pub async fn drain_engine_events(&mut self) {
        while let Ok(event) = self.events_rx.try_recv() {
            let _ = self.dispatch(Action::Engine(event)).await;
        }
    }

    /// Dispatch one action; returns the outcome plus an optional chained
    /// follow-up action (e.g. `SubmitInput` → `StartTurn(text)`).
    async fn dispatch_one(&mut self, action: Action) -> (DispatchOutcome, Option<Action>) {
        // Deferred auto-compact (see `start_turn`): the `Compacting` badge
        // has been painted by the draw between dispatch rounds — run the
        // blocking summary call NOW (worst case one idle tick after the
        // badge appeared), then continue the queued turn spawn. `Quit` is
        // exempt: never wait out a slow summary call to exit.
        if self.compact_pending && !matches!(action, Action::Quit) {
            self.compact_pending = false;
            self.run_deferred_compact().await;
        }
        match action {
            // ── Lifecycle ──
            Action::Tick => {
                self.on_tick();
                (DispatchOutcome::Continue, None)
            }
            // Ctrl+L: request a TRUE full repaint. Diff-based draws can
            // never correct stale cells that the diff excludes by design
            // (wide-glyph tails), so clear the screen + diff state.
            Action::Redraw => {
                self.needs_full_redraw = true;
                (DispatchOutcome::Continue, None)
            }
            Action::Quit => (DispatchOutcome::Quit, None),
            Action::RequestQuit => {
                if self.active_layer() == Layer::Normal {
                    self.modal = Modal::ConfirmExit;
                }
                (DispatchOutcome::Continue, None)
            }
            Action::CancelQuit => {
                if self.modal == Modal::ConfirmExit {
                    self.modal = Modal::None;
                }
                (DispatchOutcome::Continue, None)
            }

            // ── Engine events ──
            Action::Engine(event) => self.handle_engine_event(event).await,

            // ── Approvals ──
            Action::ApprovalRespond(choice) => {
                self.approval_respond(choice);
                (DispatchOutcome::Continue, None)
            }

            // ── Ctrl+C semantics (state-dependent; see action.rs) ──
            Action::CancelTurn => match self.active_layer() {
                Layer::ApprovalActive => {
                    // Deny (unblocks the engine) AND cancel the turn.
                    self.approval_respond(ApprovalChoice::Deny);
                    self.cancel_token.cancel();
                    (DispatchOutcome::Continue, None)
                }
                Layer::ConfirmExit => {
                    // A SECOND Ctrl+C inside the exit modal CONFIRMS the
                    // quit — double-Ctrl+C is the muscle-memory quit path
                    // (opencode convention). This arm runs before the
                    // modal guard below, so it must confirm here itself.
                    (DispatchOutcome::Quit, None)
                }
                _ if self.is_running() => {
                    self.cancel_token.cancel();
                    (DispatchOutcome::Continue, None)
                }
                _ => {
                    self.modal = Modal::ConfirmExit;
                    (DispatchOutcome::Continue, None)
                }
            },

            // ── Turn start (from input submit, pending auto-submit, or
            //    tests) ──
            Action::StartTurn(text) => self.handle_start_turn(text).await,

            // ── Modal input preemption (ApprovalActive > HelpOpen >
            //    ConfirmExit > routing) ──
            _ if self.active_layer() == Layer::ApprovalActive => {
                if let Action::InputChar(c) = &action {
                    match c {
                        'y' => self.approval_respond(ApprovalChoice::Approve),
                        'n' => self.approval_respond(ApprovalChoice::Deny),
                        'a' => self.approval_respond(ApprovalChoice::ApproveAll),
                        _ => {}
                    }
                }
                // Everything else is swallowed while approval is pending.
                (DispatchOutcome::Continue, None)
            }
            _ if self.active_layer() == Layer::ConfirmExit => match &action {
                // `y`/`q` confirm; Ctrl+C confirm is handled earlier in
                // the CancelTurn arm (this guard never sees CancelTurn).
                Action::InputChar('y') | Action::InputChar('q') => (DispatchOutcome::Quit, None),
                Action::DismissOverlay | Action::CancelQuit => {
                    self.modal = Modal::None;
                    (DispatchOutcome::Continue, None)
                }
                _ => (DispatchOutcome::Continue, None),
            },
            _ if self.active_layer() == Layer::HelpOpen => match &action {
                Action::DismissOverlay | Action::ToggleHelp => {
                    self.modal = Modal::None;
                    (DispatchOutcome::Continue, None)
                }
                Action::InputChar('?') | Action::InputChar('q') => {
                    self.modal = Modal::None;
                    (DispatchOutcome::Continue, None)
                }
                _ => (DispatchOutcome::Continue, None),
            },

            // ── Global (non-modal) keys ──
            Action::FocusNext => {
                let mut next = self.focus.next();
                // Narrow terminal: the sidebar is hidden — skip its focus
                // stop so keys are not swallowed by an invisible panel.
                if next == Focus::Sidebar && !self.sidebar_active() {
                    next = next.next(); // Sidebar → Input
                }
                self.focus = next;
                (DispatchOutcome::Continue, None)
            }
            Action::ToggleSidebar => {
                self.sidebar_visible = !self.sidebar_visible;
                (DispatchOutcome::Continue, None)
            }
            Action::ToggleMouseCapture => {
                self.toggle_mouse_capture();
                (DispatchOutcome::Continue, None)
            }
            Action::CopyLast => {
                self.copy_last_output();
                (DispatchOutcome::Continue, None)
            }
            Action::ToggleHelp => {
                self.modal = Modal::HelpOpen;
                (DispatchOutcome::Continue, None)
            }
            Action::DismissOverlay => {
                // No overlay open: falls through to focus routing so the
                // transcript can use Esc to unpin.
                self.route_by_focus(action)
            }
            Action::InputChar('?') if self.input.is_empty() => {
                // `?` opens help only with an empty input (design brief).
                self.modal = Modal::HelpOpen;
                (DispatchOutcome::Continue, None)
            }
            Action::WheelScrollUp | Action::WheelScrollDown => {
                // The wheel is positional: it ALWAYS scrolls the chat
                // transcript (3 lines per notch), never the input-box
                // history — the arrow keys keep that focus-routed role.
                const WHEEL_SCROLL_LINES: usize = 3;
                let base = if matches!(action, Action::WheelScrollUp) {
                    Action::ScrollUp
                } else {
                    Action::ScrollDown
                };
                let mut ctx = self.make_ctx();
                // The transcript's handle() guards on keyboard focus;
                // the wheel targets the chat regardless of where that
                // focus is, so present it as transcript-focused.
                ctx.focus = Focus::Transcript;
                let mut follow = None;
                for _ in 0..WHEEL_SCROLL_LINES {
                    follow = self.transcript.handle(&base, &mut ctx);
                }
                (DispatchOutcome::Continue, follow)
            }
            Action::Click(column, row) => {
                // Positional like the wheel: never focus-routed. The
                // pinned `↓ 新内容 +N` hint is CLICKABLE — a hit inside
                // the hint's last-rendered rectangle returns
                // ScrollBottom (the EXISTING follow-restore action),
                // which is applied straight back to the transcript:
                // the generic follow-up loop would re-route
                // ScrollBottom as InputEnd under input focus. Misses
                // (or no hint) are inert. Modal-active clicks never
                // get here — the modal guards above swallow them.
                let mut ctx = self.make_ctx();
                ctx.focus = Focus::Transcript;
                let follow = self
                    .transcript
                    .handle(&Action::Click(column, row), &mut ctx)
                    .and_then(|next| self.transcript.handle(&next, &mut ctx));
                (DispatchOutcome::Continue, follow)
            }

            // ── Everything else routes by focus ──
            _ => self.route_by_focus(action),
        }
    }

    /// Focus-based component routing, including the arrow-key
    /// reinterpretations (see action.rs module docs).
    fn route_by_focus(&mut self, action: Action) -> (DispatchOutcome, Option<Action>) {
        let mut ctx = self.make_ctx();
        match self.focus {
            Focus::Input => {
                let action = match action {
                    Action::ScrollUp => Action::InputHistoryPrev,
                    Action::ScrollDown => Action::InputHistoryNext,
                    Action::ScrollTop => Action::InputHome,
                    Action::ScrollBottom => Action::InputEnd,
                    other => other,
                };
                let follow = self.input.handle(&action, &mut ctx);
                (DispatchOutcome::Continue, follow)
            }
            Focus::Transcript => {
                let action = match action {
                    Action::InputHistoryPrev => Action::ScrollUp,
                    Action::InputHistoryNext => Action::ScrollDown,
                    Action::InputHome => Action::ScrollTop,
                    Action::InputEnd => Action::ScrollBottom,
                    Action::InputChar('g') => Action::ScrollTop,
                    Action::InputChar('G') => Action::ScrollBottom,
                    other => other,
                };
                let follow = self.transcript.handle(&action, &mut ctx);
                (DispatchOutcome::Continue, follow)
            }
            Focus::Sidebar => {
                // Sidebar panels are presentational in P2b; input is
                // swallowed (Tab cycles back).
                let _ = self.agents.handle(&action, &mut ctx);
                let follow = self.session.handle(&action, &mut ctx);
                (DispatchOutcome::Continue, follow)
            }
        }
    }

    /// Spinner advance on tick (one frame per tick while animated) plus
    /// the transient-notice countdown.
    fn on_tick(&mut self) {
        if self.run_state.is_animated() {
            self.spinner_frame =
                (self.spinner_frame + 1) % crate::components::status::SPINNER_FRAMES.len();
        }
        if self.notice_ticks > 0 {
            self.notice_ticks -= 1;
            if self.notice_ticks == 0 {
                self.status_notice = None;
            }
        }
    }

    /// Post a transient status-bar notice (yellow; auto-clears after
    /// [`NOTICE_TICKS`] ticks or on the next `StartTurn`).
    fn set_notice(&mut self, msg: impl Into<String>) {
        self.status_notice = Some(msg.into());
        self.notice_ticks = NOTICE_TICKS;
    }

    /// Flip mouse capture (Ctrl+M / `/mouse`) and report the new state
    /// plus its trade-off as a transient notice (ASCII — the notice
    /// renders inside the REVERSED status-bar row). The run loop
    /// applies the actual terminal escape on the next iteration.
    fn toggle_mouse_capture(&mut self) {
        self.mouse_capture = !self.mouse_capture;
        let msg = if self.mouse_capture {
            "mouse capture on - wheel scrolls chat"
        } else {
            "mouse capture off - text selectable; wheel = arrow keys"
        };
        self.set_notice(msg);
    }

    /// Copy the LAST assistant message's raw markdown to the system
    /// clipboard via OSC 52 (Ctrl+Y / `/copy`, fix-13): truncate to
    /// [`clipboard::COPY_MAX_BYTES`] (char-boundary safe), build the
    /// escape, write it through the sink IMMEDIATELY (real stdout in
    /// production — same channel as the mouse-capture flip; the
    /// terminal consumes the sequence at once so the next draw is
    /// unaffected), then confirm with a transient ASCII notice. Notices
    /// follow the existing first-write-wins-clears lifecycle
    /// ([`Self::set_notice`]).
    fn copy_last_output(&mut self) {
        let Some(full) = self.last_assistant_text() else {
            self.set_notice("nothing to copy");
            return;
        };
        let total_chars = full.chars().count();
        let (payload, copied_chars) = {
            let text = clipboard::truncate_for_copy(full);
            (clipboard::osc52_payload(text), text.chars().count())
        };
        if (self.clipboard_out)(&payload) {
            let msg = if copied_chars < total_chars {
                format!("copied {copied_chars} of {total_chars} chars")
            } else {
                format!("copied {copied_chars} chars")
            };
            self.set_notice(msg);
        } else {
            self.set_notice("clipboard write failed");
        }
    }

    /// The most recent committed assistant entry's RAW markdown (the
    /// entries store the original text — markdown rendering happens at
    /// draw time, so the source for copying needs no reconstruction).
    /// Live-only entries (reasoning/meta/tool rows) and the still-
    /// streaming buffer are not assistant output and never match.
    fn last_assistant_text(&self) -> Option<&str> {
        self.transcript
            .entries()
            .iter()
            .rev()
            .find_map(|entry| match entry {
                crate::components::transcript::TranscriptEntry::Assistant(text) => {
                    Some(text.as_str())
                }
                _ => None,
            })
    }

    /// Whether the sidebar is rendered at the current terminal width:
    /// the explicit `Ctrl+T` toggle AND ≥ [`SIDEBAR_MIN_COLS`] columns.
    fn sidebar_active(&self) -> bool {
        self.sidebar_visible && self.size.0 >= SIDEBAR_MIN_COLS
    }

    /// The effective modal layer (computed stack).
    fn active_layer(&self) -> Layer {
        if self.approval.has_pending() {
            Layer::ApprovalActive
        } else {
            match self.modal {
                Modal::HelpOpen => Layer::HelpOpen,
                Modal::ConfirmExit => Layer::ConfirmExit,
                Modal::None => Layer::Normal,
            }
        }
    }

    // ── Engine events ──────────────────────────────────────────────────

    async fn handle_engine_event(&mut self, event: TuiEvent) -> (DispatchOutcome, Option<Action>) {
        // State machine first (single source of truth).
        crate::components::status::transition(&mut self.run_state, &event);

        match event {
            TuiEvent::Delta(text) => {
                self.transcript.push_delta(&text);
            }
            TuiEvent::Reasoning(text) => {
                self.transcript.push_reasoning(&text);
            }
            TuiEvent::ToolStart { name, args } => {
                // Live tool-call counter (session panel `cur/max`).
                self.tool_calls_cur = self.tool_calls_cur.saturating_add(1);
                self.transcript.tool_start(&name, &args);
                if name == "call_agent" {
                    let agent = crate::components::status::delegate_target(&args);
                    self.agents.on_delegate_start(&agent);
                    // Live delegation depth: push on start…
                    self.depth_cur = self.depth_cur.saturating_add(1);
                }
            }
            TuiEvent::ToolEnd {
                name,
                bytes,
                truncated,
            } => {
                self.transcript.tool_end(&name, bytes, truncated);
                self.agents.on_delegate_end(&name);
                if name == "call_agent" {
                    // …pop on end (root-visible delegations are depth-1;
                    // deeper children are invisible until TurnDone, D2).
                    self.depth_cur = self.depth_cur.saturating_sub(1);
                }
            }
            // Per-request telemetry plumbing: RequestStart starts the
            // clock (and resets the TTFT mark), FirstToken records
            // the TTFT numerator (and freezes the live stats row's
            // precise ttft, fix-19), Usage stores the precise counts
            // (they may arrive before OR after the deltas depending
            // on the provider), RequestEnd commits the streamed
            // blocks and HOLDS the usage line (fix-19: it lands below
            // the step's tool rows — `tool_start` consumes the hold;
            // a tool-less step's hold flushes here at the NEXT
            // RequestStart or at TurnDone's merge, at the answer's
            // tail).
            TuiEvent::RequestStart { .. } => {
                self.request_started = Some(Instant::now());
                self.first_token_at = None;
                self.pending_usage = None;
                // A hold that reached the next request means the
                // previous step had no tools — its stats land at that
                // step's answer tail now; the live stats row resets
                // for the new request.
                self.transcript.flush_pending_step_meta();
                self.transcript.set_live_ttft(None);
            }
            TuiEvent::FirstToken => {
                // First content/reasoning token of the current
                // request: the TTFT mark (first arrival wins — a
                // provider double-firing must not restart the clock).
                if self.first_token_at.is_none() {
                    let now = Instant::now();
                    // fix-19: the precise ttft is frozen the moment
                    // the first token arrives — feed the live stats
                    // row (rendered only while the ANSWER streams).
                    let ttft = self
                        .request_started
                        .and_then(|start| now.checked_duration_since(start));
                    self.first_token_at = Some(now);
                    self.transcript.set_live_ttft(ttft);
                }
            }
            TuiEvent::Usage(usage) => {
                self.pending_usage = Some(usage);
                self.transcript.step_end();
            }
            TuiEvent::RequestEnd => {
                let started = self.request_started.take();
                let elapsed = started.map(|t| t.elapsed());
                // TTFT = FirstToken − RequestStart (`checked` because
                // Instant subtraction panics on a would-be negative).
                let ttft = match (self.first_token_at.take(), started) {
                    (Some(first), Some(start)) => first.checked_duration_since(start),
                    _ => None,
                };
                let usage = self.pending_usage.take();
                // Commits the streamed blocks + the reasoning estimate;
                // the exact usage line is HELD inside the transcript
                // (fix-19) until tool_start / the next RequestStart /
                // TurnDone proves where the step ended. The live stats
                // row dies with this call (the held line is its
                // successor).
                self.transcript.finish_request(usage, elapsed, ttft);
                self.transcript.step_end();
            }
            TuiEvent::StepEnd => {
                self.transcript.step_end();
            }
            TuiEvent::ApprovalRequested { id, request } => {
                self.approval.enqueue(id, request);
            }
            TuiEvent::TurnDone(result) => {
                return self.handle_turn_done(result).await;
            }
        }
        (DispatchOutcome::Continue, None)
    }

    async fn handle_turn_done(
        &mut self,
        result: Result<(TurnSummary, RunManager), (String, Option<RunManager>)>,
    ) -> (DispatchOutcome, Option<Action>) {
        // Reap the finished task (TurnDone was its last send; joining is
        // immediate).
        if let Some(task) = self.engine_task.take() {
            let _ = task.await;
        }
        // Capture the turn duration before clearing the start instant —
        // feeds the transcript's end-of-turn marker (set_turn_meta).
        let turn_elapsed = self
            .turn_started
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(0);
        self.turn_started = None;

        match result {
            Ok((summary, manager)) => {
                self.manager = Some(manager);
                // THE merge point for successful turns: the committed
                // entries are the event-ordered authority, so the
                // thinking blocks and per-request meta lines SURVIVE —
                // `merge_turn` only folds tool results in from the
                // messages (+ appends assistant content the transcript
                // never saw). The wholesale `rebuild` stays as the
                // recovery fallback (the Err store-reload path below,
                // a future /resume) where losing live-only entries is
                // acceptable. `history` below stays the engine-side
                // authority regardless — display and history are
                // decoupled.
                self.transcript.set_turn_meta(
                    summary.model.clone(),
                    turn_elapsed,
                    Some((summary.total_input_tokens, summary.total_output_tokens)),
                );
                self.transcript.merge_turn(&summary.messages);
                self.agents.calibrate(&summary.execution_tree);
                self.history = summary.messages;
                self.stats.total_steps += summary.total_steps;
                self.stats.total_input_tokens += summary.total_input_tokens;
                self.stats.total_output_tokens += summary.total_output_tokens;
                self.stats.total_cost_usd += summary.total_cost_usd;
                self.stats.turns += 1;
                // Counter calibration (turn over): the execution tree is
                // all-terminal → live depth back to 0. tool_calls_cur
                // intentionally KEEPS the finished turn's count for the
                // session panel until the next turn resets it.
                self.depth_cur = 0;
                // Pending input auto-submits as the next turn.
                if let Some(text) = self.pending_input.take() {
                    return self.handle_start_turn(text).await;
                }
            }
            Err((msg, manager)) => {
                if let Some(manager) = manager {
                    self.manager = Some(manager);
                }
                self.run_state = RunState::Error(msg.clone());
                // fix-19 兜底: a held step meta (its request completed,
                // then the turn died before tools/next-request proved
                // the step's shape) materializes at the tail — the
                // stats the user watched live are not lost. Then the
                // buffers drop (the partial streamed text is the
                // Err-path's accepted loss).
                self.transcript.flush_pending_step_meta();
                self.transcript.clear_streaming();
                self.depth_cur = 0;
                // Reload the persisted transcript so in-memory history
                // matches what /resume would restore (REPL parity).
                self.reload_history_after_error().await;
                tracing::warn!("turn failed: {msg}");
                // No auto-retry: restore pending input to the editor.
                if let Some(text) = self.pending_input.take() {
                    self.input.set_text(&text);
                }
            }
        }
        (DispatchOutcome::Continue, None)
    }

    /// Err-path history reconciliation (repl.rs:1010-1022 port):
    /// reload the persisted transcript when a session run + store
    /// exist. On a successful reload the transcript is REBUILT from
    /// the reloaded messages — the recovery fallback path (the
    /// live-only reasoning/meta entries are lost here, acceptable in
    /// the error scenario, and display == persisted truth for the
    /// next turn). Without a store the already-flushed live entries
    /// stay visible and the pushed user message remains the history.
    async fn reload_history_after_error(&mut self) {
        if let (Some(run), Some(store)) = (&self.session_run, self.store.clone()) {
            match RunRecorder::load_messages(&store, &run.run_id.0).await {
                Ok(msgs) if !msgs.is_empty() => {
                    self.transcript.rebuild(&msgs);
                    self.history = msgs;
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!("failed to reload persisted history: {e}");
                }
            }
        }
        // Without a store the already-pushed user message stays.
    }

    // ── Turn lifecycle ─────────────────────────────────────────────────

    /// Handle a submitted prompt: slash routing + turn spawn.
    async fn handle_start_turn(&mut self, text: String) -> (DispatchOutcome, Option<Action>) {
        if let Some(stripped) = text.strip_prefix("//") {
            // `//`-escape (REPL semantics): strip ONE slash, send "/text"
            // as a normal prompt.
            return self.start_turn(format!("/{stripped}")).await;
        }
        if text.starts_with('/') {
            return self.handle_slash(&text).await;
        }
        self.start_turn(text).await
    }

    /// Spawn one engine turn (REPL `handle_normal_input` port). Steps:
    /// push/persist the user message → install the sink → (deferred)
    /// auto-compact → [`Self::finish_start_turn`] builds the provider and
    /// spawns the engine task.
    #[allow(clippy::too_many_lines)]
    async fn start_turn(&mut self, prompt: String) -> (DispatchOutcome, Option<Action>) {
        if prompt.trim().is_empty() {
            return (DispatchOutcome::Continue, None);
        }
        if self.is_running() || self.compact_pending {
            // Queued; auto-submitted on TurnDone (spec: pending_input).
            // Also queued while a compaction is pending so the user
            // message is not double-pushed.
            self.pending_input = Some(prompt);
            return (DispatchOutcome::Continue, None);
        }
        // A new turn clears any transient notice (e.g. a rejected /new).
        self.status_notice = None;
        self.notice_ticks = 0;
        self.tool_calls_cur = 0;
        self.depth_cur = 0;

        let user_message = Message {
            role: MessageRole::User,
            content: prompt.clone(),
            tool_call_id: None,
            name: None,
            tool_calls: None,
        };
        self.history.push(user_message.clone());
        self.transcript.push_user(&prompt);

        // Session wiring: lazy shared run + persist user message + sink.
        let session_run = self.current_run().await;
        if let Some(run) = &session_run {
            if let Err(e) = run.recorder.write_message(&user_message).await {
                tracing::warn!("failed to persist user message: {e}");
            }
        }
        if let Some(manager) = self.manager.as_mut() {
            manager.message_sink = session_run
                .as_ref()
                .map(|r| r.recorder.clone() as Arc<dyn MessageSink>);
        }

        // Auto-compact before the turn (REPL parity). DEFERRED by one
        // dispatch round: setting `RunState::Compacting` here lets the
        // loop's draw paint the ` compacting…` badge BEFORE the blocking
        // summary call (up to `timeout_ms` on a real provider) runs at
        // the top of the next dispatched action — never a silent freeze.
        let (max_msgs, max_bytes) = self.context_limits();
        if self.auto_compact_enabled() && needs_compact(&self.history, max_msgs, max_bytes, 0) {
            self.run_state = RunState::Compacting;
            self.compact_pending = true;
            return (DispatchOutcome::Continue, None);
        }
        self.finish_start_turn().await
    }

    /// Run the deferred compaction, then continue the queued turn spawn
    /// (invoked from the top of `dispatch_one` once the badge painted).
    async fn run_deferred_compact(&mut self) {
        let result = self.run_compact().await;
        tracing::info!(
            "auto-compact: {} → {} messages",
            result.messages_before,
            result.messages_after
        );
        // Falls back through the spawn: Thinking (or Error on provider
        // failure) overwrites the Compacting state here.
        let _ = self.finish_start_turn().await;
    }

    /// Post-compaction turn steps: provider build + engine spawn. The
    /// user message is already pushed and persisted; on provider failure
    /// it is popped and restored to the editor.
    async fn finish_start_turn(&mut self) -> (DispatchOutcome, Option<Action>) {
        let prompt = self
            .history
            .last()
            .filter(|m| m.role == MessageRole::User)
            .map(|m| m.content.clone());

        // Provider per turn (REPL rebuilds it too — /model switches).
        let model_alias = self.effective_model_alias();
        let provider = match (self.provider_factory)(&self.config, &model_alias) {
            Ok(provider) => provider,
            Err(e) => {
                // Keep the session alive: undo the push, restore the text.
                if matches!(self.history.last(), Some(m) if m.role == MessageRole::User) {
                    self.history.pop();
                }
                self.run_state = RunState::Error(e.to_string());
                if let Some(prompt) = prompt {
                    self.input.set_text(&prompt);
                }
                return (DispatchOutcome::Continue, None);
            }
        };

        let run_id = self
            .session_run
            .as_ref()
            .map(|r| r.run_id.clone())
            .unwrap_or_else(RunManager::new_run_id);

        let cancel = CancellationToken::new();
        self.cancel_token = cancel.clone();
        self.run_state = RunState::Thinking;
        self.turn_started = Some(Instant::now());
        self.transcript.begin_streaming();

        let Some(manager) = self.manager.take() else {
            self.run_state = RunState::Error("manager unavailable".into());
            return (DispatchOutcome::Continue, None);
        };
        self.engine_task = Some(spawn_turn(
            manager,
            run_id,
            provider,
            self.history.clone(),
            cancel,
            self.events_tx.clone(),
        ));
        (DispatchOutcome::Continue, None)
    }

    /// `/`-command routing (TUI subset; panels themselves are P3).
    async fn handle_slash(&mut self, input: &str) -> (DispatchOutcome, Option<Action>) {
        match slash::parse(input) {
            SlashCommand::Exit => (DispatchOutcome::Quit, None),
            SlashCommand::Help => {
                self.modal = Modal::HelpOpen;
                (DispatchOutcome::Continue, None)
            }
            SlashCommand::New => {
                if self.is_running() || self.compact_pending {
                    // Clearing mid-turn would race the TurnDone rebuild;
                    // cancel first, then /new again. A transient notice
                    // (yellow), NOT RunState::Error — the red  misreads
                    // as a turn failure.
                    self.set_notice("turn running — Ctrl+C to cancel, then /new");
                    return (DispatchOutcome::Continue, None);
                }
                self.history.clear();
                self.transcript.clear();
                // Close the backing run; the next turn opens a fresh one.
                if let Some(run) = self.session_run.take() {
                    let cost = self.stats.run_cost_usd();
                    if let Err(e) = run.recorder.finish("completed", None, cost).await {
                        tracing::warn!("failed to persist previous session run: {e}");
                    }
                }
                self.stats = SessionStats::new();
                (DispatchOutcome::Continue, None)
            }
            SlashCommand::Model { alias } => {
                let known = self.config.models.contains_key(&alias)
                    || openslate_core::model_config::resolve_model(&self.config, &alias).is_ok();
                if known {
                    self.model_override = Some(alias.clone());
                    self.set_notice(format!("model → {alias}"));
                } else {
                    self.run_state = RunState::Error(format!(
                        "unknown model alias '{alias}' (available: {})",
                        self.config
                            .models
                            .keys()
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                (DispatchOutcome::Continue, None)
            }
            SlashCommand::Mouse => {
                // Same flip as Ctrl+M (select-to-copy vs wheel).
                self.toggle_mouse_capture();
                (DispatchOutcome::Continue, None)
            }
            SlashCommand::Copy => {
                // Same path as Ctrl+Y (OSC 52 clipboard copy).
                self.copy_last_output();
                (DispatchOutcome::Continue, None)
            }
            SlashCommand::Status | SlashCommand::Agents => {
                // Pre-wired minimal behavior (panels render in P3 lane-b):
                // force the sidebar visible and focus it so the command
                // already lands somewhere the user can see. On a narrow
                // terminal the width guard keeps the sidebar hidden —
                // then keep the current focus (keys must not be swallowed
                // by an invisible panel).
                self.sidebar_visible = true;
                if self.sidebar_active() {
                    self.focus = Focus::Sidebar;
                }
                (DispatchOutcome::Continue, None)
            }
            SlashCommand::Unknown { raw } => {
                self.run_state = RunState::Error(format!("unknown command: {raw} (try /help)"));
                (DispatchOutcome::Continue, None)
            }
        }
    }

    /// Lazily open (or return) the persisted session run (repl.rs:750-767
    /// port). `None` when there is no store or the insert failed — the
    /// session then runs unpersisted.
    async fn current_run(&mut self) -> Option<SessionRun> {
        if let Some(run) = &self.session_run {
            return Some(run.clone());
        }
        let store = self.store.clone()?;
        let root_agent_id = self.agent_tree.get_root().id.0.clone();
        let run_id = RunManager::new_run_id();
        match RunRecorder::begin(
            store,
            run_id.clone(),
            &root_agent_id,
            Some("tui session"),
            r#"{"kind":"tui"}"#,
        )
        .await
        {
            Ok(recorder) => {
                let run = SessionRun {
                    run_id,
                    recorder: Arc::new(recorder),
                };
                self.session_run = Some(run.clone());
                Some(run)
            }
            Err(e) => {
                tracing::warn!("session persistence unavailable ({e}); running unpersisted");
                None
            }
        }
    }

    // ── Compaction (repl.rs run_compact port) ──────────────────────────

    /// Effective context limits for compaction decisions.
    fn context_limits(&self) -> (usize, usize) {
        (
            self.config
                .limits
                .as_ref()
                .map(|l| l.max_context_messages as usize)
                .unwrap_or(16),
            self.config
                .limits
                .as_ref()
                .map(|l| l.max_context_bytes as usize)
                .unwrap_or(64_000),
        )
    }

    /// Whether auto-compact is enabled (default on).
    fn auto_compact_enabled(&self) -> bool {
        self.config
            .limits
            .as_ref()
            .map(|l| l.auto_compact)
            .unwrap_or(true)
    }

    /// Compact the in-memory history through the `fast` model when
    /// available (mechanical fallback otherwise); compact-usage cost is
    /// credited to the session but not the run row.
    async fn run_compact(&mut self) -> openslate_core::context_manager::CompactResult {
        let summary_plan: Option<(String, Box<dyn ModelProvider>, CostSpec)> = {
            match openslate_core::model_config::resolve_model(&self.config, "fast") {
                Ok(resolved) => match (self.provider_factory)(&self.config, "fast") {
                    Ok(provider) => {
                        let pricing = resolved.cost_spec();
                        Some((resolved.model_id, provider, pricing))
                    }
                    Err(e) => {
                        tracing::debug!("no provider for 'fast' — mechanical fallback: {e}");
                        None
                    }
                },
                Err(e) => {
                    tracing::debug!("no 'fast' alias — mechanical fallback: {e}");
                    None
                }
            }
        };
        let summary_pricing = summary_plan
            .as_ref()
            .map(|(_, _, pricing)| *pricing)
            .unwrap_or_default();

        let usage_slot = Arc::new(std::sync::Mutex::new(None::<Usage>));
        let slot = Arc::clone(&usage_slot);
        let (max_messages, max_bytes) = self.context_limits();

        let result = compact(
            &mut self.history,
            None,
            max_messages,
            max_bytes,
            move |text| {
                let text = text.to_owned();
                async move {
                    let (model_id, provider, _pricing) = summary_plan?;
                    let (summary, usage) =
                        generate_summary(provider.as_ref(), &model_id, &text).await;
                    if let Some(u) = usage {
                        *slot.lock().expect("compact usage slot poisoned") = Some(u);
                    }
                    summary
                }
            },
        )
        .await;

        if let Some(usage) = usage_slot
            .lock()
            .expect("compact usage slot poisoned")
            .take()
        {
            self.stats.total_input_tokens += usage.input_tokens as u64;
            self.stats.total_output_tokens += usage.output_tokens as u64;
            let cost = summary_pricing.cost_of(&usage);
            self.stats.total_cost_usd += cost;
            self.stats.compact_cost_usd += cost;
        }
        result
    }

    // ── Approvals ──────────────────────────────────────────────────────

    /// Deliver the user's answer to the front pending approval.
    fn approval_respond(&mut self, choice: ApprovalChoice) {
        if let Some(pending) = self.approval.pop_answered() {
            let label = match choice {
                ApprovalChoice::Approve => "approved",
                ApprovalChoice::Deny => "denied",
                ApprovalChoice::ApproveAll => "approve-all",
            };
            self.transcript
                .push_approval(&pending.summary.tool_name, label);
            if !self.approval_bridge.respond(pending.id, choice) {
                tracing::warn!(
                    "approval {id} no longer pending (answer dropped)",
                    id = pending.id
                );
            }
            if self.run_state == RunState::ApprovalPending {
                // The engine continues from here; its next event corrects.
                self.run_state = RunState::Thinking;
            }
        }
    }

    // ── Context / rendering ────────────────────────────────────────────

    /// Effective model alias (override > root agent).
    fn effective_model_alias(&self) -> String {
        self.model_override
            .clone()
            .unwrap_or_else(|| self.agent_tree.get_root().model_alias.clone())
    }

    /// Owned AppCtx snapshot for component calls.
    fn make_ctx(&self) -> AppCtx {
        let alias = self.effective_model_alias();
        let resolved = openslate_core::model_config::resolve_model(&self.config, &alias);
        let (model_id, provider_name) = match &resolved {
            Ok(r) => (r.model_id.clone(), r.provider_name.clone()),
            Err(_) => (alias.clone(), "?".to_owned()),
        };
        AppCtx {
            theme: self.theme,
            focus: self.focus,
            run: RunInfo {
                state: self.run_state.clone(),
                spinner_frame: self.spinner_frame,
                model_label: format!("{alias}@{provider_name}"),
                tokens_in: self.stats.total_input_tokens,
                tokens_out: self.stats.total_output_tokens,
                cost_usd: self.stats.total_cost_usd,
                elapsed: self.turn_started.map(|t| t.elapsed()),
                tool_calls_cur: self.tool_calls_cur,
                depth_cur: self.depth_cur,
            },
            config: ConfigSummary {
                model_alias: alias,
                model_id,
                provider_name,
                max_depth: self
                    .config
                    .limits
                    .as_ref()
                    .map(|l| l.max_depth)
                    .unwrap_or(4),
                max_tool_calls: self
                    .config
                    .limits
                    .as_ref()
                    .map(|l| l.max_tool_calls)
                    .unwrap_or(20),
                run_id: self.session_run_id().map(str::to_owned),
            },
            size: self.size,
            notice: self.status_notice.clone(),
        }
    }

    /// Full-screen render routing (frozen call sites): transcript /
    /// sidebar (agents+session) / input / status bar, plus overlays
    /// (approval banner, help, exit confirm) and the input cursor.
    fn render(&mut self, f: &mut ratatui::Frame) {
        let area = f.area();
        self.size = (area.width, area.height);
        let ctx = self.make_ctx();

        // Minimum-size guard: central warning overlay + no fragile layout
        // (lane-c painter; `render` returns right after — nothing below
        // the minimum size ever reaches the layout code).
        if area.width < MIN_COLS || area.height < MIN_ROWS {
            Self::render_min_size_guard(f, area, &self.theme);
            return;
        }

        // Borderless layout (layout rework): wide terminals keep the
        // right sidebar; narrow terminals replace it with a one-row
        // info footer above the input (gated by `sidebar_visible`, so
        // Ctrl+T toggles it). The input is a single row either way.
        let wide = self.sidebar_active() && area.width > SIDEBAR_WIDTH + 23;
        let footer_height: u16 = if !wide && self.sidebar_visible { 1 } else { 0 };
        // [main region][1 blank row][info footer (narrow only)][input
        // row][status bar]. The blank row is the transcript↔input
        // separator; the footer row collapses to zero when hidden.
        let [main, gap_row, info_row, input_area, status_area] =
            area.layout(&ratatui::layout::Layout::vertical([
                ratatui::layout::Constraint::Min(3),
                ratatui::layout::Constraint::Length(1),
                ratatui::layout::Constraint::Length(footer_height),
                ratatui::layout::Constraint::Length(self.input.desired_height()),
                ratatui::layout::Constraint::Length(1),
            ]));
        let _ = gap_row; // intentionally empty (the separator row)

        // Main column: unbordered block, content padded 2 columns each
        // side (the borderless design language's whitespace framing).
        let main_pad =
            ratatui::widgets::Block::new().padding(ratatui::widgets::Padding::horizontal(2));

        // Wide layout: [main column][gap][┃ divider][gap][sidebar 30].
        // The divider column carries the region separator (full height
        // of the main region; it disappears with the sidebar).
        let mut input_width = main.width; // narrow: the full width
        if wide {
            let [transcript_col, _gap_a, divider_col, _gap_b, sidebar] =
                main.layout(&ratatui::layout::Layout::horizontal([
                    ratatui::layout::Constraint::Fill(1),
                    ratatui::layout::Constraint::Length(1),
                    ratatui::layout::Constraint::Length(1),
                    ratatui::layout::Constraint::Length(1),
                    ratatui::layout::Constraint::Length(SIDEBAR_WIDTH),
                ]));
            input_width = transcript_col.width;
            self.transcript
                .render(f, main_pad.inner(transcript_col), &ctx);
            Self::render_bar_divider(f, divider_col, &self.theme);
            // Sidebar panels: the area is handed to the components as-is.
            let [agents_area, session_area] = sidebar.layout(&ratatui::layout::Layout::vertical([
                ratatui::layout::Constraint::Fill(1),
                ratatui::layout::Constraint::Fill(1),
            ]));
            self.agents.render(f, agents_area, &ctx);
            self.session.render(f, session_area, &ctx);
        } else {
            self.transcript.render(f, main_pad.inner(main), &ctx);
            // The narrow info footer (sidebar replacement): compact
            // agents line + session counters, one row, no frame.
            if footer_height > 0 {
                Self::render_info_footer(f, info_row, &self.agents, &ctx);
            }
        }

        // Input row: single line, aligned with the transcript COLUMN on
        // wide layouts (the area right of it — under the sidebar —
        // stays empty); full width when narrow.
        let input_rect = Rect {
            width: input_width,
            ..input_area
        };
        self.input.render(f, input_rect, &ctx);

        // Narrow-terminal degradation: below 100 columns the sidebar is
        // gone, so its model info condenses into the status bar as the
        // short alias (no `@provider` suffix).
        if wide {
            self.status.render(f, status_area, &ctx);
        } else {
            let mut narrow = ctx.clone();
            narrow.run.model_label = ctx.config.model_alias.clone();
            self.status.render(f, status_area, &narrow);
        }

        // Modal stack, strictly by frozen precedence: ApprovalActive >
        // HelpOpen > ConfirmExit > Normal — exactly one overlay layer
        // paints per frame. The approval banner is a transcript-bottom
        // banner rather than a full-screen modal (non-modal: the base
        // UI behind it is NOT dimmed), but it still preempts the help /
        // exit-confirm overlays: `active_layer()` stays
        // `ApprovalActive` until the queue drains, so anything opened
        // underneath simply waits and repaints afterwards (unchanged
        // P2b semantics, made explicit here).
        match self.active_layer() {
            Layer::ApprovalActive => {
                // Bottom of the transcript/main region (existing P2b
                // placement), clamped so the banner never covers the
                // whole region.
                let banner_height = 5u16.min(main.height.saturating_sub(1));
                let banner = ratatui::layout::Rect {
                    x: main.x,
                    y: main.y + main.height - banner_height,
                    width: main.width,
                    height: banner_height,
                };
                self.approval.render(f, banner, &ctx);
            }
            Layer::HelpOpen => {
                let inner = Self::help_overlay_rect(area);
                // Occlusion semantics: dim the base UI OUTSIDE the
                // overlay before it paints (the interior is never
                // dimmed — Clear resets it).
                Self::dim_outside(f.buffer_mut(), inner);
                f.render_widget(ratatui::widgets::Clear, inner);
                self.help.render(f, inner, &ctx);
            }
            Layer::ConfirmExit => {
                let inner = Self::exit_confirm_rect(area);
                Self::dim_outside(f.buffer_mut(), inner);
                Self::render_exit_confirm(f, area, &self.theme);
            }
            Layer::Normal => {}
        }

        // Input cursor (only when editing and no modal).
        if self.focus == Focus::Input && self.active_layer() == Layer::Normal {
            if let Some(position) = self.input.cursor_position(input_rect) {
                f.set_cursor_position(position);
            }
        }
    }

    // ── borderless layout painters (Wave 1 render-section helpers) ────

    /// Full-height `┃` region-separator column between the main column
    /// and the sidebar (DarkGray via `theme.bar_divider`; hidden together
    /// with the sidebar). The borderless design language's replacement
    /// for the old framed panels.
    fn render_bar_divider(f: &mut ratatui::Frame, area: ratatui::layout::Rect, theme: &Theme) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let strip = vec![Line::from("┃"); area.height as usize];
        let paragraph = ratatui::widgets::Paragraph::new(ratatui::text::Text::from(strip))
            .style(theme.bar_divider);
        f.render_widget(paragraph, area);
    }

    /// The narrow-terminal info footer (layout rework): ONE row that
    /// replaces the hidden sidebar — the compact agents line
    /// ([`AgentsComponent::footer_spans`]) + a dim ` │ ` separator +
    /// the session counters (`d<depth> <cur/max> · t<tools> <cur/max>`
    /// and, once a run backs the session, ` · <run id first 8>`).
    /// Pure-ASCII session part, no frame; the Paragraph clips at the
    /// line width naturally.
    fn render_info_footer(
        f: &mut ratatui::Frame,
        area: Rect,
        agents: &crate::components::AgentsComponent,
        ctx: &AppCtx,
    ) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let mut spans = agents.footer_spans(&ctx.theme);
        spans.push(Span::styled(" │ ".to_owned(), ctx.theme.fine));
        let mut session = format!(
            "d{}/{} · t{}/{}",
            ctx.run.depth_cur,
            ctx.config.max_depth,
            ctx.run.tool_calls_cur,
            ctx.config.max_tool_calls
        );
        if let Some(run_id) = &ctx.config.run_id {
            session.push_str(" · ");
            session.push_str(&run_id.chars().take(8).collect::<String>());
        }
        spans.push(Span::styled(session, ctx.theme.muted));
        f.render_widget(
            ratatui::widgets::Paragraph::new(Line::from(spans)),
            Rect { height: 1, ..area },
        );
    }

    /// Overlay rect for the help layer (60% × 70%, centered). Extracted
    /// so the dimming pass and the painter (and tests) agree on the
    /// exact geometry.
    fn help_overlay_rect(area: Rect) -> Rect {
        area.centered(
            ratatui::layout::Constraint::Percentage(60),
            ratatui::layout::Constraint::Percentage(70),
        )
    }

    /// Overlay rect for the exit-confirmation layer (44% × 5 rows,
    /// centered). Same extraction rationale as [`Self::help_overlay_rect`].
    fn exit_confirm_rect(area: Rect) -> Rect {
        area.centered(
            ratatui::layout::Constraint::Percentage(44),
            ratatui::layout::Constraint::Length(5),
        )
    }

    /// Occlusion dimming (layout rework): add `Modifier::DIM` to every
    /// cell OUTSIDE `keep` — the four strips around it (above / below /
    /// left / right) via [`Buffer::set_style`], never cell-by-cell.
    /// Called after the base UI painted and BEFORE the overlay itself
    /// (which `Clear`s its own rect), so the overlay interior is never
    /// dimmed. No background color is painted — the DIM modifier reads
    /// correctly on light and dark terminals alike.
    fn dim_outside(buf: &mut Buffer, keep: Rect) {
        let screen = buf.area;
        let style = Style::new().add_modifier(Modifier::DIM);
        // Top / bottom strips span the full screen width.
        if keep.y > screen.y {
            buf.set_style(
                Rect {
                    y: screen.y,
                    height: keep.y - screen.y,
                    ..screen
                },
                style,
            );
        }
        let keep_bottom = keep.y.saturating_add(keep.height);
        let screen_bottom = screen.y.saturating_add(screen.height);
        if keep_bottom < screen_bottom {
            buf.set_style(
                Rect {
                    y: keep_bottom,
                    height: screen_bottom - keep_bottom,
                    ..screen
                },
                style,
            );
        }
        // Left / right strips span only the overlay's rows (the corners
        // above/below are already covered by the first two strips).
        if keep.x > screen.x {
            buf.set_style(
                Rect {
                    x: screen.x,
                    width: keep.x - screen.x,
                    y: keep.y,
                    height: keep.height,
                },
                style,
            );
        }
        let keep_right = keep.x.saturating_add(keep.width);
        let screen_right = screen.x.saturating_add(screen.width);
        if keep_right < screen_right {
            buf.set_style(
                Rect {
                    x: keep_right,
                    width: screen_right - keep_right,
                    y: keep.y,
                    height: keep.height,
                },
                style,
            );
        }
    }

    // ── lane-c overlay painters (render-section helpers) ────────────

    /// Minimum-size guard painter (borderless, Wave 2 lane-c): blanks
    /// the screen, then centers the three warning lines — red error
    /// headline plus muted size/recovery detail. [`Self::render`]
    /// returns immediately after this — no layout runs below the
    /// minimum size, so a clipped terminal can never panic the
    /// splitter.
    fn render_min_size_guard(f: &mut ratatui::Frame, area: ratatui::layout::Rect, theme: &Theme) {
        f.render_widget(ratatui::widgets::Clear, area);
        let inner = area.centered(
            ratatui::layout::Constraint::Percentage(80),
            ratatui::layout::Constraint::Length(3),
        );
        let warning = ratatui::widgets::Paragraph::new(ratatui::text::Text::from(vec![
            ratatui::text::Line::styled(
                format!("终端过小，请 ≥ {MIN_COLS} 列 × {MIN_ROWS} 行"),
                theme.error,
            ),
            ratatui::text::Line::styled(
                format!("当前 {} × {}", area.width, area.height),
                theme.muted,
            ),
            ratatui::text::Line::styled("放大窗口后自动恢复", theme.muted),
        ]))
        .alignment(ratatui::layout::Alignment::Center);
        f.render_widget(warning, inner);
    }

    /// Exit-confirmation painter (`ConfirmExit` layer — lowest overlay
    /// priority), borderless (Wave 2 lane-c): a REVERSED title bar
    /// (` Exit? `) across the modal's full width — the help
    /// overlay's color-bar language — then the yellow warning and the
    /// key line, centered. `[y]/[q]` quit, `[Esc]` stays (key handling
    /// is the frozen dispatcher's job; this only paints).
    fn render_exit_confirm(f: &mut ratatui::Frame, area: ratatui::layout::Rect, theme: &Theme) {
        let inner = Self::exit_confirm_rect(area);
        f.render_widget(ratatui::widgets::Clear, inner);
        // Row 0 — REVERSED title bar padded out to the modal's full
        // width. Rendered as a bare Line widget so the style lands on
        // EVERY cell of the row (a Paragraph skips padding cells after
        // wide glyphs). CJK-aware: pad by display columns, not chars.
        let title = " Exit? ";
        let fill = " "
            .repeat((inner.width as usize).saturating_sub(ratatui::text::Span::raw(title).width()));
        let title_rect = ratatui::layout::Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: 1,
        };
        f.render_widget(
            ratatui::text::Line::styled(format!("{title}{fill}"), theme.overlay_title),
            title_rect,
        );
        // Post-pass: CJK in the title resets the style of the skip
        // cells after wide glyphs (same mechanism as the status bar's
        // black-gaps bug) — re-apply the treatment so the color bar
        // renders continuously.
        f.buffer_mut().set_style(title_rect, theme.overlay_title);
        // Rows 1.. — a breathing blank, the yellow warning, the key
        // line (vertically balanced inside the 5-row modal).
        let body = ratatui::text::Text::from(vec![
            ratatui::text::Line::default(),
            ratatui::text::Line::styled("未完成的轮次将被取消", theme.approval),
            ratatui::text::Line::from(vec![
                ratatui::text::Span::styled("[y/q/Ctrl+C] ", theme.approval),
                ratatui::text::Span::raw("退出  "),
                ratatui::text::Span::styled("[Esc] ", theme.approval),
                ratatui::text::Span::raw("取消"),
            ]),
        ]);
        f.render_widget(
            ratatui::widgets::Paragraph::new(body).alignment(ratatui::layout::Alignment::Center),
            ratatui::layout::Rect {
                x: inner.x,
                y: inner.y + 1,
                width: inner.width,
                height: inner.height.saturating_sub(1),
            },
        );
    }

    // ── Shutdown & summary ─────────────────────────────────────────────

    /// Ordered shutdown (spec R2): cancel token → deny pending approvals
    /// → await the engine task with a timeout (sink flush) → finalize the
    /// run row. Terminal restore and log flush are main's job.
    async fn shutdown(&mut self) {
        self.cancel_token.cancel();
        self.approval_bridge.deny_all();
        self.approval.clear();
        if let Some(task) = self.engine_task.take() {
            match tokio::time::timeout(SHUTDOWN_TIMEOUT, task).await {
                Ok(_) => {}
                Err(_) => {
                    tracing::warn!("engine task did not finish within 5s; aborting");
                }
            }
        }
        if let Some(run) = &self.session_run {
            let cost = self.stats.run_cost_usd();
            if let Err(e) = run.recorder.finish("completed", None, cost).await {
                tracing::warn!("failed to persist session completion: {e}");
            }
        }
    }

    /// The stdout one-liner printed after terminal restore.
    fn summary(&self) -> SessionSummary {
        SessionSummary {
            run_id: self.session_run_id().map(str::to_owned),
            turns: self.stats.turns,
            total_cost_usd: self.stats.total_cost_usd,
        }
    }
}

/// Merge adjacent `Engine(Delta)` actions in a drained batch (delta
/// flooding defense, spec R4). Public for the dispatch-level drain
/// pipeline tests (`dispatch` itself does not coalesce — coalescing
/// happens between the drain and the dispatch loop in `App::run`).
pub fn coalesce_deltas(batch: &mut Vec<Action>) {
    let mut out: Vec<Action> = Vec::with_capacity(batch.len());
    for action in batch.drain(..) {
        let merged = if let Action::Engine(TuiEvent::Delta(next)) = &action {
            if let Some(Action::Engine(TuiEvent::Delta(prev))) = out.last_mut() {
                prev.push_str(next);
                true
            } else {
                false
            }
        } else {
            false
        };
        if !merged {
            out.push(action);
        }
    }
    *batch = out;
}

/// One LLM summarization attempt for compaction (REPL port; degraded to
/// `None` on any failure — the mechanical fallback inside `compact`
/// takes over).
async fn generate_summary(
    provider: &dyn ModelProvider,
    model_id: &str,
    conversation_text: &str,
) -> (Option<String>, Option<Usage>) {
    let request = GenerateRequest {
        model_id: model_id.to_owned(),
        system_prompt: Some(SUMMARY_SYSTEM_PROMPT.to_owned()),
        messages: vec![Message {
            role: MessageRole::User,
            content: format!(
                "Summarize the following conversation for continuation:\n\n{}",
                conversation_text
            ),
            tool_call_id: None,
            name: None,
            tool_calls: None,
        }],
        tools: Vec::new(),
        max_tokens: None,
        temperature: None,
    };
    match provider.generate(request).await {
        Ok(response) => {
            let usage = response.usage;
            // An empty/blank reply is treated as failure → mechanical
            // fallback (an empty summary message would be worse).
            let summary = response.content.filter(|c| !c.trim().is_empty());
            (summary, usage)
        }
        Err(e) => {
            tracing::warn!("compact summary failed ({e}); mechanical fallback");
            (None, None)
        }
    }
}

#[cfg(test)]
mod overlay_render_tests {
    //! lane-c overlay painters: minimum-size guard + exit confirmation.
    //! (`App::render` itself needs a fully wired App; these tests cover
    //! the extracted painters — the component-level overlays are tested
    //! in their modules and in `tests/overlays_render.rs`.)

    use super::*;
    use ratatui::backend::TestBackend;

    /// Render through a TestBackend and return the screen rows (the
    /// backend's Display view wraps each row in quotes and may append a
    /// multi-width-overwrite note — take exactly the content between
    /// the first two quotes).
    fn draw(width: u16, height: u16, paint: impl FnOnce(&mut ratatui::Frame)) -> Vec<String> {
        let mut terminal =
            ratatui::Terminal::new(TestBackend::new(width, height)).expect("test terminal");
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

    #[test]
    fn min_size_guard_shows_the_spec_warning() {
        let rows = draw(40, 10, |f| {
            App::render_min_size_guard(f, f.area(), &Theme::new())
        });
        let screen = rows.join("\n");
        assert!(screen.contains("终端过小"), "guard shows the warning");
        assert!(
            screen.contains("≥ 60 列 × 12 行"),
            "guard names the minimum"
        );
        assert!(
            screen.contains("当前 40 × 10"),
            "guard reports the actual size"
        );
        // Borderless (Wave 2): no frame characters anywhere.
        for frame_char in ['┌', '┐', '└', '┘', '─', '│'] {
            assert!(
                !screen.contains(frame_char),
                "guard must be frameless, found {frame_char}"
            );
        }
        // No transcript/status content leaked around the guard: every
        // row outside the centered text is blank.
        assert!(rows.len() == 10);
    }

    #[test]
    fn min_size_guard_never_panics_on_absurd_sizes() {
        // Just-below-minimum and absurdly small terminals must not panic
        // the centered layout (the reason the guard exists).
        for (w, h) in [
            (1, 1),
            (2, 2),
            (10, 3),
            (5, 20),
            (59, 11),
            (59, 40),
            (80, 11),
        ] {
            draw(w, h, |f| {
                App::render_min_size_guard(f, f.area(), &Theme::new())
            });
        }
    }

    #[test]
    fn exit_confirm_shows_warning_and_choices() {
        let rows = draw(80, 24, |f| {
            App::render_exit_confirm(f, f.area(), &Theme::new())
        });
        let screen = rows.join("\n");
        assert!(screen.contains("Exit?"), "title");
        assert!(screen.contains("未完成的轮次将被取消"), "warning line");
        assert!(screen.contains("[y/q/Ctrl+C]"), "quit keys");
        assert!(screen.contains("[Esc]"), "cancel key");
        // Borderless (Wave 2): no frame characters anywhere.
        for frame_char in ['┌', '┐', '└', '┘'] {
            assert!(
                !screen.contains(frame_char),
                "exit confirm must be frameless, found {frame_char}"
            );
        }
    }

    #[test]
    fn exit_confirm_box_is_centered() {
        let rows = draw(80, 24, |f| {
            App::render_exit_confirm(f, f.area(), &Theme::new())
        });
        // 44% of 80 = 35 wide → the box starts around column 22; every
        // content row has a wide blank margin on both sides.
        let warning_row = rows
            .iter()
            .find(|r| r.contains("未完成的轮次将被取消"))
            .expect("warning row");
        let left_margin = warning_row
            .chars()
            .take_while(|c| c.is_whitespace())
            .count();
        assert!(
            (15..=30).contains(&left_margin),
            "horizontally centered (left margin {left_margin}): {warning_row:?}"
        );
        let title_row = rows
            .iter()
            .find(|r| r.contains("Exit?"))
            .expect("title row");
        // Vertically: Length(5) centered in 24 rows → title row ~9-11.
        let title_y = rows
            .iter()
            .position(|r| r.contains("Exit?"))
            .expect("title row index");
        assert!(
            (7..=12).contains(&title_y),
            "vertically centered (title at row {title_y}): {title_row:?}"
        );
    }

    /// The exit-confirm title row carries the REVERSED bar style across
    /// the modal's full width. Ratatui's buffer semantics reset the
    /// "skip" cell after each wide glyph (CJK), so those stay unstyled
    /// holes — every OTHER cell of the title row (and no other row) is
    /// reversed.
    #[test]
    fn exit_confirm_title_bar_is_reversed_full_width() {
        let mut terminal = ratatui::Terminal::new(TestBackend::new(80, 24)).expect("test terminal");
        terminal
            .draw(|f| App::render_exit_confirm(f, f.area(), &Theme::new()))
            .expect("painting must not panic");
        let buf = terminal.backend().buffer().clone();
        let title = " Exit? ";
        let wide = title
            .chars()
            .filter(|c| ratatui::text::Span::raw(c.to_string()).width() > 1)
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
            let first_rev = cells.iter().position(&is_rev).expect("a reversed cell");
            let last_rev = cells.iter().rposition(&is_rev).expect("a reversed cell");
            // The bar covers its whole extent — every cell between the
            // first and last reversed cell is reversed, except exactly
            // one skip hole per wide glyph in the title text.
            let bar_width = last_rev - first_rev + 1;
            assert_eq!(
                reversed,
                bar_width - wide,
                "bar spans its full width minus wide-glyph skips"
            );
            for c in &cells {
                if !is_rev(c) {
                    assert_eq!(c.symbol(), " ", "holes only inside the bar");
                }
            }
            // The bar is inset from the screen edges: centered, not
            // flush with either side.
            assert!(
                first_rev > 0 && (last_rev as u16) + 1 < buf.area.width,
                "the modal is centered: bar at {first_rev}..{last_rev}"
            );
        }
        assert_eq!(title_rows, 1, "exactly the title row is reversed");
    }
}

#[cfg(test)]
mod borderless_layout_tests {
    //! Wave 1 borderless layout composition (the `App::render` region
    //! section): divider column, transcript↔input gap row, input block
    //! structure, and the sidebar-hidden degradation. The components
    //! inside the regions still draw their legacy frames this round
    //! (lane-a/lane-b de-frame them in Wave 2) — these tests assert only
    //! the LAYER the App owns.

    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;
    use ratatui::Terminal;

    /// Hermetic temp project (absolute `[database]` path inside the
    /// tempdir — a relative path would leak into the user's global
    /// store). Mirrors `tests/bridge_integration.rs::temp_project`.
    fn temp_project() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let openslate_dir = tmp.path().join(".openslate");
        std::fs::create_dir(&openslate_dir).expect("create .openslate dir");
        let db_path = openslate_dir.join("test.sqlite");
        std::fs::write(
            openslate_dir.join("openslate.toml"),
            format!(
                r#"
[providers.mock]
base_url = "http://localhost"
api_key_env = "TUI_TEST_KEY"

[models.main]
provider = "mock"
model = "mock-model"

[models.fast]
provider = "mock"
model = "mock-model"

[database]
path = {db_path:?}

[limits]
max_steps = 10
max_depth = 4
max_tool_calls = 20
max_context_bytes = 100_000
max_output_bytes = 10_000
"#
            ),
        )
        .expect("write toml");
        let agents_dir = openslate_dir.join("agents");
        std::fs::create_dir(&agents_dir).expect("create agents dir");
        std::fs::write(
            agents_dir.join("root.md"),
            "---\nid: root\nname: Root Agent\nmodel: main\ntools:\n  - read_file\n---\nYou are the root agent.\n",
        )
        .expect("write root.md");
        tmp
    }

    async fn test_app() -> App {
        let tmp = temp_project();
        let config_path = tmp.path().join(".openslate/openslate.toml");
        let ctx = openslate_app::wiring::build_app_context(config_path.to_str())
            .await
            .expect("build app context");
        // Leak the TempDir for the App's lifetime (test process is short;
        // the store file must outlive the App).
        std::mem::forget(tmp);
        App::new(ctx)
    }

    /// Draw the app at `w x h` and return the buffer clone.
    async fn draw(app: &mut App, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("test terminal");
        terminal.draw(|f| app.render(f)).expect("render");
        terminal.backend().buffer().clone()
    }

    /// Draw and return the FRONT buffer (the exact post-render state,
    /// unlike the backend buffer which only receives the frame diff —
    /// cells following wide glyphs are skipped by design). The seam for
    /// style-level assertions (e.g. the dim pass).
    async fn draw_front(app: &mut App, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("test terminal");
        let mut front = None;
        terminal
            .draw(|f| {
                app.render(f);
                front = Some(f.buffer_mut().clone());
            })
            .expect("render");
        front.expect("front buffer captured")
    }

    /// The x of the full-height DarkGray `┃` divider column inside
    /// `x_range`, if any (a column where EVERY row in `y_range` is `┃`).
    fn divider_column(buf: &ratatui::buffer::Buffer, y_range: std::ops::Range<u16>) -> Option<u16> {
        (1..buf.area.width).find(|&x| {
            y_range.clone().all(|y| {
                let cell = &buf[(x, y)];
                cell.symbol() == "┃" && cell.style().fg == Some(Color::DarkGray)
            })
        })
    }

    #[tokio::test]
    async fn wide_layout_divider_gap_row_single_line_input() {
        let mut app = test_app().await;
        // 100x20 (wide): main=17 rows (y 0..16), gap=1 (y=17), info
        // footer ABSENT (sidebar owns that column), input=1 row
        // (y=18), status=1 (y=19).
        let buf = draw(&mut app, 100, 20).await;

        // Full-height divider ┃ (DarkGray) between main column and the
        // 30-col sidebar (right edge block), spanning the main region.
        let x_d = divider_column(&buf, 0..17).expect("divider column exists");
        assert!(
            (100 - SIDEBAR_WIDTH - 4..=100 - SIDEBAR_WIDTH).contains(&x_d),
            "divider sits in the gap left of the sidebar, got {x_d}"
        );

        // The transcript↔input gap row is completely blank.
        for x in 0..100u16 {
            assert_eq!(buf[(x, 17)].symbol(), " ", "gap row not blank at {x}");
        }

        // Input row (single line): ┃ bar at x=0 (Cyan — Input focus by
        // default) + muted placeholder; NO hint row below.
        assert_eq!(buf[(0, 18)].symbol(), "┃");
        assert_eq!(buf[(0, 18)].style().fg, Some(Color::Cyan));
        assert_eq!(buf[(2, 18)].symbol(), "输");
        // The input row spans ONLY the transcript column's width — the
        // area under the sidebar stays empty.
        let transcript_w = 100 - SIDEBAR_WIDTH - 3;
        for x in (transcript_w + 1)..100u16 {
            assert_eq!(
                buf[(x, 18)].symbol(),
                " ",
                "input must stop at the transcript column ({x})"
            );
        }
        // Status bar fills the bottom row (reversed content).
        assert!(
            (0..100u16).any(|x| buf[(x, 19)].modifier.contains(Modifier::REVERSED)),
            "status bar paints the last row"
        );
    }

    #[tokio::test]
    async fn narrow_layout_info_footer_replaces_sidebar() {
        let mut app = test_app().await;
        // 80x20 (narrow): main=16 rows (y 0..15), gap=1 (y=16), info
        // footer=1 (y=17), input=1 (y=18), status=1 (y=19).
        let buf = draw(&mut app, 80, 20).await;

        // No sidebar → no divider column in the main region.
        assert!(
            divider_column(&buf, 0..16).is_none(),
            "divider must disappear with the sidebar"
        );

        // The info footer carries the compact agents line (root anchor
        // glyph, U+F111) + the dim ` │ ` separator + the session
        // counters.
        let footer: String = (0..40).map(|x| buf[(x, 17)].symbol()).collect();
        assert!(footer.contains('\u{F111}'), "root glyph: {footer:?}");
        assert!(footer.contains("root"), "root id: {footer:?}");
        assert!(footer.contains("│"), "agents/session separator: {footer:?}");
        assert!(footer.contains("d0/4"), "depth counters: {footer:?}");
        assert!(footer.contains("t0/20"), "tool-call counters: {footer:?}");
        // No run yet → no run-id segment.
        assert!(
            !footer.contains("ses_"),
            "run id absent before the first turn"
        );

        // Input structure unchanged: single row with the Cyan bar.
        assert_eq!(buf[(0, 18)].symbol(), "┃");
        assert_eq!(buf[(0, 18)].style().fg, Some(Color::Cyan));
    }

    #[tokio::test]
    async fn ctrl_t_toggles_narrow_footer_visibility() {
        let mut app = test_app().await;
        // Prime the size-dependent guards with one draw, then toggle.
        let _ = draw(&mut app, 80, 20).await;
        app.dispatch(Action::ToggleSidebar).await;
        let buf = draw(&mut app, 80, 20).await;
        // Footer hidden: its row is blank, the input row stays put.
        for x in 0..80u16 {
            assert_eq!(buf[(x, 17)].symbol(), " ", "footer row must clear at {x}");
        }
        assert_eq!(
            buf[(0, 18)].symbol(),
            "┃",
            "input row remains above the status bar"
        );

        // /status re-opens the footer (sidebar_visible=true) WITHOUT
        // stealing focus (the sidebar is invisible at this width —
        // keys must not be swallowed by it).
        app.dispatch(Action::StartTurn("/status".into())).await;
        let buf = draw(&mut app, 80, 20).await;
        let footer: String = (0..40).map(|x| buf[(x, 17)].symbol()).collect();
        assert!(
            footer.contains('\u{F111}'),
            "footer back after /status: {footer:?}"
        );
        assert_ne!(
            app.focus,
            Focus::Sidebar,
            "narrow /status must not focus the sidebar"
        );
    }

    #[tokio::test]
    async fn main_column_content_is_padded_two_columns() {
        let mut app = test_app().await;
        let buf = draw(&mut app, 100, 20).await;
        // The transcript renders INSIDE the 2-col padded main column:
        // the leftmost 2 columns of the main region are blank on the
        // first content row (the empty-session hint).
        let x_d = divider_column(&buf, 0..17).expect("divider");
        // Find a transcript content row: any row y with a non-blank
        // cell left of the divider; padding means nothing renders in
        // x∈{0,1}.
        let content_row = (0..17u16).find(|&y| (0..x_d).any(|x| buf[(x, y)].symbol() != " "));
        if let Some(y) = content_row {
            assert_eq!(buf[(0, y)].symbol(), " ", "padding col 0");
            assert_eq!(buf[(1, y)].symbol(), " ", "padding col 1");
            assert_ne!(buf[(2, y)].symbol(), " ", "content starts at col 2");
        }
    }

    // ── overlay occlusion dimming (layout rework) ──────────────────────

    /// Assert `dim == !inside` for every cell, reading the FRONT buffer
    /// (the backend buffer misses wide-glyph skip cells by design).
    async fn assert_dim_corridor(app: &mut App, inner: Rect, w: u16, h: u16) {
        let buf = draw_front(app, w, h).await;
        for y in 0..h {
            for x in 0..w {
                let dim = buf
                    .cell((x, y))
                    .expect("cell in bounds")
                    .modifier
                    .contains(Modifier::DIM);
                let inside = x >= inner.x
                    && x < inner.x + inner.width
                    && y >= inner.y
                    && y < inner.y + inner.height;
                assert_eq!(
                    dim, !inside,
                    "cell ({x},{y}) dim={dim}, inside overlay={inside}"
                );
            }
        }
    }

    /// Every cell OUTSIDE the help overlay carries DIM; every cell
    /// inside does not (the overlay `Clear`s + repaints its interior).
    #[tokio::test]
    async fn help_open_dims_background_outside_the_overlay() {
        let mut app = test_app().await;
        app.modal = Modal::HelpOpen;
        let inner = App::help_overlay_rect(ratatui::layout::Rect::new(0, 0, 80, 24));
        assert_dim_corridor(&mut app, inner, 80, 24).await;
    }

    #[tokio::test]
    async fn exit_confirm_dims_background_outside_the_overlay() {
        let mut app = test_app().await;
        app.modal = Modal::ConfirmExit;
        let inner = App::exit_confirm_rect(ratatui::layout::Rect::new(0, 0, 80, 24));
        assert_dim_corridor(&mut app, inner, 80, 24).await;
    }

    #[tokio::test]
    async fn approval_banner_does_not_dim_the_base_ui() {
        // The approval banner is a NON-modal overlay: no dim pass runs.
        // Base-UI styles legitimately contain DIM (session fine print),
        // so the assertion compares each cell's modifier against a
        // baseline draw without the pending approval — identical
        // outside the banner area.
        let mut app = test_app().await;
        let baseline = draw_front(&mut app, 100, 24).await;
        app.approval.enqueue(
            1,
            crate::event::ApprovalSummary {
                tool_name: "shell".into(),
                arguments: "{}".into(),
                agent_id: "root".into(),
                risk_level: "high".into(),
            },
        );
        assert!(matches!(app.active_layer(), Layer::ApprovalActive));
        let buf = draw_front(&mut app, 100, 24).await;
        // 100x24 wide layout: main = 21 rows (y 0..20); the banner
        // covers its bottom 5 rows (y 16..=20).
        for y in 0..24u16 {
            for x in 0..100u16 {
                if (16..=20).contains(&y) {
                    continue; // banner area — content legitimately differs
                }
                let got = buf.cell((x, y)).expect("cell").modifier;
                let want = baseline.cell((x, y)).expect("cell").modifier;
                assert_eq!(got, want, "modifier drift at ({x},{y}) — dim pass ran?");
            }
        }
    }

    #[tokio::test]
    async fn min_size_guard_does_not_dim() {
        // Below the minimum size the guard paints alone (no base UI,
        // no overlay) — nothing is dimmed.
        let mut app = test_app().await;
        let buf = draw_front(&mut app, 40, 10).await;
        for y in 0..10u16 {
            for x in 0..40u16 {
                assert!(
                    !buf.cell((x, y))
                        .expect("cell in bounds")
                        .modifier
                        .contains(Modifier::DIM),
                    "guard must not dim ({x},{y})"
                );
            }
        }
    }

    // ── fix-17: the pinned `↓ 新内容 +N` hint is clickable ────────────

    /// Seed streaming content, pin the transcript well above the tail
    /// (wheel notches — the input keeps focus throughout), draw once at
    /// 80x20 so the hint renders and its hit rectangle records, then
    /// return the terminal cell of the hint's `新` glyph (inside the
    /// hit rectangle by construction).
    async fn pinned_app_with_hint() -> (App, (u16, u16)) {
        let mut app = test_app().await;
        for i in 0..40 {
            app.dispatch(Action::Engine(TuiEvent::Delta(format!("line{i}\n"))))
                .await;
        }
        for _ in 0..10 {
            app.dispatch(Action::WheelScrollUp).await;
        }
        assert!(app.transcript_pinned());
        let buf = draw(&mut app, 80, 20).await;
        // The hint is the only `新` on a non-overlay screen (the help
        // overlay's `新一轮` needs `?` + empty input; the status bar
        // hints are ASCII by design).
        let hit = (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
            .find(|&(x, y)| buf.cell((x, y)).is_some_and(|c| c.symbol() == "新"))
            .expect("the hint renders bottom-right");
        (app, hit)
    }

    /// Dispatch level: a click on the hint jumps back to the bottom
    /// (re-follow) even while the INPUT box holds focus — the click is
    /// positional, like the wheel.
    #[tokio::test]
    async fn click_on_new_content_hint_jumps_to_bottom() {
        let (mut app, (hx, hy)) = pinned_app_with_hint().await;
        app.dispatch(Action::Click(hx, hy)).await;
        assert!(
            !app.transcript_pinned(),
            "the hint click re-follows the tail"
        );
    }

    /// Dispatch level: while the approval modal is active the existing
    /// modal guard swallows the click (the pin survives) and the
    /// y/n/a priority is unchanged — `y` still answers the approval
    /// instead of typing.
    #[tokio::test]
    async fn approval_modal_swallows_hint_click() {
        let (mut app, (hx, hy)) = pinned_app_with_hint().await;
        app.dispatch(Action::Engine(TuiEvent::ApprovalRequested {
            id: 1,
            request: crate::event::ApprovalSummary {
                tool_name: "shell".into(),
                arguments: "{}".into(),
                agent_id: "root".into(),
                risk_level: "high".into(),
            },
        }))
        .await;
        assert_eq!(*app.run_state(), RunState::ApprovalPending);

        // The click never reaches the transcript while the modal is up.
        app.dispatch(Action::Click(hx, hy)).await;
        assert!(app.transcript_pinned(), "modal guard swallows the click");

        // `y` answers the approval (not typed into the editor); the
        // queue drains and ordinary input resumes.
        app.dispatch(Action::InputChar('y')).await;
        assert_eq!(app.input_text(), "");
        assert_ne!(
            app.active_layer(),
            Layer::ApprovalActive,
            "the drained queue ends preemption"
        );
        app.dispatch(Action::InputChar('x')).await;
        assert_eq!(app.input_text(), "x");
    }
}
