//! App — aggregate state, main loop, modal stack, turn lifecycle,
//! shutdown. Interface-frozen as of P2b (see the spec's crate layout).
//!
//! # Main loop shape
//!
//! ```text
//! select! { tick │ crossterm EventStream │ server-link channel }
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
//! # Client ownership (web-1)
//!
//! The App is a pure client: turns, approvals, session state and config
//! persistence all live in `openslate-server`. Submitting a turn sends
//! [`openslate_protocol::ClientMsg::Submit`] through the
//! [`crate::client::ServerLink`]; every server broadcast arrives as
//! [`crate::event::TuiEvent`]s through the same channel the local
//! engine bridge used to feed (the `select!`/coalesce pipeline is
//! unchanged). The transcript mirror is rebuilt wholesale from each
//! [`TuiEvent::Snapshot`]; a link drop freezes outbound actions behind
//! a transport-state gate.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use openslate_core::config::OpenSlateConfig;
use openslate_core::types::{Message, MessageRole, Usage};
use openslate_protocol::{ClientMsg, ConfigViewDto, SnapshotDto};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
use tokio_stream::StreamExt;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::action::{map_event, Action, ApprovalChoice};
use crate::client::{
    approval_choice_label, approval_choice_to_msg, config_from_view, entries_from_dto, LinkPhase,
    ServerLink,
};
use crate::clipboard;
use crate::components::models::{ModelsChange, ModelsIntent};
use crate::components::transcript::SelectionEnd;
use crate::components::{
    AgentsComponent, AppCtx, ApprovalComponent, Component, ConfigSummary, Focus, HelpComponent,
    InputComponent, ModelsComponent, RunInfo, RunState, SessionComponent, StatusComponent,
    TranscriptComponent,
};
use crate::event::{TuiEvent, TurnSummary};
use crate::icons::localize;
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
/// Ticks a transient status-bar notice stays up (~5 s at the 250 ms idle
/// tick) before auto-clearing.
const NOTICE_TICKS: u8 = 20;
/// Minimum usable terminal; below this a warning panel replaces layout.
const MIN_COLS: u16 = 60;
const MIN_ROWS: u16 = 12;

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
    /// The agents/session panel overlay (restyle-1: `/agents`, `/status`,
    /// Ctrl+T — the old persistent right sidebar became a floating panel).
    AgentsPanel,
    /// The `/provider` management overlay (model-mgmt-2): providers /
    /// model library / level mapping over the layered config.
    ModelsPanel,
    /// A tool approval is pending — y/n/a preempt all input.
    ApprovalActive,
}

/// interactive-1: a hover-able / clickable UI button under the mouse.
/// The App tracks at most ONE (`hover`), hit-tested against the
/// rectangles the LAST render recorded; the same targets drive the
/// press+release click contract (`mouse_down_target`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoverTarget {
    /// The hint row's `? help` segment (T1) — opens the help overlay.
    HelpHint,
    /// One of the approval banner's `[y]/[n]/[a]` buttons (T2).
    ApproveButton(ApprovalChoice),
    /// A visible completion-list row by INDEX (T3) — not a screen
    /// position, so a re-filter between frames is detectable.
    CompletionRow(usize),
    /// The pinned transcript's `+N ↓ 回到底部` hint (T4) — the
    /// pre-existing fix-17 jump button.
    JumpBottom,
}

/// Overlays stored on the App (`ApprovalActive` is derived from the
/// approval queue instead).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Modal {
    None,
    HelpOpen,
    ConfirmExit,
    AgentsPanel,
    ModelsOpen,
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

/// Session-accumulated statistics. web-1: the server snapshot's
/// `session_stats` is the truth source — this local mirror accumulates
/// per-turn for immediate feedback and is overwritten wholesale by
/// every snapshot (which also reconciles server-side compact costs
/// the local sum cannot see).
#[derive(Debug, Clone, Default)]
struct SessionStats {
    total_steps: u32,
    total_input_tokens: u64,
    total_output_tokens: u64,
    turns: u32,
    total_cost_usd: f64,
}

/// What `main` hands the App at construction (web-1): the outbound
/// link, the inbound event feed, and seed state for the window between
/// construction and the first snapshot. Tests build one from a parsed
/// config + a `MemLink::pair()` channel.
pub struct ClientBootstrap {
    /// Outbound message sink (WS writer queue / test recorder).
    pub link: Arc<dyn ServerLink>,
    /// Inbound server events (the WS task's conversion output / the
    /// test MemLink's `emit` producer).
    pub events: UnboundedReceiver<TuiEvent>,
    /// Config mirror seed — production passes an empty default (the
    /// queued first snapshot hydrates on the first loop iteration);
    /// tests may seed a parsed TOML fixture directly.
    pub config: OpenSlateConfig,
    /// Root agent id seed (snapshot refines).
    pub root_agent_id: String,
}

/// The application aggregate. Public surface frozen; internals evolve
/// only within the P3 write-domain rules.
pub struct App {
    // ── client state (web-1: the server owns engine + persistence) ────
    /// Outbound message sink (WS writer queue; tests record).
    link: Arc<dyn ServerLink>,
    /// Transport state of the link (freeze gate source).
    link_phase: LinkPhase,
    /// Config mirror for local display/UX decisions (model panel,
    /// `/model` alias check, limits). The server is the authority;
    /// `ConfigChanged` swaps this wholesale.
    config: OpenSlateConfig,
    /// Root agent id (agents panel seed; refreshed on config swaps).
    root_agent_id: String,
    /// Flattened `(agent name, model alias)` pairs from the config
    /// view's agent tree — `models_change_guard`'s reference check
    /// (delete-a-model-still-used-by-an-agent).
    agents_model_refs: Vec<(String, String)>,
    /// Server-assigned session id (first snapshot; `/new` swaps it).
    session_id: Option<String>,
    /// Display label of the current session.
    session_label: String,
    /// Server-authoritative active model alias (`ModelChanged` swaps it).
    model_alias: String,
    /// Whether THIS client believes a turn is in flight: set optimisti-
    /// cally on `Submit`, cleared on `TurnDone` (both arms); snapshots
    /// re-sync from `running`. Drives `is_running`, the input-queue
    /// gate and the tick cadence.
    turn_active: bool,
    /// Test observer parity for the old cancel-token flip: set when a
    /// `Cancel` left for the server, reset when the next turn starts.
    cancel_sent: bool,
    /// The server-link event feed (WS task / test MemLink producer on
    /// the other end). The App never produces events itself (web-1) —
    /// this is a receive-only leg of the main `select!`.
    events_rx: UnboundedReceiver<TuiEvent>,
    /// A pending client-initiated request whose success confirmation
    /// should notice (CRUD save / model switch); cleared on the matching
    /// broadcast.
    pending_confirm: Option<String>,

    // ── conversation mirror ────────────────────────────────────────────
    /// Server history mirror (last `TurnOk` message list — display
    /// parity for tests; the server never reads it back).
    history: Vec<Message>,
    /// Input submitted while a turn was running; auto-submitted after
    /// `TurnDone` (restored to the editor on error instead — no retry
    /// storms).
    pending_input: Option<String>,
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
    /// Copy-file directory override (copy-1 test seam): `None` = the
    /// production default (`~/.local/share/openslate/`, the tui.log
    /// dir's parent); tests inject a tempdir via
    /// [`App::set_copy_file_dir`].
    copy_file_dir: Option<std::path::PathBuf>,

    // ── UI state ───────────────────────────────────────────────────────
    theme: Theme,
    focus: Focus,
    modal: Modal,
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
    /// The completion overlay's last-rendered rectangle (slash-3):
    /// the mouse-swallow hit rect. `None` while the list is closed
    /// (or a modal owns the screen) — recomputed every render.
    completion_hit_rect: Option<ratatui::layout::Rect>,
    /// interactive-1: the button currently under the mouse (hover
    /// highlight source), or `None`. Hit-tests run against the
    /// LAST render's recorded rectangles; cleared by a miss, by
    /// every target-vanishing transition (see `hover_valid`) and by
    /// resize / `/new` / approval answers / toggling mouse capture.
    hover: Option<HoverTarget>,
    /// interactive-1: the button a left-press landed on — the release
    /// triggers it iff it lands on the SAME target and the gesture
    /// never dragged (a drag hands the gesture to select-1 instead).
    mouse_down_target: Option<HoverTarget>,
    /// interactive-1: the hint row's `? help` segment rectangle in
    /// the LAST render (`None`: a notice occupies the row, the rung
    /// ladder dropped the segment, or the min-size guard owns the
    /// frame) — T1's hit rect.
    help_hit_rect: Option<ratatui::layout::Rect>,
    /// interactive-1: the approval banner's `[y]/[n]/[a` button
    /// rectangles in the LAST render (empty when no banner painted)
    /// — T2's hit rects.
    approval_button_rects: Vec<(ApprovalChoice, ratatui::layout::Rect)>,

    // ── components ─────────────────────────────────────────────────────
    input: InputComponent,
    transcript: TranscriptComponent,
    agents: AgentsComponent,
    session: SessionComponent,
    help: HelpComponent,
    approval: ApprovalComponent,
    status: StatusComponent,
    models: ModelsComponent,
}

impl App {
    /// Build the App from the client bootstrap (web-1): the link plus
    /// the snapshot-derived initial state. The engine-era
    /// `AppContext` assembly (manager/store/MCP/provider factory) is
    /// gone — the server owns all of it.
    pub fn new(bootstrap: ClientBootstrap) -> Self {
        let ClientBootstrap {
            link,
            events,
            config,
            root_agent_id,
        } = bootstrap;
        let events_rx = events;

        let mut agents = AgentsComponent::new();
        agents.set_root(&root_agent_id);

        Self {
            link,
            link_phase: LinkPhase::Connecting,
            config,
            root_agent_id,
            agents_model_refs: Vec::new(),
            session_id: None,
            session_label: String::new(),
            model_alias: String::new(),
            turn_active: false,
            cancel_sent: false,
            events_rx,
            pending_confirm: None,
            history: Vec::new(),
            pending_input: None,
            stats: SessionStats::default(),
            needs_full_redraw: false,
            mouse_capture: true,
            clipboard_out: clipboard::stdout_clipboard_sink(),
            copy_file_dir: None,
            theme: Theme::new(),
            focus: Focus::Input,
            modal: Modal::None,
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
            completion_hit_rect: None,
            hover: None,
            mouse_down_target: None,
            help_hit_rect: None,
            approval_button_rects: Vec::new(),
            input: InputComponent::new(),
            transcript: TranscriptComponent::new(),
            agents,
            session: SessionComponent::new(),
            help: HelpComponent::new(),
            approval: ApprovalComponent::new(),
            status: StatusComponent::new(),
            models: ModelsComponent::new(),
        }
    }

    /// Override the theme (colors + icon tier) — the CLI's
    /// `--theme`/`--icons` entry point (theme-1).
    pub fn with_theme(mut self, theme: Theme) -> Self {
        self.theme = theme;
        self
    }

    // ── Observers (frozen pub surface for tests/panels) ────────────────

    /// Current run-state machine position.
    pub fn run_state(&self) -> &RunState {
        &self.run_state
    }

    /// Whether a turn is executing (submit sent, `TurnDone` pending).
    pub fn is_running(&self) -> bool {
        self.turn_active
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

    /// Server-assigned session id, once the first snapshot arrived.
    pub fn session_run_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Current input-editor text (dispatch-level test observer; also the
    /// Err-path pending-restore assertion point).
    pub fn input_text(&self) -> String {
        self.input.text()
    }

    /// Whether the input's slash-completion list is open (slash-1
    /// dispatch-level test observer).
    pub fn input_completion_open(&self) -> bool {
        self.input.completion_open()
    }

    /// Whether the current turn's cancel has been sent to the server
    /// (dispatch-level test observer for the CancelTurn / Ctrl+C paths).
    /// web-1: the sent-`Cancel` observer — the local token is gone.
    pub fn is_cancelled(&self) -> bool {
        self.cancel_sent
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

    /// interactive-1: the current hover target (dispatch-level test
    /// observer for the MouseMove hit-tests).
    pub fn hover(&self) -> Option<HoverTarget> {
        self.hover
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

    /// Redirect the copy-file leg (`last-copy.md`) into `dir` (test
    /// seam, copy-1): production writes to the user data dir
    /// (`~/.local/share/openslate/`); tests inject a tempdir so the
    /// suite never touches the real directory.
    pub fn set_copy_file_dir(&mut self, dir: std::path::PathBuf) {
        self.copy_file_dir = Some(dir);
    }

    // ── Main loop ──────────────────────────────────────────────────────

    /// Run until quit. Restores NOTHING here (terminal restore is
    /// main's job, symmetric with init); quitting drops the server
    /// link, which the transport task observes as teardown.
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
        // Force a full repaint before the first draw: a resize race
        // during terminal handover (e.g. tmux window-size negotiation)
        // can leave the first frame rendered against a stale buffer
        // size, after which diff-rendering sees no changes and never
        // repaints — observed as a blank screen until the first
        // keypress. `clear()` resets both the screen and the previous
        // buffer, so the first `draw()` is guaranteed to be full-size.
        terminal.clear()?;
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

    /// Terminal teardown is main's job; the only App-side cleanup is
    /// dropping the link (its send-channel close tells the transport
    /// task to exit — the server denies still-blocked approvals on
    /// disconnect).
    async fn shutdown(&mut self) {
        tracing::debug!("app shutdown: dropping server link");
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
                // interactive-1: resize/full-repaint invalidates every
                // recorded hit rect — drop the hover with it.
                self.hover = None;
                (DispatchOutcome::Continue, None)
            }
            Action::Quit => (DispatchOutcome::Quit, None),
            Action::RequestQuit => {
                if self.active_layer() == Layer::Normal {
                    self.modal = Modal::ConfirmExit;
                    // select-1: an overlay opening clears the
                    // selection (the base UI dims behind it).
                    self.transcript.clear_selection();
                    // slash-1: no modal opens on top of a live
                    // completion list.
                    self.input.close_completion();
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
                    // Deny (unblocks the server) AND cancel the turn.
                    self.approval_respond(ApprovalChoice::Deny);
                    self.send_cancel();
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
                    self.send_cancel();
                    (DispatchOutcome::Continue, None)
                }
                _ => {
                    self.modal = Modal::ConfirmExit;
                    // select-1: overlay opening clears the selection.
                    self.transcript.clear_selection();
                    // slash-1: no modal opens on top of a live
                    // completion list.
                    self.input.close_completion();
                    (DispatchOutcome::Continue, None)
                }
            },

            // ── Turn start (from input submit, pending auto-submit, or
            //    tests) ──
            Action::StartTurn(text) => self.handle_start_turn(text).await,

            // ── interactive-1: hover tracking ──
            // Ahead of the modal guards so the approval banner's
            // buttons stay hover-able under its modal; every layer
            // besides ApprovalActive/Normal exposes no targets (the
            // move then CLEARS a stale hover — the layer-change
            // trigger). Pure state: the next draw paints it.
            Action::MouseMove(column, row) => {
                // adapter-combo-1: the models panel's combo dropdown
                // tracks its OWN row hover — forward the move while
                // the panel owns the screen (this arm runs BEFORE the
                // panel's modal guard, which therefore never sees a
                // MouseMove; the generic hit-test below knows no
                // ModelsPanel targets, so nothing is lost). A move is
                // pure state — the component answers None for it.
                if self.mouse_capture && self.active_layer() == Layer::ModelsPanel {
                    let ctx = self.make_ctx();
                    let _ = self.models.on_key(&action, &ctx);
                }
                self.on_mouse_move(column, row);
                (DispatchOutcome::Continue, None)
            }

            // ── Modal input preemption (ApprovalActive > HelpOpen >
            //    ConfirmExit > routing) ──
            _ if self.active_layer() == Layer::ApprovalActive => {
                match &action {
                    // interactive-1: the banner's y/n/a BUTTONS see the
                    // mouse first — press + release on one segment
                    // answers exactly like the key (the ApprovalRespond
                    // follow-up runs the same dispatcher arm). The rest
                    // of the gesture stays swallowed by the modal.
                    Action::MouseDown(column, row) => {
                        self.mouse_down_target = self.button_hit(*column, *row);
                    }
                    Action::MouseDrag(..) => {
                        self.mouse_down_target = None;
                    }
                    Action::MouseUp(column, row) => {
                        let follow = self
                            .take_button_trigger(*column, *row)
                            .and_then(|target| self.trigger_button(target, *column, *row));
                        return (DispatchOutcome::Continue, follow);
                    }
                    Action::InputChar(c) => match c {
                        'y' => self.approval_respond(ApprovalChoice::Approve),
                        'n' => self.approval_respond(ApprovalChoice::Deny),
                        'a' => self.approval_respond(ApprovalChoice::ApproveAll),
                        _ => {}
                    },
                    _ => {}
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
            // restyle-1: the agents panel overlay — Esc / Ctrl+T close
            // it; everything else is swallowed while it is up (same
            // guard shape as the help overlay).
            _ if self.active_layer() == Layer::AgentsPanel => match &action {
                Action::DismissOverlay | Action::ToggleSidebar => {
                    self.close_agents_panel();
                    (DispatchOutcome::Continue, None)
                }
                _ => (DispatchOutcome::Continue, None),
            },
            // model-mgmt-2: the `/provider` management overlay. Same
            // guard shape as the agents panel, but the component runs a
            // small state machine (list ↔ forms ↔ pickers) and answers
            // with plain-data intents the App executes (persist →
            // reload → validate → hot-swap `self.config`).
            _ if self.active_layer() == Layer::ModelsPanel => {
                let ctx = self.make_ctx();
                let intent = self.models.on_key(&action, &ctx);
                match intent {
                    ModelsIntent::None => (DispatchOutcome::Continue, None),
                    ModelsIntent::Close => {
                        self.close_models_panel();
                        (DispatchOutcome::Continue, None)
                    }
                    ModelsIntent::Commit(change) => {
                        self.apply_models_change(change);
                        (DispatchOutcome::Continue, None)
                    }
                }
            }

            // ── slash-1: completion preemption ──
            // While the input's slash-completion list is open under
            // input focus, Tab and Esc carry completion semantics
            // (apply the selection / dismiss the list). They must be
            // routed into the input BEFORE the global keys claim them
            // (Tab cycles focus, Esc clears the transcript selection);
            // the component consumes them without falling through.
            // The modal guards above run earlier, so an open overlay
            // still wins.
            Action::FocusNext | Action::DismissOverlay
                if self.focus == Focus::Input && self.input.completion_open() =>
            {
                let mut ctx = self.make_ctx();
                let follow = self.input.handle(&action, &mut ctx);
                (DispatchOutcome::Continue, follow)
            }

            // ── slash-3: completion-overlay mouse swallow ──
            // Gestures landing inside the last-rendered overlay rect
            // are no-ops: the list must not leak presses to the
            // transcript beneath (selection anchors, jump-to-bottom,
            // reasoning/tool toggles). interactive-1 carves ONE
            // exception out of the swallow: a press+release on the
            // same visible ROW is the row's Enter semantics
            // (selection moves there + the three submit branches).
            // OUTSIDE the rect every gesture keeps its exact
            // semantics.
            Action::MouseDown(column, row) if self.completion_hit(column, row) => {
                self.mouse_down_target = self.button_hit(column, row);
                (DispatchOutcome::Continue, None)
            }
            Action::MouseDrag(column, row) if self.completion_hit(column, row) => {
                self.mouse_down_target = None;
                (DispatchOutcome::Continue, None)
            }
            Action::MouseUp(column, row) if self.completion_hit(column, row) => {
                let follow = self
                    .take_button_trigger(column, row)
                    .and_then(|target| self.trigger_button(target, column, row));
                (DispatchOutcome::Continue, follow)
            }
            Action::Click(column, row) if self.completion_hit(column, row) => {
                (DispatchOutcome::Continue, None)
            }
            Action::WheelScrollUp(column, row) | Action::WheelScrollDown(column, row)
                if self.completion_hit(column, row) =>
            {
                (DispatchOutcome::Continue, None)
            }

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
                // restyle-1: Ctrl+T toggles the floating agents panel
                // (the persistent right sidebar is retired).
                if matches!(self.modal, Modal::AgentsPanel) {
                    self.close_agents_panel();
                } else {
                    self.open_agents_panel();
                }
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
                // select-1: overlay opening clears the selection.
                self.transcript.clear_selection();
                // slash-1: no modal opens on top of a live
                // completion list.
                self.input.close_completion();
                (DispatchOutcome::Continue, None)
            }
            Action::DismissOverlay => {
                // No overlay open: select-1 — Esc also drops any text
                // selection, then focus routing gives the transcript
                // its Esc-unpin.
                self.transcript.clear_selection();
                self.route_by_focus(action)
            }
            Action::InputChar('?') if self.input.is_empty() => {
                // `?` opens help only with an empty input (design brief).
                self.modal = Modal::HelpOpen;
                // select-1: overlay opening clears the selection.
                self.transcript.clear_selection();
                (DispatchOutcome::Continue, None)
            }
            Action::WheelScrollUp(..) | Action::WheelScrollDown(..) => {
                // The wheel is positional: it ALWAYS scrolls the chat
                // transcript (3 lines per notch), never the input-box
                // history — the arrow keys keep that focus-routed role.
                // (In-overlay notches were already swallowed by the
                // slash-3 guard above.)
                const WHEEL_SCROLL_LINES: usize = 3;
                let base = if matches!(action, Action::WheelScrollUp(..)) {
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
                // pinned `+N ↓ 回到底部` hint is CLICKABLE — a hit inside
                // the hint's last-rendered rectangle returns
                // ScrollBottom (the EXISTING follow-restore action),
                // which is applied straight back to the transcript:
                // the generic follow-up loop would re-route
                // ScrollBottom as InputEnd under input focus. Reasoning
                // summary/block and tool row hits toggle in-component
                // (None — nothing for the App to do). Misses (or no
                // target) are inert. Modal-active clicks never get
                // here — the modal guards above swallow them.
                let mut ctx = self.make_ctx();
                ctx.focus = Focus::Transcript;
                let follow = self
                    .transcript
                    .handle(&Action::Click(column, row), &mut ctx)
                    .and_then(|next| self.transcript.handle(&next, &mut ctx));
                (DispatchOutcome::Continue, follow)
            }

            // ── select-1: drag-to-select (tmux copy-mode style) ──
            // Down records the anchor and SUPPRESSES the legacy
            // click; a drag past ≥1 cell promotes the press into a
            // live selection (click suppression locks in); Up then
            // either fires the legacy click (no drag — every
            // fix-17/23/25 behavior intact) or extracts the
            // selection and copies it through the existing chain.
            // Positional like Click: never focus-routed, and the
            // modal guards above swallow the whole gesture while an
            // overlay is up. interactive-1: a Down on a BUTTON also
            // records the press target; a same-target Up without a
            // drag triggers the button INSTEAD of the legacy click
            // (for the jump hint the trigger replays the exact Click
            // action, so that behavior is byte-identical).
            Action::MouseDown(column, row) => {
                self.mouse_down_target = self.button_hit(column, row);
                self.transcript.selection_begin(column, row);
                (DispatchOutcome::Continue, None)
            }
            Action::MouseDrag(column, row) => {
                // A drag cancels any pending button press (select-1
                // owns the gesture from here).
                self.mouse_down_target = None;
                self.transcript.selection_drag(column, row);
                (DispatchOutcome::Continue, None)
            }
            Action::MouseUp(column, row) => {
                // End the selection state machine FIRST (cleanup is
                // unconditional); then decide which click fires.
                let end = self.transcript.selection_end(&self.theme);
                let triggered = self
                    .take_button_trigger(column, row)
                    .and_then(|target| self.trigger_button(target, column, row));
                match triggered {
                    Some(follow) => (DispatchOutcome::Continue, Some(follow)),
                    None => match end {
                        SelectionEnd::Click => {
                            // Never dragged: the legacy positional click
                            // fires with the release coordinates (the
                            // same cell as the press — no drag means no
                            // displacement).
                            (DispatchOutcome::Continue, Some(Action::Click(column, row)))
                        }
                        SelectionEnd::Selected(text) => {
                            // Dragged: auto-copy through the same chain
                            // as /copy (OSC 52 + last-copy.md + a
                            // clipboard tool; the notice keeps copy-1's
                            // format).
                            self.copy_text(&text);
                            (DispatchOutcome::Continue, None)
                        }
                    },
                }
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
            let frames = self.theme.icons.set().spinner.chars().count().max(1);
            self.spinner_frame = (self.spinner_frame + 1) % frames;
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
        // interactive-1: capture off = the terminal sends no mouse
        // events at all — no hover may persist (nor a pending press).
        self.hover = None;
        self.mouse_down_target = None;
        let msg = if self.mouse_capture {
            "mouse capture on - wheel scrolls chat"
        } else {
            "mouse capture off - text selectable; wheel = arrow keys"
        };
        self.set_notice(msg);
    }

    /// Copy the LAST assistant message's raw markdown to the system
    /// clipboard (Ctrl+Y / `/copy`, fix-13) through the copy chain
    /// (copy-1). Notices follow the existing first-write-wins-clears
    /// lifecycle ([`Self::set_notice`]).
    fn copy_last_output(&mut self) {
        match self.last_assistant_text().map(str::to_owned) {
            Some(text) => self.copy_text(&text),
            None => self.set_notice("nothing to copy"),
        }
    }

    /// `/copy all`: the whole transcript as plain text — user messages
    /// `> `-prefixed, assistant text verbatim, tool entries one-line
    /// summaries (with their retained output's line count, P2).
    /// Reasoning/meta/delegation entries are live-only decoration and
    /// are skipped. The assembled text rides the same copy chain.
    fn copy_all(&mut self) {
        let plain = self.transcript_plain_text();
        self.copy_text(&plain);
    }

    /// `/copy tool`: the most recent tool call's RETAINED output text
    /// (fix-25's detail storage; a live entry without output falls
    /// back to the last one that has text — see
    /// [`TranscriptComponent::last_tool_output`]).
    fn copy_tool_output(&mut self) {
        match self.transcript.last_tool_output().map(str::to_owned) {
            Some(text) if !text.trim().is_empty() => self.copy_text(&text),
            _ => self.set_notice("nothing to copy"),
        }
    }

    /// The environment-independent copy chain (copy-1): all three legs
    /// run and the notice reports HONESTLY what happened —
    ///
    /// * OSC 52 through the sink (success only means "sent to the
    ///   terminal"; the terminal may still refuse the sequence, so the
    ///   notice never claims receipt);
    /// * the copy FILE (`<data dir>/last-copy.md` or the injected test
    ///   dir) — the one path guaranteed available everywhere, named by
    ///   ABSOLUTE path in the notice;
    /// * a clipboard TOOL detected on PATH (`+tool` suffix when its
    ///   run exited 0).
    ///
    /// Truncation ([`clipboard::truncate_for_copy`]) applies to ALL
    /// legs alike so the notice's `N of M` describes every copy. Every
    /// leg failing → `clipboard write failed`.
    fn copy_text(&mut self, full: &str) {
        let total = full.chars().count();
        if total == 0 {
            self.set_notice("nothing to copy");
            return;
        }
        let text = clipboard::truncate_for_copy(full);
        let copied = text.chars().count();
        let osc52_sent = (self.clipboard_out)(&clipboard::osc52_payload(text));
        let file_path = self.write_copy_file(text);
        let tool_used = clipboard::detect_clipboard_tool()
            .filter(|tool| clipboard::run_clipboard_tool(tool, text));

        if !osc52_sent && file_path.is_none() && tool_used.is_none() {
            self.set_notice("clipboard write failed");
            return;
        }
        let count = if copied < total {
            format!("copied {copied} of {total} chars")
        } else {
            format!("copied {copied} chars")
        };
        let mut msg = match &file_path {
            Some(path) => format!("{count} -> {}", path.display()),
            None => count,
        };
        let mut extras: Vec<String> = Vec::new();
        if osc52_sent {
            extras.push("+osc52".to_owned());
        }
        if let Some(tool) = tool_used {
            extras.push(format!("+{tool}"));
        }
        if !extras.is_empty() {
            msg.push_str(&format!(" ({})", extras.join(" ")));
        }
        self.set_notice(msg);
    }

    /// The copy-file leg: write `text` to the effective copy dir and
    /// return the ABSOLUTE path (a relative dir joins the cwd — the
    /// notice must name a path the shell can open). `None` + a warn
    /// log on write failure (one leg of the chain, never fatal).
    fn write_copy_file(&self, text: &str) -> Option<std::path::PathBuf> {
        let result = match &self.copy_file_dir {
            Some(dir) => clipboard::write_copy_file(dir, text),
            None => clipboard::copy_file_default(text),
        };
        match result {
            Ok(path) => Some(absolute_path(path)),
            Err(e) => {
                tracing::warn!("copy file write failed: {e}");
                None
            }
        }
    }

    /// `/copy all`'s plain-text rendering of the committed transcript
    /// (see [`Self::copy_all`]).
    fn transcript_plain_text(&self) -> String {
        use crate::components::transcript::TranscriptEntry as Entry;
        let mut out = String::new();
        for entry in self.transcript.entries() {
            match entry {
                Entry::User(text) => {
                    for line in text.split('\n') {
                        out.push_str("> ");
                        out.push_str(line);
                        out.push('\n');
                    }
                    out.push('\n');
                }
                Entry::Assistant(text) => {
                    out.push_str(text);
                    out.push_str("\n\n");
                }
                Entry::ToolCall {
                    name, args, detail, ..
                } => {
                    out.push_str(&format!("[tool] {name}({args})"));
                    if let Some(output) = &detail.output {
                        out.push_str(&format!(" · {} output lines", output.lines().count()));
                    }
                    out.push('\n');
                }
                _ => {}
            }
        }
        out
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

    /// Whether the sidebar is rendered: the agents-panel overlay is
    /// open (restyle-1 — the persistent right sidebar is retired, so
    /// Tab only stops on Sidebar while the panel is up).
    fn sidebar_active(&self) -> bool {
        matches!(self.modal, Modal::AgentsPanel)
    }

    /// Open the floating agents/session panel (restyle-1): the overlay
    /// claims the Sidebar focus stop so its titles light up.
    /// select-1: an overlay opening clears the selection.
    /// slash-1: it also closes a live completion list (no modal opens
    /// on top of one).
    fn open_agents_panel(&mut self) {
        self.modal = Modal::AgentsPanel;
        self.focus = Focus::Sidebar;
        self.transcript.clear_selection();
        self.input.close_completion();
    }

    /// Close the floating agents/session panel, restoring the input
    /// focus (a Sidebar focus with no visible panel would eat keys).
    fn close_agents_panel(&mut self) {
        if matches!(self.modal, Modal::AgentsPanel) {
            self.modal = Modal::None;
        }
        if self.focus == Focus::Sidebar {
            self.focus = Focus::Input;
        }
    }

    /// Open the `/provider` management overlay (model-mgmt-2). Same
    /// overlay-open hygiene as the agents panel (selection cleared, no
    /// completion list underneath). Focus stays on the input — the
    /// modal guard swallows every key while the panel is up.
    fn open_models_panel(&mut self) {
        self.modal = Modal::ModelsOpen;
        self.models.reset_view();
        self.models.sync(&self.config);
        self.transcript.clear_selection();
        self.input.close_completion();
    }

    /// Close the `/provider` overlay.
    fn close_models_panel(&mut self) {
        if matches!(self.modal, Modal::ModelsOpen) {
            self.modal = Modal::None;
        }
    }

    // ── model-mgmt-2: CRUD send + confirm routing (web-1) ──────────────

    /// Execute one models-overlay change: emit the CRUD `ClientMsg`s
    /// and wait for the server's verdict — `ConfigChanged` (success,
    /// swap the mirror + notice) or a directed `Notice` (failure, keep
    /// the previous config). The overlay stays open on both paths.
    ///
    /// Defense in depth: the overlay's reference guards run first
    /// (App-side, instant, exact referencing names), the server
    /// re-validates everything (its `validate_config` backstop) before
    /// writing to disk.
    fn apply_models_change(&mut self, change: ModelsChange) {
        // First line of defense: refuse reference-breaking removals
        // with the exact referencing names (the overlay stays open).
        if let Some(msg) = self.models_change_guard(&change) {
            self.set_notice(msg);
            self.models.sync(&self.config);
            return;
        }
        if !self.link_ready() {
            self.set_notice("已断线，重连中 · 保存请求未发送");
            self.models.sync(&self.config);
            return;
        }
        let (what, msgs) = client_msgs_for_change(&change);
        self.pending_confirm = Some(what);
        for msg in msgs {
            self.link.send(msg);
        }
    }

    /// `ConfigChanged` handling: swap the mirror, re-sync the overlay,
    /// and post the pending CRUD's success notice (any client's save
    /// refreshes us; only the initiator sees the confirmation text).
    fn handle_config_changed(&mut self, config: ConfigViewDto) {
        self.apply_config_view(&config);
        if let Some(what) = self.pending_confirm.take() {
            self.set_notice(format!("已保存并生效 · {what}"));
        }
    }

    /// Swap the config mirror + everything derived from the view
    /// (agents panel seed, guard reference pairs, overlay sync).
    fn apply_config_view(&mut self, view: &ConfigViewDto) {
        self.config = config_from_view(view);
        self.root_agent_id = view.agents.id.clone();
        fn walk(node: &openslate_protocol::AgentNodeDto, out: &mut Vec<(String, String)>) {
            out.push((node.name.clone(), node.model.clone()));
            for child in &node.children {
                walk(child, out);
            }
        }
        let mut refs = Vec::new();
        walk(&view.agents, &mut refs);
        self.agents_model_refs = refs;
        self.agents.set_root(&self.root_agent_id);
        self.models.sync(&self.config);
    }

    /// Reference guards for the models overlay (run BEFORE the commit):
    /// refuse removals that would strand references, with the exact
    /// referencing names in the notice. Provider deletions are blocked
    /// by model references; model-entry deletions by levels mappings
    /// and agent `model` references; `main`/`fast` levels are required
    /// and undeletable. Returns `None` when the change may proceed.
    fn models_change_guard(&self, change: &ModelsChange) -> Option<String> {
        match change {
            ModelsChange::RemoveProvider { name } => {
                let referencing: Vec<&String> = self
                    .config
                    .models
                    .iter()
                    .filter(|(_, m)| &m.provider == name)
                    .map(|(n, _)| n)
                    .collect();
                if referencing.is_empty() {
                    None
                } else {
                    Some(format!(
                        "无法删除 provider '{name}'：被模型条目引用（{}）",
                        referencing
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                }
            }
            ModelsChange::RemoveModel { entry } => {
                let mut refs: Vec<String> = self
                    .config
                    .levels
                    .iter()
                    .filter(|(_, e)| e.as_str() == entry.as_str())
                    .map(|(l, _)| format!("levels.{l}"))
                    .collect();
                for (name, model) in &self.agents_model_refs {
                    if model == entry {
                        refs.push(format!("agent '{name}'"));
                    }
                }
                if refs.is_empty() {
                    None
                } else {
                    Some(format!(
                        "无法删除模型条目 '{entry}'：被引用（{}）",
                        refs.join(", ")
                    ))
                }
            }
            ModelsChange::RemoveLevel { level } => {
                if crate::components::models::REQUIRED_LEVELS.contains(&level.as_str()) {
                    Some(format!("级别 '{level}' 为 required，不可删除"))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// The effective modal layer (computed stack).
    fn active_layer(&self) -> Layer {
        if self.approval.has_pending() {
            Layer::ApprovalActive
        } else {
            match self.modal {
                Modal::HelpOpen => Layer::HelpOpen,
                Modal::ConfirmExit => Layer::ConfirmExit,
                Modal::AgentsPanel => Layer::AgentsPanel,
                Modal::ModelsOpen => Layer::ModelsPanel,
                Modal::None => Layer::Normal,
            }
        }
    }

    // ── Server events (display mirror + client lifecycle) ─────────────

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
                // A new request means the previous turn's delegation
                // view retires (web-1: no post-turn calibration — the
                // live history lingers only until the next turn).
                self.agents.turn_reset();
                // A hold that reached the next request means the
                // previous step had no tools — its stats land at that
                // step's answer tail now; the live stats row resets
                // for the new request.
                self.transcript.flush_pending_step_meta();
                self.transcript.set_live_ttft(None);
                self.transcript.set_live_input_estimate(None);
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
            TuiEvent::InputEstimate(tokens) => {
                // Server-side input estimate for the streaming request
                // (`↑~N` live-row segment; cleared per request).
                self.transcript.set_live_input_estimate(Some(tokens));
            }
            TuiEvent::ApprovalRequested { id, request } => {
                self.approval.enqueue(id, request);
                // select-1: the approval banner preempts input (and
                // swallows the whole mouse gesture) — clear the
                // selection. slash-1: same for a live completion list.
                self.transcript.clear_selection();
                self.input.close_completion();
            }
            TuiEvent::ApprovalResolved { id, choice } => {
                // Broadcast verdict (ANY client may have answered —
                // first answer wins server-side). All clients clear the
                // banner for `id` and record the decision line.
                if let Some(pending) = self.approval.resolve(id) {
                    let label = approval_choice_label(&choice);
                    self.transcript
                        .push_approval(&pending.summary.tool_name, label);
                }
                // The turn resumes (or dies) server-side; the local
                // banner-preempted layer closes with the queue.
            }
            TuiEvent::Snapshot(snapshot) => {
                return self.handle_snapshot(*snapshot);
            }
            TuiEvent::ConfigChanged(config) => {
                self.handle_config_changed(*config);
            }
            TuiEvent::ModelChanged(alias) => {
                self.model_alias = alias.clone();
                // Model switches confirm via ModelChanged (not
                // ConfigChanged — no config CRUD involved); only the
                // initiating client holds a pending "model →" marker.
                if self
                    .pending_confirm
                    .as_deref()
                    .is_some_and(|what| what.starts_with("model →"))
                {
                    self.pending_confirm = None;
                    self.set_notice(format!("model → {alias}"));
                }
            }
            TuiEvent::SessionReset => {
                // The server broadcasts reset, then every connection
                // receives a fresh snapshot — light prep only (the
                // snapshot owns every wholesale swap).
                self.pending_input = None;
                self.pending_confirm = None;
                self.cancel_sent = false;
                self.set_notice("已开启新会话");
            }
            TuiEvent::Notice(text) => {
                // A failed client-initiated request (CRUD/model) clears
                // its pending confirmation — the notice IS the verdict.
                if self.pending_confirm.is_some() {
                    self.pending_confirm = None;
                }
                self.set_notice(text);
            }
            TuiEvent::LinkState(phase) => {
                let was_reconnecting = matches!(self.link_phase, LinkPhase::Reconnecting { .. });
                self.link_phase = phase;
                match self.link_phase {
                    // Initial connect: the UI is not on screen yet
                    // (main's connect() returns only after the first
                    // snapshot), so a notice here would linger as a
                    // stale banner on the first frame. Silent.
                    LinkPhase::Connecting => {}
                    // True mid-run drop: freeze notice until the
                    // Connected event (or a snapshot) lands.
                    LinkPhase::Reconnecting { attempt } => {
                        self.set_notice(format!("已断线，第 {attempt} 次重连中…"));
                    }
                    // Recovery AND first connect both arrive here.
                    // Announce only a RE-connect: a first connect never
                    // showed a drop notice, so recovery would be noise.
                    LinkPhase::Connected => {
                        if was_reconnecting {
                            self.set_notice("已重新连接");
                        }
                    }
                }
            }
            TuiEvent::TurnDone(result) => {
                return self.handle_turn_done(result).await;
            }
        }
        (DispatchOutcome::Continue, None)
    }

    /// Full-session snapshot: the server's authoritative state. Every
    /// mirror (session id/label/model, running flags, counters, config,
    /// transcript, pending approval, session stats) is swapped
    /// wholesale — no incremental reconciliation (reconnects can have
    /// missed arbitrary events; the snapshot is the reset point).
    fn handle_snapshot(&mut self, snapshot: SnapshotDto) -> (DispatchOutcome, Option<Action>) {
        // A snapshot IS proof the link is up (hello accepted): set the
        // phase defensively — normally the LinkState(Connected) event
        // preceded it, and this is idempotent.
        self.link_phase = LinkPhase::Connected;
        self.session_id = Some(snapshot.session_id);
        self.session_label = snapshot.session_label;
        self.model_alias = snapshot.model_alias;
        self.turn_active = snapshot.running;
        self.depth_cur = snapshot.depth_cur;
        self.apply_config_view(&snapshot.config);
        self.transcript
            .replace_entries(entries_from_dto(snapshot.transcript));
        // Pending approval: swap the queue (a mid-approval attach sees
        // the server's truth; the banner re-arms).
        self.approval.clear();
        if let Some(pending) = snapshot.pending_approval {
            self.approval.enqueue(
                pending.id,
                crate::event::ApprovalSummary::from(pending.summary),
            );
        }
        // Server-truth stats (reconciles compaction costs the local
        // per-turn accumulation cannot see).
        self.stats = SessionStats {
            total_steps: 0,
            total_input_tokens: snapshot.session_stats.total_input_tokens,
            total_output_tokens: snapshot.session_stats.total_output_tokens,
            turns: snapshot.session_stats.turns as u32,
            total_cost_usd: snapshot.session_stats.total_cost_usd,
        };
        if snapshot.running && !self.run_state.is_running() {
            self.run_state = RunState::Thinking;
        }
        (DispatchOutcome::Continue, None)
    }

    async fn handle_turn_done(
        &mut self,
        result: Result<TurnSummary, String>,
    ) -> (DispatchOutcome, Option<Action>) {
        // The turn ended server-side: the optimistic in-flight flag
        // and the cancel marker retire first.
        self.turn_active = false;
        self.cancel_sent = false;
        // Capture the turn duration before clearing the start instant —
        // feeds the transcript's end-of-turn marker (set_turn_meta).
        let turn_elapsed = self
            .turn_started
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(0);
        self.turn_started = None;

        match result {
            Ok(summary) => {
                // THE merge point for successful turns: the committed
                // entries are the event-ordered authority, so the
                // thinking blocks and per-request meta lines SURVIVE —
                // `merge_turn` only folds tool results in from the
                // messages (+ appends assistant content the transcript
                // never saw). `history` mirrors the server's message
                // list (display parity; the server is the authority).
                self.transcript.set_turn_meta(
                    summary.model.clone(),
                    turn_elapsed,
                    Some((summary.total_input_tokens, summary.total_output_tokens)),
                );
                self.transcript.merge_turn(&summary.messages);
                self.history = summary.messages;
                self.stats.total_steps += summary.total_steps;
                self.stats.total_input_tokens += summary.total_input_tokens;
                self.stats.total_output_tokens += summary.total_output_tokens;
                self.stats.total_cost_usd += summary.total_cost_usd;
                self.stats.turns += 1;
                // Counter reset (turn over; live depth back to 0 —
                // the server's delegation view closed with the turn).
                // tool_calls_cur intentionally KEEPS the finished
                // turn's count for the session panel until the next
                // turn resets it.
                self.depth_cur = 0;
                // Pending input auto-submits as the next turn.
                if let Some(text) = self.pending_input.take() {
                    return self.handle_start_turn(text).await;
                }
            }
            Err(msg) => {
                self.run_state = RunState::Error(msg.clone());
                // fix-19 兜底: a held step meta (its request completed,
                // then the turn died before tools/next-request proved
                // the step's shape) materializes at the tail — the
                // stats the user watched live are not lost. Then the
                // buffers drop (the partial streamed text is the
                // Err-path's accepted loss). The server owns history —
                // no local reload; a reconnect snapshot reconciles.
                self.transcript.flush_pending_step_meta();
                self.transcript.clear_streaming();
                self.depth_cur = 0;
                tracing::warn!("turn failed: {msg}");
                // No auto-retry: restore pending input to the editor.
                if let Some(text) = self.pending_input.take() {
                    self.input.set_text(&text);
                }
            }
        }
        (DispatchOutcome::Continue, None)
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

    /// Submit one user turn (REPL `handle_normal_input` semantics,
    /// client edition): push the local display mirrors → send
    /// `Submit` → optimistic in-flight state. The server owns
    /// history/compaction/provider; events stream back as broadcasts.
    async fn start_turn(&mut self, prompt: String) -> (DispatchOutcome, Option<Action>) {
        if prompt.trim().is_empty() {
            return (DispatchOutcome::Continue, None);
        }
        if self.is_running() {
            // Queued; auto-submitted on TurnDone (spec: pending_input).
            self.pending_input = Some(prompt);
            return (DispatchOutcome::Continue, None);
        }
        if !self.link_ready() {
            // Disconnected: restore to the editor (queueing would
            // deadlock — nothing would drain the queue).
            self.input.set_text(&prompt);
            self.set_notice("已断线，重连中…");
            return (DispatchOutcome::Continue, None);
        }
        // A new turn clears any transient notice (e.g. a rejected /new).
        self.status_notice = None;
        self.notice_ticks = 0;
        self.tool_calls_cur = 0;
        self.depth_cur = 0;
        self.cancel_sent = false;

        let user_message = Message {
            role: MessageRole::User,
            content: prompt.clone(),
            tool_call_id: None,
            name: None,
            tool_calls: None,
            reasoning_content: None,
        };
        self.history.push(user_message);
        self.transcript.push_user(&prompt);

        self.link.send(ClientMsg::Submit { text: prompt });
        // Optimistic in-flight state; the broadcasts (RequestStart
        // first) take over from here. If the server is mid-turn from
        // ANOTHER client, its directed notice corrects us (the pushed
        // user entry stays — it mirrors what the user sees themselves
        // type; the next snapshot reconciles).
        self.turn_active = true;
        self.run_state = RunState::Thinking;
        self.turn_started = Some(Instant::now());
        self.transcript.begin_streaming();
        (DispatchOutcome::Continue, None)
    }

    /// `/`-command routing (TUI subset; panels themselves are P3).
    async fn handle_slash(&mut self, input: &str) -> (DispatchOutcome, Option<Action>) {
        match slash::parse(input) {
            SlashCommand::Exit => (DispatchOutcome::Quit, None),
            SlashCommand::Help => {
                self.modal = Modal::HelpOpen;
                // select-1: overlay opening clears the selection.
                self.transcript.clear_selection();
                // slash-1: no modal opens on top of a live completion
                // list (unreachable via submit — take() cleared it —
                // but pending-input restores make this reachable).
                self.input.close_completion();
                (DispatchOutcome::Continue, None)
            }
            SlashCommand::New => {
                if self.is_running() {
                    // Clearing mid-turn would race the turn's broadcasts;
                    // cancel first, then /new again. A transient notice
                    // (yellow), NOT RunState::Error — the red × misreads
                    // as a turn failure.
                    self.set_notice("turn running — Ctrl+C to cancel, then /new");
                    return (DispatchOutcome::Continue, None);
                }
                if !self.link_ready() {
                    self.set_notice("已断线，重连中…");
                    return (DispatchOutcome::Continue, None);
                }
                // The server resets and broadcasts `session_reset` +
                // fresh snapshots (wholesale swap); nothing clears
                // locally here — localhost round-trip is imperceptible.
                self.link.send(ClientMsg::NewSession);
                (DispatchOutcome::Continue, None)
            }
            SlashCommand::Model { alias } => {
                let known = self.config.models.contains_key(&alias)
                    || openslate_core::model_config::resolve_model(&self.config, &alias).is_ok();
                if known {
                    if self.link_ready() {
                        self.pending_confirm = Some(format!("model → {alias}"));
                        self.link.send(ClientMsg::SetModel {
                            alias: alias.clone(),
                        });
                    } else {
                        self.set_notice("已断线，重连中…");
                    }
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
            SlashCommand::Copy { arg } => {
                // copy-1: the `/copy [all|tool]` family — the same
                // chain as Ctrl+Y; anything else shows the usage.
                match arg.as_deref() {
                    None => self.copy_last_output(),
                    Some("all") => self.copy_all(),
                    Some("tool") => self.copy_tool_output(),
                    Some(_) => self.set_notice("usage: /copy [all|tool]"),
                }
                (DispatchOutcome::Continue, None)
            }
            SlashCommand::Status | SlashCommand::Agents => {
                // restyle-1: both commands open the floating
                // agents/session panel (the old persistent sidebar is
                // retired). The panel claims the Sidebar focus stop —
                // it is visible, so keys are not swallowed by an
                // invisible region.
                self.open_agents_panel();
                (DispatchOutcome::Continue, None)
            }
            SlashCommand::Provider => {
                self.open_models_panel();
                (DispatchOutcome::Continue, None)
            }
            SlashCommand::Unknown { raw } => {
                self.run_state = RunState::Error(format!("unknown command: {raw} (try /help)"));
                (DispatchOutcome::Continue, None)
            }
        }
    }

    // ── Approvals ──────────────────────────────────────────────────────

    /// Deliver the user's answer to the front pending approval: send it
    /// and let the server's `approval_resolved` BROADCAST drive every
    /// client-side effect (banner removal, decision line, state resume)
    /// — single path for the answerer and every observer, first answer
    /// wins server-side, late answers get a directed notice.
    fn approval_respond(&mut self, choice: ApprovalChoice) {
        // interactive-1: the banner's buttons die with the answer.
        self.hover = None;
        self.mouse_down_target = None;
        let Some(pending) = self.approval.current() else {
            return;
        };
        self.link.send(ClientMsg::ApprovalAnswer {
            id: pending.id,
            choice: approval_choice_to_msg(choice),
        });
        // The banner stays until the broadcast arrives (localhost
        // round-trip is imperceptible; a lost race simply means another
        // client answered first and the broadcast carries THEIR choice).
    }

    /// Send `Cancel` for the in-flight turn (Ctrl+C while running /
    /// deny-and-cancel from the approval banner). The engine-side
    /// cancel runs on the server; `TurnOk/TurnError` closes the turn.
    fn send_cancel(&mut self) {
        self.cancel_sent = true;
        self.link.send(ClientMsg::Cancel);
    }

    /// Whether outbound turn-affecting actions may leave (freeze gate:
    /// the link must be up).
    fn link_ready(&self) -> bool {
        self.link_phase == LinkPhase::Connected
    }

    // ── Context / rendering ────────────────────────────────────────────

    /// Effective model alias — the server's session-level state
    /// (snapshot-seeded, `ModelChanged`-swapped).
    fn effective_model_alias(&self) -> String {
        self.model_alias.clone()
    }

    /// All configured model aliases, sorted (slash-1: the `/model`
    /// argument completion's dynamic choices — sorted because the
    /// backing `HashMap` iterates unordered and the list must rank
    /// deterministically).
    fn sorted_model_aliases(&self) -> Vec<String> {
        let mut aliases: Vec<String> = self.config.models.keys().cloned().collect();
        aliases.sort();
        aliases
    }

    /// Whether a terminal coordinate sits inside the completion
    /// overlay's last-rendered rectangle (slash-3's mouse-swallow hit
    /// test; `None` rect = list closed → always false).
    fn completion_hit(&self, column: u16, row: u16) -> bool {
        self.completion_hit_rect.is_some_and(|r| {
            column >= r.x
                && column < r.x.saturating_add(r.width)
                && row >= r.y
                && row < r.y.saturating_add(r.height)
        })
    }

    /// Whether a terminal coordinate sits inside `rect` (the shared
    /// cell-containment test for the interactive-1 button rects).
    fn rect_hit(rect: Rect, column: u16, row: u16) -> bool {
        column >= rect.x
            && column < rect.x.saturating_add(rect.width)
            && row >= rect.y
            && row < rect.y.saturating_add(rect.height)
    }

    // ── interactive-1: hover & button clicks ───────────────────────────

    /// A plain mouse move at `(column, row)` — pure hover-state
    /// update. A miss CLEARS the hover; the next draw repaints either
    /// way. No-op while mouse capture is OFF (the terminal then sends
    /// no mouse events at all; a synthetically dispatched Move must
    /// not light anything up).
    fn on_mouse_move(&mut self, column: u16, row: u16) {
        if !self.mouse_capture {
            return;
        }
        let hit = self.button_hit(column, row);
        if self.hover != hit {
            self.hover = hit;
        }
    }

    /// The button under a terminal cell, hit-tested against the LAST
    /// render's recorded rectangles, by the spec's fixed priority:
    /// approval buttons (the modal outranks everything) > completion
    /// rows > the hint-row `? help` > the jump-to-bottom hint.
    /// Non-row cells inside the completion overlay are DEAD space —
    /// no target and no fall-through to the covered hint beneath.
    /// Layers other than ApprovalActive/Normal expose no buttons.
    fn button_hit(&self, column: u16, row: u16) -> Option<HoverTarget> {
        match self.active_layer() {
            Layer::ApprovalActive => self
                .approval_button_rects
                .iter()
                .find(|(_, rect)| Self::rect_hit(*rect, column, row))
                .map(|(choice, _)| HoverTarget::ApproveButton(*choice)),
            Layer::Normal => {
                if self.completion_hit(column, row) {
                    return self
                        .completion_hit_rect
                        .and_then(|rect| self.input.completion_row_at(rect, row))
                        .map(HoverTarget::CompletionRow);
                }
                if self
                    .help_hit_rect
                    .is_some_and(|rect| Self::rect_hit(rect, column, row))
                {
                    return Some(HoverTarget::HelpHint);
                }
                if self
                    .transcript
                    .hint_rect()
                    .is_some_and(|rect| Self::rect_hit(rect, column, row))
                {
                    return Some(HoverTarget::JumpBottom);
                }
                None
            }
            _ => None,
        }
    }

    /// Consume a pending button press: the release triggers it iff it
    /// lands on the SAME target the press hit. Any mismatched release
    /// (or no press) just drops the press — the gesture falls back to
    /// its normal semantics.
    fn take_button_trigger(&mut self, column: u16, row: u16) -> Option<HoverTarget> {
        let pressed = self.mouse_down_target.take()?;
        (self.button_hit(column, row) == Some(pressed)).then_some(pressed)
    }

    /// Fire one button click (called at release). Every target maps
    /// onto the EXISTING action path — no parallel behavior branches:
    /// the help button rides ToggleHelp (the `?` key's open path),
    /// the banner buttons ride ApprovalRespond (the y/n/a keys), a
    /// completion row rides the input's Enter three-branch submit,
    /// and the jump hint replays the legacy positional Click (which
    /// is exactly what the no-drag release would have fired).
    fn trigger_button(&mut self, target: HoverTarget, column: u16, row: u16) -> Option<Action> {
        match target {
            HoverTarget::HelpHint => Some(Action::ToggleHelp),
            HoverTarget::ApproveButton(choice) => Some(Action::ApprovalRespond(choice)),
            HoverTarget::CompletionRow(index) => {
                let ctx = self.make_ctx();
                self.input.completion_click(&ctx, index)
            }
            HoverTarget::JumpBottom => Some(Action::Click(column, row)),
        }
    }

    /// Whether the current hover still has a live target (checked at
    /// the top of every render): the owning layer must be active and
    /// the target must still exist (approval pending, completion row
    /// in range, `? help` segment drawn, jump hint drawn). This one
    /// validation point implements the spec's "clear on completion
    /// close / approval answer / layer change" triggers — a vanished
    /// rect can never keep painting a highlight.
    fn hover_valid(&self) -> bool {
        match self.hover {
            None => true,
            Some(HoverTarget::ApproveButton(_)) => {
                self.active_layer() == Layer::ApprovalActive
                    && !self.approval_button_rects.is_empty()
            }
            Some(HoverTarget::HelpHint) => {
                self.active_layer() == Layer::Normal && self.help_hit_rect.is_some()
            }
            Some(HoverTarget::JumpBottom) => {
                self.active_layer() == Layer::Normal && self.transcript.hint_rect().is_some()
            }
            Some(HoverTarget::CompletionRow(index)) => {
                self.active_layer() == Layer::Normal && self.input.completion_row_exists(index)
            }
        }
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
                // restyle-1: no context-window snapshot reaches the TUI
                // yet — the status meter stays hidden until one does.
                context_remaining: None,
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
                model_aliases: self.sorted_model_aliases(),
            },
            size: self.size,
            notice: self.status_notice.clone(),
        }
    }

    /// Full-screen render routing (frozen call sites). restyle-1:
    /// single-column document flow — [transcript / hint row / rule /
    /// input row / rule / status line], plus overlays (approval
    /// banner, help, exit confirm, agents panel) and the input cursor.
    fn render(&mut self, f: &mut ratatui::Frame) {
        let area = f.area();
        self.size = (area.width, area.height);
        let ctx = self.make_ctx();

        // Minimum-size guard: central warning panel + no fragile layout
        // (lane-c painter; `render` returns right after — nothing below
        // the minimum size ever reaches the layout code).
        if area.width < MIN_COLS || area.height < MIN_ROWS {
            Self::render_min_size_guard(f, area, &self.theme);
            return;
        }

        // Bottom区 (restyle-1, top→bottom): hint row → full-width `─`
        // rule → single-line input → `─` rule → status line. The main
        // transcript column fills everything above. slash-3: the
        // completion list is an OVERLAY floating over the transcript's
        // bottom rows (growing up from the hint row) — the layout
        // itself never changes: full-height main whether the list is
        // open or closed (no inline reflow).
        let [main, hint_row, rule_a, input_row, rule_b, status_row] =
            area.layout(&ratatui::layout::Layout::vertical([
                ratatui::layout::Constraint::Min(3),
                ratatui::layout::Constraint::Length(1),
                ratatui::layout::Constraint::Length(1),
                ratatui::layout::Constraint::Length(self.input.desired_height()),
                ratatui::layout::Constraint::Length(1),
                ratatui::layout::Constraint::Length(1),
            ]));

        // Main column: unbordered block, content padded 2 columns each
        // side (the document-flow whitespace framing).
        let main_pad =
            ratatui::widgets::Block::new().padding(ratatui::widgets::Padding::horizontal(2));
        // interactive-1 (T4): the jump hint's hovered presentation.
        self.transcript
            .set_hint_hovered(matches!(self.hover, Some(HoverTarget::JumpBottom)));
        self.transcript.render(f, main_pad.inner(main), &ctx);

        // interactive-1 (T1): the painter returns the `? help`
        // segment's rect for the hit-tests (None when absent).
        self.help_hit_rect = Self::render_hint_row(
            f,
            hint_row,
            &ctx,
            matches!(self.hover, Some(HoverTarget::HelpHint)),
        );
        Self::render_rule(f, rule_a, &self.theme);
        self.input.render(f, input_row, &ctx);
        Self::render_rule(f, rule_b, &self.theme);
        self.status.render(f, status_row, &ctx);

        // slash-3: the completion overlay — `Clear` + the surface
        // background + the existing row renderer, in a rectangle
        // growing UP from the hint row over the transcript's bottom
        // rows. Height = min(row count (incl. a potential `(i/n)`
        // line), 8, available height above the hint row) — clamped to
        // the space that exists, never touching the hint/input area.
        // Non-modal: no frame (mcode select-list row look), no
        // dim_outside, NOT in the modal stack. Painted after the base
        // UI and before the modal stack purely defensively — modals
        // close the list on open (slash-1), so the two never coexist.
        self.completion_hit_rect = None;
        if self.active_layer() == Layer::Normal {
            let rows = self.input.completion_rows();
            let avail = hint_row.y.saturating_sub(area.y);
            let height = rows.min(avail);
            if height > 0 {
                let rect = ratatui::layout::Rect {
                    x: area.x,
                    y: hint_row.y - height,
                    width: area.width,
                    height,
                };
                f.render_widget(ratatui::widgets::Clear, rect);
                // The surface: the user-message band slot (bg-only
                // Style — dark #262626 / light #F5F5F5 / ansi 235,
                // exactly the spec's low-key surface family; slot
                // reuse, no new palette entry).
                f.buffer_mut().set_style(rect, self.theme.user_message_bg);
                // interactive-1 (T3): the hovered row's light style.
                let hover_row = match self.hover {
                    Some(HoverTarget::CompletionRow(index)) => Some(index),
                    _ => None,
                };
                self.input.render_completion(f, rect, &ctx, hover_row);
                self.completion_hit_rect = Some(rect);
            }
        }

        // Modal stack, strictly by frozen precedence: ApprovalActive >
        // HelpOpen > ConfirmExit > AgentsPanel — exactly one overlay
        // layer paints per frame. The approval banner is a
        // transcript-bottom banner rather than a full-screen modal
        // (non-modal: the base UI behind it is NOT dimmed), but it
        // still preempts the other overlays: `active_layer()` stays
        // `ApprovalActive` until the queue drains, so anything opened
        // underneath simply waits and repaints afterwards.
        // interactive-1 (T2): the banner's y/n/a button rects are
        // recorded for the hit-tests; cleared whenever no banner
        // paints (the arm below only runs on ApprovalActive frames).
        self.approval_button_rects.clear();
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
                self.approval.set_hover(match self.hover {
                    Some(HoverTarget::ApproveButton(choice)) => Some(choice),
                    _ => None,
                });
                self.approval.render(f, banner, &ctx);
                // The hint line is the banner BODY's 3rd row (the body
                // = the Paragraph's rect inside `render`). Buttons
                // clipped by a narrow banner stay unregistered.
                if banner.width >= 2 {
                    let body = ratatui::layout::Rect {
                        x: banner.x + 2,
                        y: banner.y,
                        width: banner.width - 2,
                        height: banner.height,
                    };
                    self.approval_button_rects = crate::components::approval::button_rects(body);
                }
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
                f.render_widget(ratatui::widgets::Clear, inner);
                Self::render_exit_confirm(f, inner, &self.theme);
            }
            Layer::AgentsPanel => {
                // restyle-1: the floating agents/session panel — the
                // rounded panel frame carrying the two former sidebar
                // components over a dimmed base UI.
                let inner = Self::agents_panel_rect(area);
                Self::dim_outside(f.buffer_mut(), inner);
                f.render_widget(ratatui::widgets::Clear, inner);
                let content = crate::panel::render_frame(
                    f,
                    inner,
                    "Agents",
                    self.theme.overlay_title,
                    &self.theme,
                    // panel-meta-1: the close-key hint in the header.
                    Some("Ctrl+T 关闭"),
                );
                if content.width > 0 && content.height > 0 {
                    let [agents_area, session_area] =
                        content.layout(&ratatui::layout::Layout::vertical([
                            ratatui::layout::Constraint::Fill(1),
                            ratatui::layout::Constraint::Fill(1),
                        ]));
                    self.agents.render(f, agents_area, &ctx);
                    self.session.render(f, session_area, &ctx);
                }
            }
            Layer::ModelsPanel => {
                // model-mgmt-2: the `/provider` management panel — same
                // rounded-panel family as Agents, over a dimmed base.
                let inner = Self::models_panel_rect(area);
                Self::dim_outside(f.buffer_mut(), inner);
                f.render_widget(ratatui::widgets::Clear, inner);
                let content = crate::panel::render_frame(
                    f,
                    inner,
                    "Models",
                    self.theme.overlay_title,
                    &self.theme,
                    // panel-meta-1: the close-key hint in the header.
                    Some("Esc 关闭"),
                );
                if content.width > 0 && content.height > 0 {
                    self.models.render(f, content, &ctx);
                }
            }
            Layer::Normal => {}
        }

        // Input cursor (only when editing and no modal).
        if self.focus == Focus::Input && self.active_layer() == Layer::Normal {
            if let Some(position) = self.input.cursor_position(input_row) {
                f.set_cursor_position(position);
            }
        }

        // interactive-1: drop a hover whose target did not survive
        // THIS frame (approval answered, completion closed/re-filtered,
        // `? help` segment dropped, jump hint gone, layer changed) —
        // validated AFTER the fresh hit rects are recorded so a
        // vanished target dies the same frame it stops painting (the
        // stale highlight never gets a frame to show). The single
        // validation point behind every clear trigger besides
        // MouseMove's own miss test.
        if !self.hover_valid() {
            self.hover = None;
            // Repaint-dependent components re-read the flag next frame;
            // the vanished target painted nothing this frame, so there
            // is no stale highlight to correct.
        }
    }

    // ── restyle-1 bottom-area painters ──────────────────────────────────

    /// The rotating tip list (P1, restyle-1): one tip at a time rides
    /// the hint row's right slot (muted, right-aligned), swapping every
    /// 30 s (composer.ts tips rotation).
    const HINT_TIPS: [&str; 5] = [
        "Ctrl+T 打开 agents 浮层",
        "Ctrl+Y 复制最后输出",
        "/model 别名 切换模型",
        "点击思维链/工具行可展开",
        "/provider 管理 provider/模型/级别",
    ];

    /// The current rotation slot (elapsed-since-start / 30 s).
    fn current_tip() -> &'static str {
        static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        let start = START.get_or_init(Instant::now);
        let slot = (start.elapsed().as_secs() / 30) as usize % Self::HINT_TIPS.len();
        Self::HINT_TIPS[slot]
    }

    /// The hint row above the input (minimax composer header):
    /// `Message · Enter send · ? help` — mode label BOLD signal, key
    /// names BOLD text, verbs muted, ` · ` dim separators. Narrow
    /// windows shorten the label rung by rung (composer.ts:266), and a
    /// transient notice replaces the whole row (warning color). P1:
    /// a rotating tip fills the right slot when ≥4 columns remain.
    ///
    /// interactive-1 (T1): the `? help` segment is a BUTTON. `hovered`
    /// restyles exactly its two spans (`?` and ` help`) in
    /// `theme.hover`; the return value is the segment's terminal rect
    /// — `None` whenever the segment did not render (a notice owns
    /// the row, the rung ladder dropped it, or the row is empty).
    /// Painter and hit-test share the returned geometry, so the
    /// button can never drift from its pixels.
    fn render_hint_row(
        f: &mut ratatui::Frame,
        area: Rect,
        ctx: &AppCtx,
        hovered: bool,
    ) -> Option<Rect> {
        if area.width == 0 || area.height == 0 {
            return None;
        }
        let theme = &ctx.theme;
        let mut help_rect = None;
        let line = if let Some(notice) = &ctx.notice {
            Line::from(Span::styled(format!("  {notice}"), theme.warning))
        } else {
            // (label, key, verb) parts; pick the longest run that fits.
            let width = area.width as usize;
            let dot = theme.icons.set().dot;
            let sep = move |out: &mut Vec<Span<'static>>| {
                out.push(Span::styled(format!(" {dot} "), theme.line));
            };
            // The return carries the INDEX of the `?` span when the
            // run includes the help segment (the two trailing spans
            // `?` + ` help` are the button).
            let run = |with_help: bool| -> (Vec<Span<'static>>, usize, Option<usize>) {
                let mut spans = vec![Span::styled("  ".to_owned(), theme.line)];
                spans.push(Span::styled("Message".to_owned(), theme.user_label));
                sep(&mut spans);
                spans.push(Span::styled("Enter".to_owned(), theme.header));
                spans.push(Span::styled(" send".to_owned(), theme.muted));
                let help_at = with_help.then(|| {
                    sep(&mut spans);
                    spans.push(Span::styled("?".to_owned(), theme.header));
                    spans.push(Span::styled(" help".to_owned(), theme.muted));
                    spans.len() - 2
                });
                let w = spans.iter().map(|s| s.width()).sum();
                (spans, w, help_at)
            };
            let (mut spans, used, help_at) = match run(true) {
                (spans, w, help_at) if w <= width => (spans, w, help_at),
                _ => match run(false) {
                    (spans, w, help_at) if w <= width => (spans, w, help_at),
                    _ => (
                        vec![
                            Span::styled("  ".to_owned(), theme.line),
                            Span::styled("Message".to_owned(), theme.user_label),
                        ],
                        11,
                        None,
                    ),
                },
            };
            // interactive-1: hover restyles the `?` + ` help` spans
            // (nothing else) and fixes the button rect from the SAME
            // span walk the renderer performs.
            if let Some(i) = help_at {
                let start = spans[..i].iter().map(|s| s.width()).sum::<usize>() as u16;
                let seg_w = (spans[i].width() + spans[i + 1].width()) as u16;
                if hovered {
                    spans[i].style = theme.hover;
                    spans[i + 1].style = theme.hover;
                }
                help_rect = Some(Rect {
                    x: area.x + start,
                    y: area.y,
                    width: seg_w,
                    height: 1,
                });
            }
            // Right-slot tip (P1): ≥4-column gap, right-aligned, muted.
            let tip = Self::current_tip();
            let tip_w = ratatui::text::Span::raw(tip).width();
            if used + 4 + tip_w <= width {
                let gap = area.width as usize - used - tip_w;
                spans.push(Span::raw(" ".repeat(gap)));
                spans.push(Span::styled(tip.to_owned(), theme.muted));
            }
            Line::from(spans)
        };
        f.render_widget(ratatui::widgets::Paragraph::new(line), area);
        help_rect
    }

    /// One full-width dim `─` rule (the line color) — the input row's
    /// top and bottom separators (restyle-1).
    fn render_rule(f: &mut ratatui::Frame, area: Rect, theme: &Theme) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let rule = theme.icons.set().horizontal.repeat(area.width as usize);
        f.render_widget(
            ratatui::widgets::Paragraph::new(Line::from(rule)).style(theme.line),
            Rect { height: 1, ..area },
        );
    }

    // ── overlay geometry (restyle-1) ────────────────────────────────────

    /// Overlay rect for the help layer (60% × 70%, centered). Extracted
    /// so the dimming pass and the painter (and tests) agree on the
    /// exact geometry.
    fn help_overlay_rect(area: Rect) -> Rect {
        area.centered(
            ratatui::layout::Constraint::Percentage(60),
            ratatui::layout::Constraint::Percentage(70),
        )
    }

    /// Overlay rect for the exit-confirmation layer (44% × 7 rows,
    /// centered — the rounded panel frame needs ≥6 rows).
    fn exit_confirm_rect(area: Rect) -> Rect {
        area.centered(
            ratatui::layout::Constraint::Percentage(44),
            ratatui::layout::Constraint::Length(7),
        )
    }

    /// Overlay rect for the agents/session panel (restyle-1): 50% × 60%,
    /// centered.
    fn agents_panel_rect(area: Rect) -> Rect {
        area.centered(
            ratatui::layout::Constraint::Percentage(50),
            ratatui::layout::Constraint::Percentage(60),
        )
    }

    /// Overlay rect for the `/provider` management panel (model-mgmt-2):
    /// 60% × 66%, centered — one notch wider than Agents for the
    /// provider/model rows and Chinese hints.
    fn models_panel_rect(area: Rect) -> Rect {
        area.centered(
            ratatui::layout::Constraint::Percentage(60),
            ratatui::layout::Constraint::Percentage(66),
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

    // ── lane-c overlay painters (render-section helpers) ────────

    /// Minimum-size guard painter (restyle-1: panel-ized): clears the
    /// screen, then a rounded panel (提示) centers the three warning
    /// lines — error headline plus muted size/recovery detail.
    /// [`Self::render`] returns immediately after this — no layout runs
    /// below the minimum size, so a clipped terminal can never panic
    /// the splitter (degenerate panel sizes degrade to a bare title
    /// via [`crate::panel`]).
    fn render_min_size_guard(f: &mut ratatui::Frame, area: ratatui::layout::Rect, theme: &Theme) {
        f.render_widget(ratatui::widgets::Clear, area);
        // The `×` dimension signs are chrome — localize (identity
        // outside the ascii tier).
        let g = theme.icons.set();
        let inner = area.centered(
            ratatui::layout::Constraint::Percentage(80),
            ratatui::layout::Constraint::Length(7),
        );
        // No close-key meta: the guard has no key to dismiss it (the
        // terminal recovering its size is the only way out).
        let content =
            crate::panel::render_frame(f, inner, "提示", theme.overlay_title, theme, None);
        let body = ratatui::text::Text::from(vec![
            ratatui::text::Line::styled(
                localize(&format!("终端过小，请 ≥ {MIN_COLS} 列 × {MIN_ROWS} 行"), &g),
                theme.error,
            ),
            ratatui::text::Line::styled(
                localize(&format!("当前 {} × {}", area.width, area.height), &g),
                theme.muted,
            ),
            ratatui::text::Line::styled("放大窗口后自动恢复", theme.muted),
        ]);
        f.render_widget(
            ratatui::widgets::Paragraph::new(body).alignment(ratatui::layout::Alignment::Center),
            content,
        );
    }

    /// Exit-confirmation painter (`ConfirmExit` layer — lowest overlay
    /// priority), restyle-1: the rounded panel frame (`Exit?` title)
    /// with the warning and key line centered inside.
    /// `[y]/[q]` quit, `[Esc]` stays (key handling is the frozen
    /// dispatcher's job; this only paints).
    fn render_exit_confirm(f: &mut ratatui::Frame, inner: ratatui::layout::Rect, theme: &Theme) {
        // panel-meta-1: Esc stays/cancels — the close-key hint rides
        // the header's right slot.
        let content = crate::panel::render_frame(
            f,
            inner,
            "Exit?",
            theme.overlay_title,
            theme,
            Some("Esc 取消"),
        );
        // Content rows: breathing blank, the warning, the key line —
        // vertically balanced inside the 5-row content area.
        let body = ratatui::text::Text::from(vec![
            ratatui::text::Line::default(),
            ratatui::text::Line::styled("未完成的轮次将被取消", theme.warning),
            ratatui::text::Line::from(vec![
                ratatui::text::Span::styled("[y/q/Ctrl+C] ", theme.approval),
                ratatui::text::Span::styled("退出", theme.assistant),
                ratatui::text::Span::styled(format!(" {} ", theme.icons.set().dot), theme.line),
                ratatui::text::Span::styled("[Esc] ", theme.approval),
                ratatui::text::Span::styled("取消", theme.assistant),
            ]),
        ]);
        f.render_widget(
            ratatui::widgets::Paragraph::new(body).alignment(ratatui::layout::Alignment::Center),
            content,
        );
    }

    // ── Shutdown & summary ─────────────────────────────────────────────

    // web-1: shutdown is link-drop only — see the stub near `run`.
    // (Kept as a section marker; the stub logs and returns.)

    /// The stdout one-liner printed after terminal restore.
    fn summary(&self) -> SessionSummary {
        SessionSummary {
            run_id: self.session_run_id().map(str::to_owned),
            turns: self.stats.turns,
            total_cost_usd: self.stats.total_cost_usd,
        }
    }
}

/// Map one models-overlay commit to its `ClientMsg`s (web-1: the CRUD
/// wire vocabulary). Returns the human description (used in the success
/// notice) plus the messages — a provider upsert with a pasted API key
/// maps to TWO sends (`upsert_provider` + `set_api_key`, per the wire
/// contract; the server derives the env var name and writes `.env`
/// 0600 itself).
fn client_msgs_for_change(change: &ModelsChange) -> (String, Vec<ClientMsg>) {
    use openslate_protocol::{ModelDto, ProviderDto};
    match change {
        ModelsChange::UpsertProvider { name, cfg, env_key } => {
            let mut msgs = vec![ClientMsg::UpsertProvider {
                name: name.clone(),
                provider: ProviderDto {
                    base_url: cfg.base_url.clone(),
                    api_key_env: cfg.api_key_env.clone(),
                    adapter: cfg.adapter.clone(),
                    title: cfg.title.clone(),
                    max_attempts: cfg.max_attempts,
                    retry_base_ms: cfg.retry_base_ms,
                },
            }];
            if let Some((var, value)) = env_key {
                // `set_api_key` carries the provider name + the raw
                // value; the server owns the `<NAME>_API_KEY`/`.env`
                // derivation. The var name stays client-side only as
                // form-display context.
                let _ = var;
                msgs.push(ClientMsg::SetApiKey {
                    provider: name.clone(),
                    value: value.clone(),
                });
            }
            (format!("provider {name}"), msgs)
        }
        ModelsChange::UpsertModel { entry, cfg } => (
            format!("模型条目 {entry}"),
            vec![ClientMsg::UpsertModel {
                entry: entry.clone(),
                model: ModelDto {
                    provider: cfg.provider.clone(),
                    model: cfg.model.clone(),
                    max_context_tokens: cfg.max_context_tokens,
                    max_output_tokens: cfg.max_output_tokens,
                    supports_tool_call: cfg.supports_tool_call,
                    supports_vision: cfg.supports_vision,
                    supports_reasoning: cfg.supports_reasoning,
                    input_price_per_mtok: cfg.input_price_per_mtok,
                    output_price_per_mtok: cfg.output_price_per_mtok,
                },
            }],
        ),
        ModelsChange::SetLevel { level, entry } => (
            format!("levels.{level} → {entry}"),
            vec![ClientMsg::SetLevel {
                level: level.clone(),
                entry: entry.clone(),
            }],
        ),
        ModelsChange::RemoveProvider { name } => (
            format!("删除 provider {name}"),
            vec![ClientMsg::DeleteProvider { name: name.clone() }],
        ),
        ModelsChange::RemoveModel { entry } => (
            format!("删除模型条目 {entry}"),
            vec![ClientMsg::DeleteModel {
                entry: entry.clone(),
            }],
        ),
        ModelsChange::RemoveLevel { level } => (
            format!("删除级别 {level}"),
            vec![ClientMsg::DeleteLevel {
                level: level.clone(),
            }],
        ),
    }
}

/// Absolutize `path` (joining the cwd) so copy notices always name a
/// path the shell can open. The cwd being unresolvable keeps the
/// original (best effort — an unopenable path in a notice is still
/// more honest than nothing).
fn absolute_path(path: std::path::PathBuf) -> std::path::PathBuf {
    if path.is_absolute() {
        path
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(path),
            Err(_) => path,
        }
    }
}

/// Merge adjacent `Engine(Delta)` actions in a drained batch (delta
/// flooding defense, spec R4). interactive-1: an adjacent run of
/// `MouseMove` actions likewise keeps only the LAST position — hover
/// hit-tests the final resting cell, not the path. Public for the
/// dispatch-level drain pipeline tests (`dispatch` itself does not
/// coalesce — coalescing happens between the drain and the dispatch
/// loop in `App::run`).
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
        } else if let Action::MouseMove(next_col, next_row) = &action {
            if let Some(Action::MouseMove(prev_col, prev_row)) = out.last_mut() {
                *prev_col = *next_col;
                *prev_row = *next_row;
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

#[cfg(test)]
mod overlay_render_tests {
    //! lane-c overlay painters: minimum-size guard + exit confirmation
    //! (restyle-1: both are rounded panels now).
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
        // restyle-1: the guard is a rounded panel — the frame header
        // with the 提示 title renders.
        assert!(screen.contains('╭'), "panel frame header present");
        // panel-meta-1: the guard has NO close-key meta (nothing can
        // dismiss it — the terminal recovering its size is the exit).
        assert!(
            !screen.contains("关闭"),
            "no close-key meta on the guard: {screen:?}"
        );
        // No transcript/status content leaked around the guard: every
        // row outside the panel is blank.
        assert!(rows.len() == 10);
    }

    #[test]
    fn min_size_guard_never_panics_on_absurd_sizes() {
        // Just-below-minimum and absurdly small terminals must not panic
        // the centered layout (the reason the guard exists); degenerate
        // panel areas degrade to a bare title via `crate::panel`.
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
            let inner = App::exit_confirm_rect(f.area());
            App::render_exit_confirm(f, inner, &Theme::new())
        });
        let screen = rows.join("\n");
        assert!(screen.contains("Exit?"), "title");
        assert!(screen.contains("未完成的轮次将被取消"), "warning line");
        assert!(screen.contains("[y/q/Ctrl+C]"), "quit keys");
        assert!(screen.contains("[Esc]"), "cancel key");
        // panel-meta-1: the close-key meta rides the header row (the
        // body's key line renders `[Esc] 取消` — bracketed, distinct).
        let title_row = rows
            .iter()
            .find(|r| r.contains("Exit?"))
            .expect("title row");
        assert!(
            title_row.contains("Esc 取消"),
            "close-key meta on the header: {title_row:?}"
        );
        // restyle-1: the rounded panel frame (no legacy box-drawing
        // corners).
        assert!(screen.contains('╭'), "panel header present");
        for frame_char in ['┌', '┐', '└', '┘'] {
            assert!(
                !screen.contains(frame_char),
                "legacy frame corners retired, found {frame_char}"
            );
        }
        // No REVERSED cells anywhere (the title-bar era is over).
        let mut terminal = ratatui::Terminal::new(TestBackend::new(80, 24)).expect("terminal");
        terminal
            .draw(|f| {
                let inner = App::exit_confirm_rect(f.area());
                App::render_exit_confirm(f, inner, &Theme::new())
            })
            .expect("painting must not panic");
        let buf = terminal.backend().buffer();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                assert!(
                    !buf.cell((x, y))
                        .expect("cell")
                        .modifier
                        .contains(Modifier::REVERSED),
                    "REVERSED retired at ({x},{y})"
                );
            }
        }
    }

    #[test]
    fn exit_confirm_box_is_centered() {
        let rows = draw(80, 24, |f| {
            let inner = App::exit_confirm_rect(f.area());
            App::render_exit_confirm(f, inner, &Theme::new())
        });
        // 44% of 80 = 35 wide → the panel starts around column 22; every
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
            (12..=26).contains(&left_margin),
            "horizontally centered (left margin {left_margin}): {warning_row:?}"
        );
        let title_row = rows
            .iter()
            .find(|r| r.contains("Exit?"))
            .expect("title row");
        // Vertically: Length(7) centered in 24 rows → title row ~8-10.
        let title_y = rows
            .iter()
            .position(|r| r.contains("Exit?"))
            .expect("title row index");
        assert!(
            (7..=11).contains(&title_y),
            "vertically centered (title at row {title_y}): {title_row:?}"
        );
    }
}

#[cfg(test)]
mod borderless_layout_tests {
    // restyle-1 single-column layout composition (the `App::render`
    // region section): hint row / rules / single-line input / status
    // line, the floating agents panel, and the overlay occlusion
    // semantics. The components inside the regions draw their own
    // content — these tests assert only the LAYER the App owns.

    use super::*;
    use crate::client::MemLink;
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;
    use ratatui::Terminal;

    /// The base client-fixture TOML (two mock models; web-1: pure parse
    /// — no tempdir, no store, no engine wiring; the config mirror is
    /// all these UI tests need).
    fn fixture_toml() -> String {
        fixture_toml_with(
            r#"[models.main]
provider = "mock"
model = "mock-model"

[models.fast]
provider = "mock"
model = "mock-model"
"#,
        )
    }

    /// The fixture with a custom `[models.*]` block (the provider and
    /// limits parts stay fixed).
    fn fixture_toml_with(models_toml: &str) -> String {
        format!(
            r#"
[providers.mock]
base_url = "http://localhost"
api_key_env = "TUI_TEST_KEY"

{models_toml}
[limits]
max_steps = 10
max_depth = 4
max_tool_calls = 20
max_context_bytes = 100_000
max_output_bytes = 10_000
"#
        )
    }

    /// Build a client App from a raw config string + a recording link.
    fn app_from_toml(toml: &str) -> (App, std::sync::Arc<MemLink>) {
        let config = openslate_core::config::parse_openslate_toml(toml).expect("fixture parses");
        let (link, events) = MemLink::pair();
        let app = App::new(ClientBootstrap {
            link: link.clone(),
            events,
            config,
            root_agent_id: "root".into(),
        });
        (app, link)
    }

    /// The inverse of [`crate::client::config_from_view`]: build a wire
    /// config view from a parsed fixture config (the snapshot's
    /// `ConfigViewDto` payload for test seeding).
    fn view_dto_from(config: &OpenSlateConfig) -> ConfigViewDto {
        use openslate_protocol::{AgentNodeDto, ConfigViewDto, LimitsDto, ModelDto, ProviderDto};
        let limits = config.limits.as_ref();
        ConfigViewDto {
            providers: config
                .providers
                .iter()
                .map(|(k, v)| (k.clone(), ProviderDto::from(v)))
                .collect(),
            models: config
                .models
                .iter()
                .map(|(k, v)| (k.clone(), ModelDto::from(v)))
                .collect(),
            levels: config.levels.clone().into_iter().collect(),
            limits: LimitsDto {
                max_steps: limits.map(|l| l.max_steps).unwrap_or(8),
                max_depth: limits.map(|l| l.max_depth).unwrap_or(4),
                max_tool_calls: limits.map(|l| l.max_tool_calls).unwrap_or(20),
                max_child_agent_calls: limits.map(|l| l.max_child_agent_calls).unwrap_or(8),
                timeout_ms: limits.map(|l| l.timeout_ms).unwrap_or(300000),
                max_context_messages: limits.map(|l| l.max_context_messages).unwrap_or(200),
                max_context_bytes: limits.map(|l| l.max_context_bytes).unwrap_or(512000),
                max_output_bytes: limits.map(|l| l.max_output_bytes).unwrap_or(65536),
                auto_compact: limits.map(|l| l.auto_compact).unwrap_or(true),
                parallel_tool_calls: limits.map(|l| l.parallel_tool_calls).unwrap_or(true),
            },
            agents: AgentNodeDto {
                id: "root".into(),
                name: "Root".into(),
                model: "main".into(),
                children: vec![],
            },
            skills: vec![],
            active_config: "/fixture/openslate.toml".into(),
            global_config: None,
            local_config: None,
        }
    }

    /// Seed a hello-ack snapshot into the app (production receives one
    /// through the link before the first draw; tests dispatch it).
    async fn seed_snapshot(app: &mut App, model_alias: &str) {
        let view = view_dto_from(&app.config);
        app.dispatch(Action::Engine(TuiEvent::Snapshot(Box::new(SnapshotDto {
            proto: 1,
            session_id: "test-session".into(),
            session_label: "tui test".into(),
            transcript: vec![],
            running: false,
            depth_cur: 0,
            agents_running: 0,
            tool_calls_cur: 0,
            model_alias: model_alias.into(),
            pending_approval: None,
            config: view,
            session_stats: Default::default(),
        }))))
        .await;
    }

    /// The default client App WITH a seeded snapshot (model alias
    /// `main` — the status bar's model segment renders from it).
    async fn test_app() -> App {
        test_app_linked().await.0
    }

    /// [`Self::test_app`] keeping the recording link (broadcast-driven
    /// assertions: approval races, turn submits, CRUD sends).
    async fn test_app_linked() -> (App, std::sync::Arc<MemLink>) {
        let (mut app, link) = app_from_toml(&fixture_toml());
        seed_snapshot(&mut app, "main").await;
        (app, link)
    }

    /// [`Self::test_app`] with TWELVE model aliases — `main`+`fast`
    /// (both REQUIRED by config validation) plus a01..=a10 — the >8
    /// overflow fixture for the `/model` argument completion
    /// (slash-2). Sorted, the list reads a01..a10, fast, main.
    async fn many_models_app() -> App {
        let mut models = r#"[models.main]
provider = "mock"
model = "mock-model"

[models.fast]
provider = "mock"
model = "mock-model"

"#
        .to_owned();
        for i in 1..=10 {
            models.push_str(&format!(
                "[models.a{i:02}]\nprovider = \"mock\"\nmodel = \"mock-model\"\n\n"
            ));
        }
        app_from_toml(&fixture_toml_with(&models)).0
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

    /// restyle-1: the persistent sidebar and its `┃` divider column
    /// are RETIRED — no column of the main region is a full-height
    /// divider, at any width.
    #[tokio::test]
    async fn single_column_layout_has_no_divider_column() {
        for (w, h) in [(100u16, 20u16), (80, 20), (120, 30)] {
            let mut app = test_app().await;
            let buf = draw(&mut app, w, h).await;
            for x in 0..w {
                let all_bar = (0..h - 5).all(|y| buf[(x, y)].symbol() == "┃");
                assert!(!all_bar, "no divider column at x={x} ({w}x{h})");
            }
        }
    }

    /// The bottom区 stack (top→bottom): hint row, `─` rule,
    /// input row (`›` prompt + placeholder), `─` rule, status line —
    /// the last five rows of the screen.
    #[tokio::test]
    async fn bottom_area_hint_rules_input_status_stack() {
        let mut app = test_app().await;
        let buf = draw(&mut app, 80, 20).await;
        let h = 20;
        // Hint row (y=14): `  Message · Enter send · ? help`.
        let hint: String = (0..40).map(|x| buf[(x, h - 5)].symbol()).collect();
        assert!(hint.contains("Message"), "hint row: {hint:?}");
        assert!(hint.contains("Enter send"), "hint row: {hint:?}");
        assert!(hint.contains("? help"), "hint row: {hint:?}");
        // Rule rows (y=h-4, y=h-2): full-width `─` in the line color.
        for y in [h - 4, h - 2] {
            for x in 0..80u16 {
                assert_eq!(buf[(x, y)].symbol(), "─", "rule row {y} at x={x}");
                assert_eq!(buf[(x, y)].style().fg, Some(Color::Rgb(0x66, 0x66, 0x66)));
            }
        }
        // Input row (y=h-3): `›` prompt (signal #67E8F9 + BOLD — the
        // input holds focus by default) + the muted placeholder.
        assert_eq!(buf[(0, h - 3)].symbol(), "›");
        assert_eq!(
            buf[(0, h - 3)].style().fg,
            Some(Color::Rgb(0x67, 0xE8, 0xF9))
        );
        assert!(buf[(0, h - 3)]
            .style()
            .add_modifier
            .contains(Modifier::BOLD));
        assert_eq!(buf[(2, h - 3)].symbol(), "输"); // placeholder text color muted
        assert_eq!(
            buf[(2, h - 3)].style().fg,
            Some(Color::Rgb(0xAD, 0xAD, 0xAD))
        );
        // Status line (y=19): plain spans — no REVERSED anywhere.
        for x in 0..80u16 {
            assert!(
                !buf[(x, h - 1)].modifier.contains(Modifier::REVERSED),
                "status line is plain at x={x}"
            );
        }
        let status: String = (0..50).map(|x| buf[(x, h - 1)].symbol()).collect();
        assert!(status.contains("✦ main"), "model segment: {status:?}");
    }

    /// The hint row shortens rung by rung on narrow windows
    /// (composer.ts:266): `? help` drops, then `Enter send`, leaving
    /// `Message`. Rendered through the painter directly — below
    /// MIN_COLS the min-size guard owns the whole screen, so the full
    /// App cannot exercise narrow hint rows.
    #[test]
    fn hint_row_shortens_on_narrow_windows() {
        let theme = Theme::new();
        let ctx = AppCtx {
            theme,
            focus: Focus::Input,
            run: RunInfo {
                state: RunState::Idle,
                spinner_frame: 0,
                model_label: "main@mock".into(),
                tokens_in: 0,
                tokens_out: 0,
                cost_usd: 0.0,
                elapsed: None,
                tool_calls_cur: 0,
                depth_cur: 0,
                context_remaining: None,
            },
            config: ConfigSummary {
                model_alias: "main".into(),
                model_id: "m".into(),
                provider_name: "mock".into(),
                max_depth: 4,
                max_tool_calls: 20,
                run_id: None,
                model_aliases: vec!["fast".into(), "main".into()],
            },
            size: (80, 24),
            notice: None,
        };
        let draw_hint = |w: u16| -> String {
            let mut terminal = Terminal::new(TestBackend::new(w, 1)).expect("terminal");
            terminal
                .draw(|f| {
                    App::render_hint_row(f, f.area(), &ctx, false);
                })
                .expect("draw");
            let buf = terminal.backend().buffer();
            (0..w).map(|x| buf[(x, 0)].symbol()).collect()
        };
        // 31 cols: the full run (2 + 7 + 3 + 10 + 3 + 6 = 31) fits.
        assert!(draw_hint(31).contains("? help"), "full hint at 31");
        // 30 cols: `? help` drops.
        let mid = draw_hint(30);
        assert!(mid.contains("Enter send"), "mid hint at 30: {mid:?}");
        assert!(!mid.contains("help"), "help dropped at 30: {mid:?}");
        // 9 cols: `Message` alone.
        let bare = draw_hint(9);
        assert!(bare.contains("Message"), "bare label at 9: {bare:?}");
        assert!(!bare.contains("Enter"), "send dropped at 9: {bare:?}");
    }

    /// A transient notice replaces the hint row (warning color) —
    /// e.g. the mouse-capture toggle's feedback.
    #[tokio::test]
    async fn notice_rides_the_hint_row_in_warning_color() {
        let mut app = test_app().await;
        app.set_notice("mouse capture off - text selectable");
        let buf = draw_front(&mut app, 80, 20).await;
        let y = 20 - 5;
        let text: String = (0..40)
            .map(|x| {
                buf.cell((x, y))
                    .map(|c| c.symbol().to_string())
                    .unwrap_or_default()
            })
            .collect();
        assert!(
            text.contains("mouse capture off"),
            "notice on the hint row: {text:?}"
        );
        assert_eq!(
            buf.cell((2, y)).expect("cell").fg,
            Color::Rgb(0xFF, 0xC3, 0x40)
        );
    }

    /// Ctrl+T opens the floating agents panel: rounded frame over a
    /// dimmed base UI, the agents tree and session panel inside, and
    /// the Sidebar focus stop. Ctrl+T again closes it.
    #[tokio::test]
    async fn ctrl_t_toggles_the_agents_panel_overlay() {
        let mut app = test_app().await;
        let _ = draw(&mut app, 80, 24).await;
        app.dispatch(Action::ToggleSidebar).await;
        assert_eq!(app.active_layer(), Layer::AgentsPanel);
        assert_eq!(app.focus, Focus::Sidebar);
        let buf = draw_front(&mut app, 80, 24).await;
        // The rounded panel frame + title.
        let screen: String = (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
            .map(|(x, y)| {
                buf.cell((x, y))
                    .map(|c| c.symbol().to_string())
                    .unwrap_or_default()
            })
            .collect();
        assert!(screen.contains('╭'), "panel header renders");
        assert!(screen.contains("Agents"), "panel title renders");
        // panel-meta-1: the close-key meta rides the header (the dim
        // hint row behind the overlay says 打开, never 关闭). Buffer
        // cells hidden under wide glyphs join as spaces — squeeze the
        // whitespace before matching the CJK meta text.
        let squeezed: String = screen.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            squeezed.contains("Ctrl+T关闭"),
            "close-key meta on the agents panel header"
        );
        assert!(screen.contains("root"), "agents tree renders");
        assert!(screen.contains("session"), "session panel renders");
        // Dimmed OUTSIDE the panel (the corridor assertion covers the
        // full grid in the dedicated tests below — spot-check here).
        let inner = App::agents_panel_rect(ratatui::layout::Rect::new(0, 0, 80, 24));
        let outside = (inner.x, 0u16); // top-left corner, outside the panel
        assert!(
            buf.cell(outside)
                .expect("cell")
                .modifier
                .contains(Modifier::DIM),
            "base UI dimmed outside the panel"
        );

        // Ctrl+T again closes it and restores the input focus.
        app.dispatch(Action::ToggleSidebar).await;
        assert_eq!(app.active_layer(), Layer::Normal);
        assert_eq!(app.focus, Focus::Input);
        let buf = draw_front(&mut app, 80, 24).await;
        let screen: String = (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
            .map(|(x, y)| {
                buf.cell((x, y))
                    .map(|c| c.symbol().to_string())
                    .unwrap_or_default()
            })
            .collect();
        assert!(!screen.contains('╭'), "panel gone");

        // `/status` opens it too.
        app.dispatch(Action::StartTurn("/status".into())).await;
        assert_eq!(app.active_layer(), Layer::AgentsPanel);
    }

    /// Tab skips the Sidebar focus stop while the panel is closed (no
    /// invisible panel may swallow keys) and stops on it while open.
    #[tokio::test]
    async fn tab_focus_cycle_gates_on_the_panel() {
        let mut app = test_app().await;
        let _ = draw(&mut app, 80, 20).await;
        app.dispatch(Action::FocusNext).await; // Input → Transcript
        app.dispatch(Action::FocusNext).await; // would be Sidebar — skipped
        assert_eq!(app.focus, Focus::Input, "closed panel is skipped");
        app.dispatch(Action::ToggleSidebar).await;
        app.dispatch(Action::FocusNext).await; // Input → Transcript
        app.dispatch(Action::FocusNext).await; // Transcript → Sidebar (visible)
        assert_eq!(app.focus, Focus::Sidebar, "open panel is focusable");
    }

    // ── slash-1: completion dispatch & layout ───────────────────────────────────

    /// Type a string into the input through the REAL dispatcher.
    async fn type_into(app: &mut App, s: &str) {
        for c in s.chars() {
            app.dispatch(Action::InputChar(c)).await;
        }
    }

    /// The completion list floats as an OVERLAY over the transcript's
    /// bottom rows (slash-3): `/` opens all 8 commands in a rect
    /// growing up from the hint row, the layout keeps its closed-frame
    /// geometry (input/rule/status rows unchanged), the selected head
    /// row is the signal+BOLD `→ /help`, and the covered cells carry
    /// the surface background.
    #[tokio::test]
    async fn completion_overlay_floats_over_the_transcript_bottom() {
        let mut app = test_app().await;
        let _ = draw(&mut app, 80, 20).await;
        type_into(&mut app, "/").await;
        assert!(app.input_completion_open());
        let buf = draw_front(&mut app, 80, 20).await;
        // Full-height layout (identical to a closed frame): main 0..14,
        // hint 15, rule_a 16, input 17, rule_b 18, status 19 — the
        // overlay then covers main's bottom 8 rows (y 7..=14).
        let hint: String = (0..20u16).map(|x| buf[(x, 15)].symbol()).collect();
        assert!(hint.contains("Message"), "hint row at y=15: {hint:?}");
        assert_eq!(buf[(0, 17)].symbol(), "›", "input row at y=17");
        for x in 0..80u16 {
            assert_eq!(buf[(x, 16)].symbol(), "─", "rule_a at y=16 x={x}");
            assert_eq!(buf[(x, 18)].symbol(), "─", "rule_b at y=18 x={x}");
        }
        let status: String = (0..30u16).map(|x| buf[(x, 19)].symbol()).collect();
        assert!(status.contains("main"), "status line last: {status:?}");
        // Head row (overlay top, y=7): marker + /help — signal + BOLD.
        assert_eq!(buf[(2, 7)].symbol(), "→");
        assert_eq!(buf[(3, 7)].symbol(), " ");
        let head: String = (4..9u16).map(|x| buf[(x, 7)].symbol()).collect();
        assert_eq!(head, "/help");
        assert_eq!(buf[(2, 7)].style().fg, Some(Color::Rgb(0x67, 0xE8, 0xF9)));
        assert!(buf[(2, 7)].modifier.contains(Modifier::BOLD));
        // 9 commands since model-mgmt-2: the 8-row overlay shows the
        // first 7 items then spends its last row (y=14) on the
        // dim `(1/9)` indicator.
        let tail_item: String = (4..9u16).map(|x| buf[(x, 13)].symbol()).collect();
        assert_eq!(tail_item, "/copy", "7th item on the second-to-last row");
        let indicator: String = (2..8u16).map(|x| buf[(x, 14)].symbol()).collect();
        assert_eq!(indicator.trim(), "(1/9)", "overflow indicator at y=14");
        // Clear + surface bg: EVERY cell of the covered rows carries
        // the user-band surface background (dark #262626).
        for y in 7..=14u16 {
            for x in 0..80u16 {
                assert_eq!(
                    buf[(x, y)].style().bg,
                    Some(Color::Rgb(0x26, 0x26, 0x26)),
                    "surface bg at ({x},{y})"
                );
            }
        }
    }

    /// The overlay's essence: opening the list does NOT reflow the
    /// layout — rows above the overlay are byte-identical between the
    /// closed and open frames, and the covered transcript rows hold
    /// list content (no text residue under the Clear+bg overlay).
    #[tokio::test]
    async fn completion_overlay_does_not_reflow_the_layout() {
        let mut app = test_app().await;
        for i in 0..40 {
            app.dispatch(Action::Engine(TuiEvent::Delta(format!("line{i}\n"))))
                .await;
        }
        let _ = draw(&mut app, 80, 20).await;
        let closed = draw_front(&mut app, 80, 20).await;
        // Sanity: transcript content reaches into the rows the overlay
        // will cover (the tail is followed).
        let row_text = |buf: &ratatui::buffer::Buffer, y: u16| -> String {
            (0..80u16).map(|x| buf[(x, y)].symbol()).collect()
        };
        let has_line = |buf: &ratatui::buffer::Buffer, y: u16| row_text(buf, y).contains("line");
        assert!(has_line(&closed, 14), "content at the bottom row");

        type_into(&mut app, "/").await;
        let open = draw_front(&mut app, 80, 20).await;
        // Rows ABOVE the overlay (y 0..=6): identical content at
        // identical positions (full-height layout unchanged).
        for y in 0..=6u16 {
            for x in 0..80u16 {
                assert_eq!(
                    closed[(x, y)].symbol(),
                    open[(x, y)].symbol(),
                    "no reflow at ({x},{y})"
                );
            }
        }
        // The bottom area rows are identical too (hint/rules/status
        // all kept their places; the input row itself is EXCLUDED —
        // it legitimately now shows the typed `/`).
        for y in [15u16, 16, 18, 19] {
            for x in 0..80u16 {
                assert_eq!(
                    closed[(x, y)].symbol(),
                    open[(x, y)].symbol(),
                    "bottom rows stable at ({x},{y})"
                );
            }
        }
        // Covered rows: the list replaced the transcript text — no
        // `line` residue leaks through the Clear+bg overlay.
        for y in 7..=14u16 {
            assert!(!has_line(&open, y), "transcript text wiped at y={y}");
        }
        let head: String = (4..9u16).map(|x| open[(x, 7)].symbol()).collect();
        assert_eq!(head, "/help", "the list paints over the covered rows");
    }

    /// Small terminals clamp the overlay against the available height
    /// (rows above the hint row): at 60×12 the 8 commands get 7 rows →
    /// 6 items + the `(1/8)` indicator, and the hint/input rows stay
    /// uncovered.
    #[tokio::test]
    async fn completion_clamps_on_small_terminals() {
        let mut app = test_app().await;
        let _ = draw(&mut app, 60, 12).await;
        type_into(&mut app, "/").await;
        // 12 rows: main y 0..=6 (7 rows) — avail=7 clamps the 8-row
        // request → 6 items + `(1/9)` at y 0..=6 (9 commands since
        // model-mgmt-2); hint 7, rule_a 8, input 9, rule_b 10, status
        // 11 all stay uncovered.
        let buf = draw_front(&mut app, 60, 12).await;
        let first: String = (4..9u16).map(|x| buf[(x, 0)].symbol()).collect();
        assert_eq!(first, "/help");
        let second: String = (4..9u16).map(|x| buf[(x, 1)].symbol()).collect();
        assert_eq!(second, "/exit");
        let indicator: String = (2..8u16).map(|x| buf[(x, 6)].symbol()).collect();
        assert_eq!(indicator.trim(), "(1/9)", "dim indicator: {indicator:?}");
        assert_eq!(buf[(2, 6)].style().fg, Some(Color::Rgb(0x66, 0x66, 0x66)));
        // The hint row was NOT covered (still the default/reset
        // background — no surface bg leaked past the overlay).
        for x in 0..60u16 {
            assert_ne!(
                buf[(x, 7)].style().bg,
                Some(Color::Rgb(0x26, 0x26, 0x26)),
                "hint row untouched by the overlay at x={x}"
            );
        }
        let hint: String = (0..20u16).map(|x| buf[(x, 7)].symbol()).collect();
        assert!(hint.contains("Message"), "hint row at y=7: {hint:?}");
        assert_eq!(buf[(0, 9)].symbol(), "›", "input row at y=9");
        assert_eq!(buf[(0, 10)].symbol(), "─", "rule_b at y=10");
        let status: String = (0..30u16).map(|x| buf[(x, 11)].symbol()).collect();
        assert!(status.contains("✦"), "status at y=11: {status:?}");
    }

    /// Roomy terminal, MORE items than max_visible: 12 model aliases
    /// (sorted a01..a10, fast, main) overflow the 8-row overlay — 7
    /// items + the `(i/n)` indicator render, and ↑/↓ move the
    /// selection with the window re-centering at the head/middle/tail
    /// (the slash-2 overflow coverage, migrated to overlay coords).
    #[tokio::test]
    async fn completion_args_overflow_scrolls_on_roomy_terminals() {
        let mut app = many_models_app().await;
        let _ = draw(&mut app, 80, 20).await;
        type_into(&mut app, "/model ").await;
        assert!(app.input_completion_open(), "the args list opened");
        // 12 aliases, overlay = min(8, avail=15) = 8 rows over
        // y 7..=14 → 7 items + (1/12).
        let buf = draw_front(&mut app, 80, 20).await;
        let visible: String = (4..9u16).map(|x| buf[(x, 7)].symbol()).collect();
        assert_eq!(visible.trim(), "a01", "first item on the head row");
        let seventh: String = (4..9u16).map(|x| buf[(x, 13)].symbol()).collect();
        assert_eq!(seventh.trim(), "a07", "7 items visible (a01..=a07)");
        let indicator: String = (2..9u16).map(|x| buf[(x, 14)].symbol()).collect();
        assert_eq!(indicator.trim(), "(1/12)", "indicator: {indicator:?}");
        // a08 never renders (below the window).
        for y in 7..=13u16 {
            let row: String = (4..9u16).map(|x| buf[(x, y)].symbol()).collect();
            assert_ne!(row.trim(), "a08", "a08 hidden at y={y}");
        }

        // ↓×6 → selected 6 (a07): window start = 6−3 = 3 → a04..=a10,
        // selection centered on display row 3 (y=10), indicator (7/12).
        for _ in 0..6 {
            app.dispatch(Action::InputHistoryNext).await;
        }
        let buf = draw_front(&mut app, 80, 20).await;
        let head: String = (4..9u16).map(|x| buf[(x, 7)].symbol()).collect();
        assert_eq!(head.trim(), "a04", "window start = selected − shown/2");
        assert_eq!(buf[(2, 10)].symbol(), "→", "selection centered at y=10");
        let sel: String = (4..9u16).map(|x| buf[(x, 10)].symbol()).collect();
        assert_eq!(sel.trim(), "a07");
        let indicator: String = (2..9u16).map(|x| buf[(x, 14)].symbol()).collect();
        assert_eq!(indicator.trim(), "(7/12)");

        // ↑ wraps backward to the head; ↑ again wraps to the TAIL
        // (11 = `main`): window pins to 12−7 = 5 → a06..a10, fast,
        // main — selection on the last item row, indicator (12/12).
        for _ in 0..7 {
            app.dispatch(Action::InputHistoryPrev).await; // 6→…→0→wrap→11
        }
        let buf = draw_front(&mut app, 80, 20).await;
        let head: String = (4..9u16).map(|x| buf[(x, 7)].symbol()).collect();
        assert_eq!(head.trim(), "a06", "window pinned at the tail");
        let sel: String = (4..9u16).map(|x| buf[(x, 13)].symbol()).collect();
        assert_eq!(sel.trim(), "main", "selection on the last visible item");
        assert_eq!(buf[(2, 13)].symbol(), "→");
        let indicator: String = (2..9u16).map(|x| buf[(x, 14)].symbol()).collect();
        assert_eq!(indicator.trim(), "(12/12)");
    }

    /// In-overlay mouse gestures are swallowed (slash-3): a click on
    /// the (covered) `+N ↓ 回到底部` hint does NOT jump to the bottom,
    /// and press/drag/release plus a wheel notch inside the rect are
    /// all no-ops.
    #[tokio::test]
    async fn in_overlay_mouse_gestures_are_swallowed() {
        let (mut app, _link, (hx, hy)) = pinned_app_with_hint().await;
        assert!(app.transcript_pinned());
        // The pinned hint renders at the main region's bottom — the
        // overlay covers it once the list opens.
        type_into(&mut app, "/").await;
        assert!(app.input_completion_open());
        let _ = draw(&mut app, 80, 20).await; // records the hit rect
                                              // The recorded hint cell now sits inside the overlay rows
                                              // (the overlay covers main's bottom 8 rows at 80×20).
        app.dispatch(Action::Click(hx, hy)).await;
        assert!(
            app.transcript_pinned(),
            "the in-overlay click never reaches the jump-to-bottom hint"
        );
        // Press/drag/release inside the rect: no-ops (no crash, pin
        // and list unchanged).
        app.dispatch(Action::MouseDown(10, 10)).await;
        app.dispatch(Action::MouseDrag(20, 11)).await;
        app.dispatch(Action::MouseUp(20, 11)).await;
        assert!(app.transcript_pinned());
        assert!(app.input_completion_open());
        // An in-overlay wheel notch is swallowed too: a fresh app —
        // a notch over the overlay does NOT pin the transcript, while
        // a notch ABOVE the rect still scrolls.
        let mut app = test_app().await;
        for i in 0..40 {
            app.dispatch(Action::Engine(TuiEvent::Delta(format!("line{i}\n"))))
                .await;
        }
        let _ = draw(&mut app, 80, 20).await;
        type_into(&mut app, "/").await;
        let _ = draw(&mut app, 80, 20).await;
        app.dispatch(Action::WheelScrollUp(40, 10)).await; // in-rect
        assert!(!app.transcript_pinned(), "in-overlay wheel swallowed");
        app.dispatch(Action::WheelScrollUp(40, 2)).await; // above the rect
        assert!(app.transcript_pinned(), "outside wheel keeps scrolling");
    }

    /// Outside the overlay rect the mouse semantics are untouched
    /// (slash-3): wheels above the overlay still scroll the chat while
    /// the list is open, and a click on the status row stays inert.
    #[tokio::test]
    async fn outside_overlay_gestures_keep_their_semantics() {
        let mut app = test_app().await;
        for i in 0..40 {
            app.dispatch(Action::Engine(TuiEvent::Delta(format!("line{i}\n"))))
                .await;
        }
        let _ = draw(&mut app, 80, 20).await;
        type_into(&mut app, "/").await;
        assert!(app.input_completion_open());
        let _ = draw(&mut app, 80, 20).await;
        // Wheel above the overlay (y=2 < rect top 7): scrolls+pins.
        for _ in 0..10 {
            app.dispatch(Action::WheelScrollUp(40, 2)).await;
        }
        assert!(app.transcript_pinned(), "outside wheel scrolls the chat");
        // Click on the status row (far below the rect): inert, list
        // unaffected.
        app.dispatch(Action::Click(40, 19)).await;
        assert!(app.input_completion_open());
        assert!(app.transcript_pinned());
    }

    /// Dispatch level: `/mo` + Tab completes to `/model ` and reopens
    /// the argument list (dynamic aliases from the config); Tab never
    /// cycles focus while the list is open.
    #[tokio::test]
    async fn mo_tab_completes_to_model_and_reopens_args() {
        let mut app = test_app().await;
        let _ = draw(&mut app, 80, 20).await;
        type_into(&mut app, "/mo").await;
        assert!(app.input_completion_open());
        app.dispatch(Action::FocusNext).await; // Tab → apply
        assert_eq!(app.input_text(), "/model ");
        assert!(app.input_completion_open(), "the args list reopens");
        assert_eq!(app.focus, Focus::Input, "Tab applied instead of cycling");
        // The argument list carries the config's model aliases
        // (sorted) — typing filters it.
        type_into(&mut app, "fa").await;
        assert_eq!(app.input_text(), "/model fa");
        // Esc closes the list but keeps the text.
        app.dispatch(Action::DismissOverlay).await;
        assert!(!app.input_completion_open());
        assert_eq!(app.input_text(), "/model fa");
    }

    /// Dispatch level: Enter runs the completed command through the
    /// REAL slash routing — `/exit` quits without a confirmation.
    #[tokio::test]
    async fn exit_enter_quits_through_handle_slash() {
        let mut app = test_app().await;
        let _ = draw(&mut app, 80, 20).await;
        type_into(&mut app, "/exit").await;
        assert!(app.input_completion_open());
        assert_eq!(
            app.dispatch(Action::SubmitInput).await,
            DispatchOutcome::Quit
        );
    }

    /// An overlay opening closes the list (slash-1: no modal opens on
    /// top of a live completion) — Ctrl+T here, and it stays closed
    /// after the overlay dismisses.
    #[tokio::test]
    async fn overlay_opening_closes_the_completion() {
        let mut app = test_app().await;
        let _ = draw(&mut app, 80, 20).await;
        type_into(&mut app, "/mo").await;
        assert!(app.input_completion_open());
        app.dispatch(Action::ToggleSidebar).await;
        assert_eq!(app.active_layer(), Layer::AgentsPanel);
        assert!(
            !app.input_completion_open(),
            "the agents overlay closed the list"
        );
        // Dismiss the overlay; the list does not resurrect.
        app.dispatch(Action::DismissOverlay).await;
        assert_eq!(app.active_layer(), Layer::Normal);
        assert!(!app.input_completion_open());
        assert_eq!(app.input_text(), "/mo", "the draft survives");
    }

    /// The transcript still renders inside the 2-column-padded main
    /// column: the leftmost 2 columns of the main region are blank on
    /// the first content row.
    #[tokio::test]
    async fn main_column_content_is_padded_two_columns() {
        let mut app = test_app().await;
        // Seed real content — the empty session paints the centered
        // splash (splash-1), whose art starts far right of col 2; the
        // padding contract targets the document flow's first row.
        app.transcript.push_user("hello");
        let buf = draw(&mut app, 100, 20).await;
        let content_row = (0..15u16).find(|&y| (0..100u16).any(|x| buf[(x, y)].symbol() != " "));
        if let Some(y) = content_row {
            assert_eq!(buf[(0, y)].symbol(), " ", "padding col 0");
            assert_eq!(buf[(1, y)].symbol(), " ", "padding col 1");
            // The user block's own `  › ` indent stacks on the app
            // padding — the first glyph lands at x=4; nothing may
            // leak into the padded columns.
            let first_x = (0..100u16)
                .find(|&x| buf[(x, y)].symbol() != " ")
                .expect("a content glyph");
            assert!(first_x >= 2, "content respects the padding (x={first_x})");
        }
    }

    // ── overlay occlusion dimming (layout rework) ───────────────────────────────────────────────────

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

    /// The agents panel dims the base UI outside its frame.
    #[tokio::test]
    async fn agents_panel_dims_background_outside_the_overlay() {
        let mut app = test_app().await;
        app.modal = Modal::AgentsPanel;
        let inner = App::agents_panel_rect(ratatui::layout::Rect::new(0, 0, 80, 24));
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
        // 100x24 layout: main = 19 rows (y 0..18); the banner covers
        // its bottom 5 rows (y 14..=18).
        for y in 0..24u16 {
            for x in 0..100u16 {
                if (14..=18).contains(&y) {
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

    // ── fix-17: the pinned `+N ↓ 回到底部` hint is clickable ───────────────────────────────────────

    /// Seed streaming content, pin the transcript well above the tail
    /// (wheel notches — the input keeps focus throughout), draw once at
    /// 80x20 so the hint renders and its hit rectangle records, then
    /// return the terminal cell of the hint's `回` glyph (inside the
    /// hit rectangle by construction).
    async fn pinned_app_with_hint() -> (App, std::sync::Arc<MemLink>, (u16, u16)) {
        let (mut app, link) = test_app_linked().await;
        for i in 0..40 {
            app.dispatch(Action::Engine(TuiEvent::Delta(format!("line{i}\n"))))
                .await;
        }
        for _ in 0..10 {
            app.dispatch(Action::WheelScrollUp(40, 2)).await;
        }
        assert!(app.transcript_pinned());
        let buf = draw(&mut app, 80, 20).await;
        // The hint is the only `回` on a non-overlay screen (the help
        // overlay needs `?` + empty input; the status line carries no
        // CJK hints anymore).
        let hit = (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
            .find(|&(x, y)| buf.cell((x, y)).is_some_and(|c| c.symbol() == "回"))
            .expect("the hint renders bottom-right");
        (app, link, hit)
    }

    /// Dispatch level: a click on the hint jumps back to the bottom
    /// (re-follow) even while the INPUT box holds focus — the click is
    /// positional, like the wheel.
    #[tokio::test]
    async fn click_on_new_content_hint_jumps_to_bottom() {
        let (mut app, _link, (hx, hy)) = pinned_app_with_hint().await;
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
        let (mut app, link, (hx, hy)) = pinned_app_with_hint().await;
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

        // `y` answers the approval (not typed into the editor). web-1:
        // the answer LEAVES for the server; the banner stays until the
        // `approval_resolved` broadcast lands (any client's answer).
        app.dispatch(Action::InputChar('y')).await;
        assert_eq!(app.input_text(), "");
        assert_eq!(
            link.take_sent(),
            vec![ClientMsg::ApprovalAnswer {
                id: 1,
                choice: openslate_protocol::ApprovalAnswerChoice::Approve,
            }],
            "the answer left for the server"
        );
        assert_eq!(
            app.active_layer(),
            Layer::ApprovalActive,
            "the banner waits for the broadcast"
        );
        link.emit(openslate_protocol::ServerMsg::ApprovalResolved {
            id: 1,
            choice: "approve".into(),
        });
        app.drain_engine_events().await;
        assert_ne!(
            app.active_layer(),
            Layer::ApprovalActive,
            "the broadcast ends preemption"
        );
        app.dispatch(Action::InputChar('x')).await;
        assert_eq!(app.input_text(), "x");
    }

    // ── interactive-1: hover & button clicks ───────────────────────────

    /// The dark board's hover style (signal + UNDERLINED + BOLD).
    fn hover_style() -> ratatui::style::Style {
        ratatui::style::Style::new()
            .fg(Color::Rgb(0x67, 0xE8, 0xF9))
            .add_modifier(Modifier::UNDERLINED)
            .add_modifier(Modifier::BOLD)
    }

    /// The recorded `? help` button rect after one draw (the painter
    /// and the hit-test share it — read it back rather than
    /// hard-coding columns).
    async fn help_rect(app: &mut App) -> Rect {
        draw(app, 100, 30).await;
        app.help_hit_rect.expect("? help segment rendered")
    }

    /// T1: the `? help` segment hovers, its cells carry the hover
    /// style (and ONLY them), and a click opens the help overlay —
    /// the same open path as the `?` key. A miss clears the hover.
    #[tokio::test]
    async fn hint_help_button_hover_style_click_and_miss() {
        let mut app = test_app().await;
        let rect = help_rect(&mut app).await;
        assert_eq!(rect.y, 25, "hint row at 100x30");
        assert_eq!(rect.x, 25, "`?` after `  Message · Enter send · `");
        assert_eq!(rect.width, 6, "`?` + ` help`");

        // Hover: the state targets the segment and the next frame
        // paints the hover style on exactly its columns.
        app.dispatch(Action::MouseMove(rect.x + 1, rect.y)).await;
        assert_eq!(app.hover(), Some(HoverTarget::HelpHint));
        let buf = draw_front(&mut app, 100, 30).await;
        for x in rect.x..rect.x + rect.width {
            assert_eq!(
                buf[(x, rect.y)].style().fg,
                hover_style().fg,
                "hover at {x}"
            );
            assert!(
                buf[(x, rect.y)]
                    .style()
                    .add_modifier
                    .contains(Modifier::UNDERLINED),
                "underline at {x}"
            );
        }
        // The separator column just before the button stays plain.
        assert!(!buf[(rect.x - 1, rect.y)]
            .style()
            .add_modifier
            .contains(Modifier::UNDERLINED));

        // Click (press + release inside the segment): help opens —
        // exactly the ToggleHelp path.
        app.dispatch(Action::MouseDown(rect.x + 2, rect.y)).await;
        app.dispatch(Action::MouseUp(rect.x + 2, rect.y)).await;
        assert!(
            matches!(app.modal, Modal::HelpOpen),
            "the button click opens help"
        );
        // The help layer exposes no buttons: the hover cleared with
        // the layer change on the next move.
        app.dispatch(Action::MouseMove(rect.x, rect.y)).await;
        assert_eq!(app.hover(), None, "no hover under the help overlay");

        // Miss: moving off clears (back on Normal after closing).
        app.dispatch(Action::DismissOverlay).await;
        app.dispatch(Action::MouseMove(rect.x, rect.y)).await;
        assert_eq!(app.hover(), Some(HoverTarget::HelpHint));
        app.dispatch(Action::MouseMove(1, 1)).await;
        assert_eq!(app.hover(), None, "a miss clears the hover");
    }

    /// T1 degenerate rows: a notice replaces the row and the narrow
    /// rung ladder drops the segment — neither is a button.
    #[tokio::test]
    async fn hint_help_button_absent_on_notice_and_narrow_rungs() {
        // Notice row: no help rect is recorded, so no hover.
        let mut app = test_app().await;
        draw(&mut app, 100, 30).await;
        app.set_notice("mouse capture off - text selectable");
        draw(&mut app, 100, 30).await;
        assert!(app.help_hit_rect.is_none(), "notice row has no button");
        app.dispatch(Action::MouseMove(25, 25)).await;
        assert_eq!(app.hover(), None);

        // Narrow widths: run(true) needs 31 cols — at the layout
        // minimum (60) the button still renders; the rung only drops
        // `? help` below 31 cols, which the App-level min-size guard
        // already owns. Exercise the degenerate rung at the PAINTER
        // level (same seam as hint_row_shortens_on_narrow_windows;
        // notice cleared first — it replaces the whole row):
        // the button rect is None wherever the segment is not painted.
        app.status_notice = None;
        app.notice_ticks = 0;
        let mut narrow_ctx = app.make_ctx();
        narrow_ctx.theme = Theme::new();
        let rect_at = |ctx: &AppCtx, w: u16| -> Option<Rect> {
            let mut terminal = Terminal::new(TestBackend::new(w, 1)).expect("terminal");
            let mut captured = None;
            terminal
                .draw(|f| captured = App::render_hint_row(f, f.area(), ctx, true))
                .expect("draw");
            captured
        };
        assert_eq!(rect_at(&narrow_ctx, 30), None, "30 cols drop ? help");
        assert!(rect_at(&narrow_ctx, 31).is_some(), "31 cols show it");
        assert!(
            rect_at(&narrow_ctx, 60).is_some(),
            "the layout minimum keeps it"
        );
    }

    /// T2: the banner's [n] button hovers (its cells styled), a click
    /// answers the approval through the same ApprovalRespond path as
    /// the key, and the answer clears the hover.
    #[tokio::test]
    async fn approval_buttons_hover_click_and_clear() {
        let (mut app, link) = test_app_linked().await;
        draw(&mut app, 100, 30).await; // records the help rect too
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
        assert_eq!(app.active_layer(), Layer::ApprovalActive);

        // The recorded button rects: 100x30 → banner y=20, hint line
        // y=22, body x=2 → [y] 2..10, [n] 13..21, [a] 24..34.
        draw(&mut app, 100, 30).await;
        let rects = app.approval_button_rects.clone();
        assert_eq!(rects.len(), 3);
        let (deny_choice, deny_rect) = rects[1];
        assert_eq!(deny_choice, ApprovalChoice::Deny);
        assert_eq!((deny_rect.x, deny_rect.y), (13, 22));

        // Hover the [n] segment: state + painted style. (Wide-glyph
        // TAIL cells are `Cell::EMPTY` — symbol " ", `fg == Reset` —
        // assert only the styled cells.)
        let mid = deny_rect.x + deny_rect.width / 2;
        app.dispatch(Action::MouseMove(mid, deny_rect.y)).await;
        assert_eq!(app.hover(), Some(HoverTarget::ApproveButton(deny_choice)));
        let buf = draw_front(&mut app, 100, 30).await;
        for x in deny_rect.x..deny_rect.x + deny_rect.width {
            let cell = buf
                .cell((x, deny_rect.y))
                .expect("cell inside the banner width");
            if cell.fg == Color::Reset {
                continue; // CJK tail cell (reset by the buffer writer)
            }
            assert_eq!(cell.style().fg, hover_style().fg, "col {x}");
            assert!(
                cell.style().add_modifier.contains(Modifier::UNDERLINED),
                "underline at {x}"
            );
        }
        // The [y] button before it keeps the BOLD warning style.
        let (approve_choice, y_rect) = rects[0];
        assert_eq!(
            buf[(y_rect.x, y_rect.y)].style().fg,
            Some(Color::Rgb(0xFF, 0xC3, 0x40))
        );

        // Priority: under the approval modal NO other button hovers —
        // not even the still-recorded `? help` segment.
        app.dispatch(Action::MouseMove(25, 25)).await;
        assert_eq!(
            app.hover(),
            None,
            "approval modal outranks every other button"
        );

        // Click the [y] button: the approval answer leaves through the
        // same ApprovalRespond path as the key (web-1: the banner
        // clears on the broadcast; the hover clears immediately) and
        // the run state leaves ApprovalPending with the broadcast.
        let mid_y = y_rect.x + y_rect.width / 2;
        app.dispatch(Action::MouseMove(mid_y, y_rect.y)).await;
        assert_eq!(
            app.hover(),
            Some(HoverTarget::ApproveButton(approve_choice))
        );
        app.dispatch(Action::MouseDown(mid_y, y_rect.y)).await;
        app.dispatch(Action::MouseUp(mid_y, y_rect.y)).await;
        assert_eq!(
            link.take_sent(),
            vec![ClientMsg::ApprovalAnswer {
                id: 1,
                choice: openslate_protocol::ApprovalAnswerChoice::Approve,
            }],
            "the click sent the answer"
        );
        assert_eq!(app.hover(), None, "the answer clears the hover");
        link.emit(openslate_protocol::ServerMsg::ApprovalResolved {
            id: 1,
            choice: "approve".into(),
        });
        app.drain_engine_events().await;
        assert!(!app.approval.has_pending(), "the broadcast answered it");
        assert_ne!(*app.run_state(), RunState::ApprovalPending);
    }

    /// T3: a visible completion row hovers WITHOUT moving the keyboard
    /// selection or the scroll window, and a click runs the row's
    /// Enter semantics (`/exit` quits through handle_slash).
    #[tokio::test]
    async fn completion_rows_hover_without_selection_and_click_submits() {
        let mut app = test_app().await;
        type_into(&mut app, "/").await;
        assert!(app.input_completion_open());
        draw(&mut app, 100, 30).await;
        let rect = app.completion_hit_rect.expect("overlay rendered");
        assert_eq!(rect.y, 17, "8 rows growing up from the hint row 25");
        assert_eq!(rect.height, 8);

        // Find the screen row displaying `/exit` (label column starts
        // at rect.x + PROMPT_COLS + 2 = x 4).
        let buf = draw_front(&mut app, 100, 30).await;
        let exit_d = (0..rect.height)
            .find(|&d| {
                let y = rect.y + d;
                let row: String = (rect.x + 4..rect.x + 12)
                    .map(|x| {
                        buf.cell((x, y))
                            .map(|c| c.symbol().to_string())
                            .unwrap_or_default()
                    })
                    .collect();
                row.contains("exit")
            })
            .expect("/exit row visible");
        let y = rect.y + exit_d;

        // Hover: the target is the ROW INDEX (not a position)…
        app.dispatch(Action::MouseMove(10, y)).await;
        let index = app.input.completion_row_at(rect, y).expect("row maps back");
        assert_eq!(app.hover(), Some(HoverTarget::CompletionRow(index)));

        // …and it does NOT move the keyboard selection: the selected
        // row (0, `→ /help`) keeps its marker on the next frame.
        let buf = draw_front(&mut app, 100, 30).await;
        assert_eq!(
            buf[(2, rect.y)].symbol(),
            "→",
            "keyboard selection untouched"
        );

        // Click the row: Enter's branch (a) — apply + submit through
        // handle_slash → /exit quits.
        app.dispatch(Action::MouseDown(10, y)).await;
        let outcome = app.dispatch(Action::MouseUp(10, y)).await;
        assert_eq!(outcome, DispatchOutcome::Quit, "clicking /exit quits");
    }

    /// T3: presses inside the overlay but NOT on a row (the overflow
    /// indicator) stay inert — swallowed, no hover, no click.
    #[tokio::test]
    async fn completion_indicator_row_is_dead_space() {
        let mut app = test_app().await;
        // 12 model aliases overflow the 8-row cap → the last overlay
        // row is the (i/n) indicator.
        app.config.models = (1..=12)
            .map(|i| {
                (
                    format!("m{i:02}"),
                    openslate_core::config::ModelConfig {
                        provider: "mock".into(),
                        model: "mock-model".into(),
                        max_context_tokens: None,
                        max_output_tokens: None,
                        supports_tool_call: true,
                        supports_vision: false,
                        supports_reasoning: false,
                        input_price_per_mtok: None,
                        output_price_per_mtok: None,
                    },
                )
            })
            .collect();
        type_into(&mut app, "/model ").await;
        draw(&mut app, 100, 30).await;
        let rect = app.completion_hit_rect.expect("overlay rendered");
        let indicator_y = rect.y + rect.height - 1;
        app.dispatch(Action::MouseMove(10, indicator_y)).await;
        assert_eq!(app.hover(), None, "the indicator row is not a button");
        app.dispatch(Action::MouseDown(10, indicator_y)).await;
        app.dispatch(Action::MouseUp(10, indicator_y)).await;
        assert!(app.input_completion_open(), "inert press-release");
        assert_eq!(app.input_text(), "/model ");
    }

    /// adapter-combo-1: while the models panel owns the screen, a
    /// MouseMove reaches the component (the generic hover arm runs
    /// BEFORE the panel's modal guard and knows no ModelsPanel
    /// targets) — the provider form's Adapter combo tracks its row
    /// hover, and a full click applies WITHOUT submitting.
    #[tokio::test]
    async fn models_panel_combo_dropdown_hover_and_click() {
        let mut app = test_app().await;
        let _ = draw(&mut app, 100, 30).await;
        type_into(&mut app, "/provider").await;
        let _ = app.dispatch(Action::SubmitInput).await;
        assert!(matches!(app.active_layer(), Layer::ModelsPanel));
        // Provider ADD form, Tab to the Adapter combo.
        let _ = app.dispatch(Action::InputChar('a')).await;
        for _ in 0..4 {
            let _ = app.dispatch(Action::FocusNext).await;
        }
        let _ = draw(&mut app, 100, 30).await; // records the dropdown rows
        let rects = app.models.combo_row_rects();
        assert_eq!(rects.len(), 4, "all four protocol candidates render");

        // The forwarded move lights the hovered row.
        let (x, y) = (rects[1].x + 2, rects[1].y);
        let _ = app.dispatch(Action::MouseMove(x, y)).await;
        assert_eq!(app.models.combo_hover(), Some(1));
        // A miss clears it (still swallowed by the panel).
        let _ = app.dispatch(Action::MouseMove(1, 1)).await;
        assert_eq!(app.models.combo_hover(), None);

        // Press + release on the same row applies — no submit.
        let _ = app.dispatch(Action::MouseMove(x, y)).await;
        let _ = app.dispatch(Action::MouseDown(x, y)).await;
        let _ = app.dispatch(Action::MouseUp(x, y)).await;
        assert_eq!(app.models.form_field_value(4).as_deref(), Some("anthropic"));
        assert!(matches!(app.active_layer(), Layer::ModelsPanel));
    }

    /// T4: the pinned jump hint hovers (hover style painted) and its
    /// click still jumps to the bottom — the fix-17 behavior via the
    /// exact same Click action the legacy release fires.
    #[tokio::test]
    async fn jump_hint_hover_style_and_click_behavior() {
        let (mut app, _link, (hx, hy)) = pinned_app_with_hint().await;
        app.dispatch(Action::MouseMove(hx, hy)).await;
        assert_eq!(app.hover(), Some(HoverTarget::JumpBottom));
        let buf = draw_front(&mut app, 80, 20).await;
        assert_eq!(buf[(hx, hy)].style().fg, hover_style().fg);
        assert!(buf[(hx, hy)]
            .style()
            .add_modifier
            .contains(Modifier::UNDERLINED));

        // Click: press + release on the hint → back to following.
        app.dispatch(Action::MouseDown(hx, hy)).await;
        app.dispatch(Action::MouseUp(hx, hy)).await;
        assert!(
            !app.transcript_pinned(),
            "the jump survives the button path"
        );
        // The hint is gone with the pin — the hover dies on the next
        // frame's validation.
        draw(&mut app, 80, 20).await;
        assert_eq!(app.hover(), None, "no hint, no hover");
    }

    /// A drag cancels the button press: press on `? help`, drag a
    /// cell, release back inside the segment — help must NOT open
    /// (select-1 owns the gesture).
    #[tokio::test]
    async fn drag_cancels_the_button_press() {
        let mut app = test_app().await;
        let rect = help_rect(&mut app).await;
        app.dispatch(Action::MouseDown(rect.x + 2, rect.y)).await;
        app.dispatch(Action::MouseDrag(rect.x + 4, rect.y)).await;
        app.dispatch(Action::MouseUp(rect.x + 2, rect.y)).await;
        assert!(
            !matches!(app.modal, Modal::HelpOpen),
            "a dragged press never triggers the button"
        );
    }

    /// A release OUTSIDE the pressed button does not trigger either
    /// (press on `? help`, release far away — no click semantics for
    /// the button, the legacy path takes over inertly).
    #[tokio::test]
    async fn release_off_target_does_not_trigger() {
        let mut app = test_app().await;
        let rect = help_rect(&mut app).await;
        app.dispatch(Action::MouseDown(rect.x + 2, rect.y)).await;
        app.dispatch(Action::MouseUp(1, 1)).await;
        assert!(!matches!(app.modal, Modal::HelpOpen));
    }

    /// Mouse capture OFF: the terminal sends nothing, and synthetic
    /// MouseMoves must stay inert — plus the toggle itself clears any
    /// live hover.
    #[tokio::test]
    async fn mouse_off_disables_hover_entirely() {
        let mut app = test_app().await;
        let rect = help_rect(&mut app).await;
        app.dispatch(Action::MouseMove(rect.x, rect.y)).await;
        assert_eq!(app.hover(), Some(HoverTarget::HelpHint));
        // Ctrl+M flips capture off → hover drops with it.
        app.dispatch(Action::ToggleMouseCapture).await;
        assert!(!app.mouse_capture());
        assert_eq!(app.hover(), None);
        // Synthetic moves while OFF never light anything up.
        app.dispatch(Action::MouseMove(rect.x, rect.y)).await;
        assert_eq!(app.hover(), None);
    }

    /// coalesce (interactive-1): an adjacent MouseMove run keeps only
    /// the LAST position; interleaved actions break the run.
    #[test]
    fn coalesce_deltas_merges_adjacent_mouse_moves() {
        let mut batch = vec![
            Action::MouseMove(1, 1),
            Action::MouseMove(2, 2),
            Action::MouseMove(3, 3),
            Action::Tick, // separator: no merge across it
            Action::MouseMove(4, 4),
            Action::MouseMove(5, 5),
        ];
        coalesce_deltas(&mut batch);
        assert_eq!(
            batch,
            vec![
                Action::MouseMove(3, 3),
                Action::Tick,
                Action::MouseMove(5, 5),
            ]
        );
    }
}
