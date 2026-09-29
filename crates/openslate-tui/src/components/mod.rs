//! Components — renderable/handleable UI units.
//!
//! Frozen contract (P2b): the [`Component`] trait and [`AppCtx`] shape.
//! P3 lanes implement component *internals* only.
//!
//! Architecture (ratatui component pattern): the App owns all components,
//! converts every input into an [`crate::action::Action`], and dispatches
//! it; each component reacts via [`Component::handle`] (returning an
//! optional follow-up action, e.g. the input editor answering
//! `SubmitInput` with `StartTurn(text)`) and draws via
//! [`Component::render`].

pub mod agents;
pub mod approval;
pub mod help;
pub mod input;
pub mod models;
pub mod session;
pub mod status;
pub mod transcript;

pub use agents::AgentsComponent;
pub use approval::ApprovalComponent;
pub use help::HelpComponent;
pub use input::InputComponent;
pub use models::ModelsComponent;
pub use session::SessionComponent;
pub use status::{RunState, StatusComponent};
pub use transcript::TranscriptComponent;

use ratatui::layout::Rect;
use ratatui::Frame;

use crate::action::Action;
use crate::theme::Theme;

/// Which region currently receives editing/scroll keys (Tab cycles).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// The prompt editor (default).
    Input,
    /// The transcript (scrolling).
    Transcript,
    /// The right sidebar (agents/session panels).
    Sidebar,
}

impl Focus {
    /// Next focus in the Tab cycle: input → transcript → sidebar → input.
    pub fn next(self) -> Self {
        match self {
            Focus::Input => Focus::Transcript,
            Focus::Transcript => Focus::Sidebar,
            Focus::Sidebar => Focus::Input,
        }
    }
}

/// Snapshot of run timing/counters for the status bar and panels.
#[derive(Debug, Clone)]
pub struct RunInfo {
    /// Current run-state machine position (see [`status::RunState`]).
    pub state: RunState,
    /// Spinner frame index (0..=9, advanced once per tick by the App while
    /// a turn is running; the braille sequence lives in `status`).
    pub spinner_frame: usize,
    /// `alias@provider` label of the effective model.
    pub model_label: String,
    /// Session-accumulated input tokens.
    pub tokens_in: u64,
    /// Session-accumulated output tokens.
    pub tokens_out: u64,
    /// Session-accumulated cost in USD (turns + compact summaries).
    pub cost_usd: f64,
    /// Elapsed time of the current turn (`None` while idle).
    pub elapsed: Option<std::time::Duration>,
    /// Tool calls issued in the CURRENT turn (counted from `ToolStart`,
    /// reset when the next turn starts — the last turn's count stays
    /// visible for the session panel until then). Pairs with
    /// [`ConfigSummary::max_tool_calls`].
    pub tool_calls_cur: u32,
    /// Live delegation depth: `call_agent` ToolStart pushes / ToolEnd
    /// pops; calibrated to 0 on `TurnDone` (the execution tree's nodes
    /// are all terminal post-turn). Pairs with [`ConfigSummary::max_depth`].
    pub depth_cur: u32,
    /// Remaining context headroom as a percentage (restyle-1 status
    /// line meter). `None` while no usage snapshot exists — the meter
    /// segment only renders with data.
    pub context_remaining: Option<u8>,
}

/// Static-ish configuration summary the components may display.
#[derive(Debug, Clone)]
pub struct ConfigSummary {
    /// Effective model alias (root agent's, or `/model` override).
    pub model_alias: String,
    /// Resolved model id behind the alias.
    pub model_id: String,
    /// Provider name behind the alias.
    pub provider_name: String,
    /// `[limits].max_depth`.
    pub max_depth: u32,
    /// `[limits].max_tool_calls`.
    pub max_tool_calls: u32,
    /// Backing session run id (once a turn has been submitted).
    pub run_id: Option<String>,
    /// All configured model aliases, sorted (slash-1): the `/model`
    /// argument completion's dynamic choices. Sorted because the
    /// backing map iterates unordered and the list must rank
    /// deterministically.
    pub model_aliases: Vec<String>,
}

/// The context every component sees on `handle`/`render`. Owned snapshot
/// (cheap: theme is `Copy`, strings are short) so the App can build it
/// while mutably borrowing one component.
#[derive(Debug, Clone)]
pub struct AppCtx {
    /// Semantic palette (see [`crate::theme`]).
    pub theme: Theme,
    /// Current focus target.
    pub focus: Focus,
    /// Run state snapshot.
    pub run: RunInfo,
    /// Configuration summary.
    pub config: ConfigSummary,
    /// Terminal size `(width, height)`.
    pub size: (u16, u16),
    /// Transient status-bar notice (yellow; e.g. a rejected `/new`).
    /// Set by the App, cleared on the next `StartTurn` or after a tick
    /// countdown; `None` most of the time. Rendered by the status bar in
    /// place of the key hints.
    pub notice: Option<String>,
}

/// A UI component. Frozen trait: P3 lanes fill internals, never this shape.
pub trait Component {
    /// React to one action. Return `Some(action)` to feed a follow-up
    /// action back into the dispatcher (e.g. the input editor answering
    /// `SubmitInput` with `StartTurn(text)`).
    fn handle(&mut self, action: &Action, ctx: &mut AppCtx) -> Option<Action>;

    /// Draw into `area`. Modals/overlays clear their own background.
    fn render(&self, f: &mut Frame, area: Rect, ctx: &AppCtx);
}
