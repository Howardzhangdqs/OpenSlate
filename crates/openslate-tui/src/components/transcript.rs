//! Transcript — the conversation view (chat main region).
//!
//! Data model and Action consumption were frozen in P2b; this module
//! implements the full P3 lane-a rendering (restyle-1: minimax-code
//! visual language):
//!
//! * user messages render as full-width #262626 background bands —
//!   body width = area−4, 2-column left indent, BOLD signal `› `
//!   anchor on the first content row, one blank band row above and
//!   below; assistant text carries the text-colored `● ` anchor on
//!   its first display row with the body indented 2 (no background);
//!   reasoning keeps fix-23/24's collapsed-single-line shape behind
//!   an accent `• ` marker (BOLD muted summary); tool rows are
//!   single-line entries `├/└ ` (dim line-color connector) + status
//!   marker (`•` running accent / `✓` success / `×` error) + BOLD
//!   English verb ([`tool_verb`]) + muted args summary + a muted
//!   ` · N output lines`-style suffix; icons-5 splits the marker
//!   semantics — reasoning rows take [`IconSet::reasoning`] (the
//!   brain in nerd) while tool-running keeps [`IconSet::bullet`] (the
//!   gear); unicode/ascii render both as the same `•`/`*`; delegation markers render
//!   `● agent Running` / `✓ agent Done`; approval outcomes are
//!   `✓/×/! tool — decision`; the turn-end marker is
//!   `└ model · Ns · ↑in ↓out · ⚡ tok/s` (muted, ⚡ signal);
//! * all glyphs are pure Unicode (no Nerd Font PUA), one-blank-line
//!   step separators ([`TranscriptComponent::step_end`]);
//! * the A4 ruling (locked): LIVE tool entries only ever show Yellow
//!   running (icon + name, spinner) or muted check-icon done — core's
//!   `ToolEnd` carries no success flag. The times-icon failure state
//!   exists ONLY on the `TurnDone` rebuild path, reconstructed
//!   heuristically from Tool-role message content
//!   ([`tool_failure_summary`]);
//! * the streaming area (live deltas/reasoning): reasoning keeps its
//!   dim `┆` raw-text gutter, while the live ANSWER renders markdown
//!   in place through the same pipeline as committed blocks (fix-18)
//!   — no `▍` full-height gutter anymore; the live signal is the
//!   status-bar spinner plus a `▍` tail cursor on the answer's last
//!   row. **event order is visual order**: every mid-turn
//!   entry-producing event (tool start, approval outcome) first
//!   flushes the pending streaming buffers into committed entries
//!   ([`TranscriptComponent::flush_streaming_to_entries`]), so a tool
//!   line lands BELOW the reasoning/answer text that streamed before
//!   it and already-shown text never re-positions when later content
//!   arrives; on `TurnDone(Ok)` the committed entries become the
//!   turn's record ([`TranscriptComponent::merge_turn`]): tool
//!   results fold in from the messages, assistant content the
//!   transcript never saw is appended, and the reasoning/meta entries
//!   SURVIVE (the wholesale [`TranscriptComponent::rebuild`] remains
//!   the recovery fallback — the Err store-reload path, a future
//!   /resume, auto-compact);
//! * reasoning collapse (fix-23, summary rule reworked in fix-24):
//!   every reasoning block — committed entry or live streaming
//!   buffer — renders COLLAPSED to one dim summary row by default
//!   ([`reasoning_summary`]: the block flattened to one line — lines
//!   trimmed, blanks dropped, joined with single spaces — cut to the
//!   available columns from the TAIL, a leading `…` marking dropped
//!   text); clicking the row expands it to the full `┆` gutter block
//!   and clicking the expanded block collapses it again (hit rects
//!   ride the render-records/handle-consumes pattern of
//!   [`TranscriptComponent::hint_hit_rect`], see
//!   [`TranscriptComponent::reasoning_hit_rects`]); an expanded
//!   streaming block commits expanded
//!   ([`TranscriptComponent::flush_streaming_to_entries`]), and
//!   rebuild/merge//new reset the expansion state;
//! * tool row expansion (fix-25): every ToolCall entry retains its
//!   FULL call and output text ([`ToolEntryDetail`], storage-capped
//!   4KB/8KB) — args from the live `ToolStart`/rebuild, output from
//!   the fold paths (the live `ToolEnd` carries bytes only, core's
//!   callback surface is textless) — and CLICKING the row expands a
//!   dim indent-2 detail block: `调用 {name}` then the wrapped args,
//!   `输出` then the wrapped output (运行行内占位 运行中…、（待回
//!   填）、（空）), each section capped at 30 rows behind a
//!   `…（共 N 行）` marker. The click lifecycle mirrors the reasoning
//!   toggle ([`TranscriptComponent::tool_hit_rects`], hit order
//!   hint → reasoning → tool) and rebuild/merge//new reset it too;
//! * scroll pinning: auto-follow at the bottom while content streams;
//!   any user scroll-up pins (new content stops following and a
//!   `+N ↓ 回到底部` hint appears at the bottom-right — CLICKABLE, see
//!   [`TranscriptComponent::hint_hit_rect`]; N counts the rendered
//!   rows below the pin, folded blocks included — fix-25 moved the
//!   count to a `+N` prefix on the fix-23 jump-back copy);
//!   `G`/`End`/`ScrollBottom`/`Esc` release the pin, as does
//!   scrolling back down to the bottom;
//! * manual word wrap ([`wrap_to_width`]): word boundaries preserved
//!   where possible, CJK breaks per character — so scroll offsets are
//!   computed against the exact same wrapped line set that renders;
//! * ASSISTANT text renders through the lightweight markdown subset
//!   ([`crate::md`] via [`push_markdown_block`]) — headings, inline
//!   `**bold**`/`*italic*`/`` `code` ``, fenced code blocks (verbatim,
//!   clipped), lists, blockquotes, rules, links, tables — in BOTH the
//!   finalized blocks AND the live streaming area (fix-18): the same
//!   parse→wrap pipeline runs over the streaming buffer every frame,
//!   unclosed markers render literally until their closing syntax
//!   arrives, and the boundary flush is pixel-seamless (same lines,
//!   same styles — only the tail-cursor span disappears). Reasoning
//!   and user blocks stay exempt (plain `┆`/`┃` gutters);
//! * per-request telemetry ([`TranscriptComponent::finish_request`]):
//!   a dim estimated line at the reasoning block's tail
//!   (`~Ntok · ~Rtok/s`) and the exact usage line
//!   (`↑in ↓out[ ⎓cached] · ttft S.Ss · Rtok/s` — TTFT measured
//!   `RequestStart`→`FirstToken` by the App, omitted when no first
//!   token was observed; the request's total duration is deliberately
//!   not shown); both SURVIVE the successful turn via the merge path
//!   (only the recovery rebuilds drop them — the turn marker's
//!   aggregate totals are that path's usage display). fix-19 moved
//!   the usage line BELOW the step's tool rows (the numbers only
//!   arrive at `RequestEnd`, but tools execute after it — showing
//!   them above the tool rows read wrongly): [`Self::finish_request`]
//!   HOLDS the line in `pending_step_meta` and it lands at the first
//!   boundary that proves where the step ends — `tool_start` appends
//!   it directly below the tool entry (每步配对), the next
//!   `RequestStart`/`merge_turn`/error path flushes an unconsumed
//!   hold at the answer's tail (无工具步); a step separator that
//!   landed while the meta was held swaps BEHIND it
//!   ([`Self::flush_pending_step_meta`]);
//! * live streaming stats (fix-19): while the ANSWER streams, a dim
//!   ASCII row rides the streaming area's head — `ttft S.Ss ·
//!   ~Rtok/s` — the ttft frozen at `FirstToken`
//!   ([`Self::set_live_ttft`], App-fed) and the rate recomputed EVERY
//!   FRAME from the trimmed answer view (chars/2.2 ÷ time since the
//!   first answer delta, [`Self::push_delta`]'s `answer_started`;
//!   suppressed below [`LIVE_RATE_FLOOR_SECS`] — a microseconds-old
//!   block would show absurd rates). Reasoning-only streaming shows
//!   NO live row (the status-bar spinner is the activity signal
//!   there); `RequestEnd` clears it and the held exact line takes
//!   over (above).
//! * drag-to-select (select-1): hold the left button and drag to
//!   select transcript text (tmux copy-mode style) — the selection
//!   lives in LOGICAL space ([`LogicalPos`]: entry/streaming block +
//!   row-in-block + display column, attributed per row by
//!   [`Self::layout_lines`]), so scrolling, resize re-wraps and
//!   streaming appends re-map it every frame instead of
//!   invalidating it. Release AUTO-COPIES the covered text through
//!   the App's copy chain (OSC 52 + `last-copy.md` + clipboard
//!   tool — the same notice as `/copy`); a press that never drags
//!   still fires the legacy positional click (hint jump / row
//!   expansion), so every fix-17/23/25 click behavior survives
//!   unchanged. Highlighting is a post-render style pass
//!   ([`Self::paint_selection`]) painting `theme.selection_bg` over
//!   whole glyph cells (a wide char's halves never split); Esc, a
//!   new press, rebuild/merge//new and overlay opens clear it.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::{AppCtx, Component, Focus};
use crate::action::Action;
use crate::icons::localize;
use crate::theme::Theme;
use openslate_core::types::{Message, MessageRole, Usage};

/// The live stats row's rate floor (fix-19): below this many seconds
/// since the first answer delta the estimated rate is noise (a
/// one-token block microseconds old would render a five-digit tok/s)
/// and the segment is suppressed until the clock makes it meaningful.
const LIVE_RATE_FLOOR_SECS: f64 = 0.3;

/// Storage cap for retained tool ARGUMENTS (fix-25): the expandable
/// detail keeps the full call text up to this many BYTES (marker
/// included); beyond it the text cuts at a char boundary and the
/// [`STORE_TRUNCATED_MARKER`] names the cut.
const ARGS_STORE_CAP: usize = 4 * 1024;
/// Storage cap for retained tool OUTPUT (fix-25), marker included.
const OUTPUT_STORE_CAP: usize = 8 * 1024;
/// Marker appended to retained detail text that hit its storage cap.
const STORE_TRUNCATED_MARKER: &str = "\n…（已截断）";

/// splash-1 (rev): the empty-session wordmark art, ANSI Shadow style
/// (the minimax-code hero look — `█` blocks over `╗ ║ ╔ ╝ ╚ ═` shadow
/// corners), BYTE-FROZEN — the contract test pins every line of both
/// tiers verbatim; do not touch a glyph. Pure BMP box/block glyphs +
/// space (no PUA, no ASCII-art punctuation), rows rstripped, every
/// row padded to the tier width at render time. The unicode/nerd
/// icon tiers show the art; the pure-ASCII icon tier never does
/// (straight to the legacy wordmark fallback).
const SPLASH_ART_FULL: [&str; 6] = [
    " ██████╗ ██████╗ ███████╗███╗   ██╗███████╗██╗      █████╗ ████████╗███████╗",
    "██╔═══██╗██╔══██╗██╔════╝████╗  ██║██╔════╝██║     ██╔══██╗╚══██╔══╝██╔════╝",
    "██║   ██║██████╔╝█████╗  ██╔██╗ ██║███████╗██║     ███████║   ██║   █████╗",
    "██║   ██║██╔═══╝ ██╔══╝  ██║╚██╗██║╚════██║██║     ██╔══██║   ██║   ██╔══╝",
    "╚██████╔╝██║     ███████╗██║ ╚████║███████║███████╗██║  ██║   ██║   ███████╗",
    " ╚═════╝ ╚═╝     ╚══════╝╚═╝  ╚═══╝╚══════╝╚══════╝╚═╝  ╚═╝   ╚═╝   ╚══════╝",
];

/// The full tier's block width in columns (the widest art row):
/// `OpenSlate`.
const SPLASH_FULL_WIDTH: u16 = 76;

/// splash-1 (rev): the medium tier — `Slate` — for narrow (but not
/// tiny) main columns; same glyph contract as the full tier.
const SPLASH_ART_MEDIUM: [&str; 6] = [
    "███████╗██╗      █████╗ ████████╗███████╗",
    "██╔════╝██║     ██╔══██╗╚══██╔══╝██╔════╝",
    "███████╗██║     ███████║   ██║   █████╗",
    "╚════██║██║     ██╔══██║   ██║   ██╔══╝",
    "███████║███████╗██║  ██║   ██║   ███████╗",
    "╚══════╝╚══════╝╚═╝  ╚═╝   ╚═╝   ╚══════╝",
];

/// The medium tier's block width in columns: `Slate`.
const SPLASH_MEDIUM_WIDTH: u16 = 41;

/// The splash block's height: art 6 + blank 1 + hint 1.
const SPLASH_ROWS: u16 = 8;

/// The full tier's minimum MAIN width: 76 art + a 2-column margin
/// each side (the minimax hero.ts ladder threshold).
const SPLASH_FULL_MIN_WIDTH: u16 = 80;

/// The medium tier's minimum MAIN width: 41 art + the same margin.
const SPLASH_MEDIUM_MIN_WIDTH: u16 = 45;

/// The splash's minimum MAIN height (block 8 + one blank row above
/// and below); below it the legacy wordmark fallback renders.
const SPLASH_MIN_HEIGHT: u16 = 10;

/// splash-1 (rev): the size ladder's picked tier — which wordmark
/// block [`TranscriptComponent::render_splash`] paints. `None` (from
/// [`SplashTier::pick`]) means the legacy `✦ OpenSlate` wordmark
/// fallback in the flow path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SplashTier {
    /// The full `OpenSlate` wordmark (76 wide) — main width ≥ 80.
    Full,
    /// The compact `Slate` wordmark (41 wide) — main width ≥ 45.
    Medium,
}

impl SplashTier {
    fn art(self) -> &'static [&'static str; 6] {
        match self {
            Self::Full => &SPLASH_ART_FULL,
            Self::Medium => &SPLASH_ART_MEDIUM,
        }
    }

    fn width(self) -> u16 {
        match self {
            Self::Full => SPLASH_FULL_WIDTH,
            Self::Medium => SPLASH_MEDIUM_WIDTH,
        }
    }

    /// Pick the tier for a main area (the minimax hero ladder; the
    /// micro single-letter tier is deliberately cut — our min-size
    /// guard floors the real area at 60×12): roomy widths take the
    /// full wordmark, medium widths the compact one; narrower areas,
    /// short areas, and the pure-ASCII icon tier (the art's box/block
    /// glyphs are exactly what that tier exists to avoid) all fall
    /// back to `None`.
    fn pick(width: u16, height: u16, ascii: bool) -> Option<Self> {
        if ascii || height < SPLASH_MIN_HEIGHT {
            return None;
        }
        if width >= SPLASH_FULL_MIN_WIDTH {
            Some(Self::Full)
        } else if width >= SPLASH_MEDIUM_MIN_WIDTH {
            Some(Self::Medium)
        } else {
            None
        }
    }
}

/// The empty-session hint line (splash-1): shared verbatim by the
/// splash and the narrow/short fallback (same copy, muted style,
/// localize treatment).
const EMPTY_SESSION_HINT: &str = "空会话 — 输入 prompt 开始,Enter 发送";

/// Retain `s` up to `cap` BYTES (marker included) for the expandable
/// tool detail (fix-25). Byte-budgeted, cut at a UTF-8 char boundary
/// so the total stays within `cap`.
fn cap_stored_text(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_owned();
    }
    let mut end = cap.saturating_sub(STORE_TRUNCATED_MARKER.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &s[..end], STORE_TRUNCATED_MARKER)
}

/// Status of a tool entry line. Live transitions are Running → Done only
/// (A4); [`ToolEntryStatus::Failed`] is produced exclusively by the
/// `TurnDone` rebuild's content heuristics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolEntryStatus {
    /// Running — icon + name(args) in Yellow, spinner frame + live
    /// elapsed rendered from the start instant captured in
    /// [`TranscriptComponent::tool_start`]).
    Running,
    /// Check-icon finished (Nerd U+F00C) — the whole line renders
    /// DarkGray muted with the check keeping its Green accent (bytes +
    /// truncated flag; live-completed entries also carry the measured
    /// duration, rebuild-folded ones do not).
    Done {
        bytes: usize,
        truncated: bool,
        elapsed_ms: Option<u64>,
    },
    /// Times-icon failed (Nerd U+F00D) — rebuild-path only (A4): the
    /// folded Tool message carried one of core's failure markers.
    /// Carries the first-line error summary.
    Failed { summary: String },
}

/// fix-25: retained FULL call/output text behind a tool row, shown
/// when the entry is click-expanded. `args` is the complete argument
/// string ([`ARGS_STORE_CAP`]-capped) — the single-line row keeps its
/// 40-column preview. `output` is the complete tool output
/// ([`OUTPUT_STORE_CAP`]-capped): `None` until a path that carries
/// text observes it — the LIVE `ToolEnd` event only reports bytes
/// (core's `ProgressCallback` carries no output text), so a live-path
/// entry fills at `merge_turn`/`rebuild` from its Tool message
/// (`fold_tool_outcome`/`fold_tool_outcomes_merge`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToolEntryDetail {
    /// Full arguments, storage-capped.
    pub args: String,
    /// Full output, storage-capped; `None` = no text observed yet
    /// (Running, or a live-path Done awaiting the turn's fold).
    pub output: Option<String>,
}

/// One transcript line-item. Frozen data model (fields may gain `..`
/// patterns externally; variant shapes are append-only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptEntry {
    /// User message — Cyan `┃` bar over the block's full height, body
    /// hanging-indented 2 (no text label; the bar IS the identity).
    User(String),
    /// Assistant message text.
    Assistant(String),
    /// Reasoning trace (dim, `┆` prefix).
    Reasoning(String),
    /// Tool call line: `{icon} name(args≤40列)` + status (icon from the
    /// semantic table, [`tool_icon`]). `call_id` links the rebuild
    /// path's Tool-role result messages back to this entry. `detail`
    /// (fix-25) retains the full args/output for the click-expanded
    /// view.
    ToolCall {
        name: String,
        args: String,
        status: ToolEntryStatus,
        call_id: Option<String>,
        detail: ToolEntryDetail,
    },
    /// Approval outcome line.
    Approval { tool_name: String, decision: String },
    /// Delegation marker (restyle-1): `● agent Running` while the
    /// child runs, `✓ agent Done` once its `call_agent` ToolEnd
    /// arrives (FIFO, mirroring the agents panel).
    Delegate { agent: String, done: bool },
    /// Step separator (one blank line) — pushed by
    /// [`TranscriptComponent::step_end`], live-only (the rebuild emits a
    /// clean structure without separators).
    StepBreak,
    /// Dim per-request telemetry line attached at a block's tail by
    /// [`TranscriptComponent::finish_request`]: either an ESTIMATED
    /// reasoning line (`~Ntok · ~Rtok/s`, chars/2.2 heuristic — the
    /// API exposes no reasoning/answer split) or the EXACT request
    /// usage line (`↑in ↓out[ ⎓cached] · ttft S.Ss · Rtok/s`, the
    /// ttft segment omitted when no first token was observed).
    /// Preformatted text; rendered muted, indented 2. These lines
    /// SURVIVE a successful `TurnDone` via the merge path; only the
    /// recovery rebuilds drop them (`TurnSummary` carries no per-step
    /// usage — the turn marker's aggregates are that path's display).
    Meta(String),
}

/// Data for the end-of-turn marker line. The aggregate token totals
/// ride the marker on every path (the turn's sums); on the merge path
/// the per-request meta lines survive BELOW it, on the recovery
/// rebuilds the marker is the turn's only usage display.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TurnMeta {
    model: String,
    elapsed_secs: u64,
    /// Aggregate `(input, output)` tokens, when the turn reported any.
    tokens: Option<(u64, u64)>,
}

// ── select-1: drag-to-select (tmux copy-mode style) ───────────────────
//
// The selection lives in LOGICAL space — block (committed entry or
// the streaming tail) + row-within-block + display column — never in
// screen coordinates, so scrolling, resize re-wraps and streaming
// appends re-map it deterministically every frame instead of
// invalidating it. `layout_lines_at` records the per-row block
// attribution (`line_blocks`, the same render-records/handle-consumes
// pattern as the fix-23/25 hit rects); the screen⇄logical mapping is
// computed against the LAST render's geometry (`sel_area`/`sel_scroll`).

/// Which rendered block a logical position refers to: a committed
/// entry (index into `entries`) or the live streaming tail (the
/// trailing marker fallback + reasoning/answer streaming rows, one
/// contiguous run).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectionBlock {
    Entry(usize),
    Streaming,
}

/// A selection position in logical space (select-1): block + row
/// within the block's rendered lines + display column within that
/// row. `row` clamps to the block's CURRENT row span at map time, so
/// a re-wrap degrades to the nearest row rather than dying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LogicalPos {
    block: SelectionBlock,
    row: usize,
    col: usize,
}

/// The drag-to-select state machine (select-1):
/// `Normal` → (press) `Pending` → (≥1-cell drag) `Selecting` →
/// (release) `Selected` (highlight persists) → (clear trigger) `Normal`.
/// A `Pending` release never dragged → the legacy positional click.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum SelectionState {
    /// No selection.
    #[default]
    Normal,
    /// Button down, anchor recorded, not yet dragged past one cell:
    /// the legacy Click stays suppressed until release proves no
    /// drag happened. `down` holds the press's screen cell for the
    /// displacement threshold.
    Pending {
        anchor: LogicalPos,
        down: (u16, u16),
    },
    /// Dragging: the cursor tracks every drag event.
    Selecting {
        anchor: LogicalPos,
        cursor: LogicalPos,
    },
    /// Released with a selection: the highlight persists until a
    /// clear trigger (Esc, a new press, rebuild/merge//new, an
    /// overlay opening).
    Selected {
        anchor: LogicalPos,
        cursor: LogicalPos,
    },
}

impl SelectionState {
    /// The `(anchor, cursor)` pair while a selection is on screen
    /// (`Selecting` or `Selected`); `None` in `Normal`/`Pending`.
    fn range(&self) -> Option<(LogicalPos, LogicalPos)> {
        match *self {
            SelectionState::Selecting { anchor, cursor }
            | SelectionState::Selected { anchor, cursor } => Some((anchor, cursor)),
            _ => None,
        }
    }
}

/// What a left-button release should do (select-1) — the App's
/// [`Action::MouseUp`] routing consumes this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionEnd {
    /// The press never dragged: the caller dispatches the legacy
    /// positional [`Action::Click`] (hint jump / row expansion /
    /// inert miss — every fix-17/23/25 behavior).
    Click,
    /// The drag completed a selection; carries the extracted text
    /// (row texts joined with `\n`, boundary rows char-sliced by
    /// display column — byte-capping is the copy chain's job). The
    /// highlight persists as `Selected`.
    Selected(String),
}

/// The conversation view. Holds committed entries plus the live streaming
/// buffer; scroll is a line offset from the top with a follow/pinned
/// toggle. Line offsets are computed over the SAME wrapped line set the
/// renderer produces ([`TranscriptComponent::layout_lines`]), so pinning
/// math and rendering can never disagree.
#[derive(Debug, Default)]
pub struct TranscriptComponent {
    entries: Vec<TranscriptEntry>,
    /// Last turn's metadata (model, elapsed secs, aggregate tokens) for
    /// the end-of-turn marker line (` model · Ns [· ↑in ↓out]`, cube
    /// icon). Pre-wired by the App right before `rebuild`; consumed by
    /// the borderless restyle rendering.
    turn_meta: Option<TurnMeta>,
    /// Live streamed answer text of the CURRENT still-streaming step
    /// (flushed into entries at the next boundary event; cleared on
    /// rebuild).
    streaming: String,
    /// Live streamed reasoning text of the CURRENT still-streaming step
    /// (same flush/clear lifecycle as `streaming`).
    streaming_reasoning: String,
    /// When the FIRST reasoning delta of the current reasoning block
    /// arrived (set by [`TranscriptComponent::push_reasoning`]; cleared
    /// on every flush/clear) — the reasoning meta line's duration
    /// numerator.
    reasoning_started: Option<Instant>,
    /// When the FIRST answer delta of the current answer block arrived
    /// (set by [`TranscriptComponent::push_delta`]; cleared on every
    /// flush/clear) — the live stats row's rate denominator (fix-19).
    answer_started: Option<Instant>,
    /// The request's precise TTFT, frozen the moment `FirstToken`
    /// arrives (App-fed via [`TranscriptComponent::set_live_ttft`];
    /// cleared per request). Rendered by the live stats row only —
    /// the final usage line gets its own copy at `RequestEnd`.
    live_ttft: Option<Duration>,
    /// web-1: the server's input-token estimate for the streaming
    /// request (`input_estimate` broadcast → App-fed via
    /// [`TranscriptComponent::set_live_input_estimate`]); rendered as
    /// the live row's leading `↑~N` segment, cleared per request.
    live_input_est: Option<u32>,
    /// The HELD exact usage line (fix-19's position move): set by
    /// [`TranscriptComponent::finish_request`] instead of committing
    /// immediately, because tools of the same step execute AFTER
    /// `RequestEnd` and the stats belong BELOW them. Flushed
    /// ([`TranscriptComponent::flush_pending_step_meta`]) at the
    /// first boundary that proves where the step ended: `tool_start`
    /// (below the tool entry), the next `RequestStart`, `merge_turn`
    /// or the error path (at the answer's tail for tool-less steps).
    pending_step_meta: Option<String>,
    /// Line scroll offset from the top (valid only while pinned).
    scroll: u16,
    /// `true` → stick to the bottom on new content; user scroll-up pins.
    follow: bool,
    /// `(inner_width, inner_height)` of the area the last render drew
    /// into. `handle` needs the viewport geometry to decide when a
    /// scroll-down has reached the bottom (re-follow); render updates it
    /// (interior mutability — `Component::render` takes `&self`).
    viewport: Cell<(u16, u16)>,
    /// Terminal-absolute hit rectangle of the last-rendered
    /// `+N ↓ 回到底部` hint (the hint's own row, its right-aligned span
    /// widened 2 columns each side); `None` whenever the hint did NOT
    /// render (following, nothing below, zero-height viewport). `handle`
    /// hit-tests [`Action::Click`] against it — a hit returns
    /// [`Action::ScrollBottom`] (the existing follow-restore action).
    /// Interior mutability like `viewport` (render takes `&self`).
    hint_hit_rect: Cell<Option<Rect>>,
    /// interactive-1: whether the jump hint renders HOVERED (the App's
    /// `MouseMove` hit-test feeds this before each render via
    /// [`TranscriptComponent::set_hint_hovered`]; pure presentation —
    /// the click behavior is the pre-existing fix-17 path).
    hint_hovered: bool,
    /// fix-23: entries whose Reasoning blocks render EXPANDED (the
    /// full `┆` gutter block) — every other reasoning block collapses
    /// to one summary row ([`reasoning_summary`]). Keyed by entry
    /// index; cleared by rebuild/merge//new (indices die with the
    /// entries).
    expanded_reasoning: HashSet<usize>,
    /// fix-23: whether the LIVE streaming reasoning block renders
    /// expanded (its own toggle — independent of the committed
    /// entries' state). Resets with the buffers (begin/clear/flush).
    streaming_reasoning_expanded: bool,
    /// fix-23 hit-test records: `(entry index, layout start row, row
    /// count)` of every committed Reasoning block this layout pass
    /// produced — a collapsed block spans its single summary row, an
    /// expanded one its whole block. [`Self::render`] converts them
    /// into terminal-absolute Rects (`reasoning_hit_rects`).
    /// Interior mutability because `layout_lines` takes `&self`;
    /// cleared at the top of every pass (scroll actions re-run the
    /// layout between frames — without the clear, records accumulate).
    reasoning_rows: RefCell<Vec<(usize, usize, usize)>>,
    /// The streaming reasoning block's `(start row, row count)` in
    /// the same-pass coordinates — `None` when no live block renders.
    streaming_reasoning_rows: Cell<Option<(usize, usize)>>,
    /// fix-23: TERMINAL-ABSOLUTE hit rects (entry index → Rect) of
    /// the committed reasoning blocks the LAST RENDER drew (clipped
    /// to the viewport). `handle`'s [`Action::Click`] toggles the
    /// entry's expansion on a hit. Rewritten unconditionally every
    /// render (empty when nothing renders) — the same
    /// render-records/handle-consumes pattern as
    /// [`Self::hint_hit_rect`].
    reasoning_hit_rects: RefCell<Vec<(usize, Rect)>>,
    /// The streaming reasoning block's terminal-absolute hit rect
    /// (same lifecycle as `reasoning_hit_rects`).
    streaming_reasoning_hit_rect: Cell<Option<Rect>>,
    /// fix-25: entries whose ToolCall rows render EXPANDED — the head
    /// line plus the dim call/output detail block — same click-toggle
    /// lifecycle as `expanded_reasoning`; cleared by
    /// rebuild/merge//new.
    expanded_tools: HashSet<usize>,
    /// fix-25 hit-test records: `(entry index, layout start row, row
    /// count)` of every ToolCall entry this pass produced — collapsed
    /// covers the head row, expanded the head + detail block. Mirrors
    /// `reasoning_rows`.
    tool_rows: RefCell<Vec<(usize, usize, usize)>>,
    /// fix-25: TERMINAL-ABSOLUTE hit rects (entry index → Rect) of
    /// the ToolCall entries the LAST RENDER drew (clipped to the
    /// viewport); `handle`'s [`Action::Click`] toggles expansion on
    /// a hit. Mirrors `reasoning_hit_rects`.
    tool_hit_rects: RefCell<Vec<(usize, Rect)>>,
    /// select-1: one block id ([`SelectionBlock`]) per emitted row,
    /// recorded by every `layout_lines` pass — the selection's
    /// screen⇄logical mapping source (same render-records pattern
    /// as `reasoning_rows`; cleared at the top of every pass).
    line_blocks: RefCell<Vec<SelectionBlock>>,
    /// select-1: the area the LAST RENDER drew into — the origin the
    /// screen⇄logical mapping converts against. Interior mutability
    /// like `viewport` (render takes `&self`).
    sel_area: Cell<Option<Rect>>,
    /// select-1: the effective scroll offset the last render used
    /// (mapping rows against anything else would misalign under
    /// pinning).
    sel_scroll: Cell<usize>,
    /// select-1: the drag-to-select state (logical positions). See
    /// [`SelectionState`] for the transitions and clear triggers.
    selection: SelectionState,
    /// Entries-index the turn marker renders BEFORE (recorded by
    /// `merge_turn`/`rebuild` at the finished turn's tail). Keeps the
    /// marker ABOVE the next turn's content: the marker used to render
    /// layout-trailing, which slid it below the next turn's user
    /// message the moment that entry appeared. `None` → trailing
    /// render (fresh transcript, tests that only `set_turn_meta`).
    marker_at: Option<usize>,
    /// Start instants of LIVE running tool entries, keyed by entry index
    /// (live entries are append-only, so indices are stable until the
    /// next rebuild clears this).
    running_since: HashMap<usize, Instant>,
}

impl TranscriptComponent {
    pub fn new() -> Self {
        Self {
            follow: true,
            ..Self::default()
        }
    }

    /// Snapshot for tests/panels.
    pub fn entries(&self) -> &[TranscriptEntry] {
        &self.entries
    }

    /// interactive-1: the last-rendered `+N ↓ 回到底部` hint's hit
    /// rectangle (`None` whenever the hint did not render) — the
    /// App's hover hit-test source.
    pub fn hint_rect(&self) -> Option<Rect> {
        self.hint_hit_rect.get()
    }

    /// interactive-1: toggle the jump hint's hovered presentation for
    /// the next render (fed from the App's hover state).
    pub fn set_hint_hovered(&mut self, hovered: bool) {
        self.hint_hovered = hovered;
    }

    /// Read-only access to the most recent RETAINED tool output
    /// (fix-25's [`ToolEntryDetail`] storage — the `/copy tool`
    /// source). Scans from the tail, so a live-path entry whose output
    /// has not folded in yet (`None` until the turn's merge/rebuild)
    /// falls back to the last entry that DOES have output. `None` when
    /// no tool entry ever retained text.
    pub fn last_tool_output(&self) -> Option<&str> {
        self.entries.iter().rev().find_map(|entry| match entry {
            TranscriptEntry::ToolCall { detail, .. } => detail.output.as_deref(),
            _ => None,
        })
    }

    /// Live streaming buffer (answer deltas only).
    pub fn streaming_text(&self) -> &str {
        &self.streaming
    }

    /// Whether the view is pinned (not following the tail).
    pub fn is_pinned(&self) -> bool {
        !self.follow
    }

    /// Whether the transcript holds NOTHING renderable — no committed
    /// entries, no streaming buffers, no pending turn meta (splash-1:
    /// gates the splash / fallback empty-state render).
    fn is_blank(&self) -> bool {
        self.entries.is_empty()
            && self.streaming.is_empty()
            && self.streaming_reasoning.is_empty()
            && self.turn_meta.is_none()
    }

    /// A user message was submitted (live path; the rebuild after
    /// `TurnDone` will re-emit it from messages).
    pub fn push_user(&mut self, text: &str) {
        self.entries.push(TranscriptEntry::User(text.to_owned()));
    }

    /// Start a new turn's streaming area (clears leftovers from a
    /// previous cancelled/errored turn). Any still-held step meta
    /// flushes FIRST — every regular path (`merge_turn`, the next
    /// `RequestStart`, the error path) has already flushed it by the
    /// time a new turn starts, so this is an unreachable safety net
    /// that prefers landing the stats at the transcript tail over
    /// silently losing them.
    pub fn begin_streaming(&mut self) {
        self.flush_pending_step_meta();
        self.streaming.clear();
        self.streaming_reasoning.clear();
        self.streaming_reasoning_expanded = false;
        self.reasoning_started = None;
        self.answer_started = None;
        self.live_ttft = None;
        self.live_input_est = None;
    }

    /// One answer delta arrived. The first delta of a block also
    /// starts the block's timer (`answer_started`, feeds the live
    /// stats row's rate — fix-19).
    pub fn push_delta(&mut self, text: &str) {
        if self.streaming.is_empty() && !text.is_empty() {
            self.answer_started = Some(Instant::now());
        }
        self.streaming.push_str(text);
    }

    /// The current request's precise TTFT (fix-19): fed by the App at
    /// `FirstToken` (`first − request_started`, exact — the value is
    /// frozen the moment the first token arrives); `None` clears it
    /// (the App resets at every `RequestStart`). Rendered by the live
    /// stats row while the answer streams; `finish_request` clears it
    /// when the held exact line takes over.
    pub fn set_live_ttft(&mut self, ttft: Option<Duration>) {
        self.live_ttft = ttft;
    }

    /// web-1: the server's input-token estimate for the current
    /// streaming request (`↑~N` live-row segment). `None` clears
    /// (every `RequestStart`; the exact usage line replaces it at
    /// `RequestEnd`).
    pub fn set_live_input_estimate(&mut self, tokens: Option<u32>) {
        self.live_input_est = tokens;
    }

    /// One reasoning delta arrived. The first delta of a block also
    /// starts the block's timer (feeds the reasoning meta line).
    pub fn push_reasoning(&mut self, text: &str) {
        if self.streaming_reasoning.is_empty() && !text.is_empty() {
            self.reasoning_started = Some(Instant::now());
        }
        self.streaming_reasoning.push_str(text);
    }

    /// A tool started (live entry, Running status). `call_agent` becomes
    /// a delegation marker instead (child content never enters the
    /// transcript — spec D2). Any still-streaming text is flushed into
    /// committed entries FIRST ([`TranscriptComponent::flush_streaming_to_entries`])
    /// so the tool line lands below text that streamed before it —
    /// event order IS visual order. The tool entry also proves the
    /// current step HAD tools, so a held per-request meta line
    /// (`pending_step_meta`, fix-19) lands directly BELOW it — the
    /// stats describe the request whose tool calls this is.
    pub fn tool_start(&mut self, name: &str, args: &str) {
        self.flush_streaming_to_entries();
        if name == "call_agent" {
            self.entries.push(TranscriptEntry::Delegate {
                agent: super::status::delegate_target(args),
                done: false,
            });
            self.flush_pending_step_meta();
            return;
        }
        let index = self.entries.len();
        self.entries.push(TranscriptEntry::ToolCall {
            name: name.to_owned(),
            args: args_preview(args),
            status: ToolEntryStatus::Running,
            call_id: None,
            // fix-25: retain the FULL args for the expanded view (the
            // live ToolStart event carries them complete); output
            // fills at merge/rebuild (live ToolEnd has bytes only).
            detail: ToolEntryDetail {
                args: cap_stored_text(args, ARGS_STORE_CAP),
                output: None,
            },
        });
        self.running_since.insert(index, Instant::now());
        self.flush_pending_step_meta();
    }

    /// A tool finished: mark the LAST running entry with this name Done
    /// (A4: no failure state on the live path — the rebuild decides).
    /// A `call_agent` end additionally completes the OLDEST still-live
    /// Delegate marker (FIFO — same pairing as the agents panel).
    pub fn tool_end(&mut self, name: &str, bytes: usize, truncated: bool) {
        if name == "call_agent" {
            for entry in &mut self.entries {
                if let TranscriptEntry::Delegate { done, .. } = entry {
                    if !*done {
                        *done = true;
                        break;
                    }
                }
            }
        }
        for (index, entry) in self.entries.iter_mut().enumerate().rev() {
            if let TranscriptEntry::ToolCall {
                name: entry_name,
                status,
                ..
            } = entry
            {
                if entry_name == name && *status == ToolEntryStatus::Running {
                    let elapsed_ms = self
                        .running_since
                        .remove(&index)
                        .map(|t| t.elapsed().as_millis() as u64);
                    *status = ToolEntryStatus::Done {
                        bytes,
                        truncated,
                        elapsed_ms,
                    };
                    break;
                }
            }
        }
    }

    /// An approval request was answered (decision display string). The
    /// outcome line flushes any pending streaming text first, exactly
    /// like [`TranscriptComponent::tool_start`].
    pub fn push_approval(&mut self, tool_name: &str, decision: &str) {
        self.flush_streaming_to_entries();
        self.entries.push(TranscriptEntry::Approval {
            tool_name: tool_name.to_owned(),
            decision: decision.to_owned(),
        });
    }

    /// Flush the still-streaming buffers into committed entries,
    /// preserving event order (the streaming-order fix): every
    /// mid-turn entry-producing event (`tool_start`,
    /// `push_approval`) calls this FIRST, so the new entry lands BELOW
    /// text that streamed before it instead of the old behavior where
    /// ALL entries rendered above the streaming area and a mid-turn
    /// tool line re-positioned (overlapped) reasoning the user was
    /// already reading.
    ///
    /// After the flush the streaming area holds only the tail that is
    /// still growing, so previously-shown text NEVER moves again —
    /// append-only layout from the reader's perspective. Buffers are
    /// cleared unconditionally (`mem::take`); a whitespace-only buffer
    /// is dropped rather than committed, and a non-empty buffer is
    /// committed with its EDGE BLANK LINES TRIMMED
    /// ([`trim_edge_blank_lines`]) so committed blocks carry no
    /// leading/trailing empty gutter rows (matching the live view).
    fn flush_streaming_to_entries(&mut self) {
        let reasoning = std::mem::take(&mut self.streaming_reasoning);
        // fix-23: an expanded live block commits EXPANDED (no visual
        // jump at the boundary); the flag resets with the buffer (the
        // next streaming block starts collapsed).
        let reasoning_was_expanded = self.streaming_reasoning_expanded;
        self.streaming_reasoning_expanded = false;
        self.reasoning_started = None;
        self.answer_started = None;
        if !reasoning.trim().is_empty() {
            self.commit_reasoning(trim_edge_blank_lines(&reasoning), reasoning_was_expanded);
        }
        let answer = std::mem::take(&mut self.streaming);
        if !answer.trim().is_empty() {
            self.entries
                .push(TranscriptEntry::Assistant(trim_edge_blank_lines(&answer)));
        }
    }

    /// Commit one Reasoning entry (fix-23's flush/finish paths),
    /// carrying the live block's expansion state: a block the reader
    /// had expanded while it streamed lands in `expanded_reasoning`,
    /// so the boundary flush never collapses text the user is
    /// reading.
    fn commit_reasoning(&mut self, text: String, was_expanded: bool) {
        if was_expanded {
            self.expanded_reasoning.insert(self.entries.len());
        }
        self.entries.push(TranscriptEntry::Reasoning(text));
    }

    /// A model request completed (`Usage` already stored, `RequestEnd`
    /// measured by the App): flush the request's streamed blocks into
    /// committed entries and attach the dim meta lines — an ESTIMATED
    /// line at the reasoning block's tail (`~Ntok · ~Rtok/s`) and the
    /// EXACT per-request usage line (`↑in ↓out[ ⎓cached] · ttft S.Ss ·
    /// Rtok/s`; `ttft` is the App-measured `RequestStart`→`FirstToken`
    /// latency, omitted when `None` — no first token was observed).
    ///
    /// fix-19: the usage line is no longer committed here — it is
    /// HELD in `pending_step_meta`, because the engine fires
    /// `RequestEnd` BEFORE this step's tools execute and the stats
    /// belong BELOW the tool rows. It lands at the first boundary
    /// that proves where the step ended: `tool_start` (directly below
    /// the tool entry — 每步配对), the next `RequestStart`,
    /// [`Self::merge_turn`] or the error path (at the answer's tail —
    /// 无工具步, the pre-fix-19 position). A stale hold from a
    /// tool-less step flushes HERE first, above the new step's
    /// blocks. The live stats row dies with this call (its exact
    /// successor is the held line).
    ///
    /// When the answer is EMPTY (the model went straight to tool
    /// calls — no content streamed for this step) the estimated
    /// reasoning line and the held usage line would stack with no
    /// block between them; they MERGE into one held line instead
    /// ([`meta_merged_line`]). Committed blocks carry their edge
    /// blank lines trimmed ([`trim_edge_blank_lines`]).
    pub fn finish_request(
        &mut self,
        usage: Option<Usage>,
        request_elapsed: Option<Duration>,
        ttft: Option<Duration>,
    ) {
        // A hold that survived to the NEXT request's end means the
        // previous step had no tools — land it at that step's tail
        // before the new blocks commit above nothing.
        self.flush_pending_step_meta();
        let reasoning_raw = std::mem::take(&mut self.streaming_reasoning);
        let reasoning_started = self.reasoning_started.take();
        // fix-23: the live block's expansion rides the commit (see
        // `commit_reasoning`); the flag resets with the buffers.
        let reasoning_was_expanded = self.streaming_reasoning_expanded;
        self.streaming_reasoning_expanded = false;
        let answer_raw = std::mem::take(&mut self.streaming);
        self.answer_started = None;
        self.live_ttft = None;
        self.live_input_est = None;

        let has_reasoning = !reasoning_raw.trim().is_empty();
        let has_answer = !answer_raw.trim().is_empty();
        let reasoning = if has_reasoning {
            trim_edge_blank_lines(&reasoning_raw)
        } else {
            reasoning_raw
        };
        let chars = reasoning.chars().count();
        let elapsed = request_elapsed.unwrap_or_default();

        if has_answer {
            // Interleaved flush: reasoning → reasoning meta → answer;
            // the usage line is HELD (fix-19) for the boundary that
            // decides its position.
            if has_reasoning {
                self.commit_reasoning(reasoning, reasoning_was_expanded);
                if let Some(started) = reasoning_started {
                    self.entries.push(TranscriptEntry::Meta(meta_reasoning_line(
                        chars,
                        started.elapsed(),
                    )));
                }
            }
            self.entries
                .push(TranscriptEntry::Assistant(trim_edge_blank_lines(
                    &answer_raw,
                )));
            if let Some(usage) = usage {
                self.pending_step_meta = Some(meta_request_line(&usage, ttft, elapsed));
            }
        } else {
            // Empty answer: the reasoning estimate and the held usage
            // line merge into ONE held line — never two stacked metas
            // with no output block between them.
            match (has_reasoning, usage) {
                (true, Some(usage)) => {
                    self.commit_reasoning(reasoning, reasoning_was_expanded);
                    self.pending_step_meta = Some(meta_merged_line(chars, &usage, ttft, elapsed));
                }
                (true, None) => {
                    self.commit_reasoning(reasoning, reasoning_was_expanded);
                    if let Some(started) = reasoning_started {
                        self.entries.push(TranscriptEntry::Meta(meta_reasoning_line(
                            chars,
                            started.elapsed(),
                        )));
                    }
                }
                (false, Some(usage)) => {
                    self.pending_step_meta = Some(meta_request_line(&usage, ttft, elapsed));
                }
                (false, None) => {}
            }
        }
    }

    /// Land a held per-request usage line (fix-19) — no-op when
    /// nothing is held. Called at every boundary that proves where a
    /// step ended:
    ///
    /// * [`Self::tool_start`] — directly below the (first) tool entry
    ///   of the step, so the stats trail the tool rows they describe;
    /// * the next [`Self::finish_request`] / the App's next
    ///   `RequestStart` / [`Self::merge_turn`] / the error path — at
    ///   the answer's tail (the step provably had no tools).
    ///
    /// A step separator that landed while the meta was held (the
    /// engine's `RequestEnd`/`StepEnd` separators fire before tools
    /// start) SWAPS BEHIND the meta: the line belongs to the answer's
    /// tail, not dangling below the blank line.
    pub fn flush_pending_step_meta(&mut self) {
        let Some(text) = self.pending_step_meta.take() else {
            return;
        };
        if matches!(self.entries.last(), Some(TranscriptEntry::StepBreak)) {
            self.entries.pop();
            self.entries.push(TranscriptEntry::Meta(text));
            self.entries.push(TranscriptEntry::StepBreak);
        } else {
            self.entries.push(TranscriptEntry::Meta(text));
        }
    }

    /// Drop the streaming buffers (error path: keep committed entries)
    /// along with the live-stats timers (fix-19). A held step meta is
    /// NOT dropped — the App flushes it first (the stats the user
    /// watched live still materialize).
    pub fn clear_streaming(&mut self) {
        self.streaming.clear();
        self.streaming_reasoning.clear();
        self.streaming_reasoning_expanded = false;
        self.reasoning_started = None;
        self.answer_started = None;
        self.live_ttft = None;
        self.live_input_est = None;
    }

    /// A step boundary was observed (`Usage` / `RequestEnd` / `StepEnd`
    /// engine events — see `App::handle_engine_event`). Renders as one
    /// blank line (the borderless spec retired the full-width `─`
    /// rule); consecutive boundary events collapse into one separator,
    /// and no separator is emitted directly after the user's message
    /// (turn start) or another separator.
    pub fn step_end(&mut self) {
        match self.entries.last() {
            None | Some(TranscriptEntry::User(_)) | Some(TranscriptEntry::StepBreak) => {}
            _ => self.entries.push(TranscriptEntry::StepBreak),
        }
    }

    /// The RECOVERY fallback (wipe & rebuild): re-derive every entry
    /// from the finished turn's message list and clear the streaming
    /// area. Called on the TurnDone(Err) store-reload path (and
    /// reserved for a future /resume; /new's auto-compact shares the
    /// history-reset semantics); successful turns take the merge path
    /// ([`Self::merge_turn`]) instead — this runs only where losing
    /// the live-only reasoning/meta entries is acceptable.
    ///
    /// Tool-role result messages do NOT create entries — they fold back
    /// into their matching ToolCall entry ([`fold_tool_outcome`]):
    /// success refines `bytes`/`truncated`, failure markers flip the
    /// entry to Failed (the A4 heuristic).
    ///
    /// The pin decision the user made while streaming SURVIVES the
    /// rebuild (a pin is only released explicitly — G/End/Esc/scroll to
    /// bottom); the offset is clamped against the new content at render.
    /// Record the finished turn's metadata (model, elapsed secs, and
    /// the turn's aggregate token usage) for the end-of-turn marker.
    /// Pre-wired by the App before `merge_turn`/`rebuild`;
    /// [`TranscriptComponent::layout_lines`] renders it as the trailing
    /// ` model · Ns [· ↑in ↓out]` line (cube icon Cyan, the rest
    /// muted). The totals are the turn's sums: the only usage display
    /// on the recovery rebuild path (which drops the live per-request
    /// meta lines — `TurnSummary` carries no per-step usage), the
    /// aggregate summary above the surviving per-request lines on the
    /// merge path.
    pub fn set_turn_meta(&mut self, model: String, elapsed_secs: u64, tokens: Option<(u64, u64)>) {
        self.turn_meta = Some(TurnMeta {
            model,
            elapsed_secs,
            tokens,
        });
    }

    pub fn rebuild(&mut self, messages: &[Message]) {
        let mut entries: Vec<TranscriptEntry> = Vec::new();
        for msg in messages {
            if msg.role == MessageRole::Tool {
                fold_tool_outcome(&mut entries, msg);
            } else {
                entries.extend(entry_from_message(msg));
            }
        }
        // Any call whose Tool message never materialized is finished by
        // now (the turn ended) — finalize it as a plain Done.
        for entry in &mut entries {
            if let TranscriptEntry::ToolCall { status, .. } = entry {
                if *status == ToolEntryStatus::Running {
                    *status = ToolEntryStatus::Done {
                        bytes: 0,
                        truncated: false,
                        elapsed_ms: None,
                    };
                }
            }
        }
        self.entries = entries;
        self.clear_streaming();
        // fix-23/fix-25: entry indices died with the old entries —
        // every reasoning block and tool row re-derives collapsed.
        // select-1: the logical block ids died with them.
        self.expanded_reasoning.clear();
        self.expanded_tools.clear();
        self.clear_selection();
        // The rebuild re-derives everything from messages: a held
        // step meta (live-only state, like the reasoning entries) is
        // dropped — the turn marker's aggregates are this path's
        // usage display.
        self.pending_step_meta = None;
        self.running_since.clear();
        // The (previous turn's) marker repositions to the rebuilt
        // content's tail — still the LAST line of committed content.
        self.marker_at = Some(self.entries.len());
    }

    /// THE `TurnDone(Ok)` merge point: the committed entries — the
    /// event-ordered authority since fix-11/15's flush semantics —
    /// become the finished turn's record, so reasoning blocks and
    /// per-request meta lines SURVIVE the turn end. Only four things
    /// happen here (never a wipe):
    ///
    /// 1. streaming stragglers commit FIRST
    ///    ([`Self::flush_streaming_to_entries`]) — an Interrupted turn
    ///    cut mid-flight leaves text the user already read in the
    ///    buffers; dropping it would be visible content vanishing (no
    ///    meta lines: that request never finished);
    /// 2. every Tool-role message folds its outcome into the matching
    ///    ToolCall entry ([`fold_tool_outcomes_merge`]) — refining
    ///    even live-completed entries, because the live `ToolEnd`
    ///    carries no success flag (A4: failure markers are only
    ///    visible in the message content); calls still Running (no
    ///    live end, no message) finalize as a plain Done;
    /// 3. a drift guard (the ora-2 dual-source concern, inverted: the
    ///    merge path only ever ADDS): assistant content present in
    ///    `messages` but missing from the transcript — dropped events,
    ///    a non-streaming provider — is APPENDED so it is never lost;
    ///    existing assistant/reasoning entries are never modified or
    ///    removed;
    /// 4. trailing step separators pop so the turn marker's block gap
    ///    ([`Self::set_turn_meta`]) is the single blank line before it;
    /// 5. a still-HELD step meta (fix-19: the final request of the
    ///    turn ended with no tools following — the common
    ///    answer-and-done shape) flushes at the answer's tail — the
    ///    `TurnDone` 兜底.
    ///
    /// The engine-side history stays `messages` (the App owns that
    /// assignment) — transcript display and history are decoupled.
    /// The wholesale [`Self::rebuild`] remains the fallback for the
    /// recovery paths (TurnDone(Err) store reload, a future /resume)
    /// where losing the live-only reasoning/meta entries is
    /// acceptable.
    pub fn merge_turn(&mut self, messages: &[Message]) {
        // 1. Commit any streaming stragglers at their event position.
        self.flush_streaming_to_entries();
        // 2. Fold tool outcomes from the message list, then finalize
        //    any call that never observed an end at all.
        fold_tool_outcomes_merge(&mut self.entries, messages);
        for entry in &mut self.entries {
            if let TranscriptEntry::ToolCall { status, .. } = entry {
                if *status == ToolEntryStatus::Running {
                    *status = ToolEntryStatus::Done {
                        bytes: 0,
                        truncated: false,
                        elapsed_ms: None,
                    };
                }
            }
        }
        // 3. Assistant-content drift guard: multiset comparison keyed
        //    on edge-trimmed content (the flush commits trimmed
        //    blocks; the raw message may carry `\n\n` edges) — each
        //    unmatched message appends exactly one entry.
        let mut present: HashMap<String, u32> = HashMap::new();
        for entry in &self.entries {
            if let TranscriptEntry::Assistant(text) = entry {
                *present.entry(text.clone()).or_insert(0) += 1;
            }
        }
        for msg in messages {
            if msg.role != MessageRole::Assistant || msg.content.trim().is_empty() {
                continue;
            }
            let key = trim_edge_blank_lines(&msg.content);
            let count = present.entry(key.clone()).or_insert(0);
            if *count == 0 {
                tracing::debug!(
                    content = %key,
                    "assistant content missing from transcript (drift guard): appended"
                );
                self.entries.push(TranscriptEntry::Assistant(key));
                // The pushed entry satisfies THIS message; the count
                // stays 0 so an identical later message appends too.
            } else {
                *count -= 1;
            }
        }
        // 4. Trailing separators collapse (the marker's block gap
        // becomes the single blank line before it).
        while matches!(self.entries.last(), Some(TranscriptEntry::StepBreak)) {
            self.entries.pop();
        }
        // 5. fix-19 兜底: a held step meta (the turn's last request
        // had no tools) lands at the answer's tail, below the turn's
        // content and above the marker.
        self.flush_pending_step_meta();
        // The flush already drained the streaming buffers
        // (clear_streaming is implicit); only the live-tool timers
        // still need dropping. The marker pins HERE: rendering it
        // layout-trailing would slide it below the NEXT turn's user
        // message once that entry appears.
        self.running_since.clear();
        self.marker_at = Some(self.entries.len());
        // fix-23/fix-25: conservative reset (indices are technically
        // stable on this append-only path, but the finished turn's
        // record reads cleaner re-collapsed). select-1: the turn
        // boundary clears any selection too.
        self.expanded_reasoning.clear();
        self.expanded_tools.clear();
        self.clear_selection();
    }

    /// Clear everything (`/new`). The turn marker goes too — a fresh
    /// session has no last turn to mark (a held step meta is dropped
    /// with it).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.clear_streaming();
        self.expanded_reasoning.clear();
        self.expanded_tools.clear();
        self.clear_selection();
        self.pending_step_meta = None;
        self.turn_meta = None;
        self.marker_at = None;
        self.scroll = 0;
        self.follow = true;
        self.running_since.clear();
    }

    /// web-1 snapshot rebuild: swap in the server's entry mirror
    /// wholesale (hello ack / reconnect / post-`new_session`). This is
    /// the ONLY wholesale path that preserves the live-only entry
    /// kinds (Reasoning / Meta / Delegate / Tool detail) — the server
    /// mirrors them; the message-based [`Self::rebuild`] cannot.
    /// Live-view state resets like [`Self::rebuild`] (indices died
    /// with the old entries) plus the turn marker (the snapshot's
    /// Meta entries already carry the historical marker lines) and
    /// the scroll pin (jump to the live tail of the restored view).
    pub fn replace_entries(&mut self, entries: Vec<TranscriptEntry>) {
        self.entries = entries;
        self.clear_streaming();
        self.expanded_reasoning.clear();
        self.expanded_tools.clear();
        self.clear_selection();
        self.pending_step_meta = None;
        self.turn_meta = None;
        self.marker_at = None;
        self.running_since.clear();
        self.scroll = 0;
        self.follow = true;
    }

    // ── Scroll / pinning ────────────────────────────────────────────────

    /// Page size for PageUp/PageDown: viewport rows with a 2-row context
    /// overlap (10-line fallback before the first render).
    fn page(&self) -> usize {
        let (_, h) = self.viewport.get();
        if h > 2 {
            (h - 2) as usize
        } else {
            10
        }
    }

    /// Maximum scroll offset (content lines minus one screen) under the
    /// last-rendered viewport. `0` before the first render.
    fn max_scroll(&self, ctx: &AppCtx) -> usize {
        let (w, h) = self.viewport.get();
        if w == 0 || h == 0 {
            return 0;
        }
        let total = self
            .layout_lines(w as usize, &ctx.theme, ctx.run.spinner_frame)
            .len();
        total.saturating_sub(h as usize)
    }

    /// The scroll offset currently in effect (bottom offset while
    /// following; the stored offset clamped into range while pinned).
    fn current_scroll(&self, ctx: &AppCtx) -> usize {
        let max = self.max_scroll(ctx);
        if self.follow {
            max
        } else {
            (self.scroll as usize).min(max)
        }
    }

    // ── Layout (single source of lines for BOTH render and scroll math) ─

    /// Build every display line (committed entries + streaming area),
    /// wrapped to `width` columns. Called once per render and once per
    /// scroll action — the scroll math therefore always matches what is
    /// on screen. Time-dependent rows (running-tool elapsed, the live
    /// stats row) read the wall clock at frame time.
    fn layout_lines(
        &self,
        width: usize,
        theme: &Theme,
        spinner_frame: usize,
    ) -> Vec<Line<'static>> {
        // `spinner_frame` rides the signature for call-site stability;
        // restyle-1 moved the spinner to the status line (tool rows no
        // longer animate), so it forwards unused.
        let _ = spinner_frame;
        self.layout_lines_at(width, theme, Instant::now())
    }

    /// [`Self::layout_lines`] with an injectable clock — the test
    /// seam for the time-derived rows (fix-19's live rate needs
    /// deterministic instants; production always passes `now`).
    fn layout_lines_at(&self, width: usize, theme: &Theme, now: Instant) -> Vec<Line<'static>> {
        // fix-23/fix-25: fresh hit-test records every pass — `render`
        // converts THIS pass's records into terminal rects, and
        // scroll actions re-run the layout between frames (without
        // the clear, records would accumulate into duplicates).
        // select-1: same lifecycle for the per-row block attribution.
        self.reasoning_rows.borrow_mut().clear();
        self.streaming_reasoning_rows.set(None);
        self.tool_rows.borrow_mut().clear();
        self.line_blocks.borrow_mut().clear();
        let width = width.max(1);
        let mut out: Vec<Line<'static>> = Vec::new();

        let is_empty = self.is_blank();
        if is_empty {
            // The flow-path empty state (splash-1): the legacy
            // wordmark + hint. On roomy areas `render` paints the
            // centered ASCII splash INSTEAD (see `render_splash`) —
            // this branch stays byte-identical for the narrow/short
            // fallback (and any layout_lines consumer).
            let g = theme.icons.set();
            let mut spans = vec![Span::styled(format!("{} ", g.brand), theme.tool_running)];
            let tones = [
                theme.wordmark_highlight.fg,
                theme.tool_running.fg,
                theme.wordmark_shadow.fg,
            ];
            for (i, ch) in "OpenSlate".chars().enumerate() {
                // `tones` are `Option<Color>` (the theme slots' fg) —
                // `None` (never in practice) falls back to the brand.
                let tone = tones[(i / 2).min(tones.len() - 1)].unwrap_or_default();
                spans.push(Span::styled(ch.to_string(), Style::new().fg(tone)));
            }
            out.push(Line::from(spans));
            out.push(Line::from(Span::styled(
                // The em-dash is chrome — localize (identity outside
                // the ascii tier).
                localize(EMPTY_SESSION_HINT, &g),
                theme.muted,
            )));
            return out;
        }

        // Borderless spacing rules: block entries (user / assistant /
        // turn marker) open with one blank line of breathing room —
        // except the very first line of the transcript. Single-line
        // entries (tools, delegation, approval) never open a gap, so
        // runs of tool rows stay compact.
        let gap_before_block = |out: &mut Vec<Line<'static>>| {
            if !out.is_empty() {
                out.push(Line::from(""));
            }
        };

        // Turn-marker position (entries-index it renders before;
        // recorded by merge_turn/rebuild at the finished turn's
        // tail) — `None` (or a stale index past the entries) falls
        // back to the trailing render below.
        let marker_at = self.marker_at.filter(|_| self.turn_meta.is_some());
        let mut emitted_marker = false;
        for (index, entry) in self.entries.iter().enumerate() {
            // select-1: attribute every row this iteration emits
            // (the marker's block gap, if any, plus the entry's own
            // rows) to this entry's selection block.
            let block_start = out.len();
            // The marker of the LAST finished turn renders at its
            // recorded position (merge_turn/rebuild) — above the
            // next turn's entries, never below them.
            if !emitted_marker && marker_at == Some(index) {
                emitted_marker = true;
                if let Some(meta) = &self.turn_meta {
                    gap_before_block(&mut out);
                    out.push(turn_marker_line(meta, theme));
                }
            }
            match entry {
                TranscriptEntry::User(text) => {
                    gap_before_block(&mut out);
                    // restyle-1: full-width #262626 background band with
                    // a BOLD signal `› ` anchor (minimax user message).
                    push_user_band(&mut out, text, theme, width);
                }
                TranscriptEntry::Assistant(text) => {
                    gap_before_block(&mut out);
                    // No bar, no label — default foreground, indented 2
                    // so the body aligns with the user block's text
                    // column. Committed assistant text renders through
                    // the lightweight markdown renderer ([`crate::md`])
                    // — the SAME pipeline the live streaming area uses
                    // (fix-18), so the boundary flush is seamless:
                    // every soft line still wraps (word boundaries, CJK
                    // per character) except fenced-code lines, which
                    // stay verbatim and clip.
                    push_markdown_block(&mut out, text, theme, width, None, true);
                }
                TranscriptEntry::Reasoning(text) => {
                    // fix-23: collapsed to one dim summary row by
                    // default; the full `┆` gutter block only while
                    // expanded (click-toggled via
                    // `reasoning_hit_rects`). The block's layout span
                    // records for the click hit-test — collapsed
                    // covers its single row, expanded the whole block.
                    let start = out.len();
                    if self.expanded_reasoning.contains(&index) {
                        push_reasoning_block(&mut out, text, theme, width);
                    } else {
                        out.push(collapsed_reasoning_line(text, width, theme));
                    }
                    self.reasoning_rows
                        .borrow_mut()
                        .push((index, start, out.len() - start));
                }
                TranscriptEntry::ToolCall {
                    name,
                    args,
                    status,
                    detail,
                    ..
                } => {
                    let running_for = if matches!(status, ToolEntryStatus::Running) {
                        self.running_since
                            .get(&index)
                            .map(|t| now.duration_since(*t))
                    } else {
                        None
                    };
                    // restyle-1: `├` while another execution row follows
                    // (tool/delegation run continues), `└` to close it.
                    let connected = matches!(
                        self.entries.get(index + 1),
                        Some(TranscriptEntry::ToolCall { .. })
                            | Some(TranscriptEntry::Delegate { .. })
                    );
                    // fix-25: the row's layout span records for the
                    // click hit-test — collapsed covers the head row,
                    // expanded the head + detail block.
                    let start = out.len();
                    push_tool_entry(
                        &mut out,
                        name,
                        args,
                        status,
                        running_for,
                        detail.output.as_ref().map(|t| t.lines().count()),
                        theme,
                        width,
                        connected,
                    );
                    if self.expanded_tools.contains(&index) {
                        push_tool_detail(
                            &mut out,
                            name,
                            detail,
                            matches!(status, ToolEntryStatus::Running),
                            theme,
                            width,
                            connected,
                        );
                    }
                    self.tool_rows
                        .borrow_mut()
                        .push((index, start, out.len() - start));
                }
                TranscriptEntry::Approval {
                    tool_name,
                    decision,
                } => push_approval_line(&mut out, tool_name, decision, theme, width),
                TranscriptEntry::Delegate { agent, done } => {
                    let g = theme.icons.set();
                    let (marker, mstyle, label) = if *done {
                        (g.check, theme.tool_success, "Done")
                    } else {
                        (g.delegate, theme.delegate, "Running")
                    };
                    out.push(Line::from(vec![
                        Span::styled(format!("{marker} "), mstyle),
                        Span::styled(agent.clone(), theme.header),
                        Span::styled(format!(" {label}"), theme.muted),
                    ]));
                }
                // Step separator: one blank line (the borderless spec
                // retired the full-width `─` rule).
                TranscriptEntry::StepBreak => out.push(Line::from("")),
                // Per-request telemetry (dim, indented 2 — single-line
                // entry: no block gap).
                TranscriptEntry::Meta(text) => {
                    // Stored fix-16 lines carry unicode chrome (↑↓·…) —
                    // localize for the ascii tier.
                    let g = theme.icons.set();
                    out.push(Line::from(Span::styled(
                        format!("  {}", localize(text, &g)),
                        theme.muted,
                    )))
                }
            }
            // select-1: the rows this iteration emitted (marker gap +
            // entry rows) become the entry's selection block span.
            for _ in block_start..out.len() {
                self.line_blocks
                    .borrow_mut()
                    .push(SelectionBlock::Entry(index));
            }
        }
        // End-of-turn marker, trailing fallback: renders when no
        // in-loop position emitted it yet (fresh transcript, or
        // set_turn_meta without merge_turn/rebuild). restyle-1:
        // `└ model · Ns · ↑in ↓out · ⚡ tok/s` — muted throughout, the
        // ⚡ span signal (see [`turn_marker_line`]).
        // select-1: everything below the committed entries (this
        // fallback marker + the live streaming blocks) is ONE tail
        // block.
        let tail_start = out.len();
        if !emitted_marker {
            if let Some(meta) = &self.turn_meta {
                gap_before_block(&mut out);
                out.push(turn_marker_line(meta, theme));
            }
        }
        // Live streaming blocks render through an EDGE-TRIMMED view of
        // the buffers ([`trim_edge_blank_lines`]): leading/trailing
        // blank lines the model emitted (`\n\n` openings/closings)
        // are suppressed so no runs of blank `┆`/empty rows appear —
        // the buffers themselves keep accumulating verbatim. Interior
        // blank lines (paragraph separators) survive.
        let reasoning_view = trim_edge_blank_lines(&self.streaming_reasoning);
        if !reasoning_view.trim().is_empty() {
            // fix-23: the live reasoning block collapses to a
            // summary row that re-renders with every delta (always
            // the current last line) unless click-expanded — the
            // streaming block's layout span records like a committed
            // entry's.
            let start = out.len();
            if self.streaming_reasoning_expanded {
                push_reasoning_block(&mut out, &reasoning_view, theme, width);
            } else {
                out.push(collapsed_reasoning_line(&reasoning_view, width, theme));
            }
            self.streaming_reasoning_rows
                .set(Some((start, out.len() - start)));
        }
        let answer_view = trim_edge_blank_lines(&self.streaming);
        if !answer_view.trim().is_empty() {
            // Live answer (fix-18): renders through the SAME markdown
            // pipeline as the committed blocks — including the block
            // gap — so a boundary flush never re-styles or re-flows
            // anything the user already read. The `▍` full-height
            // gutter is retired; the live signal is the status-bar
            // spinner plus a `▍` tail cursor on the last row (see
            // [`push_markdown_block`]). Unclosed markers render
            // literally per the parser's existing semantics and flip
            // to their styled form as the closing syntax arrives.
            gap_before_block(&mut out);
            // fix-19: the live stats row rides the answer area's
            // head (below the reasoning block, above the markdown
            // rendering) — `ttft S.Ss · ~Rtok/s`, recomputed every
            // frame. It is live-only chrome: at `RequestEnd` it dies
            // and the HELD exact line takes over (below the step's
            // tool rows). Reasoning-only streaming shows no row.
            if let Some(stats) = self.live_stats_line(&answer_view, now) {
                // The `·` separator is chrome — localize (identity
                // outside the ascii tier), like the committed Meta
                // rows above.
                let g = theme.icons.set();
                out.push(Line::from(Span::styled(
                    format!("  {}", localize(&stats, &g)),
                    theme.muted,
                )));
            }
            push_markdown_block(
                &mut out,
                &answer_view,
                theme,
                width,
                Some(theme.tool_running),
                true,
            );
        }
        // select-1: attribute the tail rows (trailing marker fallback
        // + streaming blocks) to the streaming selection block.
        for _ in tail_start..out.len() {
            self.line_blocks
                .borrow_mut()
                .push(SelectionBlock::Streaming);
        }
        out
    }

    /// The live streaming stats row (fix-19): `ttft S.Ss · ~Rtok/s`,
    /// recomputed every frame. `ttft` is the App-fed precise value
    /// frozen at `FirstToken` ([`Self::set_live_ttft`]); the rate is
    /// the existing chars/2.2 estimate ([`estimate_tokens`]) over the
    /// trimmed answer view divided by the time since the first answer
    /// delta ([`Self::answer_started`]). The rate segment waits out
    /// [`LIVE_RATE_FLOOR_SECS`] (a microseconds-old block would show
    /// an absurd five-digit rate); `None` when nothing can render yet
    /// (no first delta, or nothing measurable) — pure ASCII, rendered
    /// muted like the meta rows.
    fn live_stats_line(&self, answer_view: &str, now: Instant) -> Option<String> {
        let started = self.answer_started?;
        let mut segments: Vec<String> = Vec::new();
        if let Some(tokens) = self.live_input_est {
            segments.push(format!("↑~{tokens}"));
        }
        if let Some(ttft) = self.live_ttft {
            segments.push(format!("ttft {}", format_secs(ttft.as_secs_f64())));
        }
        let secs = now
            .checked_duration_since(started)
            .unwrap_or_default()
            .as_secs_f64();
        if secs >= LIVE_RATE_FLOOR_SECS {
            let est = estimate_tokens(answer_view.chars().count());
            segments.push(format!("~{}tok/s", (est as f64 / secs).round() as u32));
        }
        if segments.is_empty() {
            None
        } else {
            Some(segments.join(" · "))
        }
    }

    // ── select-1: drag-to-select ───────────────────────────────────

    /// Left-button press at terminal `(col, row)` (select-1): clear
    /// any previous selection and record the anchor. No-op before
    /// the first render or on an empty transcript (no geometry to
    /// map against) — the following release then behaves as a plain
    /// click.
    pub fn selection_begin(&mut self, col: u16, row: u16) {
        self.selection = match self.map_screen_to_logical(col, row) {
            Some(anchor) => SelectionState::Pending {
                anchor,
                down: (col, row),
            },
            None => SelectionState::Normal,
        };
    }

    /// Pointer motion while pressed (select-1). The FIRST ≥1-cell
    /// displacement promotes a `Pending` press into a live selection
    /// (click suppression locks in for the gesture); the cursor then
    /// tracks every event. Returns whether a selection is active.
    pub fn selection_drag(&mut self, col: u16, row: u16) -> bool {
        let mapped = self.map_screen_to_logical(col, row);
        let prior = std::mem::take(&mut self.selection);
        self.selection = match (prior, mapped) {
            // Threshold crossed (a different cell than the press):
            // the press becomes a live selection with the ORIGINAL
            // anchor.
            (SelectionState::Pending { anchor, down }, Some(cursor)) if down != (col, row) => {
                SelectionState::Selecting { anchor, cursor }
            }
            // Mid-drag cursor update; a mapping blip (no layout —
            // practically unreachable between renders) keeps the
            // last cursor rather than killing the gesture.
            (SelectionState::Selecting { anchor, cursor }, next) => SelectionState::Selecting {
                anchor,
                cursor: next.unwrap_or(cursor),
            },
            // Same-cell motion, no gesture in flight, or a press
            // that never mapped: unchanged.
            (other, _) => other,
        };
        matches!(self.selection, SelectionState::Selecting { .. })
    }

    /// Left-button release (select-1): a press that never dragged
    /// reports [`SelectionEnd::Click`] (the caller dispatches the
    /// legacy positional click); a completed selection extracts the
    /// covered text and persists the highlight as `Selected`.
    pub fn selection_end(&mut self, theme: &Theme) -> SelectionEnd {
        let prior = std::mem::take(&mut self.selection);
        match prior {
            SelectionState::Selecting { anchor, cursor } => {
                let text = self.extract_selection_text(theme, &anchor, &cursor);
                self.selection = SelectionState::Selected { anchor, cursor };
                SelectionEnd::Selected(text)
            }
            // A pending release never dragged → the click path (the
            // `take` already reset the gesture state).
            SelectionState::Pending { .. } | SelectionState::Normal => SelectionEnd::Click,
            // A release with no gesture of ours preceding it (the
            // press was swallowed by a modal): keep any persisted
            // highlight, still behave as a click.
            selected @ SelectionState::Selected { .. } => {
                self.selection = selected;
                SelectionEnd::Click
            }
        }
    }

    /// Drop any selection (select-1 clear trigger: Esc, a new press,
    /// rebuild/merge//new, an overlay opening). No-op in `Normal`.
    pub fn clear_selection(&mut self) {
        self.selection = SelectionState::Normal;
    }

    /// Map a terminal `(col, row)` into logical space against the
    /// LAST RENDER's geometry (`sel_area` + `sel_scroll` + the
    /// `line_blocks` attribution that render recorded). Rows outside
    /// the viewport clamp into it (dragging off the edges selects
    /// the edge rows — no auto-scroll, that is P2); columns clamp to
    /// the area. `None` when nothing renders.
    fn map_screen_to_logical(&self, col: u16, row: u16) -> Option<LogicalPos> {
        let area = self.sel_area.get()?;
        let blocks = self.line_blocks.borrow();
        if blocks.is_empty() || area.width == 0 || area.height == 0 {
            return None;
        }
        let vy = row.saturating_sub(area.y).min(area.height - 1) as usize;
        let layout_row = (vy + self.sel_scroll.get()).min(blocks.len() - 1);
        let block = blocks[layout_row];
        // Block occurrences are contiguous and unique per pass — the
        // FIRST occurrence is the block's start row.
        let start = blocks.iter().position(|b| *b == block)?;
        let col_in_line = (col.saturating_sub(area.x)).min(area.width - 1) as usize;
        Some(LogicalPos {
            block,
            row: layout_row - start,
            col: col_in_line,
        })
    }

    /// Map a logical position back to a `(layout row, column)` pair
    /// against the CURRENT `line_blocks` attribution — the per-frame
    /// deterministic logical→screen direction. `row` clamps into the
    /// block's current span (a resize re-wrap degrades to the
    /// nearest row); `None` when the block no longer renders.
    fn map_logical_to_layout(&self, pos: &LogicalPos) -> Option<(usize, usize)> {
        let blocks = self.line_blocks.borrow();
        let start = blocks.iter().position(|b| *b == pos.block)?;
        let span = blocks.iter().filter(|b| **b == pos.block).count();
        Some((start + pos.row.min(span - 1), pos.col))
    }

    /// The selection's plain text (select-1): the covered rows'
    /// concatenated span text — boundary rows char-sliced by display
    /// column ([`slice_by_cols`], wide chars whole), middle rows in
    /// full — joined with `\n`. Runs one fresh layout pass (the
    /// same deterministic pipeline the renderer uses); byte-capping
    /// to the 32 KiB copy budget is the App's copy chain's job.
    fn extract_selection_text(
        &self,
        theme: &Theme,
        anchor: &LogicalPos,
        cursor: &LogicalPos,
    ) -> String {
        let (w, _) = self.viewport.get();
        if w == 0 {
            return String::new();
        }
        let lines = self.layout_lines(w as usize, theme, 0);
        let (Some((r1, c1)), Some((r2, c2))) = (
            self.map_logical_to_layout(anchor),
            self.map_logical_to_layout(cursor),
        ) else {
            return String::new();
        };
        let ((r1, c1), (r2, c2)) = if (r1, c1) <= (r2, c2) {
            ((r1, c1), (r2, c2))
        } else {
            ((r2, c2), (r1, c1))
        };
        let mut rows: Vec<String> = Vec::with_capacity(r2.saturating_sub(r1) + 1);
        for (i, line) in lines.iter().enumerate().skip(r1).take(r2 - r1 + 1) {
            let text: String = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            // [start, end) display-column window: the first row keeps
            // only its tail from the anchor column, the last row its
            // head to the cursor column (inclusive → +1), single-row
            // selections both.
            let (start, end) = if i == r1 && i == r2 {
                (c1, c2 + 1)
            } else if i == r1 {
                (c1, usize::MAX)
            } else if i == r2 {
                (0, c2 + 1)
            } else {
                (0, usize::MAX)
            };
            // Trailing whitespace trims per row (tmux copy-mode
            // convention): full-width band rows pad out to the area
            // width with spaces — that padding is OUR paint artifact,
            // never content the reader chose.
            rows.push(slice_by_cols(&text, start, end).trim_end().to_owned());
        }
        rows.join("\n")
    }

    /// The selection highlight overlay (select-1, post-render style
    /// pass): map the anchor/cursor through THIS frame's attribution,
    /// then paint the covered cells' background with
    /// `theme.selection_bg` — glyphs and their own styles stay
    /// untouched. The first/last rows take the column range between
    /// the boundary and the area edge, middle rows the full width;
    /// boundaries widen onto whole glyphs (a wide char's halves are
    /// never split — no half-character highlight).
    fn paint_selection(&self, f: &mut Frame, area: Rect, scroll: usize, theme: &Theme) {
        let Some((anchor, cursor)) = self.selection.range() else {
            return; // Normal/Pending — nothing on screen
        };
        let (Some((r1, c1)), Some((r2, c2))) = (
            self.map_logical_to_layout(&anchor),
            self.map_logical_to_layout(&cursor),
        ) else {
            return;
        };
        let ((r1, c1), (r2, c2)) = if (r1, c1) <= (r2, c2) {
            ((r1, c1), (r2, c2))
        } else {
            ((r2, c2), (r1, c1))
        };
        let Some(bg) = theme.selection_bg.bg else {
            return;
        };
        if area.width == 0 || area.height == 0 {
            return;
        }
        let buf = f.buffer_mut();
        let right = area.x + area.width - 1;
        let visible_end = scroll.saturating_add(area.height as usize);
        for layout_row in r1..=r2 {
            if layout_row < scroll || layout_row >= visible_end {
                continue; // scrolled out of the viewport — not painted
            }
            let y = area.y + (layout_row - scroll) as u16;
            let clamp = |c: usize| {
                area.x
                    .saturating_add(c.min(u16::MAX as usize) as u16)
                    .min(right)
            };
            let mut x1 = if layout_row == r1 { clamp(c1) } else { area.x };
            let mut x2 = if layout_row == r2 { clamp(c2) } else { right };
            // Whole-glyph alignment: ratatui 0.30 renders a wide
            // glyph as a lead cell (symbol width ≥2) followed by
            // RESET trailing cells — a boundary landing on a
            // trailing cell pulls the lead cell in, a boundary on a
            // lead cell pulls its trailing cell in (a wide char's
            // halves are never split — no half-character highlight).
            let wide_lead = |x: u16| {
                buf.cell((x, y))
                    .is_some_and(|cell| str_width(cell.symbol()) >= 2)
            };
            if x1 > area.x && wide_lead(x1 - 1) {
                x1 -= 1;
            }
            if x2 < right && wide_lead(x2) {
                x2 += 1;
            }
            for x in x1..=x2 {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_bg(bg);
                }
            }
        }
    }
}

/// Map one message to transcript entries (rebuild path, non-Tool roles).
/// Assistant messages with `tool_calls` emit ToolCall entries in the
/// Running state — the following Tool-role message folds each one to its
/// final status ([`fold_tool_outcome`]).
fn entry_from_message(msg: &Message) -> Vec<TranscriptEntry> {
    let mut out = Vec::new();
    match msg.role {
        MessageRole::User => out.push(TranscriptEntry::User(msg.content.clone())),
        MessageRole::Assistant => {
            if let Some(tool_calls) = &msg.tool_calls {
                for tc in tool_calls {
                    let args_text = tc.arguments.to_string();
                    out.push(TranscriptEntry::ToolCall {
                        name: tc.name.clone(),
                        args: args_preview(&args_text),
                        status: ToolEntryStatus::Running,
                        call_id: Some(tc.id.0.clone()),
                        // fix-25: full args retained (rebuilt from the
                        // message); output fills when the Tool-role
                        // result folds in below.
                        detail: ToolEntryDetail {
                            args: cap_stored_text(&args_text, ARGS_STORE_CAP),
                            output: None,
                        },
                    });
                }
            }
            if !msg.content.trim().is_empty() {
                out.push(TranscriptEntry::Assistant(msg.content.clone()));
            }
        }
        MessageRole::Tool | MessageRole::System => {}
    }
    out
}

/// A4 (locked): fold one Tool-role result message back into its matching
/// open ToolCall entry (matched by `tool_call_id`, falling back to tool
/// name for providers that drop ids). Success refines bytes/truncated;
/// content carrying one of core's failure markers flips the entry to
/// Failed. Orphan results (no matching call) are dropped silently.
fn fold_tool_outcome(entries: &mut [TranscriptEntry], msg: &Message) {
    let Some(index) = find_tool_entry(entries, msg, MatchDone::No) else {
        return;
    };
    let status = tool_outcome_from_message(msg);
    if let TranscriptEntry::ToolCall {
        status: s, detail, ..
    } = &mut entries[index]
    {
        *s = status;
        // fix-25: the fold is the one path that sees the FULL output
        // text — retain it (capped) for the expanded view.
        detail.output = Some(cap_stored_text(&msg.content, OUTPUT_STORE_CAP));
    }
}

/// Whether [`find_tool_entry`] may match live-completed (Done) entries
/// in addition to Running ones.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MatchDone {
    /// Rebuild-path semantics: only Running entries fold (they were
    /// just derived from the message list).
    No,
    /// Merge-path semantics: Done entries refine too — the live
    /// `ToolEnd` cannot detect failure (A4), so the message content
    /// is the authority for failure markers and bytes.
    Yes,
}

/// Locate the ToolCall entry a Tool-role result message folds into:
/// scanning for the LATEST match (backwards), by `tool_call_id` with
/// a tool-name fallback for providers that drop ids. Orphan results
/// (no matching call) return `None`.
fn find_tool_entry(
    entries: &[TranscriptEntry],
    msg: &Message,
    match_done: MatchDone,
) -> Option<usize> {
    let status_ok = |e: &TranscriptEntry| {
        matches!(
            e,
            TranscriptEntry::ToolCall { status, .. }
                if *status == ToolEntryStatus::Running
                    || (match_done == MatchDone::Yes
                        && matches!(status, ToolEntryStatus::Done { .. }))
        )
    };
    let by_id = msg.tool_call_id.as_ref().and_then(|id| {
        entries.iter().rposition(|e| {
            status_ok(e)
                && matches!(e, TranscriptEntry::ToolCall { call_id: Some(cid), .. } if cid == &id.0)
        })
    });
    by_id.or_else(|| {
        msg.name.as_ref().and_then(|name| {
            entries.iter().rposition(|e| {
                status_ok(e) && matches!(e, TranscriptEntry::ToolCall { name: n, .. } if n == name)
            })
        })
    })
}

/// The final status a Tool-role result message implies for its call:
/// failure markers → Failed (the A4 heuristics), else Done with
/// content-derived bytes/truncated (no live timing — `elapsed_ms`
/// only exists on the live-completion path).
fn tool_outcome_from_message(msg: &Message) -> ToolEntryStatus {
    if let Some(summary) = tool_failure_summary(&msg.content) {
        ToolEntryStatus::Failed { summary }
    } else {
        ToolEntryStatus::Done {
            bytes: msg.content.len(),
            truncated: msg.content.contains("[TRUNCATED:"),
            elapsed_ms: None,
        }
    }
}

/// Merge-path tool-result folding (the `TurnDone(Ok)`
/// [`TranscriptComponent::merge_turn`] step): Tool-role messages, in
/// message order, fold into the matching ToolCall entries —
/// REFINING live-completed entries too (the live `ToolEnd` cannot
/// detect failure, A4 — the message content is the authority) while a
/// live-measured `elapsed_ms` survives a success refine.
///
/// Matching: by `tool_call_id` where an entry carries one (live
/// entries never do — core's `on_tool_start` passes no id), else by
/// tool name with a FIFO cursor per name: the Nth result of a name
/// pairs with the Nth entry of that name, because root-level tool
/// calls START (`ToolStart` events → entries) and COMPLETE (Tool
/// messages, in message order) in the same order. Orphan results are
/// dropped with a debug log.
fn fold_tool_outcomes_merge(entries: &mut [TranscriptEntry], messages: &[Message]) {
    // Resolve every fold target BEFORE mutating (the plan indices are
    // unique — an entry is consumed at most once).
    let mut plan: Vec<(usize, ToolEntryStatus, String)> = Vec::new();
    let mut consumed: HashSet<usize> = HashSet::new();
    let mut cursors: HashMap<&str, usize> = HashMap::new();

    for msg in messages.iter().filter(|m| m.role == MessageRole::Tool) {
        let target = msg
            .tool_call_id
            .as_ref()
            .and_then(|id| {
                entries.iter().rposition(|e| {
                    matches!(e, TranscriptEntry::ToolCall { call_id: Some(cid), .. } if cid == &id.0)
                })
            })
            .filter(|&i| !consumed.contains(&i) && foldable(&entries[i]))
            .or_else(|| {
                let name = msg.name.as_deref()?;
                let start = cursors.get(name).copied().unwrap_or(0);
                let found = entries
                    .iter()
                    .enumerate()
                    .skip(start)
                    .find(|&(i, e)| {
                        !consumed.contains(&i)
                            && foldable(e)
                            && matches!(e, TranscriptEntry::ToolCall { name: n, .. } if n == name)
                    })
                    .map(|(i, _)| i);
                if let Some(i) = found {
                    cursors.insert(name, i + 1);
                }
                found
            });
        let Some(index) = target else {
            tracing::debug!(tool = ?msg.name, "tool result matched no transcript entry");
            continue;
        };
        consumed.insert(index);
        // Preserve a live-measured elapsed across a success refine.
        let status = match (&entries[index], tool_outcome_from_message(msg)) {
            (
                TranscriptEntry::ToolCall {
                    status:
                        ToolEntryStatus::Done {
                            elapsed_ms: Some(ms),
                            ..
                        },
                    ..
                },
                ToolEntryStatus::Done {
                    bytes, truncated, ..
                },
            ) => ToolEntryStatus::Done {
                bytes,
                truncated,
                elapsed_ms: Some(*ms),
            },
            (_, outcome) => outcome,
        };
        plan.push((
            index,
            status,
            cap_stored_text(&msg.content, OUTPUT_STORE_CAP),
        ));
    }

    for (index, status, output) in plan {
        if let TranscriptEntry::ToolCall {
            status: s, detail, ..
        } = &mut entries[index]
        {
            *s = status;
            // fix-25: the merge fold is where a LIVE-path entry (whose
            // ToolEnd carried bytes only) finally receives its full
            // output text.
            detail.output = Some(output);
        }
    }
}

/// Whether an entry may still receive a merge-path fold (Running or
/// live-completed Done; already-Failed entries are final).
fn foldable(e: &TranscriptEntry) -> bool {
    matches!(
        e,
        TranscriptEntry::ToolCall { status, .. }
            if *status == ToolEntryStatus::Running
                || matches!(status, ToolEntryStatus::Done { .. })
    )
}

/// Failure markers core writes into failed Tool-role results:
/// * `Error: …` — tool execution errors (core tool.rs `execute` wrapper);
/// * `approval denied tool '…'` — errors-as-data denials (runner.rs);
/// * `child agent call denied: …` — denied call_agent dispatch;
/// * `[tool call cancelled before completion]` — cancellation synthetics;
/// * `[error] …` — PTC run_code script failures.
///
/// Anchored at the start so legitimate outputs that merely CONTAIN the
/// word "error" are not misread.
fn tool_failure_summary(content: &str) -> Option<String> {
    const MARKERS: [&str; 5] = [
        "Error:",
        "approval denied tool",
        "child agent call denied",
        "[tool call cancelled",
        "[error]",
    ];
    let trimmed = content.trim_start();
    if MARKERS.iter().any(|m| trimmed.starts_with(m)) {
        Some(first_line_summary(trimmed))
    } else {
        None
    }
}

/// First line of an error, clamped to ≤60 display columns (width-aware,
/// CJK-safe ellipsis).
fn first_line_summary(s: &str) -> String {
    const MAX: usize = 60;
    let first = s.lines().next().unwrap_or_default();
    if str_width(first) <= MAX {
        return first.to_owned();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in first.chars() {
        let cw = char_width(ch);
        if used + cw > MAX - 1 {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out.push('…');
    out
}

/// The COLLAPSED reasoning summary (fix-23, rule reworked in fix-24):
/// the whole block FLATTENED to one line — every line trimmed, blank
/// lines dropped, joined with a SINGLE space (in-line spaces survive,
/// so English reads naturally; newlines become spaces) — then cut to
/// `avail` DISPLAY columns from the TAIL: the row always shows the
/// latest end of the thinking, and a leading `…` marks that earlier
/// text was dropped (the ellipsis sits on the truncation side). The
/// cut walks characters accumulating unicode widths — char-boundary
/// safety is NOT width safety (CJK wide chars, the status.rs
/// lesson). An all-blank block summarizes to the empty string (such
/// blocks never commit; the live view skips them entirely).
fn reasoning_summary(text: &str, avail: usize, ellipsis: &str) -> String {
    let flat: String = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if str_width(&flat) <= avail {
        return flat;
    }
    // Tail cut: keep the END (the latest thinking), reserving one
    // column for the leading ellipsis. A whitespace run landing at
    // the cut is trimmed — only whitespace is ever dropped, never
    // content.
    let budget = avail.saturating_sub(1);
    let mut kept: Vec<char> = Vec::new();
    let mut used = 0usize;
    for ch in flat.chars().rev() {
        let cw = char_width(ch);
        if used + cw > budget {
            break;
        }
        kept.push(ch);
        used += cw;
    }
    kept.reverse(); // the walk collected tail-first
    let tail: String = kept.into_iter().collect();
    format!("{ellipsis}{}", tail.trim_start())
}

/// Args preview clamped to ~40 columns (UTF-8 boundary safe).
fn args_preview(args: &str) -> String {
    let cleaned = args.trim().trim_matches('"');
    const MAX: usize = 40;
    if cleaned.len() <= MAX {
        cleaned.to_owned()
    } else {
        let mut end = MAX.saturating_sub(1);
        while end > 0 && !cleaned.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &cleaned[..end])
    }
}

// ── Line assembly helpers ──────────────────────────────────────────────

/// Display width of a string (unicode-width aware via ratatui).
fn str_width(s: &str) -> usize {
    Span::from(s).width()
}

/// Trim leading/trailing BLANK lines (whitespace-only), keeping
/// interior blank lines intact. Model deltas routinely open/close
/// with `\n\n`; the streaming buffers accumulate them verbatim (the
/// buffer itself is never mutated — trimming is a RENDER/COMMIT-time
/// view), so without this the streaming areas would show runs of
/// blank `┆`/empty rows at the block's edges.
fn trim_edge_blank_lines(s: &str) -> String {
    let mut lines: Vec<&str> = s.split('\n').collect();
    while lines.first().is_some_and(|l| l.trim().is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// Display width of one character (unicode-width aware via ratatui).
/// Printable ASCII fast path — see the md twin ([`crate::md`]):
/// `Span::width()` allocates per call and the wrap loops call this
/// per character (the streaming renderer re-parses every frame).
fn char_width(ch: char) -> usize {
    if ('\x20'..='\x7e').contains(&ch) {
        1
    } else {
        Span::from(ch.to_string()).width()
    }
}

/// Flush the pending word into the current row, wrapping (and
/// char-splitting over-long words) as needed. `pending` holds the run
/// of spaces seen since the last word: materialized only when the word
/// actually joins this row, dropped at a wrap or line start (so no
/// trailing/leading spaces survive a break). Break points honor CJK
/// kinsoku (禁則, via [`crate::md::is_no_line_start`] /
/// [`crate::md::is_no_line_end`]): a word opening with closing
/// punctuation pulls the preceding WIDE char down with it (never a
/// narrow one — that would split latin words), and a row ending in an
/// opening bracket pushes the opener down. Nested fn instead of a
/// closure so the caller keeps direct access to the row state.
fn flush_word(
    width: usize,
    rows: &mut Vec<String>,
    row: &mut String,
    row_w: &mut usize,
    word: &mut String,
    word_w: &mut usize,
    pending: &mut usize,
) {
    if word.is_empty() {
        return;
    }
    if *row_w > 0 {
        if *row_w + *pending + *word_w > width {
            let last = row.chars().last();
            let pull_down = word.chars().next().is_some_and(crate::md::is_no_line_start)
                && last.is_some_and(|c| char_width(c) >= 2);
            let opener_down = last.is_some_and(crate::md::is_no_line_end);
            if (pull_down || opener_down) && row.chars().count() > 1 {
                // Kinsoku: break one char earlier so the punctuation
                // stays glued to text.
                let moved = row.pop().unwrap();
                rows.push(std::mem::take(row));
                row.push(moved);
                *row_w = char_width(moved);
            } else {
                rows.push(std::mem::take(row));
                *row_w = 0;
            }
        } else {
            for _ in 0..*pending {
                row.push(' ');
            }
            *row_w += *pending;
        }
    }
    *pending = 0;
    if *word_w > width {
        // A single word longer than the line: hard-split by characters.
        // (Kinsoku deliberately does not apply inside the hard split —
        // that path only runs for words longer than the whole line.)
        for ch in word.drain(..) {
            let cw = char_width(ch);
            if *row_w > 0 && *row_w + cw > width {
                rows.push(std::mem::take(row));
                *row_w = 0;
            }
            row.push(ch);
            *row_w += cw;
        }
    } else if *row_w + *word_w <= width {
        row.push_str(word);
        *row_w += *word_w;
    } else {
        // The kinsoku-moved prefix leaves no room for the word — wrap
        // the word whole onto the next row.
        rows.push(std::mem::take(row));
        row.push_str(word);
        *row_w = *word_w;
    }
    word.clear();
    *word_w = 0;
}

/// Greedy word-wrap of ONE logical line (no `\n` inside) to `width`
/// display columns.
///
/// * space-delimited words stay intact when they fit (word boundaries
///   preserved — latin words never split mid-word);
/// * a single word longer than `width` hard-splits by characters;
/// * wide (CJK) characters are breakable units of their own, so CJK
///   text wraps per character;
/// * spaces at a wrap point (and at line starts/ends) are dropped;
/// * CJK kinsoku (禁則) holds at break points: closing punctuation
///   (`，` `。` …) never opens a row and openers (`（` `「` …) never
///   close one ([`crate::md::is_no_line_start`] /
///   [`crate::md::is_no_line_end`]);
/// * 孤字行防护: a lone wide char on the final row borrows one char
///   from the row above (wide chars only — latin words stay whole).
///
/// Always returns at least one row (`""` for empty input).
fn wrap_to_width(s: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows: Vec<String> = Vec::new();
    let mut row = String::new();
    let mut row_w = 0usize;
    let mut word = String::new();
    let mut word_w = 0usize;
    let mut pending = 0usize;
    let flush = |rows: &mut Vec<String>,
                 row: &mut String,
                 row_w: &mut usize,
                 word: &mut String,
                 word_w: &mut usize,
                 pending: &mut usize| {
        flush_word(width, rows, row, row_w, word, word_w, pending);
    };

    for ch in s.chars() {
        let cw = char_width(ch);
        if ch == ' ' {
            flush(
                &mut rows,
                &mut row,
                &mut row_w,
                &mut word,
                &mut word_w,
                &mut pending,
            );
            pending += 1;
        } else if cw >= 2 {
            // Wide (CJK) character: its own breakable unit — flush the
            // pending word first, then place this char immediately.
            flush(
                &mut rows,
                &mut row,
                &mut row_w,
                &mut word,
                &mut word_w,
                &mut pending,
            );
            word.push(ch);
            word_w += cw;
            flush(
                &mut rows,
                &mut row,
                &mut row_w,
                &mut word,
                &mut word_w,
                &mut pending,
            );
        } else {
            word.push(ch);
            word_w += cw;
        }
    }
    flush(
        &mut rows,
        &mut row,
        &mut row_w,
        &mut word,
        &mut word_w,
        &mut pending,
    );
    // 孤字行防护 (orphan guard): a lone WIDE char on the final row
    // borrows one char from the row above (wide chars only — never
    // splits a latin word, and never moves a 行首禁則 char down to
    // open the last row).
    if row.chars().count() == 1 && char_width(row.chars().next().unwrap()) >= 2 {
        let movable = |prev: &String| {
            prev.chars().count() > 1
                && prev
                    .chars()
                    .last()
                    .is_some_and(|c| char_width(c) >= 2 && !crate::md::is_no_line_start(c))
        };
        if let Some(prev) = rows.last_mut() {
            if movable(prev) {
                let ch = prev.pop().unwrap();
                row.insert(0, ch);
            }
        }
    }
    rows.push(row);
    rows
}

/// Markdown block: the assistant rendering path ([`crate::md`]) —
/// shared by the FINALIZED entries and the LIVE streaming area
/// (fix-18), which is what makes a boundary flush seamless: the same
/// text produces the same lines through this one function. Every
/// parsed soft line wraps to the available width (2-column indent)
/// with its inline styles intact; list and quote items HANG —
/// continuation rows align under the body via
/// [`crate::md::hang_width`]; fenced-code lines stay verbatim and CLIP
/// (no wrap); a horizontal rule expands to a dim line capped at
/// `min(width, 80)` (restyle-1); pipe tables render as aligned columns
/// ([`crate::md::table_lines`]) with the header row BOLD.
///
/// `anchor` (restyle-1) replaces the FIRST display row's indent with
/// the text-colored `● ` assistant anchor — both committed entries and
/// the live streaming block carry it.
///
/// `cursor` (streaming only) appends a `▍` tail-cursor span to the
/// block's LAST display row — the live-output signal now that the
/// full-height `▍` gutter is retired. It is skipped on fenced-code
/// rows (verbatim by contract), while an unterminated fence is still
/// open (the insertion point is inside the block, not on the row
/// above), and when the row has no spare column (a clipped cursor
/// would lie about where text lands). Being its own trailing span it
/// is style-safe — surrounding text styles are untouched.
fn push_markdown_block(
    out: &mut Vec<Line<'static>>,
    text: &str,
    theme: &Theme,
    width: usize,
    cursor: Option<Style>,
    anchor: bool,
) {
    const INDENT: &str = "  ";
    let block_start = out.len();
    let avail = width.saturating_sub(str_width(INDENT)).max(1);
    let g_icons = theme.icons.set();
    let styles = crate::md::MdStyles {
        text: theme.assistant,
        heading: theme.md_heading,
        code: theme.md_code,
        dim: theme.muted,
        link: theme.md_link,
        quote_prefix: g_icons.vertical,
        bullet: g_icons.bullet,
    };
    let mut last_was_code = false;
    for line in crate::md::parse(text, &styles) {
        match line {
            crate::md::MdLine::Flow(spans) => {
                last_was_code = false;
                // List/quote hanging indent: the `• `/`N. `/`> `
                // prefix pins row 0; wrapped rows align under the
                // BODY by indenting the prefix width.
                let hang = crate::md::hang_width(&spans, styles.quote_prefix, styles.bullet);
                let (prefix, body): (Option<Span<'static>>, Vec<Span<'static>>) = if hang > 0 {
                    let mut body = spans;
                    let prefix = body.remove(0);
                    (Some(prefix), body)
                } else {
                    (None, spans)
                };
                let body_avail = if hang > 0 {
                    avail.saturating_sub(hang).max(1)
                } else {
                    avail
                };
                for (i, row) in crate::md::wrap_spans(&body, body_avail)
                    .into_iter()
                    .enumerate()
                {
                    let mut line_spans = vec![Span::styled(INDENT.to_owned(), theme.assistant)];
                    match (&prefix, i) {
                        (Some(p), 0) => line_spans.push(p.clone()),
                        (Some(_), _) => {
                            line_spans.push(Span::raw(" ".repeat(hang)));
                        }
                        (None, _) => {}
                    }
                    line_spans.extend(row);
                    out.push(Line::from(line_spans));
                }
            }
            crate::md::MdLine::Code(s) => {
                last_was_code = true;
                out.push(Line::from(vec![
                    Span::styled(INDENT.to_owned(), theme.md_code),
                    Span::styled(crate::md::clip(&s, avail), theme.md_code),
                ]));
            }
            crate::md::MdLine::Hr => {
                last_was_code = false;
                // restyle-1: dim rule capped at min(width, 80); the
                // row splits into indent + rule spans so the assistant
                // anchor can replace the leading indent. theme-1: the
                // stroke comes from the icon tier.
                let rule_len = avail.min(80);
                let h = theme.icons.set().horizontal;
                out.push(Line::from(vec![
                    Span::styled(INDENT.to_owned(), theme.muted),
                    Span::styled(h.repeat(rule_len), theme.muted),
                ]));
            }
            crate::md::MdLine::Table(rows) => {
                last_was_code = false;
                for row in crate::md::table_lines(&rows, avail, &styles) {
                    let mut line_spans = vec![Span::styled(INDENT.to_owned(), theme.assistant)];
                    line_spans.extend(row);
                    out.push(Line::from(line_spans));
                }
            }
        }
    }
    if let Some(style) = cursor {
        // Fence parity: every ```-prefixed line toggles the fence
        // state (the parser's exact rule), so an odd count means the
        // buffer ends INSIDE an open fence.
        let ends_in_open_fence =
            text.lines().filter(|l| l.trim().starts_with("```")).count() % 2 == 1;
        if !last_was_code && !ends_in_open_fence {
            if let Some(last) = out.last_mut() {
                if last.width() < width {
                    last.spans
                        .push(Span::styled(theme.icons.set().cursor.to_owned(), style));
                }
            }
        }
    }
    // restyle-1: the assistant anchor — the FIRST display row's indent
    // span (always exactly `INDENT`, every row starts with it) becomes
    // the text-colored `● ` marker.
    if anchor && out.len() > block_start {
        let first = &mut out[block_start];
        if first.spans[0].content == INDENT {
            first.spans[0] =
                Span::styled(format!("{} ", theme.icons.set().anchor), theme.assistant);
        }
    }
}

/// Estimated tokens from a char count (~2.2 chars/token, mixed
/// CJK/latin heuristic — the API exposes no reasoning/answer split).
fn estimate_tokens(chars: usize) -> u32 {
    const CHARS_PER_TOKEN: f64 = 2.2;
    ((chars as f64) / CHARS_PER_TOKEN).round() as u32
}

/// ESTIMATED reasoning meta line: `~Ntok · ~Rtok/s`. The API exposes
/// no reasoning/answer token split, so N is estimated from the char
/// count at ~2.2 chars/token (mixed CJK/latin) and the `~` prefixes
/// mark it as an estimate. `rate` is omitted when no measurable time
/// elapsed (zero-division guard); the block's duration itself is not
/// shown — format convergence with the request usage line.
fn meta_reasoning_line(chars: usize, elapsed: Duration) -> String {
    let est = estimate_tokens(chars);
    let secs = elapsed.as_secs_f64();
    if secs > 0.0 {
        let rate = (est as f64 / secs).round() as u32;
        format!("~{est}tok · ~{rate}tok/s")
    } else {
        format!("~{est}tok")
    }
}

/// The shared EXACT usage segment:
/// `↑in ↓out[ ⎓cached] · [ttft S.Ss · ]Rtok/s`. The TTFT segment
/// renders only when the request observed a first token (`None` →
/// omitted entirely); the rate = output/total-elapsed and is omitted
/// at zero elapsed. The request's total duration is deliberately NOT
/// shown — the user asked for in/out/cache/tps/ttft.
fn usage_segment(usage: &Usage, ttft: Option<Duration>, elapsed: Duration) -> String {
    let mut out = format!("↑{} ↓{}", usage.input_tokens, usage.output_tokens);
    if let Some(cached) = usage.cached_input_tokens {
        out.push_str(&format!(" ⎓{cached}"));
    }
    if let Some(ttft) = ttft {
        out.push_str(&format!(" · ttft {}", format_secs(ttft.as_secs_f64())));
    }
    let secs = elapsed.as_secs_f64();
    if secs > 0.0 {
        let rate = (usage.output_tokens as f64 / secs).round() as u32;
        out.push_str(&format!(" · {rate}tok/s"));
    }
    out
}

/// MERGED meta line for requests whose assistant content is EMPTY
/// (tool-call-only steps): the reasoning estimate prefixes the exact
/// usage segment on ONE dim line — `~Ntok · ↑in ↓out[ ⎓c] · [ttft…]`
/// — so the flush never emits two consecutive meta lines with no
/// output block between them.
fn meta_merged_line(
    chars: usize,
    usage: &Usage,
    ttft: Option<Duration>,
    elapsed: Duration,
) -> String {
    format!(
        "~{}tok · {}",
        estimate_tokens(chars),
        usage_segment(usage, ttft, elapsed)
    )
}

/// EXACT per-request usage meta line (the shared usage segment):
/// `↑in ↓out[ ⎓cached] · ttft S.Ss · Rtok/s` — precise counts from
/// the request's `Usage` event, TTFT measured `RequestStart`→
/// `FirstToken` by the App (omitted when no first token arrived),
/// rate = output/total-elapsed (omitted at zero elapsed).
fn meta_request_line(usage: &Usage, ttft: Option<Duration>, elapsed: Duration) -> String {
    usage_segment(usage, ttft, elapsed)
}

/// Seconds for the ttft segment: one decimal below a minute
/// (`0.8s`), then `1m05s`.
fn format_secs(secs: f64) -> String {
    if secs >= 60.0 {
        let whole = secs as u64;
        format!("{}m{:02}s", whole / 60, whole % 60)
    } else {
        format!("{secs:.1}s")
    }
}

/// One COLLAPSED reasoning row (fix-23): a BOLD muted flattened
/// summary behind an accent `• ` marker — the block's entire
/// on-screen footprint while collapsed (a `Meta` estimate line may
/// follow as its own entry). restyle-1: the marker is accent, the
/// summary bold muted. icons-5: the marker is `g.reasoning` (brain in
/// nerd) — semantic split from the tool-running `g.bullet` gear.
fn collapsed_reasoning_line(text: &str, width: usize, theme: &Theme) -> Line<'static> {
    let g = theme.icons.set();
    let mark = format!("{} ", g.reasoning);
    let ellipsis = g.ellipsis;
    let avail = width.saturating_sub(str_width(&mark)).max(1);
    Line::from(vec![
        Span::styled(mark, theme.tool_running),
        Span::styled(
            reasoning_summary(text, avail, ellipsis),
            theme.reasoning.add_modifier(Modifier::BOLD),
        ),
    ])
}

/// One full-width band row: `spans` padded out to `width` with
/// background-styled spaces so the band color covers the whole row.
fn band_line(spans: Vec<Span<'static>>, width: usize, bg: Style) -> Line<'static> {
    let used: usize = spans.iter().map(|s| s.width()).sum();
    let mut spans = spans;
    spans.push(Span::styled(" ".repeat(width.saturating_sub(used)), bg));
    Line::from(spans)
}

/// The user message band (restyle-1, minimax): full-width #262626
/// background with one blank band row above and below; body width =
/// `width - 4`, 2-column left indent, BOLD signal `› ` anchor on
/// the first content row.
fn push_user_band(out: &mut Vec<Line<'static>>, text: &str, theme: &Theme, width: usize) {
    let bg = theme.user_message_bg;
    let anchor = theme.user_label.patch(bg);
    let body = theme.assistant.patch(bg);
    out.push(band_line(Vec::new(), width, bg));
    let avail = width.saturating_sub(4).max(1);
    let prompt = theme.icons.set().prompt;
    for (i, seg) in wrap_to_width(text, avail).into_iter().enumerate() {
        let mut spans: Vec<Span<'static>> = if i == 0 {
            vec![
                Span::styled("  ".to_owned(), bg),
                Span::styled(format!("{prompt} "), anchor),
            ]
        } else {
            vec![Span::styled("    ".to_owned(), bg)]
        };
        spans.push(Span::styled(seg, body));
        out.push(band_line(spans, width, bg));
    }
    out.push(band_line(Vec::new(), width, bg));
}

/// The EXPANDED reasoning block (fix-23): an accent `• ` marker
/// on the first display row, 2-column indent continuations — muted
/// throughout (restyle-1 replaces the old `┆` gutter). icons-5: the
/// marker is `g.reasoning` (brain in nerd), split from the
/// tool-running `g.bullet` gear.
fn push_reasoning_block(out: &mut Vec<Line<'static>>, text: &str, theme: &Theme, width: usize) {
    let marker = format!("{} ", theme.icons.set().reasoning);
    let start = out.len();
    let avail = width.saturating_sub(2).max(1);
    for logical in text.split('\n') {
        for seg in wrap_to_width(logical, avail) {
            out.push(Line::from(vec![
                Span::styled("  ".to_owned(), theme.reasoning),
                Span::styled(seg, theme.reasoning),
            ]));
        }
    }
    if out.len() > start {
        out[start].spans[0] = Span::styled(marker, theme.tool_running);
    }
}

/// The end-of-turn marker line (restyle-1):
/// `└ model · Ns · ↑in ↓out · ⚡ R tok/s` — muted throughout;
/// the `⚡` span carries signal. The token/rate tail renders only
/// when the turn reported usage (the recovery rebuild path's usage
/// display); the rate = aggregate output tokens / elapsed seconds.
/// ascii-turn-1: the pure-ASCII tier renders dedicated chrome here —
/// prefix `+` and `-` separators (user request) instead of the shared
/// `elbow`/`dot` slots, which other rows (tool connectors, exit
/// keys) keep as `` ` ``/`.`; unicode/nerd render the shared slots
/// verbatim.
fn turn_marker_line(meta: &TurnMeta, theme: &Theme) -> Line<'static> {
    let g = theme.icons.set();
    let (prefix, sep) = if g.ascii {
        ("+", "-")
    } else {
        (g.elbow, g.dot)
    };
    let mut spans = vec![
        Span::styled(format!("{prefix} "), theme.turn_marker),
        Span::styled(
            format!("{} {} {}s", meta.model, sep, meta.elapsed_secs),
            theme.turn_marker,
        ),
    ];
    if let Some((input, output)) = meta.tokens {
        spans.push(Span::styled(
            format!(" {} {}{} {}{}", sep, g.up, input, g.down, output),
            theme.turn_marker,
        ));
        if meta.elapsed_secs > 0 {
            let rate = (output as f64 / meta.elapsed_secs as f64).round() as u64;
            spans.push(Span::styled(format!(" {sep} "), theme.turn_marker));
            spans.push(Span::styled(format!("{} ", g.zap), theme.tool_running));
            spans.push(Span::styled(format!("{rate} tok/s"), theme.turn_marker));
        }
    }
    Line::from(spans)
}

/// The English verb for a tool row (restyle-1 spec table): running
/// → present participle, done/failed → past. Unknown tools
/// render `Using/Used <name>`. `call_agent` rows only exist on the
/// rebuild path (live delegations become [`TranscriptEntry::Delegate`]
/// markers).
fn tool_verb(name: &str, running: bool) -> String {
    let (present, past): (&str, &str) = if name == "call_agent" {
        ("Delegating", "Delegated")
    } else if name.contains("read") {
        ("Reading", "Read")
    } else if name.contains("write") || name.contains("edit") {
        ("Writing", "Wrote")
    } else if name.contains("shell") || name.contains("bash") {
        ("Running", "Ran")
    } else if name.contains("glob") || name.contains("grep") || name.contains("search") {
        ("Searching", "Searched")
    } else {
        return format!("{} {name}", if running { "Using" } else { "Used" });
    };
    (if running { present } else { past }).to_owned()
}

/// Truncate `s` to at most `max_cols` display columns, appending
/// `…` when cut (CJK-safe whole-char walk).
fn truncate_cols(s: &str, max_cols: usize, ellipsis: &str) -> String {
    if max_cols == 0 {
        return String::new();
    }
    if str_width(s) <= max_cols {
        return s.to_owned();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        let cw = char_width(ch);
        if used + cw > max_cols - 1 {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out.push_str(ellipsis);
    out
}

/// select-1: char-level slice of one rendered row's text by
/// DISPLAY-column range `[start, end)` — the same unicode-width walk
/// as [`truncate_cols`], but a wide char straddling either boundary
/// is included WHOLE (round outward; a selection boundary never cuts
/// half a character).
fn slice_by_cols(text: &str, start: usize, end: usize) -> String {
    let mut out = String::new();
    let mut col = 0usize;
    for ch in text.chars() {
        let cw = char_width(ch);
        // The char occupies [col, col+cw); keep it where that range
        // intersects [start, end).
        if col + cw > start && col < end {
            out.push(ch);
        }
        col += cw;
    }
    out
}

/// One tool entry row (restyle-1): a dim `├ `/`└ ` connector +
/// status marker (`•` running accent / `✓` success /
/// `×` error) + BOLD English verb ([`tool_verb`]) + a muted
/// args summary (truncating when the row overflows — rows never
/// wrap), then a muted suffix (` · N output lines` / live elapsed /
/// error summary) that joins the row when it fits or lands on its
/// own indented row below.
#[allow(clippy::too_many_arguments)]
fn push_tool_entry(
    out: &mut Vec<Line<'static>>,
    name: &str,
    args: &str,
    status: &ToolEntryStatus,
    running_for: Option<Duration>,
    output_lines: Option<usize>,
    theme: &Theme,
    width: usize,
    connected: bool,
) {
    let g = theme.icons.set();
    let connector = if connected {
        format!("{} ", g.tee)
    } else {
        format!("{} ", g.elbow)
    };
    let running = matches!(status, ToolEntryStatus::Running);
    let (marker, marker_style) = match status {
        // icons-5: tool-running deliberately keeps `g.bullet` (the
        // gear) — the reasoning rows split off to `g.reasoning`.
        ToolEntryStatus::Running => (g.bullet, theme.tool_running),
        ToolEntryStatus::Done { .. } => (g.check, theme.tool_success),
        ToolEntryStatus::Failed { .. } => (g.cross, theme.tool_failure),
    };
    let verb_style = match status {
        ToolEntryStatus::Failed { .. } => theme.tool_failure.add_modifier(Modifier::BOLD),
        _ => theme.header,
    };
    let mut head: Vec<Span<'static>> = vec![
        Span::styled(connector.to_owned(), theme.line),
        Span::styled(format!("{marker} "), marker_style),
        Span::styled(tool_verb(name, running), verb_style),
    ];
    // Args summary: muted, truncated to the remaining row budget
    // (localize maps the stored preview's unicode chrome for the
    // ascii tier).
    let mut args_text = localize(args.trim(), &g);
    if !args_text.is_empty() {
        let used: usize = head.iter().map(|sp| sp.width()).sum();
        let budget = width.saturating_sub(used + 1);
        if str_width(&args_text) > budget {
            args_text = if budget >= 2 {
                truncate_cols(&args_text, budget, g.ellipsis)
            } else {
                String::new()
            };
        }
        if !args_text.is_empty() {
            head.push(Span::styled(format!(" {args_text}"), theme.muted));
        }
    }
    let (suffix_spans, suffix_width) = status_suffix(status, running_for, output_lines, theme, &g);
    let head_width: usize = head.iter().map(|sp| sp.width()).sum();
    if head_width + suffix_width <= width {
        let mut line = Line::from(head);
        line.spans.extend(suffix_spans);
        out.push(line);
    } else {
        out.push(Line::from(head));
        let mut line = Line::from(Span::styled("  ".to_owned(), theme.muted));
        line.spans.extend(suffix_spans);
        out.push(line);
    }
}

/// The muted status suffix of a tool row: Running → live elapsed;
/// Done → ` · N output lines` when the folded output text is
/// known (`no output` when empty), else the byte count; Failed → the
/// first-line error summary in error color.
fn status_suffix(
    status: &ToolEntryStatus,
    running_for: Option<Duration>,
    output_lines: Option<usize>,
    theme: &Theme,
    g: &crate::icons::IconSet,
) -> (Vec<Span<'static>>, usize) {
    let dot = g.dot;
    match status {
        ToolEntryStatus::Running => {
            let text = format!(
                " {dot} {}",
                format_ms(running_for.map(|d| d.as_millis() as u64).unwrap_or(0))
            );
            let w = str_width(&text);
            (vec![Span::styled(text, theme.muted)], w)
        }
        ToolEntryStatus::Done {
            bytes,
            truncated,
            elapsed_ms,
        } => {
            let mut text = match output_lines {
                Some(0) => format!(" {dot} no output"),
                Some(1) => format!(" {dot} 1 output line"),
                Some(n) => format!(" {dot} {n} output lines"),
                None => format!(" {dot} {}", format_bytes(*bytes)),
            };
            if let Some(ms) = elapsed_ms {
                text.push_str(&format!(" {dot} {}", format_ms(*ms)));
            }
            if *truncated {
                text.push_str(g.ellipsis);
            }
            let w = str_width(&text);
            (vec![Span::styled(text, theme.muted)], w)
        }
        ToolEntryStatus::Failed { summary } => {
            // The stored failure summary carries unicode chrome —
            // localize (identity outside the ascii tier).
            let text = format!(" {dot} {}", localize(summary, g));
            let w = str_width(&text);
            (vec![Span::styled(text, theme.tool_failure)], w)
        }
    }
}

/// Display-row cap per section of the expanded tool detail (fix-25):
/// args and output each show at most this many wrapped rows, then a
/// `…（共 N 行）` marker names the section's true row count.
const TOOL_DETAIL_MAX_ROWS: usize = 30;

/// The EXPANDED tool entry's detail block (fix-25): a dim block in two
/// sections — `调用 {name}` + the full args wrapped to the width,
/// then `输出` + the full output wrapped. restyle-1: rows carry
/// the `│   ` dim rail prefix while the entry connects to a following
/// execution row (`├`), `    ` when it closes the run (`└`).
/// Graceful output degradation: Running → `运行中…`; a
/// live-path Done whose text only arrives at merge/rebuild →
/// `（待回填）`; an empty return → `（空）`.
fn push_tool_detail(
    out: &mut Vec<Line<'static>>,
    name: &str,
    detail: &ToolEntryDetail,
    running: bool,
    theme: &Theme,
    width: usize,
    connected: bool,
) {
    let g = theme.icons.set();
    let prefix = if connected {
        format!("{}   ", g.vertical)
    } else {
        "    ".to_owned()
    };
    let avail = width.saturating_sub(str_width(&prefix)).max(1);
    out.push(Line::from(Span::styled(
        format!("{prefix}调用 {name}"),
        theme.muted,
    )));
    push_capped_section(out, &detail.args, theme, avail, &prefix, width);
    out.push(Line::from(Span::styled(
        format!("{prefix}输出"),
        theme.muted,
    )));
    match &detail.output {
        None => {
            let placeholder = if running {
                // The ellipsis is chrome — the tier's glyph (ascii
                // renders `...`).
                format!("运行中{}", g.ellipsis)
            } else {
                "（待回填）".to_owned()
            };
            out.push(Line::from(Span::styled(
                format!("{prefix}{placeholder}"),
                theme.muted,
            )));
        }
        Some(text) if text.trim().is_empty() => out.push(Line::from(Span::styled(
            format!("{prefix}（空）"),
            theme.muted,
        ))),
        Some(text) => push_capped_section(out, text, theme, avail, &prefix, width),
    }
}

/// One detail section's wrapped rows (muted, rail-prefixed), capped at
/// [`TOOL_DETAIL_MAX_ROWS`] display rows; the `…（共 N 行）` marker
/// names the section's true wrapped-row count when the cap cut it.
///
/// P1 (restyle-1): diff-shaped lines (leading `+`/`-`, as emitted by
/// edit/patch tools) render as full-width red/green background bands —
/// [`Theme::diff_added_bg`] / [`Theme::diff_removed_bg`] — with the
/// +/- marker column colored (success/error); every wrapped row of the
/// band carries the background.
fn push_capped_section(
    out: &mut Vec<Line<'static>>,
    text: &str,
    theme: &Theme,
    avail: usize,
    prefix: &str,
    width: usize,
) {
    if text.is_empty() {
        return;
    }
    // Pre-wrap every logical line, remembering its diff band.
    let mut rows: Vec<(String, Option<Style>)> = Vec::new();
    for logical in text.split('\n') {
        let band = match logical.chars().next() {
            Some('+') => Some(theme.diff_added_bg),
            Some('-') if !logical.starts_with("---") => Some(theme.diff_removed_bg),
            _ => None,
        };
        for seg in wrap_to_width(logical, avail) {
            rows.push((seg, band));
        }
    }
    let total = rows.len();
    let mut capped = false;
    if total > TOOL_DETAIL_MAX_ROWS {
        rows.truncate(TOOL_DETAIL_MAX_ROWS);
        capped = true;
    }
    for (row, band) in rows {
        let (fg, glyph_style) = match (&row.chars().next(), band) {
            (Some('+'), Some(_)) => (theme.assistant, theme.tool_success),
            (Some('-'), Some(_)) => (theme.assistant, theme.tool_failure),
            _ => (theme.muted, theme.muted),
        };
        let mut spans = match band {
            Some(bg) => vec![Span::styled(prefix.to_owned(), theme.muted.patch(bg))],
            None => vec![Span::styled(prefix.to_owned(), theme.muted)],
        };
        match band {
            Some(bg) => {
                // Diff band: marker colored, body text-colored, the row
                // (rail prefix included) bg-padded to the FULL width.
                let head = row.chars().next().unwrap();
                spans.push(Span::styled(head.to_string(), glyph_style.patch(bg)));
                let body = row.chars().skip(1).collect::<String>();
                spans.push(Span::styled(body, fg.patch(bg)));
                let used: usize = spans.iter().map(|sp| sp.width()).sum();
                spans.push(Span::styled(" ".repeat(width.saturating_sub(used)), bg));
            }
            None => spans.push(Span::styled(row, theme.muted)),
        }
        out.push(Line::from(spans));
    }
    if capped {
        let ellipsis = theme.icons.set().ellipsis;
        out.push(Line::from(Span::styled(
            format!("{prefix}{ellipsis}（共 {total} 行）"),
            theme.muted,
        )));
    }
}

/// Approval outcome line (restyle-1): `✓/×/! tool — decision`
/// — approved green, denied red, else warning.
fn push_approval_line(
    out: &mut Vec<Line<'static>>,
    tool_name: &str,
    decision: &str,
    theme: &Theme,
    width: usize,
) {
    let g = theme.icons.set();
    let (marker, style) = match decision {
        "approved" => (g.check, theme.tool_success),
        "denied" => (g.cross, theme.tool_failure),
        _ => (g.warn, theme.warning),
    };
    // The em-dash is unicode chrome — localize (identity outside
    // the ascii tier).
    let text = localize(&format!("{marker} {tool_name} — {decision}"), &g);
    for (i, seg) in wrap_to_width(&text, width).into_iter().enumerate() {
        if i == 0 {
            let prefix = format!("{marker} ");
            if let Some(rest) = seg.strip_prefix(&prefix) {
                out.push(Line::from(vec![
                    Span::styled(prefix, style),
                    Span::styled(rest.to_owned(), style),
                ]));
                continue;
            }
        }
        out.push(Line::from(Span::styled(seg, style)));
    }
}

/// Compact duration, matching the status bar's vocabulary.
fn format_ms(ms: u64) -> String {
    if ms >= 60_000 {
        format!("{}m{:02}s", ms / 60_000, (ms % 60_000) / 1000)
    } else if ms >= 1_000 {
        format!("{}s", ms / 1_000)
    } else {
        format!("{ms}ms")
    }
}

/// Compact byte size (`128B` / `1.2kB` / `3.4MB`).
fn format_bytes(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}kB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

impl Component for TranscriptComponent {
    fn handle(&mut self, action: &Action, ctx: &mut AppCtx) -> Option<Action> {
        if ctx.focus != Focus::Transcript {
            return None;
        }
        match action {
            Action::ScrollUp => {
                // Unpin from wherever the view currently is (the bottom
                // while following) — NOT from the stored stale offset.
                let current = self.current_scroll(ctx);
                self.follow = false;
                self.scroll = clamp_u16(current.saturating_sub(1));
            }
            Action::ScrollDown => {
                let max = self.max_scroll(ctx);
                let next = self.current_scroll(ctx).saturating_add(1).min(max);
                self.scroll = clamp_u16(next);
                if next >= max {
                    self.follow = true; // reached the bottom → re-follow
                }
            }
            Action::ScrollPageUp => {
                let current = self.current_scroll(ctx);
                self.follow = false;
                self.scroll = clamp_u16(current.saturating_sub(self.page()));
            }
            Action::ScrollPageDown => {
                let max = self.max_scroll(ctx);
                let next = (self.current_scroll(ctx) + self.page()).min(max);
                self.scroll = clamp_u16(next);
                if next >= max {
                    self.follow = true;
                }
            }
            Action::ScrollTop => {
                self.follow = false;
                self.scroll = 0;
            }
            Action::ScrollBottom => {
                // G/End: cancel pinning, follow the tail again.
                self.follow = true;
            }
            Action::DismissOverlay => {
                // Esc: cancel pinning, follow the tail again — and
                // drop any text selection (select-1's clear trigger;
                // the App also clears it for the no-overlay case
                // regardless of which component holds focus).
                self.follow = true;
                self.clear_selection();
            }
            Action::Click(column, row) => {
                // fix-17/23/25 click routing, in priority order:
                // 1. the last-rendered `+N ↓ 回到底部` hint rectangle —
                //    a hit returns ScrollBottom (the EXISTING
                //    follow-restore action the App feeds straight back
                //    here); the hit test itself does not mutate the
                //    pin state;
                // 2. a reasoning block's hit rectangle — toggle its
                //    expansion (collapsed summary ⇄ full `┆` block),
                //    consumed in-component (None — the App must not
                //    react), committed entries first, then the
                //    streaming block;
                // 3. a tool entry's hit rectangle — toggle its detail
                //    block (collapsed head row ⇄ head + call/output);
                // 4. miss → inert (positional clicks never scroll,
                //    focus, or pin).
                if let Some(rect) = self.hint_hit_rect.get() {
                    if rect.x <= *column
                        && *column < rect.x + rect.width
                        && rect.y <= *row
                        && *row < rect.y + rect.height
                    {
                        return Some(Action::ScrollBottom);
                    }
                }
                if let Some(&(entry_index, _)) =
                    self.reasoning_hit_rects.borrow().iter().find(|(_, rect)| {
                        rect.x <= *column
                            && *column < rect.x + rect.width
                            && rect.y <= *row
                            && *row < rect.y + rect.height
                    })
                {
                    if !self.expanded_reasoning.remove(&entry_index) {
                        self.expanded_reasoning.insert(entry_index);
                    }
                    return None;
                }
                if let Some(rect) = self.streaming_reasoning_hit_rect.get() {
                    if rect.x <= *column
                        && *column < rect.x + rect.width
                        && rect.y <= *row
                        && *row < rect.y + rect.height
                    {
                        self.streaming_reasoning_expanded = !self.streaming_reasoning_expanded;
                        return None;
                    }
                }
                // 3. a tool entry's hit rectangle — toggle its detail
                //    block (collapsed head row ⇄ head + call/output),
                //    consumed in-component like the reasoning toggle.
                if let Some(&(entry_index, _)) =
                    self.tool_hit_rects.borrow().iter().find(|(_, rect)| {
                        rect.x <= *column
                            && *column < rect.x + rect.width
                            && rect.y <= *row
                            && *row < rect.y + rect.height
                    })
                {
                    if !self.expanded_tools.remove(&entry_index) {
                        self.expanded_tools.insert(entry_index);
                    }
                    return None;
                }
                return None;
            }
            _ => return None,
        }
        None
    }

    fn render(&self, f: &mut Frame, area: Rect, ctx: &AppCtx) {
        // Borderless (Wave 2 lane-a): no frame, no title — the App
        // already hands us the 2-column-padded main column and the
        // entry styling carries all the structure.
        let pinned = !self.follow;

        let inner_width = area.width.max(1);
        let visible = area.height;
        // Remember the geometry for the next handle() (pin math) —
        // and, select-1, the area + effective scroll the selection's
        // screen⇄logical mapping converts against.
        self.viewport.set((inner_width, visible));
        self.sel_area.set(Some(area));

        // splash-1 (rev): the empty-session splash — the centered
        // ANSI-Shadow wordmark block, painted DIRECTLY (the flow
        // Paragraph cannot center vertically), tier picked by the
        // size ladder: main width ≥ 80 takes the full `OpenSlate`
        // art, ≥ 45 the compact `Slate` art (each with a 2-column
        // margin each side), height ≥ 10; everything narrower,
        // shorter, or on the pure-ASCII icon tier falls through to
        // the flow path, whose empty branch renders the legacy
        // wordmark + hint byte-for-byte.
        let splash = if self.is_blank() {
            SplashTier::pick(area.width, area.height, ctx.theme.icons.set().ascii)
        } else {
            None
        };
        if let Some(tier) = splash {
            // Same record hygiene a layout pass would do (the flow
            // path's clears live inside layout_lines_at).
            self.reasoning_rows.borrow_mut().clear();
            self.streaming_reasoning_rows.set(None);
            self.tool_rows.borrow_mut().clear();
            self.line_blocks.borrow_mut().clear();
            self.sel_scroll.set(0);
            self.hint_hit_rect.set(None);
            self.reasoning_hit_rects.replace(Vec::new());
            self.streaming_reasoning_hit_rect.set(None);
            self.tool_hit_rects.replace(Vec::new());
            Self::render_splash(f, area, &ctx.theme, tier);
            self.paint_selection(f, area, 0, &ctx.theme); // no selection — no-op
            return;
        }

        let lines = self.layout_lines(inner_width as usize, &ctx.theme, ctx.run.spinner_frame);
        let total = lines.len();
        let max_scroll = total.saturating_sub(visible as usize);
        let scroll = if self.follow {
            max_scroll
        } else {
            (self.scroll as usize).min(max_scroll)
        };
        self.sel_scroll.set(scroll);

        f.render_widget(Paragraph::new(lines).scroll((clamp_u16(scroll), 0)), area);

        // Pinned with content below the viewport → bottom-right hint
        // (fix-23: the count-less `↓ 回到底部` copy replaced the old
        // counted hint; fix-25 reintroduced the count as a `+N` PREFIX
        // — rows below the pin, folded blocks included). The hint is
        // CLICKABLE: its span (widened 2 columns each side, clamped
        // to the hint row) is recorded in `hint_hit_rect` for
        // `handle`'s Action::Click hit-testing — cleared to `None` on
        // every render that does not draw it.
        let mut hint_hit = None;
        if pinned && visible > 0 {
            let below = total.saturating_sub(scroll + visible as usize);
            if below > 0 {
                let hint_row = Rect {
                    x: area.x.saturating_add(1),
                    y: area.y + area.height.saturating_sub(1),
                    width: area.width.saturating_sub(2),
                    height: 1,
                };
                let hint_text = format!("+{below} {} 回到底部", ctx.theme.icons.set().down);
                // interactive-1: the hovered hint renders in the hover
                // slot (signal + underline + bold); click behavior is
                // unchanged (fix-17).
                let hint_style = if self.hint_hovered {
                    ctx.theme.hover
                } else {
                    ctx.theme.user_label
                };
                f.render_widget(
                    Paragraph::new(Span::styled(hint_text.to_owned(), hint_style))
                        .alignment(Alignment::Right),
                    hint_row,
                );
                // The Paragraph right-aligns `hint_text` inside
                // `hint_row`; recover its span arithmetically (the
                // same width math the renderer uses).
                let text_w = str_width(&hint_text) as u16;
                let row_end = hint_row.x + hint_row.width; // exclusive
                let text_start = row_end.saturating_sub(text_w);
                let hit_x = text_start.saturating_sub(2).max(hint_row.x);
                let hit_end = text_start.saturating_add(text_w + 2).min(row_end);
                hint_hit = Some(Rect {
                    x: hit_x,
                    y: hint_row.y,
                    width: hit_end.saturating_sub(hit_x),
                    height: 1,
                });
            }
        }
        self.hint_hit_rect.set(hint_hit);

        // fix-23: convert the reasoning blocks' LAYOUT positions
        // (entry index, start row, row count — recorded by THIS
        // frame's `layout_lines` call) into TERMINAL-ABSOLUTE hit
        // rects, clipped to the viewport — the same
        // render-records/handle-consumes pattern as `hint_hit_rect`.
        // Rewritten unconditionally every frame: nothing rendered →
        // no rects → clicks inert.
        let row_rect = |start: usize, count: usize| -> Option<Rect> {
            let visible_end = scroll.saturating_add(visible as usize);
            let top = start.max(scroll);
            let bottom = (start + count).min(visible_end);
            (top < bottom).then(|| Rect {
                x: area.x,
                y: area.y + (top - scroll) as u16,
                width: area.width,
                height: (bottom - top) as u16,
            })
        };
        let hits = self
            .reasoning_rows
            .borrow()
            .iter()
            .filter_map(|&(index, start, count)| row_rect(start, count).map(|r| (index, r)))
            .collect();
        self.reasoning_hit_rects.replace(hits);
        self.streaming_reasoning_hit_rect.set(
            self.streaming_reasoning_rows
                .get()
                .and_then(|(start, count)| row_rect(start, count)),
        );
        // fix-25: the tool entries' rects, same conversion.
        let tool_hits = self
            .tool_rows
            .borrow()
            .iter()
            .filter_map(|&(index, start, count)| row_rect(start, count).map(|r| (index, r)))
            .collect();
        self.tool_hit_rects.replace(tool_hits);

        // select-1: the selection highlight overlay — a post-render
        // style pass over the painted frame (see
        // [`Self::paint_selection`]); nothing renders without a
        // selection on screen.
        self.paint_selection(f, area, scroll, &ctx.theme);
    }
}

impl TranscriptComponent {
    /// splash-1 (rev): paint the empty-session splash — six frozen
    /// ANSI-Shadow art rows in the minimax hero gradient (rows 1-2
    /// `wordmark_highlight`, rows 3-4 the signal slot
    /// [`Theme::tool_running`], rows 5-6 `wordmark_shadow`), every
    /// row BOLD and padded to the block width so the trailing spaces
    /// ride the same tone (the hero.ts canvasLine treatment; zero
    /// hardcoded colors — the ansi board's Indexed values flow
    /// through the same slots), one blank row, then the muted hint
    /// centered on the same axis by its own localized width. The
    /// block (tier-wide, [`SPLASH_ROWS`] tall) is centered in `area`
    /// both ways (integer division; art rows left-align to the
    /// block's left edge). Callers guarantee the roomy path
    /// ([`SplashTier::pick`]).
    fn render_splash(f: &mut Frame, area: Rect, theme: &Theme, tier: SplashTier) {
        let top = area.y + (area.height.saturating_sub(SPLASH_ROWS)) / 2;
        let left = area.x + (area.width.saturating_sub(tier.width())) / 2;
        let bands = [
            theme.wordmark_highlight,
            theme.wordmark_highlight,
            theme.tool_running, // the signal-tone slot (fg-only)
            theme.tool_running,
            theme.wordmark_shadow,
            theme.wordmark_shadow,
        ];
        for (i, (line, style)) in tier.art().iter().zip(bands).enumerate() {
            // Pad the row to the block width (char count == display
            // width — every art glyph is a width-1 BMP character) so
            // the trailing spaces carry the band tone too.
            let canvas = format!("{line:<width$}", width = tier.width() as usize);
            f.render_widget(
                Paragraph::new(Line::styled(canvas, style.add_modifier(Modifier::BOLD))),
                Rect {
                    x: left,
                    y: top + i as u16,
                    width: tier.width(),
                    height: 1,
                },
            );
        }
        // The hint row (row 7 of the block): same copy + muted style
        // as the fallback path, localized per tier (the ascii board
        // downgrades the em-dash), centered on the block's axis.
        let set = theme.icons.set();
        let hint = localize(EMPTY_SESSION_HINT, &set);
        let hint_w = str_width(&hint) as u16;
        let x = area.x + (area.width.saturating_sub(hint_w)) / 2;
        f.render_widget(
            Paragraph::new(Span::styled(hint, theme.muted)),
            Rect {
                x,
                y: top + SPLASH_ROWS - 1,
                width: hint_w.max(1),
                height: 1,
            },
        );
    }
}

/// Clamp a scroll offset into `u16` (degenerate >65k-line transcripts).
fn clamp_u16(n: usize) -> u16 {
    n.min(u16::MAX as usize) as u16
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::{ConfigSummary, Focus, RunInfo, RunState};
    use crate::theme::Theme;
    use openslate_core::types::{ToolCall, ToolCallId};
    use serde_json::json;

    fn msg(role: MessageRole, content: &str) -> Message {
        Message {
            role,
            content: content.to_owned(),
            tool_call_id: None,
            name: None,
            tool_calls: None,
        }
    }

    fn assistant_with_call(id: &str, name: &str) -> Message {
        Message {
            role: MessageRole::Assistant,
            content: String::new(),
            tool_call_id: None,
            name: None,
            tool_calls: Some(vec![ToolCall {
                id: ToolCallId(id.to_owned()),
                name: name.to_owned(),
                arguments: json!({}),
            }]),
        }
    }

    fn tool_result(id: &str, name: &str, content: &str) -> Message {
        Message {
            role: MessageRole::Tool,
            content: content.to_owned(),
            tool_call_id: Some(ToolCallId(id.to_owned())),
            name: Some(name.to_owned()),
            tool_calls: None,
        }
    }

    fn test_ctx() -> AppCtx {
        AppCtx {
            theme: Theme::new(),
            focus: Focus::Transcript,
            run: RunInfo {
                state: RunState::Idle,
                spinner_frame: 0,
                model_label: String::new(),
                tokens_in: 0,
                tokens_out: 0,
                cost_usd: 0.0,
                elapsed: None,
                tool_calls_cur: 0,
                depth_cur: 0,
                context_remaining: None,
            },
            config: ConfigSummary {
                model_alias: String::new(),
                model_id: String::new(),
                provider_name: String::new(),
                max_depth: 4,
                max_tool_calls: 20,
                run_id: None,
                model_aliases: Vec::new(),
            },
            size: (80, 24),
            notice: None,
        }
    }

    // ── Rebuild mapping ─────────────────────────────────────────────────

    #[test]
    fn rebuild_maps_roles_to_entries() {
        let mut t = TranscriptComponent::new();
        let messages = vec![
            msg(MessageRole::User, "hi"),
            msg(MessageRole::Assistant, "hello"),
            msg(MessageRole::Tool, "tool output (folded into the call)"),
            msg(MessageRole::Assistant, "done"),
        ];
        t.push_delta("live");
        t.rebuild(&messages);
        assert_eq!(
            t.entries(),
            &[
                TranscriptEntry::User("hi".into()),
                TranscriptEntry::Assistant("hello".into()),
                TranscriptEntry::Assistant("done".into()),
            ]
        );
        assert!(t.streaming_text().is_empty());
    }

    #[test]
    fn rebuild_emits_tool_call_entries() {
        let mut t = TranscriptComponent::new();
        let assistant = assistant_with_call("tc-1", "echo");
        t.rebuild(&[msg(MessageRole::User, "go"), assistant]);
        assert_eq!(t.entries().len(), 2);
        assert!(matches!(
            &t.entries()[1],
            TranscriptEntry::ToolCall { name, call_id: Some(cid), status: ToolEntryStatus::Done { .. }, .. }
                if name == "echo" && cid == "tc-1"
        ));
    }

    #[test]
    fn rebuild_folds_tool_outcomes_by_call_id() {
        let mut t = TranscriptComponent::new();
        t.rebuild(&[
            msg(MessageRole::User, "go"),
            assistant_with_call("t1", "read_file"),
            tool_result("t1", "read_file", "Error: file not found: x.rs"),
            assistant_with_call("t2", "read_file"),
            tool_result("t2", "read_file", "ok"),
        ]);
        // Same-name calls resolved by distinct ids: t1 failed, t2 done.
        match &t.entries()[1] {
            TranscriptEntry::ToolCall {
                status: ToolEntryStatus::Failed { summary },
                ..
            } => assert!(summary.starts_with("Error: file not found")),
            other => panic!("unexpected {other:?}"),
        }
        match &t.entries()[2] {
            TranscriptEntry::ToolCall {
                status: ToolEntryStatus::Done { bytes, .. },
                ..
            } => assert_eq!(*bytes, 2, "bytes come from the folded content"),
            other => panic!("unexpected {other:?}"),
        }
    }

    /// `/copy tool` source (copy-1): the getter returns the most
    /// recent RETAINED output — a tail entry without one (live path,
    /// folds in at merge/rebuild) falls back to the last entry that
    /// has text.
    #[test]
    fn last_tool_output_prefers_the_latest_retained_output() {
        let mut t = TranscriptComponent::new();
        assert_eq!(t.last_tool_output(), None, "no tools at all");

        // Live entries never carry output text (ToolEnd is bytes-only).
        t.tool_start("read_file", r#"{"path":"a.rs"}"#);
        t.tool_end("read_file", 10, false);
        assert_eq!(t.last_tool_output(), None, "live entry has no output yet");

        // Two folded calls: the LATER output wins.
        t.rebuild(&[
            msg(MessageRole::User, "go"),
            assistant_with_call("t1", "read_file"),
            tool_result("t1", "read_file", "first output"),
            assistant_with_call("t2", "read_file"),
            tool_result("t2", "read_file", "second output"),
        ]);
        assert_eq!(t.last_tool_output(), Some("second output"));

        // Tail fall-back: a fresh live call (no output yet) after a
        // folded one does not hide the retained text.
        t.tool_start("run_shell", r#"{"cmd":"ls"}"#);
        assert_eq!(
            t.last_tool_output(),
            Some("second output"),
            "live tail falls back to the last retained output"
        );
    }

    #[test]
    fn rebuild_failure_heuristics_cover_core_markers() {
        let cases = [
            ("Error: tool crashed", true),
            ("approval denied tool 'shell': User rejected", true),
            ("child agent call denied: max depth", true),
            ("[tool call cancelled before completion]", true),
            ("[error] SyntaxError: unexpected token", true),
            ("  Error: leading whitespace tolerated", true),
            ("the grep output mentions Error: but is fine", false),
            ("plain successful output", false),
        ];
        for (content, should_fail) in cases {
            let mut t = TranscriptComponent::new();
            t.rebuild(&[
                assistant_with_call("t", "x"),
                Message {
                    role: MessageRole::Tool,
                    content: content.to_owned(),
                    tool_call_id: Some(ToolCallId("t".into())),
                    name: Some("x".into()),
                    tool_calls: None,
                },
            ]);
            let status = match &t.entries()[0] {
                TranscriptEntry::ToolCall { status, .. } => status.clone(),
                other => panic!("unexpected {other:?}"),
            };
            assert_eq!(
                matches!(status, ToolEntryStatus::Failed { .. }),
                should_fail,
                "content {content:?}"
            );
        }
    }

    #[test]
    fn rebuild_detects_truncation_marker() {
        let mut t = TranscriptComponent::new();
        let content = "data\n\n[TRUNCATED: original 5000 bytes, showing 4 bytes]";
        t.rebuild(&[
            assistant_with_call("t", "read_file"),
            tool_result("t", "read_file", content),
        ]);
        match &t.entries()[0] {
            TranscriptEntry::ToolCall {
                status:
                    ToolEntryStatus::Done {
                        bytes, truncated, ..
                    },
                ..
            } => {
                assert_eq!(*bytes, content.len());
                assert!(*truncated);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn rebuild_failure_summary_is_first_line_clamped() {
        let long_error = format!("Error: {}\nsecond line", "x".repeat(200));
        let mut t = TranscriptComponent::new();
        t.rebuild(&[
            assistant_with_call("t", "x"),
            tool_result("t", "x", &long_error),
        ]);
        match &t.entries()[0] {
            TranscriptEntry::ToolCall {
                status: ToolEntryStatus::Failed { summary },
                ..
            } => {
                assert!(summary.starts_with("Error: "));
                assert!(summary.ends_with('…'));
                assert!(str_width(summary) <= 60, "clamped: {summary}");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    // ── Live tool lifecycle ─────────────────────────────────────────────

    #[test]
    fn live_tool_lifecycle_marks_last_running_entry() {
        let mut t = TranscriptComponent::new();
        t.tool_start("read_file", r#"{"path":"a.rs"}"#);
        t.tool_start("read_file", r#"{"path":"b.rs"}"#);
        assert_eq!(t.entries().len(), 2);
        t.tool_end("read_file", 128, false);
        match &t.entries()[1] {
            TranscriptEntry::ToolCall {
                status:
                    ToolEntryStatus::Done {
                        bytes, elapsed_ms, ..
                    },
                ..
            } => {
                assert_eq!(*bytes, 128);
                assert!(elapsed_ms.is_some(), "live completion measures time");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(
            &t.entries()[0],
            TranscriptEntry::ToolCall {
                status: ToolEntryStatus::Running,
                ..
            }
        ));
    }

    #[test]
    fn call_agent_becomes_delegate_marker() {
        let mut t = TranscriptComponent::new();
        t.tool_start("call_agent", r#"{"agent_id":"researcher"}"#);
        assert_eq!(
            t.entries(),
            &[TranscriptEntry::Delegate {
                agent: "researcher".into(),
                done: false
            }]
        );
    }

    // ── Streaming order (reasoning/answer/tool interleave) ─────────────

    /// Joined text of every layout line (assert helper).
    fn layout_text(t: &TranscriptComponent, width: usize, theme: &Theme) -> Vec<String> {
        t.layout_lines(width, theme, 0)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
            .collect()
    }

    /// Bug 1 (answer covers the reasoning): once answer deltas start,
    /// the previously-streamed reasoning must stay COMPLETELY visible —
    /// same rows, same order, nothing overwritten — with the answer
    /// block stacked strictly BELOW it (event order).
    #[test]
    fn reasoning_stays_visible_when_answer_streaming_starts() {
        let mut t = TranscriptComponent::new();
        t.begin_streaming();
        t.push_reasoning("first thought");
        t.push_reasoning(" continued");
        let ctx = test_ctx();
        let before = layout_text(&t, 60, &ctx.theme);
        // fix-24: collapsed to the flattened summary — single-line
        // blocks keep their spaces ("first thought continued").
        assert_eq!(before, vec!["• first thought continued"]);

        // Answer deltas begin — the reasoning row is untouched and
        // the answer appends BELOW (block gap + md-rendered rows; the
        // ▍ tail cursor marks the live row).
        t.push_delta("the answer");
        let after = layout_text(&t, 60, &ctx.theme);
        assert_eq!(
            after,
            vec!["• first thought continued", "", "● the answer▍"],
            "reasoning row stays intact above the answer block"
        );
        // More answer deltas only grow the answer block's tail.
        t.push_delta(" grows");
        assert_eq!(
            layout_text(&t, 60, &ctx.theme),
            vec!["• first thought continued", "", "● the answer grows▍"]
        );
    }

    /// Bug 1, CJK + wrap angle: multi-line reasoning keeps every
    /// wrapped row stable when the answer starts (the wrap/offset math
    /// of the two streaming buffers must not overlap).
    #[test]
    fn multiline_cjk_reasoning_rows_stay_stable_when_answer_starts() {
        let mut t = TranscriptComponent::new();
        t.begin_streaming();
        // fix-23: wrap stability is an EXPANDED-mode property (the
        // collapsed view is one summary row by default).
        t.streaming_reasoning_expanded = true;
        let reasoning = "思考第一行内容较长会自动折行处理".repeat(2);
        t.push_reasoning(&reasoning);
        let ctx = test_ctx();
        let before = layout_text(&t, 20, &ctx.theme); // forces wrapping
        assert!(before.len() > 1, "reasoning wraps: {before:?}");
        assert!(before[0].starts_with("•"));
        assert!(before.iter().skip(1).all(|l| l.starts_with("  ")));

        t.push_delta("最终答案");
        let after = layout_text(&t, 20, &ctx.theme);
        assert_eq!(
            &after[..before.len()],
            &before[..],
            "wrapped reasoning rows are byte-identical after the answer starts"
        );
        // The answer opens with its block gap, then the md-rendered
        // row carrying the tail cursor.
        assert_eq!(after[before.len()], "");
        assert_eq!(after[before.len() + 1], "● 最终答案▍");
        assert_eq!(after.len(), before.len() + 2);
    }

    /// Bug 3 (tool line position): a tool triggered MID-STREAM must
    /// render BETWEEN the text that streamed before and after it —
    /// reasoning → tool line → more reasoning → answer, exactly in
    /// event order.
    #[test]
    fn tool_line_interleaves_in_event_order() {
        let mut t = TranscriptComponent::new();
        t.push_user("go");
        t.begin_streaming();
        t.push_reasoning("think A");
        t.tool_start("read_file", r#"{"path":"a.rs"}"#);
        t.tool_end("read_file", 10, false);
        t.push_reasoning("think B");
        t.push_delta("answer B");

        // Entries: flushed Reasoning A, then the tool call — the tool
        // lands AFTER the reasoning it followed.
        assert!(matches!(
            t.entries().get(1),
            Some(TranscriptEntry::Reasoning(text)) if text == "think A"
        ));
        assert!(matches!(
            t.entries().get(2),
            Some(TranscriptEntry::ToolCall { name, status: ToolEntryStatus::Done { .. }, .. })
                if name == "read_file"
        ));

        let ctx = test_ctx();
        let rows = layout_text(&t, 60, &ctx.theme);
        // fix-24: collapsed summaries keep spaces — "think A" reads
        // as "think A" while collapsed.
        let tool_row = rows
            .iter()
            .position(|r| r.contains("a.rs"))
            .expect("tool line rendered");
        let reasoning_a = rows
            .iter()
            .position(|r| r.contains("think A"))
            .expect("reasoning A rendered");
        let reasoning_b = rows
            .iter()
            .position(|r| r.contains("think B"))
            .expect("reasoning B rendered");
        let answer = rows
            .iter()
            .position(|r| r.contains("answer B"))
            .expect("answer block rendered");
        assert!(
            reasoning_a < tool_row && tool_row < reasoning_b && reasoning_b < answer,
            "event order is visual order: {rows:?}"
        );
    }

    /// Answer text that streamed BEFORE a tool call also flushes to its
    /// event position (models may emit content and tool_calls in one
    /// response — the content streams first).
    #[test]
    fn pre_tool_answer_text_flushes_above_the_tool_line() {
        let mut t = TranscriptComponent::new();
        t.begin_streaming();
        t.push_reasoning("think");
        t.push_delta("let me check");
        t.tool_start("grep", r#"{"q":"x"}"#);
        assert_eq!(
            t.entries(),
            &[
                TranscriptEntry::Reasoning("think".into()),
                TranscriptEntry::Assistant("let me check".into()),
                TranscriptEntry::ToolCall {
                    name: "grep".into(),
                    args: r#"{"q":"x"}"#.into(),
                    status: ToolEntryStatus::Running,
                    call_id: None,
                    detail: ToolEntryDetail {
                        args: r#"{"q":"x"}"#.into(),
                        output: None,
                    },
                },
            ]
        );
        // The streaming area starts clean for the next step.
        assert!(t.streaming_text().is_empty());
    }

    /// Approval outcome lines respect event order like tool lines.
    #[test]
    fn approval_outcome_lands_below_streamed_reasoning() {
        let mut t = TranscriptComponent::new();
        t.begin_streaming();
        t.push_reasoning("pondering the request");
        t.push_approval("shell", "denied");
        assert!(matches!(
            t.entries(),
            [TranscriptEntry::Reasoning(text), TranscriptEntry::Approval { tool_name, decision }]
                if text == "pondering the request"
                    && tool_name == "shell"
                    && decision == "denied"
        ));
    }

    /// Whitespace-only buffers are dropped (not committed) but still
    /// cleared, so no phantom ┆ rows survive the flush.
    #[test]
    fn flush_drops_whitespace_only_buffers() {
        let mut t = TranscriptComponent::new();
        t.begin_streaming();
        t.push_reasoning("   ");
        t.push_delta(" ");
        t.tool_start("echo", "{}");
        assert_eq!(
            t.entries(),
            &[TranscriptEntry::ToolCall {
                name: "echo".into(),
                args: "{}".into(),
                status: ToolEntryStatus::Running,
                call_id: None,
                detail: ToolEntryDetail {
                    args: "{}".into(),
                    output: None,
                },
            }]
        );
        let ctx = test_ctx();
        let rows = layout_text(&t, 40, &ctx.theme);
        assert!(rows.iter().all(|r| r.trim() != "•"));
    }

    #[test]
    fn step_end_dedups_and_placement() {
        let mut t = TranscriptComponent::new();
        // Empty transcript: no separator.
        t.step_end();
        assert!(t.entries().is_empty());
        // Right after the user message (turn start): none either.
        t.push_user("go");
        t.step_end();
        assert_eq!(t.entries().len(), 1);
        // After real content: one separator, and consecutive boundary
        // events (Usage + RequestEnd + StepEnd) collapse into one.
        t.tool_start("echo", "{}");
        t.step_end();
        t.step_end();
        t.step_end();
        assert_eq!(t.entries().len(), 3);
        assert!(matches!(t.entries()[2], TranscriptEntry::StepBreak));
        // New content after a separator → the next boundary adds another.
        t.tool_start("grep", "{}");
        t.step_end();
        assert_eq!(t.entries().len(), 5);
    }

    #[test]
    fn args_preview_clamps_to_40_cols() {
        assert_eq!(args_preview(r#"{"path":"src"}"#), r#"{"path":"src"}"#);
        let long = args_preview(&format!("{{\"path\":\"{}\"}}", "x".repeat(80)));
        assert!(
            long.chars().count() <= 40 && long.ends_with('…'),
            "clamped to ≤40 display chars: {long}"
        );
        // CJK safety
        let cjk = args_preview(&"世".repeat(60));
        assert!(cjk.ends_with('…'));
    }

    #[test]
    fn streaming_accumulates_then_clears() {
        let mut t = TranscriptComponent::new();
        t.begin_streaming();
        t.push_delta("Hel");
        t.push_delta("lo");
        assert_eq!(t.streaming_text(), "Hello");
        t.clear_streaming();
        assert!(t.streaming_text().is_empty());
    }

    // ── Pin state machine ───────────────────────────────────────────────

    #[test]
    fn scroll_unpins_from_the_bottom_not_the_top() {
        let mut t = TranscriptComponent::new();
        for i in 0..10 {
            t.push_user(&format!("line{i}")); // one display line each at width 40
        }
        t.viewport.set((40, 4)); // restyle-1: each user = gap + 3 band
                                 // rows → 39 total → max scroll 35
        let mut ctx = test_ctx();

        t.handle(&Action::ScrollUp, &mut ctx);
        assert!(t.is_pinned());
        assert_eq!(t.scroll, 34, "unpin starts one line above the bottom");
    }

    #[test]
    fn pinned_view_ignores_new_content_until_released() {
        let mut t = TranscriptComponent::new();
        for i in 0..10 {
            t.push_user(&format!("line{i}"));
        }
        t.viewport.set((40, 4));
        let mut ctx = test_ctx();
        t.handle(&Action::ScrollUp, &mut ctx);
        assert_eq!(t.scroll, 34);

        // Streaming content arrives while pinned: the offset must not
        // move (the hint "↓ 回到底部" appears instead — render-level).
        t.push_delta("brand new content");
        t.handle(&Action::ScrollUp, &mut ctx); // one more line up: 34 → 33
        assert_eq!(t.scroll, 33);

        // Releasing: G / End / ScrollBottom / Esc all re-follow.
        for action in [Action::ScrollBottom, Action::DismissOverlay] {
            t.handle(&Action::ScrollUp, &mut ctx); // pin
            assert!(t.is_pinned());
            t.handle(&action, &mut ctx);
            assert!(!t.is_pinned(), "{action:?} releases the pin");
        }
    }

    #[test]
    fn scrolling_back_down_to_the_bottom_refollows() {
        let mut t = TranscriptComponent::new();
        for i in 0..10 {
            t.push_user(&format!("line{i}"));
        }
        t.viewport.set((40, 4));
        let mut ctx = test_ctx();
        t.handle(&Action::ScrollPageUp, &mut ctx); // page = 2 → scroll 35-2 = 33
        assert!(t.is_pinned());
        assert_eq!(t.scroll, 33);

        // ScrollDown twice: 33 → 34 (still pinned) → 35 == max → re-follow.
        t.handle(&Action::ScrollDown, &mut ctx);
        assert!(t.is_pinned());
        t.handle(&Action::ScrollDown, &mut ctx);
        assert!(!t.is_pinned(), "reaching the bottom re-follows");
    }

    #[test]
    fn scroll_top_pins_at_zero_and_page_down_clamps() {
        let mut t = TranscriptComponent::new();
        for i in 0..10 {
            t.push_user(&format!("line{i}"));
        }
        t.viewport.set((40, 4));
        let mut ctx = test_ctx();
        t.handle(&Action::ScrollTop, &mut ctx);
        assert!(t.is_pinned());
        assert_eq!(t.scroll, 0);
        // PageDown (page = 2) walks to the bottom: 0 → 2 → … → 34
        // (still pinned, one above max), then the press that reaches
        // max 35 re-follows.
        for expected in [
            2u16, 4, 6, 8, 10, 12, 14, 16, 18, 20, 22, 24, 26, 28, 30, 32, 34,
        ] {
            t.handle(&Action::ScrollPageDown, &mut ctx);
            assert!(t.is_pinned(), "still pinned at {expected}");
            assert_eq!(t.scroll, expected);
        }
        t.handle(&Action::ScrollPageDown, &mut ctx);
        assert!(!t.is_pinned(), "reaching the bottom re-follows");
    }

    #[test]
    fn rebuild_keeps_the_pin_and_clear_resets_it() {
        let mut t = TranscriptComponent::new();
        t.push_user("hello");
        t.viewport.set((40, 4));
        let mut ctx = test_ctx();
        t.handle(&Action::ScrollUp, &mut ctx); // max_scroll 0 → pinned at 0
        assert!(t.is_pinned());

        t.rebuild(&[
            msg(MessageRole::User, "hello"),
            msg(MessageRole::Assistant, "world"),
        ]);
        assert!(t.is_pinned(), "the pin survives the rebuild");

        t.clear();
        assert!(!t.is_pinned(), "/new resets to following");
    }

    #[test]
    fn non_transcript_focus_ignores_scroll_actions() {
        let mut t = TranscriptComponent::new();
        t.push_user("hello");
        let mut ctx = test_ctx();
        ctx.focus = Focus::Input;
        t.handle(&Action::ScrollUp, &mut ctx);
        assert!(!t.is_pinned());
    }

    // ── Hint click (fix-17: `↓ 回到底部` is clickable) ────────────────

    /// Draw into a fresh TestBackend terminal (records the viewport and
    /// the hint hit rectangle, exactly like a real frame).
    fn render(t: &TranscriptComponent, w: u16, h: u16) {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        let ctx = test_ctx();
        terminal.draw(|f| t.render(f, f.area(), &ctx)).unwrap();
    }

    /// Ten user lines at 40x6: 19 layout rows, 6 visible, max scroll
    /// 13. Pinned via one ScrollUp → offset 12, `below` = 1 → the hint
    /// `+1 ↓ 回到底部` (13 display cols) right-aligns into the row rect
    /// x=1..39 → text at cols 26..=38; the hit rect widens ±2 cols,
    /// clamped to the row → cols 24..=38 on the last row (y=5).
    fn pinned_with_hint() -> (TranscriptComponent, AppCtx) {
        let mut t = TranscriptComponent::new();
        for i in 0..10 {
            t.push_user(&format!("line{i}"));
        }
        render(&t, 40, 6); // establish the viewport
        let mut ctx = test_ctx();
        t.handle(&Action::ScrollUp, &mut ctx);
        assert!(t.is_pinned());
        render(&t, 40, 6); // hint visible → rect recorded
        (t, ctx)
    }

    #[test]
    fn hint_hit_rect_recorded_and_click_jumps_to_bottom() {
        let (mut t, mut ctx) = pinned_with_hint();
        assert_eq!(
            t.hint_hit_rect.get(),
            Some(Rect {
                x: 24,
                y: 5,
                width: 15,
                height: 1
            })
        );

        // Click dead-center of the hint: the EXISTING follow-restore
        // action comes back; feeding it to handle again (what the App
        // dispatch does) re-follows the tail.
        assert_eq!(
            t.handle(&Action::Click(30, 5), &mut ctx),
            Some(Action::ScrollBottom)
        );
        t.handle(&Action::ScrollBottom, &mut ctx);
        assert!(!t.is_pinned(), "the hint click restores following");
    }

    #[test]
    fn hint_click_tolerates_two_columns_of_slack_not_three() {
        let (mut t, mut ctx) = pinned_with_hint();
        // Text starts at col 26: col 24 (+2 slack) still hits, col 23
        // (+3) misses; the right edge clamps to the row rect (col 38
        // is the text's last column and also the rect's).
        assert_eq!(
            t.handle(&Action::Click(24, 5), &mut ctx),
            Some(Action::ScrollBottom)
        );
        assert_eq!(t.handle(&Action::Click(23, 5), &mut ctx), None);
        assert_eq!(
            t.handle(&Action::Click(38, 5), &mut ctx),
            Some(Action::ScrollBottom)
        );
        assert_eq!(t.handle(&Action::Click(39, 5), &mut ctx), None);
        // The rect is one row tall: any other row misses.
        assert_eq!(t.handle(&Action::Click(30, 4), &mut ctx), None);
    }

    #[test]
    fn click_without_visible_hint_is_inert() {
        let mut t = TranscriptComponent::new();
        for i in 0..10 {
            t.push_user(&format!("line{i}"));
        }
        // Following at the bottom: no hint rendered, no rect recorded.
        render(&t, 40, 6);
        assert_eq!(t.hint_hit_rect.get(), None);
        let mut ctx = test_ctx();
        assert_eq!(t.handle(&Action::Click(30, 5), &mut ctx), None);
        assert!(!t.is_pinned(), "a click must never pin the view");

        // Pinned but NOTHING below (content shrank under the pinned
        // offset): still no hint → no rect → inert, and the pin itself
        // survives (the existing rebuild rule).
        t.handle(&Action::ScrollUp, &mut ctx);
        assert!(t.is_pinned());
        t.rebuild(&[msg(MessageRole::User, "hi")]); // 1 layout row
        render(&t, 40, 6); // clamps to the bottom → below = 0
        assert_eq!(t.hint_hit_rect.get(), None);
        assert_eq!(t.handle(&Action::Click(30, 5), &mut ctx), None);
        assert!(t.is_pinned());
    }

    // ── select-1: drag-to-select ──────────────────────────────────────

    /// [`render`] but returning the drawn FRONT buffer (the exact
    /// post-render state — the backend buffer only receives the
    /// frame diff, which skips cells after wide glyphs by design).
    /// The seam for the selection overlay's style assertions.
    fn render_buf(t: &TranscriptComponent, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        let ctx = test_ctx();
        let mut front = None;
        terminal
            .draw(|f| {
                t.render(f, f.area(), &ctx);
                front = Some(f.buffer_mut().clone());
            })
            .unwrap();
        front.unwrap()
    }

    /// Two assistant entries at width 40 → 3 layout rows:
    /// 0=`● alpha beta`, 1=`` (block gap, attributed to entry 1),
    /// 2=`● gamma`.
    fn selection_fixture() -> TranscriptComponent {
        let mut t = TranscriptComponent::new();
        t.entries
            .push(TranscriptEntry::Assistant("alpha beta".into()));
        t.entries.push(TranscriptEntry::Assistant("gamma".into()));
        t
    }

    /// A press that releases without displacement reports Click and
    /// leaves no selection; same-cell motion does NOT count as a
    /// drag; before the first render the gesture is inert.
    #[test]
    fn selection_press_release_without_drag_reports_click() {
        let mut t = selection_fixture();
        render(&t, 40, 6);
        // Press + release on the same cell → the legacy click path.
        t.selection_begin(2, 0);
        assert!(matches!(t.selection, SelectionState::Pending { .. }));
        assert_eq!(t.selection_end(&Theme::new()), SelectionEnd::Click);
        assert_eq!(t.selection, SelectionState::Normal);

        // Same-cell motion during the press: still a click.
        t.selection_begin(2, 0);
        assert!(!t.selection_drag(2, 0), "same-cell motion is not a drag");
        assert_eq!(t.selection_end(&Theme::new()), SelectionEnd::Click);
        assert_eq!(t.selection, SelectionState::Normal);

        // No gesture in flight (press never mapped): a release is a
        // click, and a persisted selection survives it untouched.
        t.selection = SelectionState::Normal;
        assert_eq!(t.selection_end(&Theme::new()), SelectionEnd::Click);

        // Before the FIRST render there is no geometry: the press
        // never anchors, the release still behaves as a click.
        let mut fresh = selection_fixture();
        fresh.selection_begin(2, 0);
        assert_eq!(fresh.selection, SelectionState::Normal);
        assert_eq!(fresh.selection_end(&Theme::new()), SelectionEnd::Click);
    }

    /// Down → drag (≥1 cell) → Selecting; Up extracts the covered
    /// rows' text (boundary rows column-sliced, middle rows whole,
    /// `\n`-joined) and persists the highlight; a new press drops
    /// the persisted selection. Direction-agnostic (backwards drag
    /// normalizes).
    #[test]
    fn selection_drag_extracts_and_persists() {
        let mut t = selection_fixture();
        render(&t, 40, 6);
        t.selection_begin(2, 0);
        assert!(t.selection_drag(5, 2), "threshold crossed");
        assert!(matches!(t.selection, SelectionState::Selecting { .. }));
        match t.selection_end(&Theme::new()) {
            SelectionEnd::Selected(text) => {
                // Row 0 from col 2, blank gap row whole, row 2 to
                // col 5 inclusive.
                assert_eq!(text, "alpha beta\n\n● gamm");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(t.selection, SelectionState::Selected { .. }));

        // Backwards (right-to-left, bottom-to-top) covers the same
        // span — the pair normalizes before extraction.
        t.selection_begin(5, 2);
        assert!(t.selection_drag(2, 0));
        match t.selection_end(&Theme::new()) {
            SelectionEnd::Selected(text) => assert_eq!(text, "alpha beta\n\n● gamm"),
            other => panic!("unexpected {other:?}"),
        }

        // A new press replaces the persisted selection.
        t.selection_begin(1, 2);
        assert!(matches!(t.selection, SelectionState::Pending { .. }));
    }

    /// Logical anchors survive scrolling: pin away from the tail,
    /// come back — the extracted text is IDENTICAL at every offset,
    /// and the highlight follows the rows on screen.
    #[test]
    fn selection_survives_scrolling_text_unchanged() {
        let mut t = TranscriptComponent::new();
        for i in 0..10 {
            t.push_user(&format!("line{i}"));
        }
        render(&t, 40, 6); // 39 layout rows, following → scroll 33
                           // Entry i (i≥1) owns 4 rows (gap, band blank, content, band
                           // blank): entry 8's CONTENT row is layout 33 → screen y=0,
                           // its trailing band blank layout 34 → y=1.
        t.selection_begin(2, 0);
        assert!(t.selection_drag(6, 1));
        let (anchor, cursor) = t.selection.range().expect("selecting");
        let theme = Theme::new();
        assert_eq!(
            t.extract_selection_text(&theme, &anchor, &cursor),
            "› line8\n"
        );
        // Highlight at the following offset: layout 33 → screen y=0.
        let sel = theme.selection_bg.bg.expect("selection bg color");
        let buf = render_buf(&t, 40, 6);
        assert_eq!(buf.cell((2, 0)).unwrap().bg, sel, "highlight at y=0");

        // Pin up two rows (scroll 31): same text, rows moved down.
        let mut ctx = test_ctx();
        t.handle(&Action::ScrollUp, &mut ctx);
        t.handle(&Action::ScrollUp, &mut ctx);
        assert!(t.is_pinned());
        assert_eq!(
            t.extract_selection_text(&theme, &anchor, &cursor),
            "› line8\n",
            "text is scroll-invariant"
        );
        let buf = render_buf(&t, 40, 6);
        assert_eq!(buf.cell((2, 2)).unwrap().bg, sel, "highlight moved to y=2");
        assert_ne!(
            buf.cell((2, 0)).unwrap().bg,
            ratatui::style::Color::Rgb(0x2B, 0x64, 0x73),
            "old position no longer highlighted"
        );

        // Back to the bottom: still the same text.
        t.handle(&Action::ScrollDown, &mut ctx);
        t.handle(&Action::ScrollDown, &mut ctx);
        assert!(!t.is_pinned(), "scrolled back down re-follows");
        assert_eq!(
            t.extract_selection_text(&theme, &anchor, &cursor),
            "› line8\n"
        );
    }

    /// CJK boundaries round OUTWARD: a boundary landing on a wide
    /// char's second cell includes the whole character — extraction
    /// never yields half a character.
    #[test]
    fn selection_cjk_boundaries_round_outward() {
        let mut t = TranscriptComponent::new();
        t.entries.push(TranscriptEntry::Assistant("汉字abc".into()));
        render(&t, 40, 6);
        // Row 0 = `● 汉字abc`: ●(0) ' '(1) 汉(2-3) 字(4-5) a(6)…
        // Anchor on 汉's SECOND cell (col 3) → the whole char joins.
        t.selection_begin(3, 0);
        assert!(t.selection_drag(20, 0));
        match t.selection_end(&Theme::new()) {
            SelectionEnd::Selected(text) => assert_eq!(text, "汉字abc"),
            other => panic!("unexpected {other:?}"),
        }
        // Cursor (end boundary) on 字's second cell (col 5) → both
        // wide chars whole, `a` excluded.
        t.selection_begin(2, 0);
        assert!(t.selection_drag(5, 0));
        match t.selection_end(&Theme::new()) {
            SelectionEnd::Selected(text) => assert_eq!(text, "汉字"),
            other => panic!("unexpected {other:?}"),
        }
    }

    /// The highlight overlay paints whole rendered cells of the
    /// selection background (and nothing else): boundaries landing
    /// inside wide glyphs widen to cover BOTH cells — the row
    /// `● 汉字` selected from 汉's second cell to 字's second cell
    /// lights exactly columns 2..=5.
    #[test]
    fn selection_overlay_paints_bg_cells_and_whole_wide_glyphs() {
        let mut t = TranscriptComponent::new();
        t.entries.push(TranscriptEntry::Assistant("汉字".into()));
        render(&t, 40, 6); // establish the anchor geometry
        t.selection_begin(3, 0); // 汉's second cell
        assert!(t.selection_drag(5, 0)); // 字's second cell
        let buf = render_buf(&t, 40, 6);
        let sel = Theme::new().selection_bg.bg.expect("selection bg color");
        for x in 2..=5u16 {
            assert_eq!(
                buf.cell((x, 0)).unwrap().bg,
                sel,
                "cell ({x},0) carries the selection bg"
            );
        }
        for x in [0u16, 1, 6, 7, 39] {
            assert_eq!(
                buf.cell((x, 0)).unwrap().bg,
                ratatui::style::Color::Reset,
                "cell ({x},0) untouched"
            );
        }
        // Glyphs themselves are untouched (style-only overlay); the
        // wide glyph's trailing cell (ratatui resets it) is covered
        // by the lead cell's highlight, symbol untouched.
        assert_eq!(buf.cell((2, 0)).unwrap().symbol(), "汉");
        assert_eq!(buf.cell((3, 0)).unwrap().symbol(), " ");
    }

    /// The clear triggers: Esc (in-component), rebuild, merge and
    /// /new all drop an in-flight or persisted selection.
    #[test]
    fn selection_clear_triggers() {
        let mut ctx = test_ctx();

        // Esc: the DismissOverlay arm clears (and still re-follows).
        let mut t = selection_fixture();
        render(&t, 40, 6);
        t.selection_begin(2, 0);
        assert!(t.selection_drag(5, 2));
        t.handle(&Action::DismissOverlay, &mut ctx);
        assert_eq!(t.selection, SelectionState::Normal);

        // rebuild (the recovery path).
        let mut t = selection_fixture();
        render(&t, 40, 6);
        t.selection_begin(2, 0);
        assert!(t.selection_drag(5, 2));
        t.selection_end(&Theme::new()); // persisted
        t.rebuild(&[msg(MessageRole::User, "hi")]);
        assert_eq!(t.selection, SelectionState::Normal);

        // merge_turn (the TurnDone(Ok) boundary).
        let mut t = selection_fixture();
        render(&t, 40, 6);
        t.selection_begin(2, 0);
        assert!(t.selection_drag(5, 2));
        t.merge_turn(&[]);
        assert_eq!(t.selection, SelectionState::Normal);

        // clear (/new).
        let mut t = selection_fixture();
        render(&t, 40, 6);
        t.selection_begin(2, 0);
        assert!(t.selection_drag(5, 2));
        t.clear();
        assert_eq!(t.selection, SelectionState::Normal);

        // The explicit API is always safe.
        t.clear_selection();
        assert_eq!(t.selection, SelectionState::Normal);
    }

    /// Once a drag promoted the press into a selection, the release
    /// NEVER reports Click — even a press that STARTED on the
    /// clickable hint row (click suppression for the whole gesture).
    #[test]
    fn selection_drag_suppresses_the_click_even_from_the_hint_row() {
        let (mut t, _ctx) = pinned_with_hint();
        t.selection_begin(30, 5); // dead-center of the `+1 ↓ 回到底部` hint
        assert!(matches!(t.selection, SelectionState::Pending { .. }));
        assert!(t.selection_drag(10, 3), "dragged away → selecting");
        match t.selection_end(&Theme::new()) {
            SelectionEnd::Selected(text) => assert!(!text.is_empty()),
            other => panic!("unexpected {other:?}"),
        }
        assert!(t.is_pinned(), "no ScrollBottom fired — the pin holds");

        // Contrast: a press-release ON the hint (no drag) still
        // fires the legacy click (the fix-17 regression).
        t.selection_begin(30, 5);
        assert_eq!(t.selection_end(&Theme::new()), SelectionEnd::Click);
        let mut ctx = test_ctx();
        assert_eq!(
            t.handle(&Action::Click(30, 5), &mut ctx),
            Some(Action::ScrollBottom)
        );
    }

    /// Drags outside the transcript area clamp to the viewport
    /// edges (no auto-scroll — P2): a drag to the top of the screen
    /// selects up through the first visible row.
    #[test]
    fn selection_drag_outside_the_area_clamps_to_the_edges() {
        let mut t = TranscriptComponent::new();
        for i in 0..10 {
            t.push_user(&format!("line{i}"));
        }
        render(&t, 40, 6); // 39 rows, following → scroll 33
                           // From entry 9's leading band blank (y=3 → layout 36) up to
                           // the top edge (y=0 → layout 33, entry 8's content row).
        t.selection_begin(5, 3);
        assert!(t.selection_drag(4, 0));
        let theme = Theme::new();
        let (anchor, cursor) = t.selection.range().expect("selecting");
        let text = t.extract_selection_text(&theme, &anchor, &cursor);
        assert!(
            text.starts_with("line8"),
            "covers up through the first visible row: {text:?}"
        );
    }

    // ── fix-23: reasoning collapse (summary + click toggle) ──────────
    // fix-24 reworked the summary rule: flatten (lines trimmed,
    // blanks dropped, single-space join, in-line spaces kept) then
    // TAIL-cut to the available columns with a leading `…`.

    /// Flattening: every line trimmed, blank lines dropped, joined
    /// with ONE space; in-line spaces survive (English stays
    /// readable); a single line passes through trimmed.
    #[test]
    fn reasoning_summary_flattens_lines_with_single_spaces() {
        assert_eq!(
            reasoning_summary("第一行\n第二行\n", 40, "…"),
            "第一行 第二行"
        );
        assert_eq!(
            reasoning_summary("line one\nline two", 40, "…"),
            "line one line two"
        );
        // Blank lines (space/tab-only) collapse into the single
        // joining space — never doubles.
        assert_eq!(reasoning_summary("a\n \n\t\nb\n\n", 40, "…"), "a b");
        // Edges trimmed per line, interior spacing preserved.
        assert_eq!(
            reasoning_summary("  padded  line  \n  next  ", 40, "…"),
            "padded  line next"
        );
        // A single line passes through trimmed.
        assert_eq!(reasoning_summary("  solo  ", 40, "…"), "solo");
        // Nothing usable: empty / all-whitespace blocks.
        assert_eq!(reasoning_summary("", 40, "…"), "");
        assert_eq!(reasoning_summary(" \n\t\n ", 40, "…"), "");
    }

    /// The cut is by DISPLAY WIDTH from the TAIL (the status.rs
    /// lesson): the flattened line's END survives, a leading `…`
    /// marks dropped text, a wide char never straddles the budget,
    /// exact fits carry no ellipsis.
    #[test]
    fn reasoning_summary_keeps_the_tail_with_leading_ellipsis() {
        // 10 CJK chars = 20 cols; avail 9 → `…` + the last 4 chars.
        let s = reasoning_summary(&"字".repeat(10), 9, "…");
        assert_eq!(s, "…字字字字");
        assert_eq!(str_width(&s), 9);
        // Mixed latin+CJK: avail 6, "abc字字" (7 cols) → budget 5 →
        // greedy from the end: 字(2) 字(4) c(5) → "…c字字" (6 cols).
        assert_eq!(reasoning_summary("abc字字", 6, "…"), "…c字字");
        // A wide char never straddles the budget: avail 5, "ab字字"
        // (6 cols) → budget 4 → 字(2) 字(4), 'b' would exceed →
        // "…字字" (5 cols).
        assert_eq!(reasoning_summary("ab字字", 5, "…"), "…字字");
        // Exact fit → no ellipsis (nothing dropped).
        assert_eq!(reasoning_summary("字字字", 6, "…"), "字字字");
        assert_eq!(reasoning_summary("abcdef", 6, "…"), "abcdef");
        // One column over → the ellipsis replaces the front.
        assert_eq!(reasoning_summary("abcdefg", 6, "…"), "…cdefg");
        // A cut landing after a space trims it (whitespace only).
        assert_eq!(reasoning_summary("aaaa bbb", 5, "…"), "…bbb");
        // Degenerate avail 1: the ellipsis alone.
        assert_eq!(reasoning_summary("字字字", 1, "…"), "…");
    }

    /// Committed Reasoning entries render collapsed (one summary row
    /// of the FLATTENED block) by default; the expanded set switches
    /// to the full block. The Meta estimate line keeps its own-entry
    /// position in both states.
    #[test]
    fn committed_reasoning_renders_collapsed_by_default() {
        let mut t = TranscriptComponent::new();
        t.entries
            .push(TranscriptEntry::Reasoning("第一段\n第二段结论".into()));
        let ctx = test_ctx();
        // fix-24: flattened — both lines joined with one space.
        assert_eq!(layout_text(&t, 40, &ctx.theme), vec!["• 第一段 第二段结论"]);
        t.expanded_reasoning.insert(0);
        assert_eq!(
            layout_text(&t, 40, &ctx.theme),
            vec!["• 第一段", "  第二段结论"]
        );
        // fix-23 does not move the Meta estimate line (its own entry,
        // after the reasoning block either way).
        t.entries.push(TranscriptEntry::Meta("~3tok".into()));
        assert_eq!(
            layout_text(&t, 40, &ctx.theme),
            vec!["• 第一段", "  第二段结论", "  ~3tok"]
        );
        t.expanded_reasoning.clear();
        assert_eq!(
            layout_text(&t, 40, &ctx.theme),
            vec!["• 第一段 第二段结论", "  ~3tok"]
        );
    }

    /// The streaming reasoning block collapses to a live-updating
    /// flattened summary; once it overflows the width the TAIL
    /// survives with a leading ellipsis (always the latest
    /// thinking); expansion renders the full block.
    #[test]
    fn streaming_reasoning_summary_updates_per_delta() {
        let mut t = TranscriptComponent::new();
        t.begin_streaming();
        t.push_reasoning("first thought");
        let ctx = test_ctx();
        assert_eq!(layout_text(&t, 40, &ctx.theme), vec!["• first thought"]);
        // A second line joins with a single space — English reads.
        t.push_reasoning("\nsecond thought");
        assert_eq!(
            layout_text(&t, 40, &ctx.theme),
            vec!["• first thought second thought"]
        );
        // Overflow (avail 38): the tail cut keeps 37 cols of the end
        // behind a leading ellipsis. The z-line is exactly avail wide
        // so the EXPANDED block below renders it unwrapped.
        t.push_reasoning(&format!("\n{}", "z".repeat(38)));
        assert_eq!(
            layout_text(&t, 40, &ctx.theme),
            vec![format!("• …{}", "z".repeat(37))]
        );
        // Expanded → the full gutter block, untouched lines.
        t.streaming_reasoning_expanded = true;
        assert_eq!(
            layout_text(&t, 40, &ctx.theme),
            vec![
                "• first thought",
                "  second thought",
                &format!("  {}", "z".repeat(38))
            ]
        );
    }

    /// Click toggle: collapsed row → expanded block → collapsed row.
    /// Both hits consume the action in-component (None).
    #[test]
    fn click_toggles_reasoning_entry_expansion() {
        let mut t = TranscriptComponent::new();
        t.entries
            .push(TranscriptEntry::Reasoning("hidden line\nsummary".into()));
        let mut ctx = test_ctx();
        render(&t, 40, 6); // records the collapsed row's hit rect
        let rect = t.reasoning_hit_rects.borrow()[0].1;
        assert_eq!(
            rect,
            Rect {
                x: 0,
                y: 0,
                width: 40,
                height: 1
            }
        );
        // Collapsed → click → expanded.
        assert_eq!(t.handle(&Action::Click(3, 0), &mut ctx), None);
        assert!(t.expanded_reasoning.contains(&0));
        render(&t, 40, 6); // the expanded block spans two rows
        let rect = t.reasoning_hit_rects.borrow()[0].1;
        assert_eq!(rect.height, 2, "the expanded block is the target");
        // Expanded → click (same screen spot) → collapsed again.
        assert_eq!(t.handle(&Action::Click(3, 0), &mut ctx), None);
        assert!(!t.expanded_reasoning.contains(&0));
    }

    /// The streaming block's collapsed row click-expands — its state
    /// is independent of the committed entries'.
    #[test]
    fn click_toggles_streaming_reasoning_expansion() {
        let mut t = TranscriptComponent::new();
        t.begin_streaming();
        t.push_reasoning("thinking\nlive tail");
        let mut ctx = test_ctx();
        render(&t, 40, 6);
        assert_eq!(
            t.streaming_reasoning_hit_rect.get(),
            Some(Rect {
                x: 0,
                y: 0,
                width: 40,
                height: 1
            })
        );
        assert_eq!(t.handle(&Action::Click(3, 0), &mut ctx), None);
        assert!(t.streaming_reasoning_expanded);
        render(&t, 40, 6);
        assert_eq!(t.streaming_reasoning_hit_rect.get().unwrap().height, 2);
        assert_eq!(t.handle(&Action::Click(3, 0), &mut ctx), None);
        assert!(!t.streaming_reasoning_expanded);
    }

    /// Hint priority over reasoning rows: pinned with both the hint
    /// and a collapsed reasoning row on screen — a click in the hint's
    /// rect returns ScrollBottom WITHOUT touching the reasoning
    /// state; a click on the reasoning row toggles WITHOUT releasing
    /// the pin.
    #[test]
    fn hint_click_wins_over_reasoning_rows_and_vice_versa() {
        let mut t = TranscriptComponent::new();
        t.entries
            .push(TranscriptEntry::Reasoning("reasoning tail".into()));
        for i in 0..10 {
            t.entries.push(TranscriptEntry::User(format!("line{i}")));
        }
        let mut ctx = test_ctx();
        render(&t, 40, 6); // establish the viewport (21 rows)
        t.handle(&Action::ScrollTop, &mut ctx); // pinned at 0, all below
        render(&t, 40, 6); // hint (bottom-right) + reasoning row (top)
        let hint = t.hint_hit_rect.get().expect("hint rendered");
        assert_eq!(
            t.handle(&Action::Click(hint.x + 1, hint.y), &mut ctx),
            Some(Action::ScrollBottom)
        );
        assert!(
            !t.expanded_reasoning.contains(&0),
            "hint hit did not toggle"
        );
        t.handle(&Action::ScrollBottom, &mut ctx); // App feeds it back
        assert!(!t.is_pinned());
        // Back to the top; click the reasoning row itself.
        t.handle(&Action::ScrollTop, &mut ctx);
        render(&t, 40, 6);
        let rect = t.reasoning_hit_rects.borrow()[0].1;
        assert_eq!(t.handle(&Action::Click(rect.x + 2, rect.y), &mut ctx), None);
        assert!(t.expanded_reasoning.contains(&0), "the row toggles");
        assert!(t.is_pinned(), "the toggle never releases the pin");
        assert!(t.hint_hit_rect.get().is_some(), "the hint survives");
    }

    /// Flush/finish continuation: an EXPANDED live block commits
    /// expanded (no visual jump at the boundary); a collapsed one
    /// commits collapsed; the next streaming block always starts
    /// collapsed.
    #[test]
    fn flush_carries_streaming_expansion_into_the_entry() {
        // Expanded flush (tool_start boundary).
        let mut t = TranscriptComponent::new();
        t.begin_streaming();
        t.push_reasoning("deep\nthoughts");
        t.streaming_reasoning_expanded = true;
        t.tool_start("echo", "{}");
        assert!(matches!(t.entries()[0], TranscriptEntry::Reasoning(_)));
        assert!(t.expanded_reasoning.contains(&0), "committed expanded");
        assert!(
            !t.streaming_reasoning_expanded,
            "flag reset for the next block"
        );
        // Collapsed flush.
        let mut t2 = TranscriptComponent::new();
        t2.begin_streaming();
        t2.push_reasoning("deep\nthoughts");
        t2.tool_start("echo", "{}");
        assert!(!t2.expanded_reasoning.contains(&0), "stays collapsed");
        // finish_request carries the state too.
        let mut t3 = TranscriptComponent::new();
        t3.begin_streaming();
        t3.push_reasoning("deep\nthoughts");
        t3.streaming_reasoning_expanded = true;
        t3.finish_request(None, None, None);
        assert!(
            t3.expanded_reasoning.contains(&0),
            "finish commits expanded"
        );
    }

    /// rebuild / merge_turn / /new reset the expansion state (entry
    /// indices die with the entries; Reasoning blocks re-derive
    /// collapsed).
    #[test]
    fn rebuild_and_merge_reset_expansion() {
        let mut t = TranscriptComponent::new();
        t.begin_streaming();
        t.push_reasoning("kept block");
        t.streaming_reasoning_expanded = true;
        t.tool_start("echo", "{}"); // commits expanded at index 0
        assert!(t.expanded_reasoning.contains(&0));
        t.merge_turn(&[msg(MessageRole::User, "go")]);
        assert!(!t.expanded_reasoning.contains(&0), "merge resets");
        assert!(!t.streaming_reasoning_expanded);

        t.expanded_reasoning.insert(0);
        t.rebuild(&[msg(MessageRole::User, "hi")]);
        assert!(t.expanded_reasoning.is_empty(), "rebuild resets");
        t.expanded_reasoning.insert(0);
        t.clear();
        assert!(t.expanded_reasoning.is_empty(), "/new resets");
    }

    // ── fix-25: tool row expansion (call/output detail) ──────────────

    /// Click toggle on a tool row: collapsed head row ⇄ head + dim
    /// call/output detail block; both hits consume in-component.
    #[test]
    fn click_toggles_tool_entry_expansion() {
        let mut t = TranscriptComponent::new();
        t.tool_start("echo", r#"{"text":"hi"}"#);
        let mut ctx = test_ctx();
        render(&t, 40, 6); // records the collapsed head row's hit rect
        let rect = t.tool_hit_rects.borrow()[0].1;
        assert_eq!(
            rect,
            Rect {
                x: 0,
                y: 0,
                width: 40,
                height: 1
            }
        );
        // Collapsed: the head row only, no detail.
        let rows = layout_text(&t, 40, &ctx.theme);
        assert_eq!(rows.len(), 1, "head row only: {rows:?}");
        // Click → expanded: head + 调用 name / full args / 输出 /
        // 运行中… (Running placeholder).
        assert_eq!(t.handle(&Action::Click(3, 0), &mut ctx), None);
        assert!(t.expanded_tools.contains(&0));
        let rows = layout_text(&t, 40, &ctx.theme);
        assert!(rows.iter().any(|r| r == "    调用 echo"), "{rows:?}");
        assert!(
            rows.iter().any(|r| r == r#"    {"text":"hi"}"#),
            "full args wrapped: {rows:?}"
        );
        assert!(rows.iter().any(|r| r == "    输出"), "{rows:?}");
        assert!(rows.iter().any(|r| r == "    运行中…"), "{rows:?}");
        // head + label + args + label + placeholder = 5 rows.
        render(&t, 40, 6);
        assert_eq!(t.tool_hit_rects.borrow()[0].1.height, 5);
        // Click again (same spot, inside the block) → collapsed.
        assert_eq!(t.handle(&Action::Click(3, 0), &mut ctx), None);
        assert!(!t.expanded_tools.contains(&0));
        assert_eq!(layout_text(&t, 40, &ctx.theme).len(), 1);
    }

    /// Output placeholders through the live lifecycle, then the real
    /// text after the fold: Running → 运行中…; a live-path Done (bytes
    /// only) → （待回填）; the rebuild fold fills the text, and the
    /// re-expanded entry shows it.
    #[test]
    fn tool_expansion_output_placeholders_then_fold_fill() {
        let mut t = TranscriptComponent::new();
        t.tool_start("echo", r#"{"text":"hi"}"#);
        t.expanded_tools.insert(0);
        let ctx = test_ctx();
        assert!(layout_text(&t, 40, &ctx.theme)
            .iter()
            .any(|r| r == "    运行中…"));
        t.tool_end("echo", 2, false); // live completion: bytes only
        assert!(
            layout_text(&t, 40, &ctx.theme)
                .iter()
                .any(|r| r == "    （待回填）"),
            "live ToolEnd carries no text"
        );
        // The turn's Tool message folds the full text in (the rebuild
        // path also resets the expansion — re-expand and the output
        // is there).
        t.rebuild(&[
            assistant_with_call("tc-1", "echo"),
            tool_result("tc-1", "echo", "real output"),
        ]);
        assert!(t.expanded_tools.is_empty(), "rebuild reset the toggle");
        t.expanded_tools.insert(0);
        assert!(
            layout_text(&t, 40, &ctx.theme)
                .iter()
                .any(|r| r == "    real output"),
            "folded output renders once expanded"
        );
    }

    /// Both retention paths keep the FULL call and output: the rebuild
    /// derives args from the message and folds the output; the merge
    /// fills a live entry (whose ToolEnd had bytes only).
    #[test]
    fn folds_retain_full_args_and_output_text() {
        // Rebuild path.
        let mut t = TranscriptComponent::new();
        t.rebuild(&[
            assistant_with_call("tc-1", "echo"),
            tool_result("tc-1", "echo", "line1\nline2 output"),
        ]);
        match &t.entries()[0] {
            TranscriptEntry::ToolCall { args, detail, .. } => {
                assert_eq!(args, "{}", "preview unchanged on the head row");
                assert_eq!(detail.args, "{}");
                assert_eq!(detail.output.as_deref(), Some("line1\nline2 output"));
            }
            other => panic!("unexpected {other:?}"),
        }
        // Merge path (live entry).
        let mut t2 = TranscriptComponent::new();
        t2.tool_start("echo", r#"{"text":"hi"}"#);
        t2.tool_end("echo", 2, false);
        t2.merge_turn(&[
            assistant_with_call("t", "echo"),
            tool_result("t", "echo", "merged output"),
        ]);
        match &t2.entries()[0] {
            TranscriptEntry::ToolCall { detail, .. } => {
                assert_eq!(detail.args, r#"{"text":"hi"}"#);
                assert_eq!(detail.output.as_deref(), Some("merged output"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    /// Each detail section caps at 30 wrapped rows; the `…（共 N 行）`
    /// marker names the section's true row count and the cut content
    /// stays hidden.
    /// P1 (restyle-1): diff-shaped output rows (leading +/-) render as
    /// full-width diffAdded/diffRemoved background bands with the
    /// marker column colored; plain rows stay muted.
    #[test]
    fn tool_detail_diff_rows_render_colored_bands() {
        let mut t = TranscriptComponent::new();
        t.entries.push(TranscriptEntry::ToolCall {
            name: "write_file".into(),
            args: "{}".into(),
            status: ToolEntryStatus::Done {
                bytes: 0,
                truncated: false,
                elapsed_ms: None,
            },
            call_id: None,
            detail: ToolEntryDetail {
                args: "{}".into(),
                output: Some("+added line\n-removed line\ncontext".into()),
            },
        });
        t.expanded_tools.insert(0);
        let ctx = test_ctx();
        let lines = t.layout_lines(30, &ctx.theme, 0);
        // Locate the diff rows (after `    输出`).
        let out_label = lines
            .iter()
            .position(|l| l.spans.iter().any(|s| s.content.contains("输出")))
            .expect("output label");
        let added = &lines[out_label + 1];
        let removed = &lines[out_label + 2];
        let context = &lines[out_label + 3];
        // Full-width band: bg on every cell of the row (pad span
        // included), marker colored success/error.
        let added_bg = added
            .spans
            .iter()
            .all(|s| s.style.bg == Some(ratatui::style::Color::Rgb(0x21, 0x3A, 0x2B)));
        let removed_bg = removed
            .spans
            .iter()
            .all(|s| s.style.bg == Some(ratatui::style::Color::Rgb(0x4A, 0x22, 0x1D)));
        assert!(added_bg, "added band full-width: {added:?}");
        assert!(removed_bg, "removed band full-width: {removed:?}");
        assert_eq!(added.spans[1].content, "+");
        assert_eq!(
            added.spans[1].style.fg,
            Some(ratatui::style::Color::Rgb(0x28, 0xC5, 0x67))
        );
        assert_eq!(removed.spans[1].content, "-");
        assert_eq!(
            removed.spans[1].style.fg,
            Some(ratatui::style::Color::Rgb(0xFF, 0x5E, 0x6C))
        );
        // Context row: no band, muted.
        assert!(context.spans.iter().all(|s| s.style.bg.is_none()));
        // Band rows pad out to the full width (30 display cols).
        assert_eq!(added.width(), 30);
        assert_eq!(removed.width(), 30);
    }

    #[test]
    fn tool_detail_sections_cap_at_30_rows() {
        let mut t = TranscriptComponent::new();
        t.entries.push(TranscriptEntry::ToolCall {
            name: "echo".into(),
            args: "{}".into(),
            status: ToolEntryStatus::Done {
                bytes: 0,
                truncated: false,
                elapsed_ms: None,
            },
            call_id: None,
            detail: ToolEntryDetail {
                args: (0..40)
                    .map(|i| format!("arg-line-{i:02}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
                output: Some(
                    (0..35)
                        .map(|i| format!("out-{i:02}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
            },
        });
        t.expanded_tools.insert(0);
        let ctx = test_ctx();
        let rows = layout_text(&t, 40, &ctx.theme);
        let arg_rows = rows
            .iter()
            .filter(|r| r.starts_with("    arg-line"))
            .count();
        assert_eq!(arg_rows, 30, "args capped at 30 rows: {rows:?}");
        assert!(rows.contains(&"    …（共 40 行）".to_owned()), "{rows:?}");
        assert!(
            !rows.iter().any(|r| r.contains("arg-line-30")),
            "cut rows stay hidden"
        );
        let out_rows = rows.iter().filter(|r| r.starts_with("    out-")).count();
        assert_eq!(out_rows, 30, "output capped at 30 rows");
        assert!(rows.contains(&"    …（共 35 行）".to_owned()), "{rows:?}");
    }

    /// Retention storage caps: 4KB args / 8KB output, byte-budgeted
    /// with the marker inside the cap, cut on char boundaries.
    #[test]
    fn stored_detail_text_is_byte_capped_with_marker() {
        assert_eq!(cap_stored_text("abc", ARGS_STORE_CAP), "abc");
        // CJK: 3000 wide chars = 9000 bytes > 4KB → cut whole chars,
        // marker appended, total within the cap.
        let args = cap_stored_text(&"字".repeat(3000), ARGS_STORE_CAP);
        assert!(args.len() <= ARGS_STORE_CAP, "{}", args.len());
        assert!(args.ends_with("（已截断）"));
        // Output cap (8KB): ascii over-long text.
        let out = cap_stored_text(&"z".repeat(OUTPUT_STORE_CAP + 100), OUTPUT_STORE_CAP);
        assert!(out.len() <= OUTPUT_STORE_CAP);
        assert!(out.ends_with("（已截断）"));
        assert!(out.starts_with('z'));
    }

    /// rebuild / merge_turn / /new reset the tool expansion (mirrors
    /// the reasoning rule).
    #[test]
    fn rebuild_and_merge_reset_tool_expansion() {
        let mut t = TranscriptComponent::new();
        t.tool_start("echo", "{}");
        t.expanded_tools.insert(0);
        t.merge_turn(&[]);
        assert!(!t.expanded_tools.contains(&0), "merge resets");
        t.expanded_tools.insert(0);
        t.rebuild(&[]);
        assert!(t.expanded_tools.is_empty(), "rebuild resets");
        t.expanded_tools.insert(0);
        t.clear();
        assert!(t.expanded_tools.is_empty(), "/new resets");
    }

    /// Three click targets on one pinned screen — hint, reasoning
    /// summary, tool head — each hit routes to its own behavior and
    /// nothing leaks across.
    #[test]
    fn hint_reasoning_and_tool_hits_coexist() {
        let mut t = TranscriptComponent::new();
        t.entries
            .push(TranscriptEntry::Reasoning("reasoning tail".into()));
        t.entries.push(TranscriptEntry::ToolCall {
            name: "echo".into(),
            args: "{}".into(),
            status: ToolEntryStatus::Running,
            call_id: None,
            detail: ToolEntryDetail {
                args: "{}".into(),
                output: None,
            },
        });
        for i in 0..10 {
            t.entries.push(TranscriptEntry::User(format!("line{i}")));
        }
        let mut ctx = test_ctx();
        render(&t, 40, 6); // establish the viewport
        t.handle(&Action::ScrollTop, &mut ctx); // pinned at 0
        render(&t, 40, 6); // hint bottom-right, reasoning row 0, tool row 1

        // Tool head click: expands ONLY the tool, pin survives.
        assert_eq!(t.handle(&Action::Click(3, 1), &mut ctx), None);
        assert!(t.expanded_tools.contains(&1));
        assert!(!t.expanded_reasoning.contains(&0));
        assert!(t.is_pinned());
        render(&t, 40, 6);

        // Reasoning row click: expands ONLY the reasoning.
        assert_eq!(t.handle(&Action::Click(3, 0), &mut ctx), None);
        assert!(t.expanded_reasoning.contains(&0));
        assert!(t.expanded_tools.contains(&1), "tool state untouched");

        // Hint click: jumps to the bottom, toggles NOTHING.
        let hint = t.hint_hit_rect.get().expect("hint rendered");
        assert_eq!(
            t.handle(&Action::Click(hint.x + 1, hint.y), &mut ctx),
            Some(Action::ScrollBottom)
        );
        assert!(t.expanded_reasoning.contains(&0) && t.expanded_tools.contains(&1));
        t.handle(&Action::ScrollBottom, &mut ctx);
        assert!(!t.is_pinned());
    }

    // ── Wrap computation ────────────────────────────────────────────────

    #[test]
    fn wrap_preserves_word_boundaries() {
        assert_eq!(wrap_to_width("aaa bbb ccc", 7), vec!["aaa bbb", "ccc"]);
        assert_eq!(wrap_to_width("aaa bbb ccc", 11), vec!["aaa bbb ccc"]);
        assert_eq!(wrap_to_width("aaa bbb ccc", 3), vec!["aaa", "bbb", "ccc"]);
        // Spaces at a wrap point are dropped (no trailing/leading).
        assert_eq!(wrap_to_width("aa bb  cc", 4), vec!["aa", "bb", "cc"]);
    }

    #[test]
    fn wrap_hard_splits_over_long_words() {
        assert_eq!(wrap_to_width("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        // …but a word that fits stays with the row.
        assert_eq!(wrap_to_width("xx yyyy", 5), vec!["xx", "yyyy"]);
    }

    #[test]
    fn wrap_breaks_cjk_per_character() {
        assert_eq!(wrap_to_width("世世世世", 4), vec!["世世", "世世"]);
        assert_eq!(wrap_to_width("a世世b", 4), vec!["a世", "世b"]);
        // Mixed: latin word kept whole, CJK splits freely. The lone
        // trailing 世 borrows 界 from the row above (孤字行防护).
        assert_eq!(
            wrap_to_width("hello 世界世界", 7),
            vec!["hello", "世界", "世界"]
        );
    }

    // ── Kinsoku (禁則) ─────────────────────────────────────────────────

    /// `，` must never open a wrapped line: the break pulls back so
    /// the preceding CJK char moves down with it (content conserved).
    #[test]
    fn wrap_kinsoku_closing_punct_never_starts_a_row() {
        let rows = wrap_to_width("第一行满行，第二行", 8);
        assert_eq!(rows, vec!["第一行满", "行，第", "二行"]);
        assert_eq!(rows.join(""), "第一行满行，第二行");
    }

    /// `（` must never close a wrapped line — it is pushed down to
    /// stay glued to the text that follows.
    #[test]
    fn wrap_kinsoku_opener_never_ends_a_row() {
        assert_eq!(wrap_to_width("说明（注解", 6), vec!["说明", "（注解"]);
    }

    /// 孤字行防护: a lone CJK char on the last row borrows one char
    /// from the row above (7 wide chars at width 6: greedy 3+3+1 →
    /// guarded 3+2+2).
    #[test]
    fn wrap_orphan_guard_rebalances_lone_cjk_tail() {
        assert_eq!(
            wrap_to_width("世界世界世世世", 6),
            vec!["世界世", "界世", "世世"]
        );
    }

    /// ASCII punctuation right after CJK also pulls the break back;
    /// latin words themselves never split (词中拆不复现).
    #[test]
    fn wrap_kinsoku_ascii_punct_and_latin_integrity() {
        assert_eq!(wrap_to_width("如下:然后", 4), vec!["如", "下:", "然后"]);
        // A fitting latin word stays whole between CJK runs.
        assert_eq!(wrap_to_width("中文 word 续", 8), vec!["中文", "word 续"]);
        // Only words longer than the whole line hard-split.
        assert_eq!(wrap_to_width("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
    }

    /// Edge cases at degenerate widths must not panic or overflow the
    /// width contract (every row ≤ width display columns; a lone wide
    /// char is the floor — width 1 cannot halve a CJK char, so the
    /// sweep starts at 2).
    #[test]
    fn wrap_kinsoku_width_contract_on_narrow_widths() {
        for w in 2..=8 {
            for src in ["第一行满行，第二行", "说明（注解）结束", "如下:然后"]
            {
                for row in wrap_to_width(src, w) {
                    assert!(str_width(&row) <= w, "row {row:?} exceeds {w}");
                }
                // Content conservation (no chars lost).
                let joined = wrap_to_width(src, w).join("");
                assert_eq!(joined, src, "content conserved at width {w}");
            }
        }
    }

    #[test]
    fn wrap_edge_cases() {
        assert_eq!(wrap_to_width("", 10), vec![""]);
        assert_eq!(wrap_to_width("   ", 10), vec![""]);
        assert_eq!(wrap_to_width("ab", 1), vec!["a", "b"]);
    }

    #[test]
    fn layout_counts_match_rendered_structure() {
        let mut t = TranscriptComponent::new();
        t.push_user("one two three four five"); // width 20 → band rows
        t.viewport.set((22, 10));
        let ctx = test_ctx();
        let lines = t.layout_lines(20, &ctx.theme, 0);
        // restyle-1: full-width band = blank + 2 content rows (avail
        // 16: "one two three" / "four five") + blank; the `› ` anchor
        // leads the first content row.
        assert_eq!(lines.len(), 4);
        let total: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
            .collect();
        assert_eq!(total[0], " ".repeat(20));
        assert_eq!(total[1], "  › one two three".to_owned() + &" ".repeat(3));
        assert_eq!(total[2], "    four five".to_owned() + &" ".repeat(7));
        assert_eq!(total[3], " ".repeat(20));
    }

    #[test]
    fn layout_emits_tool_lines_with_status() {
        let mut t = TranscriptComponent::new();
        t.rebuild(&[
            assistant_with_call("t1", "echo"),
            tool_result("t1", "echo", "ok"),
            assistant_with_call("t2", "grep"),
            tool_result("t2", "grep", "Error: x"),
        ]);
        let ctx = test_ctx();
        let lines = t.layout_lines(40, &ctx.theme, 0);
        let joined: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
            .collect();
        assert_eq!(joined.len(), 2, "one line per tool entry");
        // restyle-1: connectors + markers + English verbs; the first
        // row connects to the second (`├`), the second closes
        // (`└`). Output text known → `1 output line` suffix.
        assert!(
            joined[0].starts_with("├ ✓ Used echo {} · 1 output line"),
            "{}",
            joined[0]
        );
        assert!(joined[1].starts_with("└ × Searched {}"), "{}", joined[1]);
        assert!(joined[1].contains("Error: x"), "{}", joined[1]);
    }

    #[test]
    fn tool_verb_table() {
        assert_eq!(tool_verb("shell", true), "Running");
        assert_eq!(tool_verb("shell", false), "Ran");
        assert_eq!(tool_verb("filesystem_bash", true), "Running");
        assert_eq!(tool_verb("read_file", true), "Reading");
        assert_eq!(tool_verb("read_file", false), "Read");
        assert_eq!(tool_verb("read_skill", false), "Read");
        assert_eq!(tool_verb("write_file", true), "Writing");
        assert_eq!(tool_verb("write_file", false), "Wrote");
        assert_eq!(tool_verb("edit_file", false), "Wrote");
        assert_eq!(tool_verb("call_agent", true), "Delegating");
        assert_eq!(tool_verb("call_agent", false), "Delegated");
        assert_eq!(tool_verb("glob", true), "Searching");
        assert_eq!(tool_verb("grep", false), "Searched");
        assert_eq!(tool_verb("search", true), "Searching");
        assert_eq!(tool_verb("run_code", true), "Using run_code");
        assert_eq!(tool_verb("current_time", false), "Used current_time");
    }

    /// Borderless spacing rules: one blank line before each block entry
    /// (assistant / turn marker), none between consecutive tool rows,
    /// none before the very first line.
    #[test]
    fn layout_block_spacing_rules() {
        let mut t = TranscriptComponent::new();
        t.rebuild(&[
            msg(MessageRole::User, "go"),
            assistant_with_call("t1", "read_file"),
            tool_result("t1", "read_file", "ok"),
            assistant_with_call("t2", "grep"),
            tool_result("t2", "grep", "ok"),
            msg(MessageRole::Assistant, "done"),
        ]);
        t.set_turn_meta("main".to_owned(), 9, None);
        let ctx = test_ctx();
        let lines = t.layout_lines(60, &ctx.theme, 0);
        let joined: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
            .collect();
        // restyle-1: the user band (blank + `›` row + blank, all
        // bg-padded to the full width), compact tool rows with
        // connectors/verbs, block gaps before assistant/marker, the
        // `●` assistant anchor, the `└` turn marker.
        let band = " ".repeat(60);
        let user_row = "  › go".to_owned() + &" ".repeat(60 - 6);
        assert_eq!(
            joined,
            vec![
                band.clone(),
                user_row,
                band,
                "├ ✓ Read {} · 1 output line".to_owned(),
                "└ ✓ Searched {} · 1 output line".to_owned(),
                "".to_owned(),
                "● done".to_owned(),
                "".to_owned(),
                "└ main · 9s".to_owned(),
            ]
        );
    }

    #[test]
    fn turn_marker_rendering_rules() {
        let mut t = TranscriptComponent::new();
        t.rebuild(&[
            msg(MessageRole::User, "go"),
            msg(MessageRole::Assistant, "ok"),
        ]);
        let ctx = test_ctx();
        // No meta → no marker.
        let plain = t.layout_lines(40, &ctx.theme, 0);
        assert!(plain
            .iter()
            .all(|l| !l.spans.iter().any(|s| s.content.contains("└"))));

        t.set_turn_meta("fast".to_owned(), 3, None);
        let lines = t.layout_lines(40, &ctx.theme, 0);
        let last = lines.last().expect("marker line");
        // restyle-1: `└ fast · 3s` — connector muted, body
        // muted.
        assert_eq!(last.spans.len(), 2);
        assert_eq!(last.spans[0].content, "└ ");
        assert_eq!(last.spans[0].style, ctx.theme.turn_marker);
        assert_eq!(last.spans[1].content, "fast · 3s");
        assert_eq!(last.spans[1].style, ctx.theme.turn_marker);

        // `/new` resets the marker along with everything else.
        t.clear();
        let cleared = t.layout_lines(40, &ctx.theme, 0);
        assert_eq!(cleared.len(), 2, "wordmark + hint rows");
    }

    /// ascii-turn-1: the pure-ASCII icon tier renders the turn
    /// marker with dedicated chrome — `+` prefix and `-` separators
    /// (NOT the shared `elbow`/`dot` slots), while the `^v` arrows
    /// and `~` zap ride their slots untouched. Exact shape with the
    /// full usage tail: rate = round(44/13) = 3.
    #[test]
    fn turn_marker_ascii_tier_plus_and_dash() {
        let mut t = TranscriptComponent::new();
        t.rebuild(&[
            msg(MessageRole::User, "go"),
            msg(MessageRole::Assistant, "ok"),
        ]);
        t.set_turn_meta("intern-latest".to_owned(), 13, Some((1200, 44)));
        let mut ctx = test_ctx();
        ctx.theme = Theme::dark().with_icons(crate::icons::Icons::Ascii);
        let lines = t.layout_lines(60, &ctx.theme, 0);
        let last = lines.last().expect("marker line");
        let joined: String = last.spans.iter().map(|s| s.content.clone()).collect();
        assert_eq!(joined, "+ intern-latest - 13s - ^1200 v44 - ~ 3 tok/s");
    }

    /// ascii-turn-1 guard: unicode/nerd tiers keep the shared
    /// `elbow`/`dot` slots byte-for-byte, including the full usage
    /// tail the no-tokens test above skips.
    #[test]
    fn turn_marker_unicode_tier_unchanged() {
        let mut t = TranscriptComponent::new();
        t.rebuild(&[
            msg(MessageRole::User, "go"),
            msg(MessageRole::Assistant, "ok"),
        ]);
        t.set_turn_meta("intern-latest".to_owned(), 13, Some((1200, 44)));
        let ctx = test_ctx();
        let lines = t.layout_lines(60, &ctx.theme, 0);
        let last = lines.last().expect("marker line");
        assert_eq!(last.spans[0].content, "└ ");
        let joined: String = last.spans.iter().map(|s| s.content.clone()).collect();
        assert_eq!(joined, "└ intern-latest · 13s · ↑1200 ↓44 · ⚡ 3 tok/s");
    }

    /// ascii-turn-1 guard: the shared slots keep their ascii values
    /// on TOOL rows — `` ` `` elbow connector, `.` dot in the
    /// output-lines suffix — the marker's dedicated `+`/`-` chrome
    /// must not leak into other rows.
    #[test]
    fn tool_rows_ascii_tier_keep_shared_slots() {
        let mut t = TranscriptComponent::new();
        t.rebuild(&[
            assistant_with_call("t1", "read_file"),
            tool_result("t1", "read_file", "ok"),
        ]);
        let mut ctx = test_ctx();
        ctx.theme = Theme::dark().with_icons(crate::icons::Icons::Ascii);
        let lines = t.layout_lines(60, &ctx.theme, 0);
        let joined: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
            .collect();
        let tool_row = joined
            .iter()
            .find(|l| l.contains("Read"))
            .expect("tool row present");
        assert_eq!(tool_row, "` + Read {} . 1 output line");
    }

    #[test]
    fn formatting_helpers() {
        assert_eq!(format_ms(0), "0ms");
        assert_eq!(format_ms(800), "800ms");
        assert_eq!(format_ms(1_250), "1s");
        assert_eq!(format_ms(64_000), "1m04s");
        assert_eq!(format_bytes(0), "0B");
        assert_eq!(format_bytes(128), "128B");
        assert_eq!(format_bytes(1_228), "1.2kB");
        assert_eq!(format_bytes(3_600_000), "3.4MB");
    }

    // ── Per-request meta lines ─────────────────────────────────────────

    #[test]
    fn meta_reasoning_line_estimates_from_chars() {
        // 220 chars / 2.2 → ~100tok; 100tok / 4s → 25tok/s. No
        // duration segment (format convergence with the usage line).
        assert_eq!(
            meta_reasoning_line(220, Duration::from_secs_f64(4.0)),
            "~100tok · ~25tok/s"
        );
        // Rounding: 5 chars → ~2tok.
        assert_eq!(
            meta_reasoning_line(5, Duration::from_secs_f64(1.0)),
            "~2tok · ~2tok/s"
        );
        // Zero elapsed: rate segment omitted (no zero division).
        assert_eq!(meta_reasoning_line(22, Duration::ZERO), "~10tok");
        // Long blocks keep the minutes vocabulary in the RATE math.
        assert_eq!(
            meta_reasoning_line(220, Duration::from_secs_f64(64.0)),
            "~100tok · ~2tok/s"
        );
    }

    #[test]
    fn meta_request_line_exact_counts_ttft_and_cached_branch() {
        let mut usage = Usage {
            input_tokens: 50,
            output_tokens: 10,
            cached_input_tokens: None,
        };
        // FirstToken observed → the ttft segment after the counts;
        // rate = output / total elapsed.
        assert_eq!(
            meta_request_line(
                &usage,
                Some(Duration::from_millis(800)),
                Duration::from_secs_f64(2.0)
            ),
            "↑50 ↓10 · ttft 0.8s · 5tok/s"
        );
        // cached reported → ⎓ segment (width-1 U+2393).
        usage.cached_input_tokens = Some(3);
        assert_eq!(
            meta_request_line(
                &usage,
                Some(Duration::from_millis(800)),
                Duration::from_secs_f64(2.0)
            ),
            "↑50 ↓10 ⎓3 · ttft 0.8s · 5tok/s"
        );
        // No FirstToken → the ttft segment is omitted ENTIRELY (never
        // a bare `ttft` placeholder).
        assert_eq!(
            meta_request_line(&usage, None, Duration::from_secs_f64(2.0)),
            "↑50 ↓10 ⎓3 · 5tok/s"
        );
        // Zero elapsed: no rate; ttft still renders.
        assert_eq!(
            meta_request_line(&usage, Some(Duration::from_millis(500)), Duration::ZERO),
            "↑50 ↓10 ⎓3 · ttft 0.5s"
        );
        // No ttft AND no elapsed: the bare counts.
        assert_eq!(
            meta_request_line(&usage, None, Duration::ZERO),
            "↑50 ↓10 ⎓3"
        );
        // The new segment is pure ASCII (it rides the dim meta row).
        let line = meta_request_line(
            &usage,
            Some(Duration::from_millis(812)),
            Duration::from_secs_f64(1.0),
        );
        assert!(line.contains("ttft 0.8s"), "{line}");
        // Width contract: the meta glyphs are width-1 (EAW-ambiguous
        // resolving narrow in the non-CJK context ratatui computes) so
        // the dim row stays column-stable.
        assert_eq!(str_width("⎓"), 1);
        assert_eq!(str_width("↑"), 1);
        assert_eq!(str_width("↓"), 1);
    }

    #[test]
    fn finish_request_attaches_meta_lines_in_block_order() {
        let mut t = TranscriptComponent::new();
        t.begin_streaming();
        t.push_reasoning("thinking hard");
        t.push_delta("the answer");
        t.finish_request(
            Some(Usage {
                input_tokens: 50,
                output_tokens: 10,
                cached_input_tokens: None,
            }),
            Some(Duration::from_secs_f64(2.0)),
            Some(Duration::from_millis(600)),
        );
        let entries = t.entries();
        // Block tails, not both at the end: reasoning → its estimate →
        // answer. The exact usage line is HELD (fix-19) until a
        // boundary proves where the step ended.
        assert!(matches!(&entries[0], TranscriptEntry::Reasoning(text) if text == "thinking hard"));
        match &entries[1] {
            TranscriptEntry::Meta(text) => {
                assert!(text.starts_with('~'), "estimate marker: {text}");
                assert!(text.contains("tok"), "estimate: {text}");
                assert!(
                    !text.contains("ttft"),
                    "the estimate line carries no ttft: {text}"
                );
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(&entries[2], TranscriptEntry::Assistant(text) if text == "the answer"));
        assert_eq!(
            entries.len(),
            3,
            "usage line held, not committed: {entries:?}"
        );
        // Flushing the hold (tool-less step: the next RequestStart /
        // turn end) lands it at the answer's tail.
        t.flush_pending_step_meta();
        match &t.entries()[3] {
            TranscriptEntry::Meta(text) => {
                assert!(text.starts_with("↑50 ↓10"), "exact line: {text}");
                assert!(text.contains("ttft 0.6s"), "ttft segment: {text}");
                assert!(text.ends_with("5tok/s"), "rate: {text}");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(t.entries().len(), 4);
        // Streaming area is clean for the next step.
        assert!(t.streaming_text().is_empty());
    }

    #[test]
    fn finish_request_without_usage_omits_the_request_line() {
        let mut t = TranscriptComponent::new();
        t.begin_streaming();
        t.push_reasoning("only thoughts");
        t.finish_request(
            None,
            Some(Duration::from_secs(1)),
            Some(Duration::from_millis(300)),
        );
        // Estimate line only — no Usage event, no exact line.
        assert!(matches!(&t.entries()[1], TranscriptEntry::Meta(text) if text.starts_with('~')));
        assert_eq!(t.entries().len(), 2);
    }

    #[test]
    fn finish_request_holds_meta_tool_start_lands_it_below_the_tool_row() {
        // fix-19: the engine fires RequestEnd BEFORE this step's tools
        // execute — the usage line is HELD at RequestEnd and lands
        // directly BELOW the tool row (the stats describe the request
        // whose tool calls this is), not above it.
        let mut t = TranscriptComponent::new();
        t.begin_streaming();
        t.push_delta("answer first");
        t.finish_request(
            Some(Usage {
                input_tokens: 5,
                output_tokens: 5,
                cached_input_tokens: None,
            }),
            Some(Duration::from_millis(500)),
            None,
        );
        // Held: no meta entry yet.
        assert!(
            !t.entries()
                .iter()
                .any(|e| matches!(e, TranscriptEntry::Meta(_))),
            "usage line held: {:?}",
            t.entries()
        );
        t.tool_start("echo", "{}");
        let positions: Vec<usize> = t
            .entries()
            .iter()
            .enumerate()
            .filter_map(|(i, e)| matches!(e, TranscriptEntry::Meta(_)).then_some(i))
            .collect();
        let tool = t
            .entries()
            .iter()
            .position(|e| matches!(e, TranscriptEntry::ToolCall { .. }))
            .expect("tool entry");
        assert_eq!(positions, vec![2], "one meta line, below the tool row");
        assert!(positions[0] > tool, "meta lands AFTER the tool entry");
    }

    #[test]
    fn reasoning_timer_resets_between_blocks() {
        let mut t = TranscriptComponent::new();
        t.begin_streaming();
        t.push_reasoning("first block");
        t.finish_request(None, None, None);
        assert!(t.reasoning_started.is_none(), "cleared by the flush");
        // Second block restarts the timer on its first delta.
        t.push_reasoning("second block");
        assert!(t.reasoning_started.is_some());
    }

    // ── fix-19 ①: live streaming stats row ────────────────────────────

    /// Snapshot of the streaming answer area's rows at a controlled
    /// instant (`layout_lines_at` — the injectable-clock seam).
    fn rows_at(t: &TranscriptComponent, now: Instant) -> Vec<String> {
        let ctx = test_ctx();
        t.layout_lines_at(60, &ctx.theme, now)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
            .collect()
    }

    /// No live row before the first ANSWER delta — reasoning-only
    /// streaming (even with the ttft already frozen) shows nothing:
    /// the row lives in the answer area, and the status-bar spinner
    /// is the activity signal while thinking.
    #[test]
    fn live_stats_row_hidden_before_first_answer_delta() {
        let mut t = TranscriptComponent::new();
        t.begin_streaming();
        t.set_live_ttft(Some(Duration::from_millis(800)));
        t.push_reasoning("pondering deeply");
        let started = Instant::now();
        assert_eq!(
            rows_at(&t, started + Duration::from_secs(2)),
            vec!["• pondering deeply"],
            "no live row while only reasoning streams (collapsed summary)"
        );
    }

    /// Once the answer streams the row renders `ttft S.Ss · ~Rtok/s`,
    /// recomputed EVERY FRAME: same buffer, later clock → slower
    /// rate; more deltas → faster rate. ASCII apart from the meta
    /// rows' width-1 `·` separator, muted like the meta rows.
    #[test]
    fn live_stats_row_updates_every_frame() {
        let mut t = TranscriptComponent::new();
        t.begin_streaming();
        t.set_live_ttft(Some(Duration::from_millis(800)));
        t.push_delta(&"x".repeat(220)); // → ~100tok
        let t0 = t.answer_started.expect("first delta armed the timer");

        let at_2s = rows_at(&t, t0 + Duration::from_secs(2));
        assert_eq!(at_2s[0], "  ttft 0.8s · ~50tok/s", "100tok / 2s");
        assert!(
            at_2s.len() > 1 && at_2s.last().unwrap().ends_with('▍'),
            "the md-rendered answer follows with its tail cursor: {at_2s:?}"
        );
        // Later frame, same buffer: the rate drops (no new content).
        let at_4s = rows_at(&t, t0 + Duration::from_secs(4));
        assert_eq!(at_4s[0], "  ttft 0.8s · ~25tok/s");
        // More deltas arrive: the numerator grows mid-stream.
        t.push_delta(&"y".repeat(220)); // → ~200tok total
        let at_4s_more = rows_at(&t, t0 + Duration::from_secs(4));
        assert_eq!(at_4s_more[0], "  ttft 0.8s · ~50tok/s");

        // ASCII(+`·`) and muted-style contract on the stats row.
        assert!(
            at_2s[0].chars().all(|c| c.is_ascii() || c == '·'),
            "live row keeps meta-row glyphs only: {:?}",
            at_2s[0]
        );
        let ctx = test_ctx();
        let line = t.layout_lines_at(60, &ctx.theme, t0 + Duration::from_secs(2))[0].clone();
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.spans[0].style, ctx.theme.muted);
    }

    /// The rate segment waits out the floor (a microseconds-old block
    /// would show an absurd rate); with no ttft either, the row is
    /// suppressed entirely until something is measurable.
    #[test]
    fn live_stats_row_rate_floor() {
        let mut t = TranscriptComponent::new();
        t.begin_streaming();
        t.set_live_ttft(Some(Duration::from_millis(800)));
        t.push_delta("short"); // 5 chars → ~2tok
        let t0 = t.answer_started.unwrap();
        // 100ms in: ttft only, no rate.
        assert_eq!(
            rows_at(&t, t0 + Duration::from_millis(100))[0],
            "  ttft 0.8s"
        );
        // Exactly at the floor: the rate joins (~2tok / 0.3s → 7).
        assert_eq!(
            rows_at(&t, t0 + Duration::from_secs_f64(0.3))[0],
            "  ttft 0.8s · ~7tok/s"
        );
        // No ttft (FirstToken never observed) + below floor: no row.
        let mut t2 = TranscriptComponent::new();
        t2.begin_streaming();
        t2.push_delta("short");
        let t0b = t2.answer_started.unwrap();
        let rows = rows_at(&t2, t0b + Duration::from_millis(50));
        assert_eq!(rows.len(), 1, "only the answer row: {rows:?}");
        assert!(!rows[0].contains("ttft"), "no ttft segment: {rows:?}");
        // Past the floor without ttft: the rate alone renders.
        assert_eq!(rows_at(&t2, t0b + Duration::from_secs(2))[0], "  ~1tok/s");
    }

    /// `RequestEnd` kills the live row (the held exact line is its
    /// successor); the timers reset so a later block re-arms cleanly.
    #[test]
    fn live_stats_row_dies_at_request_end() {
        let mut t = TranscriptComponent::new();
        t.begin_streaming();
        t.set_live_ttft(Some(Duration::from_millis(800)));
        t.push_delta("streaming answer");
        let t0 = t.answer_started.unwrap();
        assert!(rows_at(&t, t0 + Duration::from_secs(2))[0].contains("ttft"));

        t.finish_request(
            Some(Usage {
                input_tokens: 50,
                output_tokens: 10,
                cached_input_tokens: None,
            }),
            Some(Duration::from_secs_f64(2.0)),
            Some(Duration::from_millis(800)),
        );
        assert!(t.answer_started.is_none() && t.live_ttft.is_none());
        let now = Instant::now();
        let rows = rows_at(&t, now);
        assert!(
            rows.iter()
                .all(|r| !r.contains("tok/s") || r.starts_with("↑")),
            "no live row after RequestEnd (held line is separate): {rows:?}"
        );
        // A second answer block re-arms the timer on its first delta.
        t.push_delta("next block");
        assert!(t.answer_started.is_some());
    }

    // ── fix-19 ②: held step meta (usage below tool rows) ─────────────

    /// The hold→flush state machine against a step separator: the
    /// engine's RequestEnd separator lands while the meta is held;
    /// the flush SWAPS the meta ahead of the separator so it stays at
    /// the answer's tail (no blank line between answer and its stats).
    #[test]
    fn held_meta_swaps_ahead_of_a_landed_step_separator() {
        let mut t = TranscriptComponent::new();
        t.push_user("go");
        t.begin_streaming();
        t.push_delta("the answer");
        t.finish_request(
            Some(Usage {
                input_tokens: 5,
                output_tokens: 5,
                cached_input_tokens: None,
            }),
            Some(Duration::from_millis(500)),
            None,
        );
        t.step_end(); // the App's RequestEnd handler fires this
        assert!(matches!(
            t.entries().last(),
            Some(TranscriptEntry::StepBreak)
        ));
        t.flush_pending_step_meta();
        assert!(matches!(
            t.entries(),
            [
                TranscriptEntry::User(_),
                TranscriptEntry::Assistant(_),
                TranscriptEntry::Meta(_),
                TranscriptEntry::StepBreak,
            ]
        ));
    }

    /// A tool-LESS step's hold flushes at the NEXT request's end,
    /// above the new step's blocks (stale-first ordering).
    #[test]
    fn stale_held_meta_flushes_before_next_step_content() {
        let mut t = TranscriptComponent::new();
        t.begin_streaming();
        t.push_delta("answer one");
        t.finish_request(
            Some(Usage {
                input_tokens: 5,
                output_tokens: 5,
                cached_input_tokens: None,
            }),
            None,
            None,
        );
        // No tools came; the next request streams and ends.
        t.push_reasoning("think two");
        t.push_delta("answer two");
        t.finish_request(
            Some(Usage {
                input_tokens: 7,
                output_tokens: 7,
                cached_input_tokens: None,
            }),
            None,
            None,
        );
        assert!(matches!(
            t.entries(),
            [
                TranscriptEntry::Assistant(a1),
                TranscriptEntry::Meta(m1),
                TranscriptEntry::Reasoning(_),
                TranscriptEntry::Meta(_), // step-2 reasoning estimate
                TranscriptEntry::Assistant(a2),
            ] if a1 == "answer one" && m1.starts_with("↑5 ↓5") && a2 == "answer two"
        ));
        // The second hold is pending (exactly one slot).
        t.flush_pending_step_meta();
        assert!(matches!(
            t.entries().last(),
            Some(TranscriptEntry::Meta(m2)) if m2.starts_with("↑7 ↓7")
        ));
    }

    /// Multi-step pairing (每步配对): step 1's meta below its tool row,
    /// step 2's (tool-less) at its answer tail — the fix-19 target
    /// shape `[reasoning][answer][tool][meta1] [reasoning][answer][meta2]`.
    #[test]
    fn held_meta_pairs_per_step() {
        let mut t = TranscriptComponent::new();
        t.push_user("go");
        t.begin_streaming();
        // Step 1: reasoning + answer + tool.
        t.push_reasoning("think 1");
        t.push_delta("checking");
        t.finish_request(
            Some(Usage {
                input_tokens: 50,
                output_tokens: 10,
                cached_input_tokens: None,
            }),
            None,
            None,
        );
        t.tool_start("echo", "{}");
        t.tool_end("echo", 2, false);
        // Step 2: reasoning + final answer, no tools.
        t.push_reasoning("think 2");
        t.push_delta("final");
        t.finish_request(
            Some(Usage {
                input_tokens: 80,
                output_tokens: 5,
                cached_input_tokens: None,
            }),
            None,
            None,
        );
        t.flush_pending_step_meta();
        let kinds: Vec<&str> = t.entries().iter().map(kind_of).collect();
        assert_eq!(
            kinds,
            vec![
                "user",
                "reasoning",
                "meta_est",
                "assistant",
                "tool_done",
                "meta_usage",
                "reasoning",
                "meta_est",
                "assistant",
                "meta_usage",
            ],
            "step1 meta below its tool, step2 meta at its answer tail"
        );
    }

    /// Interrupted 兜底: the turn ends (merge path) with a hold still
    /// pending — the merge flushes it at the answer's tail so the
    /// stats the user watched live survive.
    #[test]
    fn merge_turn_flushes_a_held_meta() {
        let mut t = TranscriptComponent::new();
        t.push_user("go");
        t.begin_streaming();
        t.push_delta("partial answer");
        t.finish_request(
            Some(Usage {
                input_tokens: 9,
                output_tokens: 9,
                cached_input_tokens: None,
            }),
            None,
            None,
        );
        // TurnDone(Ok Interrupted) — merge, not rebuild.
        t.merge_turn(&[msg(MessageRole::User, "go")]);
        assert!(matches!(
            t.entries(),
            [
                TranscriptEntry::User(_),
                TranscriptEntry::Assistant(_),
                TranscriptEntry::Meta(m),
            ] if m.starts_with("↑9 ↓9")
        ));
        assert!(t.pending_step_meta.is_none(), "merge consumed the hold");
    }

    // ── Streaming edge-blank-line trim (defect A) ─────────────────────

    /// Buffers accumulating model output that opens/closes with `\n\n`
    /// render through an edge-trimmed view: only the content rows
    /// show, no runs of blank `▍`/`┆` gutter rows — and the buffer
    /// itself keeps the raw text.
    #[test]
    fn streaming_render_trims_edge_blank_lines() {
        let mut t = TranscriptComponent::new();
        t.begin_streaming();
        t.push_reasoning("\n\n思考过程\n");
        t.push_delta("\n\n最终答案\n\n");
        let ctx = test_ctx();
        assert_eq!(
            layout_text(&t, 40, &ctx.theme),
            vec!["• 思考过程", "", "● 最终答案▍"],
            "no leading/trailing blank gutter rows"
        );
        // Buffers untouched (render-time view only).
        assert_eq!(t.streaming_text(), "\n\n最终答案\n\n");
    }

    /// Interior blank lines (paragraph separators) survive the trim —
    /// only the EDGES are trimmed.
    #[test]
    fn streaming_render_keeps_interior_blank_lines() {
        let mut t = TranscriptComponent::new();
        t.begin_streaming();
        t.push_delta("\n\n第一段\n\n第二段\n\n");
        let ctx = test_ctx();
        assert_eq!(
            layout_text(&t, 40, &ctx.theme),
            vec!["● 第一段", "  ", "  第二段▍"]
        );
    }

    /// A whitespace-only buffer renders NOTHING (no phantom gutter
    /// rows) even though it is not empty.
    #[test]
    fn streaming_whitespace_only_buffer_renders_nothing() {
        let mut t = TranscriptComponent::new();
        t.begin_streaming();
        t.push_reasoning("  \n  ");
        let ctx = test_ctx();
        assert_eq!(layout_text(&t, 40, &ctx.theme), Vec::<String>::new());
    }

    /// The flush commits edge-trimmed text: committed blocks carry no
    /// leading/trailing blank lines (whitespace-only buffers still
    /// drop entirely).
    #[test]
    fn flush_commits_edge_trimmed_blocks() {
        let mut t = TranscriptComponent::new();
        t.begin_streaming();
        t.push_reasoning("\n\n思考\n\n");
        t.push_delta("\n\n答案\n");
        t.tool_start("echo", "{}");
        assert_eq!(
            &t.entries()[..2],
            &[
                TranscriptEntry::Reasoning("思考".into()),
                TranscriptEntry::Assistant("答案".into()),
            ]
        );
    }

    // ── Merged meta for empty answers (defect B) ──────────────────────

    /// Tool-call-only steps (content empty, reasoning streamed) merge
    /// the reasoning estimate and the request usage into ONE meta
    /// line — never two stacked metas with no output block between.
    #[test]
    fn finish_request_empty_answer_merges_meta_into_one_line() {
        let mut t = TranscriptComponent::new();
        t.begin_streaming();
        t.push_reasoning("thinking hard"); // 13 chars → ~6tok
        t.finish_request(
            Some(Usage {
                input_tokens: 1221,
                output_tokens: 96,
                cached_input_tokens: None,
            }),
            Some(Duration::from_secs_f64(2.6)),
            None,
        );
        // Held (fix-19): only the reasoning block committed so far.
        assert_eq!(
            t.entries(),
            &[TranscriptEntry::Reasoning("thinking hard".into())]
        );
        // The tool-less step's flush lands the MERGED line at the
        // reasoning tail — exactly one meta, never two stacked.
        t.flush_pending_step_meta();
        assert_eq!(
            t.entries(),
            &[
                TranscriptEntry::Reasoning("thinking hard".into()),
                TranscriptEntry::Meta("~6tok · ↑1221 ↓96 · 37tok/s".into()),
            ]
        );
    }

    /// The merged line carries the cached and ttft segments too.
    #[test]
    fn finish_request_merged_meta_carries_cached() {
        let mut t = TranscriptComponent::new();
        t.begin_streaming();
        t.push_reasoning("thinking");
        t.finish_request(
            Some(Usage {
                input_tokens: 100,
                output_tokens: 5,
                cached_input_tokens: Some(80),
            }),
            Some(Duration::from_secs(1)),
            Some(Duration::from_millis(500)),
        );
        t.flush_pending_step_meta();
        assert!(matches!(
            t.entries().last(),
            Some(TranscriptEntry::Meta(m)) if m == "~4tok · ↑100 ↓5 ⎓80 · ttft 0.5s · 5tok/s"
        ));
    }

    /// Content present keeps the two-segment form (reasoning meta at
    /// its block tail + the HELD usage line landing at the answer's
    /// tail on the tool-less flush).
    #[test]
    fn finish_request_with_answer_keeps_two_meta_segments() {
        let mut t = TranscriptComponent::new();
        t.begin_streaming();
        t.push_reasoning("thinking hard");
        t.push_delta("the answer");
        t.finish_request(
            Some(Usage {
                input_tokens: 50,
                output_tokens: 10,
                cached_input_tokens: None,
            }),
            Some(Duration::from_secs_f64(2.0)),
            None,
        );
        let entries = t.entries();
        assert_eq!(entries.len(), 3);
        assert!(matches!(&entries[1], TranscriptEntry::Meta(m) if m.starts_with('~')));
        t.flush_pending_step_meta();
        assert!(matches!(
            &t.entries()[3],
            TranscriptEntry::Meta(m) if m.starts_with("↑50 ↓10")
        ));
    }

    /// Both buffers empty (no reasoning, no answer): the plain usage
    /// line alone — still exactly one meta (held, then flushed).
    #[test]
    fn finish_request_all_empty_yields_single_usage_line() {
        let mut t = TranscriptComponent::new();
        t.begin_streaming();
        t.finish_request(
            Some(Usage {
                input_tokens: 50,
                output_tokens: 10,
                cached_input_tokens: None,
            }),
            Some(Duration::from_secs_f64(2.0)),
            Some(Duration::from_millis(700)),
        );
        assert!(t.entries().is_empty(), "held, nothing committed yet");
        t.flush_pending_step_meta();
        assert_eq!(
            t.entries(),
            &[TranscriptEntry::Meta("↑50 ↓10 · ttft 0.7s · 5tok/s".into())]
        );
    }

    // ── Markdown tables (defect C) + list hanging indent ──────────────

    /// Finalized assistant tables render pipes-dropped with aligned
    /// columns and a BOLD header; the separator row is gone.
    #[test]
    fn assistant_markdown_table_renders_aligned_columns() {
        let mut t = TranscriptComponent::new();
        t.entries.push(TranscriptEntry::Assistant(
            "| 类别 | 要点 |\n|---|---|\n| 代码 | 说明内容 |".into(),
        ));
        let ctx = test_ctx();
        let rows = styled_rows(&t, 40, &ctx.theme);
        let texts: Vec<&str> = rows.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(texts, vec!["● 类别  要点", "  代码  说明内容"]);
        // Header BOLD, body default.
        assert!(rows[0]
            .1
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD));
        assert_eq!(rows[1].1, ctx.theme.assistant);
    }

    /// Streaming tables render live: a pipe header row WITHOUT its
    /// separator yet is not a table (raw pipes, body style); the
    /// moment the separator row arrives the aligned layout kicks in
    /// (pipes dropped, separator consumed, header BOLD).
    #[test]
    fn streaming_table_renders_when_the_separator_arrives() {
        let mut t = TranscriptComponent::new();
        t.begin_streaming();
        t.push_delta("| a | b |");
        let ctx = test_ctx();
        let rows = styled_rows(&t, 20, &ctx.theme);
        assert_eq!(rows[0].0, "● | a | b |▍");
        assert_eq!(rows[0].1, ctx.theme.assistant);
        t.push_delta("\n|---|---|\n| 1 | 2 |");
        let rows = styled_rows(&t, 20, &ctx.theme);
        assert_eq!(rows[0].0, "● a  b");
        assert_eq!(
            rows[0].1,
            ctx.theme
                .assistant
                .add_modifier(ratatui::style::Modifier::BOLD)
        );
        assert_eq!(rows[1].0, "  1  2▍");
    }

    /// List items hang: wrapped continuation rows align under the
    /// BODY (2-col indent + the `• ` prefix width), and ordered lists
    /// hang by their wider `N. ` prefix.
    #[test]
    fn assistant_markdown_list_hanging_indent() {
        let mut t = TranscriptComponent::new();
        t.entries.push(TranscriptEntry::Assistant(
            "- 第一项内容很长\n- 短项".into(),
        ));
        let ctx = test_ctx();
        let rows = layout_text(&t, 10, &ctx.theme);
        // avail = 8, body avail = 6 → 第一项 / 内容很 / lone 长 →
        // orphan guard rebalances to 内容 / 很长.
        assert_eq!(
            rows,
            vec!["● • 第一项", "    内容", "    很长", "  • 短项",]
        );
        // Ordered: "1. " hangs 3 → continuation indent = 2 + 3.
        let mut t2 = TranscriptComponent::new();
        t2.entries
            .push(TranscriptEntry::Assistant("1. 第一项内容很长".into()));
        let rows2 = layout_text(&t2, 11, &ctx.theme);
        assert_eq!(rows2, vec!["● 1. 第一项", "     内容", "     很长"]);
    }

    /// Plain paragraphs do NOT hang (continuation rows stay at the
    /// 2-col block indent) — only list/quote items anchor.
    #[test]
    fn assistant_markdown_plain_paragraph_does_not_hang() {
        let mut t = TranscriptComponent::new();
        t.entries.push(TranscriptEntry::Assistant(
            "第一段正文很长很长需要折行处理".into(),
        ));
        let ctx = test_ctx();
        let rows = layout_text(&t, 8, &ctx.theme);
        // avail = 6: 第一段 / 正文很长... continuation at col 2.
        assert_eq!(rows[0], "● 第一段");
        assert!(rows[1].starts_with("  正文"), "{rows:?}");
        assert!(!rows[1].starts_with("    "), "no hang on plain paragraphs");
    }

    // ── Markdown rendering of finalized assistant blocks ───────────────

    /// Style-carrying snapshot of one layout line: (text, style) pairs.
    fn styled_rows(t: &TranscriptComponent, width: usize, theme: &Theme) -> Vec<(String, Style)> {
        t.layout_lines(width, theme, 0)
            .iter()
            .map(|l| {
                let text: String = l.spans.iter().map(|s| s.content.clone()).collect();
                // The first non-indent span's style represents the row.
                let style = l
                    .spans
                    .iter()
                    // restyle-1: skip the leading indent AND the ●
                    // anchor span (text color) — the first CONTENT
                    // span represents the row.
                    .find(|s| !s.content.trim().is_empty() && s.content.trim() != "●")
                    .map(|s| s.style)
                    .unwrap_or_default();
                (text, style)
            })
            .collect()
    }

    #[test]
    fn assistant_markdown_headings_bold_code_and_lists() {
        let mut t = TranscriptComponent::new();
        t.entries.push(TranscriptEntry::Assistant(
            "# Title\n\nplain **bold** `code`\n\n- item one\n\n> quoted".into(),
        ));
        let ctx = test_ctx();
        let rows = styled_rows(&t, 60, &ctx.theme);
        let texts: Vec<&str> = rows.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "● Title",
                "  ",
                "  plain bold code",
                "  ",
                "  • item one",
                "  ",
                "  │ quoted",
            ]
        );
        // Heading row: md_heading (Cyan + BOLD).
        assert_eq!(
            rows[0].1,
            ctx.theme.md_heading.add_modifier(Modifier::UNDERLINED)
        );
        // Plain body row carries the inline styles — spot check via
        // spans: "bold" is BOLD, "code" is the code color.
        let line3 = t.layout_lines(60, &ctx.theme, 0)[2].clone();
        let bold = line3
            .spans
            .iter()
            .find(|s| s.content.trim() == "bold")
            .expect("bold span");
        assert!(bold
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD));
        let code = line3
            .spans
            .iter()
            .find(|s| s.content.trim() == "code")
            .expect("code span");
        assert_eq!(code.style, ctx.theme.md_code);
    }

    #[test]
    fn assistant_markdown_fenced_code_clips_instead_of_wrapping() {
        let mut t = TranscriptComponent::new();
        t.entries.push(TranscriptEntry::Assistant(
            "```\nabcdefghij\n世界世界世界\n```".into(),
        ));
        let ctx = test_ctx();
        let rows = styled_rows(&t, 7, &ctx.theme);
        let texts: Vec<&str> = rows.iter().map(|(s, _)| s.as_str()).collect();
        // avail = 7 - 2 indent = 5 cols: ascii clips at 5, CJK at 2
        // chars; content NEVER wraps to extra rows.
        assert_eq!(texts, vec!["● abcde", "  世界"]);
        // restyle-1: the ● anchor (text color) leads row 0 — assert
        // the code style on the clipped content span instead.
        let code_line = t.layout_lines(7, &ctx.theme, 0)[0].clone();
        assert_eq!(
            code_line.spans[1].style, ctx.theme.md_code,
            "code block styled"
        );
        let _ = rows;
    }

    #[test]
    fn assistant_markdown_hr_expands_to_width() {
        let mut t = TranscriptComponent::new();
        t.entries
            .push(TranscriptEntry::Assistant("a\n\n---\nb".into()));
        let ctx = test_ctx();
        let rows = styled_rows(&t, 22, &ctx.theme);
        // hr = 2 indent + avail rule glyphs = the full row width.
        assert_eq!(
            rows[2].0.chars().count(),
            22,
            "full-width rule: {:?}",
            rows[2].0
        );
        assert!(rows[2].0.starts_with("  ─"));
        assert_eq!(rows[2].1, ctx.theme.muted, "rule is dim");
    }

    #[test]
    fn assistant_markdown_wraps_soft_lines_with_styles() {
        let mut t = TranscriptComponent::new();
        t.entries
            .push(TranscriptEntry::Assistant("**aaa bbb** ccc ddd eee".into()));
        let ctx = test_ctx();
        // avail = 12: "aaa bbb ccc" (11) then "ddd eee".
        let rows = styled_rows(&t, 14, &ctx.theme);
        let texts: Vec<&str> = rows.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(texts, vec!["● aaa bbb ccc", "  ddd eee"]);
        // The bold span survived the wrap.
        let row0 = t.layout_lines(14, &ctx.theme, 0)[0].clone();
        assert!(row0.spans.iter().any(|s| s.content == "aaa bbb"
            && s.style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)));
    }

    #[test]
    fn streaming_area_renders_markdown_live() {
        // fix-18: the live answer area renders markdown as deltas
        // arrive — same pipeline as the finalized blocks. A heading
        // styles immediately; a CLOSED `**` pair styles immediately.
        let mut t = TranscriptComponent::new();
        t.begin_streaming();
        t.push_delta("# not a heading yet **bold**");
        let ctx = test_ctx();
        let rows = styled_rows(&t, 60, &ctx.theme);
        assert_eq!(rows[0].0, "● not a heading yet bold▍");
        assert_eq!(
            rows[0].1,
            ctx.theme.md_heading.add_modifier(Modifier::UNDERLINED)
        );

        // Committed (flushed) assistant text renders identically —
        // only the tail cursor disappears at the flush.
        t.tool_start("echo", "{}");
        let rows = styled_rows(&t, 60, &ctx.theme);
        assert_eq!(rows[0].0, "● not a heading yet bold");
        assert_eq!(
            rows[0].1,
            ctx.theme.md_heading.add_modifier(Modifier::UNDERLINED)
        );
    }

    // ── Streaming markdown live rendering (fix-18) ───────────────────

    /// Strip the live tail-cursor span (▍, running-tool style) from a
    /// layout snapshot — the ONLY thing the streaming render carries
    /// that the committed render does not.
    fn strip_cursor(lines: Vec<Line<'static>>, theme: &Theme) -> Vec<Line<'static>> {
        lines
            .into_iter()
            .map(|mut l| {
                l.spans
                    .retain(|s| !(s.content == "▍" && s.style == theme.tool_running));
                l
            })
            .collect()
    }

    /// Line signature — (text, style) per span, for exact
    /// text-AND-style row comparison.
    fn sig(lines: &[Line<'static>]) -> Vec<Vec<(String, Style)>> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| (s.content.to_string(), s.style))
                    .collect()
            })
            .collect()
    }

    /// A heading has no closing marker — it styles the moment `# ` +
    /// text exists and grows as the rest of the title streams in.
    /// `#nospace` (no space after the markers) stays literal forever.
    #[test]
    fn streaming_markdown_heading_styles_arrive_with_the_deltas() {
        let mut t = TranscriptComponent::new();
        t.begin_streaming();
        t.push_delta("# 标");
        let ctx = test_ctx();
        let rows = styled_rows(&t, 40, &ctx.theme);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "● 标▍");
        assert_eq!(
            rows[0].1,
            ctx.theme.md_heading.add_modifier(Modifier::UNDERLINED)
        );
        t.push_delta("题");
        let rows = styled_rows(&t, 40, &ctx.theme);
        assert_eq!(rows[0].0, "● 标题▍");
        assert_eq!(
            rows[0].1,
            ctx.theme.md_heading.add_modifier(Modifier::UNDERLINED)
        );

        // No space after `#` → not a heading, literal body text.
        let mut t2 = TranscriptComponent::new();
        t2.begin_streaming();
        t2.push_delta("#nos");
        t2.push_delta("pace");
        let rows = styled_rows(&t2, 40, &ctx.theme);
        assert_eq!(rows[0].0, "● #nospace▍");
        assert_eq!(rows[0].1, ctx.theme.assistant);
    }

    /// An unclosed `**` renders literally (the parser's existing
    /// semantics) and flips to BOLD in place the instant the closing
    /// marker arrives — no flush involved.
    #[test]
    fn streaming_bold_is_literal_until_the_closing_marker() {
        let mut t = TranscriptComponent::new();
        t.begin_streaming();
        t.push_delta("**bo");
        let ctx = test_ctx();
        let rows = styled_rows(&t, 40, &ctx.theme);
        assert_eq!(rows[0].0, "● **bo▍");
        assert_eq!(rows[0].1, ctx.theme.assistant);
        t.push_delta("ld**");
        let rows = styled_rows(&t, 40, &ctx.theme);
        assert_eq!(rows[0].0, "● bold▍");
        assert_eq!(
            rows[0].1,
            ctx.theme
                .assistant
                .add_modifier(ratatui::style::Modifier::BOLD)
        );
    }

    /// An open fenced block swallows everything after it as verbatim
    /// code lines, and the tail cursor is NEVER appended inside a code
    /// block — nor on the row above while the fence is still open (the
    /// insertion point is inside the empty block).
    #[test]
    fn streaming_open_fence_renders_code_and_hides_the_cursor() {
        let mut t = TranscriptComponent::new();
        t.begin_streaming();
        t.push_delta("para\n```\nfn m");
        let ctx = test_ctx();
        let rows = styled_rows(&t, 20, &ctx.theme);
        assert_eq!(rows[0].0, "● para");
        assert_eq!(rows[1].0, "  fn m");
        assert_eq!(rows[1].1, ctx.theme.md_code);
        assert!(!rows[1].0.contains('▍'), "no cursor inside code");

        // Bare opening fence, nothing streamed into it yet: the cursor
        // must not land on the paragraph above either.
        let mut t2 = TranscriptComponent::new();
        t2.begin_streaming();
        t2.push_delta("para\n```");
        let rows = styled_rows(&t2, 20, &ctx.theme);
        assert_eq!(rows.last().unwrap().0, "● para");

        // Closing the fence finalizes the block (still cursorless).
        t.push_delta("() {}\n```");
        let rows = styled_rows(&t, 20, &ctx.theme);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].0, "  fn m() {}");
        assert_eq!(rows[1].1, ctx.theme.md_code);
    }

    /// Tail-cursor placement rules: exactly ONE ▍, riding the block's
    /// LAST display row (never a full-height gutter); suppressed when
    /// the row has no spare column (never clipped mid-glyph).
    #[test]
    fn streaming_tail_cursor_rules() {
        let ctx = test_ctx();
        // Multi-row block: cursor only on the last wrapped row
        // (avail 10: "one two" / "three four" / "five six").
        let mut t = TranscriptComponent::new();
        t.begin_streaming();
        t.push_delta("one two three four five six");
        let rows = layout_text(&t, 12, &ctx.theme);
        assert_eq!(
            rows,
            vec!["● one two", "  three four", "  five six▍"],
            "one cursor, on the last row only"
        );

        // Exactly-full last row: the cursor is dropped rather than
        // clipped ("one two three" = 13 cols; +2 indent = width 15).
        let mut t2 = TranscriptComponent::new();
        t2.begin_streaming();
        t2.push_delta("one two three");
        let rows = layout_text(&t2, 15, &ctx.theme);
        assert_eq!(rows, vec!["● one two three"]);
    }

    /// THE seamless-flush contract (fix-18's core payoff): for the
    /// same text, the streaming render's line set minus the tail
    /// cursor equals the committed render's line set — text AND style,
    /// span by span — so a boundary flush (`tool_start`) never
    /// re-styles or re-flows anything the user already read. Driven
    /// across the whole markdown subset, with and without a preceding
    /// entry (block-gap parity) and at a wrap-forcing width. (fix-19
    /// note: the live stats row does not appear here — no ttft is fed
    /// and the µs-old buffer sits under [`LIVE_RATE_FLOOR_SECS`], so
    /// the immediate layout carries no row; dedicated tests cover it.)
    #[test]
    fn streaming_flush_is_pixel_seamless() {
        let ctx = test_ctx();
        let cases = [
            "# Title\n\nplain **bold** `code`",
            "- item one\n- item two",
            "1. first\n2. second",
            "> quoted text",
            "```\nfn main() {}\n```",
            "| a | b |\n|---|---|\n| 1 | 2 |",
            "para\n\n---\n\nafter",
            "中文正文**加粗**测试，长文本自动折行的场景也需要保持完全一致。",
        ];
        for with_user in [true, false] {
            for md in cases {
                let mut t = TranscriptComponent::new();
                if with_user {
                    t.push_user("go");
                }
                t.begin_streaming();
                t.push_delta(md);
                let live = strip_cursor(t.layout_lines(30, &ctx.theme, 0), &ctx.theme);
                t.tool_start("echo", "{}"); // boundary flush, no metas
                let committed = t.layout_lines(30, &ctx.theme, 0);
                assert!(
                    committed.len() > live.len(),
                    "tool row appended below: {md:?}"
                );
                assert_eq!(
                    sig(&committed[..live.len()]),
                    sig(&live),
                    "flush must not re-flow anything: {md:?} (user={with_user})"
                );
            }
        }
    }

    /// Perf smoke (NON-asserting, run with --nocapture): a full
    /// layout pass over a ~50KB streaming answer buffer at a typical
    /// width — the per-frame cost the render loop pays while
    /// streaming. If this ever grows past a small fraction of the
    /// frame budget, cache parsed lines at the last delta's line
    /// boundary (see the batch spec).
    #[test]
    fn streaming_layout_50kb_smoke_timing() {
        let mut doc = String::new();
        while doc.len() < 50 * 1024 {
            doc.push_str(
                "# 标题 Heading\n\n一段中文正文，mixed with english words and \
                 `inline code`, **bold spans** and [links](https://example.com/long).\n\n\
                 - 列表项 one with a fairly long body that wraps\n- 列表项 two\n\n\
                 ```\nfn main() {\n    println!(\"hello, world\");\n}\n```\n\n\
                 | 列一 | col two |\n|---|---|\n| 数据 | data row |\n\n",
            );
        }
        let mut t = TranscriptComponent::new();
        t.begin_streaming();
        t.push_delta(&doc);
        let ctx = test_ctx();
        let iters = 20;
        let start = Instant::now();
        let mut rows = 0;
        for _ in 0..iters {
            rows = t.layout_lines(100, &ctx.theme, 0).len();
        }
        println!(
            "layout_lines on a {}KB streaming buffer: {:?}/frame → {rows} display rows",
            doc.len() / 1024,
            start.elapsed() / iters
        );
    }

    #[test]
    fn turn_marker_carries_aggregate_totals() {
        let mut t = TranscriptComponent::new();
        t.rebuild(&[
            msg(MessageRole::User, "go"),
            msg(MessageRole::Assistant, "ok"),
        ]);
        let ctx = test_ctx();
        t.set_turn_meta("main".to_owned(), 9, Some((130, 15)));
        let lines = t.layout_lines(60, &ctx.theme, 0);
        let last = lines.last().expect("marker line");
        let text: String = last.spans.iter().map(|s| s.content.clone()).collect();
        assert!(text.contains("main · 9s · ↑130 ↓15"), "marker: {text}");
        assert!(text.contains("⚡ 2 tok/s"), "rate = 15/9 rounded: {text}");
        // Without totals: the plain form (existing behavior).
        t.set_turn_meta("fast".to_owned(), 3, None);
        let lines = t.layout_lines(60, &ctx.theme, 0);
        let text: String = lines
            .last()
            .unwrap()
            .spans
            .iter()
            .map(|s| s.content.clone())
            .collect();
        assert!(text.contains("fast · 3s"), "plain form: {text}");
        assert!(!text.contains('\u{2191}'), "no totals segment: {text}");
    }

    // ── TurnDone(Ok) merge path ───────────────────────────────────────

    /// Kind shorthand for the merge tests.
    fn kind_of(e: &TranscriptEntry) -> &'static str {
        match e {
            TranscriptEntry::User(_) => "user",
            TranscriptEntry::Reasoning(_) => "reasoning",
            TranscriptEntry::Meta(m) if m.starts_with('~') => "meta_est",
            TranscriptEntry::Meta(_) => "meta_usage",
            TranscriptEntry::Assistant(_) => "assistant",
            TranscriptEntry::ToolCall { status, .. } => match status {
                ToolEntryStatus::Running => "tool_run",
                ToolEntryStatus::Done { .. } => "tool_done",
                ToolEntryStatus::Failed { .. } => "tool_fail",
            },
            TranscriptEntry::Approval { .. } => "approval",
            TranscriptEntry::Delegate { .. } => "delegate",
            TranscriptEntry::StepBreak => "sep",
        }
    }

    /// The multi-step e2e contract: reasoning → tool → reasoning →
    /// answer. After `merge_turn` the thinking blocks, BOTH requests'
    /// meta lines, the tool row and the turn marker are all in place
    /// in event order — the successful turn keeps (not wipes) the
    /// live view.
    #[test]
    fn merge_turn_preserves_reasoning_meta_tool_and_marker() {
        let mut t = TranscriptComponent::new();
        t.push_user("go");
        t.begin_streaming();
        // Request 1: reasoning + a pre-tool answer.
        t.push_reasoning("think 1");
        t.push_delta("checking");
        t.finish_request(
            Some(Usage {
                input_tokens: 50,
                output_tokens: 10,
                cached_input_tokens: None,
            }),
            Some(Duration::from_secs_f64(2.0)),
            Some(Duration::from_millis(800)),
        );
        // Tool round-trip (live path — no failure detection possible).
        t.tool_start("echo", r#"{"text":"hi"}"#);
        t.tool_end("echo", 2, false);
        t.step_end();
        // Request 2: reasoning + the final answer (no FirstToken → no
        // ttft segment on its usage line).
        t.push_reasoning("think 2");
        t.push_delta("final");
        t.finish_request(
            Some(Usage {
                input_tokens: 80,
                output_tokens: 5,
                cached_input_tokens: None,
            }),
            Some(Duration::from_secs_f64(1.0)),
            None,
        );

        let messages = vec![
            msg(MessageRole::User, "go"),
            Message {
                role: MessageRole::Assistant,
                content: "checking".into(),
                tool_call_id: None,
                name: None,
                tool_calls: Some(vec![ToolCall {
                    id: ToolCallId("tc-1".into()),
                    name: "echo".into(),
                    arguments: json!({}),
                }]),
            },
            tool_result("tc-1", "echo", "ok"),
            msg(MessageRole::Assistant, "final"),
        ];
        t.set_turn_meta("main".to_owned(), 5, Some((130, 15)));
        t.merge_turn(&messages);

        let entries = t.entries();
        let kinds: Vec<&str> = entries.iter().map(kind_of).collect();
        // fix-19: each step's USAGE line lands below its tool rows
        // (held at RequestEnd, consumed by tool_start); tool-less
        // steps keep it at the answer's tail (flushed by the merge).
        assert_eq!(
            kinds,
            vec![
                "user",
                "reasoning",
                "meta_est",
                "assistant",
                "tool_done",
                "meta_usage",
                "sep",
                "reasoning",
                "meta_est",
                "assistant",
                "meta_usage",
            ],
            "event order preserved through the merge (usage below tools)"
        );
        // The thinking blocks are untouched.
        assert!(matches!(&entries[1], TranscriptEntry::Reasoning(t1) if t1 == "think 1"));
        assert!(matches!(&entries[7], TranscriptEntry::Reasoning(t2) if t2 == "think 2"));
        // The tool row: refined by its Tool message ("ok" → 2 bytes).
        match &entries[4] {
            TranscriptEntry::ToolCall {
                status: ToolEntryStatus::Done { bytes, .. },
                ..
            } => assert_eq!(*bytes, 2),
            other => panic!("unexpected {other:?}"),
        }
        // No trailing separator survives (the marker's block gap is
        // the single blank line before it).
        assert!(!matches!(entries.last(), Some(TranscriptEntry::StepBreak)));
        // The turn marker renders as the LAST layout row, below the
        // surviving per-request meta lines, carrying the aggregates.
        let ctx = test_ctx();
        let rows = layout_text(&t, 60, &ctx.theme);
        let last = rows.last().expect("marker row");
        assert!(last.contains("main"), "marker row: {last}");
        assert!(last.contains("↑130 ↓15"), "aggregate totals: {last}");
        assert!(
            rows.iter()
                .any(|r| r.contains("↑50 ↓10") && r.contains("ttft 0.8s")),
            "request-1 usage line survived: {rows:?}"
        );
        assert!(
            rows.iter()
                .any(|r| r.contains("↑80 ↓5") && !r.contains("ttft")),
            "request-2 usage line survived (no ttft): {rows:?}"
        );
    }

    /// The A4 failure heuristic fires on the MERGE path: the live
    /// `ToolEnd` marked the row Done (it cannot see failure); the Tool
    /// message's marker flips it to Failed.
    #[test]
    fn merge_turn_folds_failure_markers_from_tool_messages() {
        let mut t = TranscriptComponent::new();
        t.push_user("go");
        t.tool_start("echo", "{}");
        t.tool_end("echo", 34, false); // live: success-shaped
        t.merge_turn(&[
            assistant_with_call("tc-1", "echo"),
            tool_result("tc-1", "echo", "Error: boom"),
        ]);
        match &t.entries()[1] {
            TranscriptEntry::ToolCall {
                status: ToolEntryStatus::Failed { summary },
                ..
            } => assert_eq!(summary, "Error: boom"),
            other => panic!("unexpected {other:?}"),
        }
    }

    /// A call still Running at TurnDone (no live ToolEnd, no Tool
    /// message) finalizes as a plain Done — the turn is over.
    #[test]
    fn merge_turn_finalizes_orphan_running_calls() {
        let mut t = TranscriptComponent::new();
        t.tool_start("echo", "{}");
        t.merge_turn(&[]);
        assert!(matches!(
            &t.entries()[0],
            TranscriptEntry::ToolCall {
                status: ToolEntryStatus::Done { bytes: 0, .. },
                ..
            }
        ));
    }

    /// Sequential same-name calls fold FIFO by name (live entries
    /// carry no call_id): the first result reaches the FIRST entry,
    /// the second the second — including the failure marker landing
    /// on the right one.
    #[test]
    fn merge_turn_name_fallback_pairs_sequential_calls_fifo() {
        let mut t = TranscriptComponent::new();
        t.tool_start("read_file", "{}");
        t.tool_end("read_file", 10, false);
        t.tool_start("read_file", "{}");
        t.tool_end("read_file", 20, false);
        t.merge_turn(&[
            assistant_with_call("t1", "read_file"),
            tool_result("t1", "read_file", "Error: missing"),
            assistant_with_call("t2", "read_file"),
            tool_result("t2", "read_file", "second"),
        ]);
        match &t.entries()[0] {
            TranscriptEntry::ToolCall {
                status: ToolEntryStatus::Failed { summary },
                ..
            } => assert_eq!(summary, "Error: missing"),
            other => panic!("first call carries the failure: {other:?}"),
        }
        match &t.entries()[1] {
            TranscriptEntry::ToolCall {
                status: ToolEntryStatus::Done { bytes, .. },
                ..
            } => assert_eq!(*bytes, 6, "second result folded onto the second entry"),
            other => panic!("unexpected {other:?}"),
        }
    }

    /// A live-measured elapsed survives a success refine (the fold
    /// keeps the timing the user watched ticking).
    #[test]
    fn merge_turn_success_refine_keeps_live_elapsed() {
        let mut t = TranscriptComponent::new();
        t.tool_start("echo", "{}");
        t.tool_end("echo", 2, false);
        assert!(matches!(
            &t.entries()[0],
            TranscriptEntry::ToolCall {
                status: ToolEntryStatus::Done {
                    elapsed_ms: Some(_),
                    ..
                },
                ..
            }
        ));
        t.merge_turn(&[
            assistant_with_call("t1", "echo"),
            tool_result("t1", "echo", "ok"),
        ]);
        assert!(matches!(
            &t.entries()[0],
            TranscriptEntry::ToolCall {
                status: ToolEntryStatus::Done {
                    elapsed_ms: Some(_),
                    bytes,
                    ..
                },
                ..
            } if *bytes == 2
        ));
    }

    /// Drift guard: assistant content that exists only in messages
    /// (dropped events, non-streaming providers) is APPENDED so it is
    /// never lost — and never duplicated when the transcript already
    /// carries it (multiset semantics, edge-trim normalized: the raw
    /// message may carry `\n\n` edges the flush trimmed).
    #[test]
    fn merge_turn_appends_missing_assistant_content_only() {
        let mut t = TranscriptComponent::new();
        t.push_user("go");
        t.begin_streaming();
        t.push_delta("\n\nstreamed fine\n");
        t.finish_request(None, None, None); // commits "streamed fine"
        t.merge_turn(&[
            msg(MessageRole::User, "go"),
            msg(MessageRole::Assistant, "\n\nstreamed fine\n"),
            msg(MessageRole::Assistant, "never streamed"),
        ]);
        assert_eq!(
            t.entries(),
            &[
                TranscriptEntry::User("go".into()),
                TranscriptEntry::Assistant("streamed fine".into()),
                TranscriptEntry::Assistant("never streamed".into()),
            ]
        );
    }

    /// Identical content twice in messages with one transcript copy →
    /// exactly one append (multiset counting, not membership).
    #[test]
    fn merge_turn_duplicate_assistant_content_counts() {
        let mut t = TranscriptComponent::new();
        t.push_user("go");
        t.begin_streaming();
        t.push_delta("same");
        t.finish_request(None, None, None);
        t.merge_turn(&[
            msg(MessageRole::User, "go"),
            msg(MessageRole::Assistant, "same"),
            msg(MessageRole::Assistant, "same"),
        ]);
        assert_eq!(t.entries().len(), 3);
        assert!(matches!(
            &t.entries()[2],
            TranscriptEntry::Assistant(text) if text == "same"
        ));
    }

    /// Streaming stragglers (an Interrupted turn cut mid-flight, or a
    /// provider that skipped RequestEnd) commit at merge instead of
    /// vanishing — without meta lines (that request never finished).
    #[test]
    fn merge_turn_commits_streaming_stragglers() {
        let mut t = TranscriptComponent::new();
        t.push_user("go");
        t.begin_streaming();
        t.push_reasoning("cut mid thought");
        t.push_delta("partial");
        t.merge_turn(&[msg(MessageRole::User, "go")]);
        assert!(matches!(
            t.entries(),
            [
                TranscriptEntry::User(_),
                TranscriptEntry::Reasoning(r),
                TranscriptEntry::Assistant(a)
            ] if r == "cut mid thought" && a == "partial"
        ));
        assert!(t.streaming_text().is_empty());
    }

    /// The merge path only ever ADDS: reasoning entries are never
    /// modified even when the messages disagree (core does not
    /// persist reasoning — there is nothing to reconcile against).
    #[test]
    fn merge_turn_never_touches_reasoning_entries() {
        let mut t = TranscriptComponent::new();
        t.push_user("go");
        t.begin_streaming();
        t.push_reasoning("inner monologue");
        t.tool_start("echo", "{}"); // flush commits the reasoning
        let before: Vec<_> = t
            .entries()
            .iter()
            .filter_map(|e| match e {
                TranscriptEntry::Reasoning(text) => Some(text.clone()),
                _ => None,
            })
            .collect();
        t.merge_turn(&[msg(MessageRole::User, "go")]);
        let after: Vec<_> = t
            .entries()
            .iter()
            .filter_map(|e| match e {
                TranscriptEntry::Reasoning(text) => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(before, after);
    }

    /// The turn marker pins at the finished turn's tail (entries
    /// index recorded by `merge_turn`): once the NEXT turn's user
    /// message arrives as an entry, the marker stays ABOVE it — the
    /// old layout-trailing render slid it below the new turn's
    /// content.
    #[test]
    fn turn_marker_stays_above_the_next_turns_content() {
        let mut t = TranscriptComponent::new();
        t.push_user("q1");
        t.begin_streaming();
        t.push_delta("a1");
        t.set_turn_meta("main".to_owned(), 5, None);
        t.merge_turn(&[
            msg(MessageRole::User, "q1"),
            msg(MessageRole::Assistant, "a1"),
        ]);
        // The next turn begins: its user block must land BELOW the
        // previous turn's marker.
        t.push_user("q2");
        let ctx = test_ctx();
        let rows = layout_text(&t, 40, &ctx.theme);
        let marker = rows
            .iter()
            .position(|r| r.contains("main · 5s"))
            .expect("marker row");
        let q2 = rows.iter().position(|r| r.contains("q2")).expect("q2 row");
        let a1 = rows.iter().position(|r| r.contains("a1")).expect("a1 row");
        assert!(a1 < marker && marker < q2, "a1 < marker < q2: {rows:?}");
    }

    // ── splash-1: the empty-session splash ────────────────────────────

    /// Both tiers' art is BYTE-FROZEN: independent transcriptions of
    /// the spec's lines must match verbatim (guards a hand-typo in
    /// the glyph tables), stay pure BMP box/block glyphs + space,
    /// rstripped, each tier's widest row exactly its pinned block
    /// width. The ladder geometry (80/45/10 + the ascii-tier veto)
    /// is pinned alongside.
    #[test]
    fn splash_art_is_byte_frozen() {
        let full: [&str; 6] = [
            " ██████╗ ██████╗ ███████╗███╗   ██╗███████╗██╗      █████╗ ████████╗███████╗",
            "██╔═══██╗██╔══██╗██╔════╝████╗  ██║██╔════╝██║     ██╔══██╗╚══██╔══╝██╔════╝",
            "██║   ██║██████╔╝█████╗  ██╔██╗ ██║███████╗██║     ███████║   ██║   █████╗",
            "██║   ██║██╔═══╝ ██╔══╝  ██║╚██╗██║╚════██║██║     ██╔══██║   ██║   ██╔══╝",
            "╚██████╔╝██║     ███████╗██║ ╚████║███████║███████╗██║  ██║   ██║   ███████╗",
            " ╚═════╝ ╚═╝     ╚══════╝╚═╝  ╚═══╝╚══════╝╚══════╝╚═╝  ╚═╝   ╚═╝   ╚══════╝",
        ];
        assert_eq!(&SPLASH_ART_FULL, &full, "byte-frozen full glyph table");
        let medium: [&str; 6] = [
            "███████╗██╗      █████╗ ████████╗███████╗",
            "██╔════╝██║     ██╔══██╗╚══██╔══╝██╔════╝",
            "███████╗██║     ███████║   ██║   █████╗",
            "╚════██║██║     ██╔══██║   ██║   ██╔══╝",
            "███████║███████╗██║  ██║   ██║   ███████╗",
            "╚══════╝╚══════╝╚═╝  ╚═╝   ╚═╝   ╚══════╝",
        ];
        assert_eq!(
            &SPLASH_ART_MEDIUM, &medium,
            "byte-frozen medium glyph table"
        );
        for (name, (art, width)) in [
            ("full", (&SPLASH_ART_FULL, SPLASH_FULL_WIDTH)),
            ("medium", (&SPLASH_ART_MEDIUM, SPLASH_MEDIUM_WIDTH)),
        ] {
            assert_eq!(art.len(), 6, "{name} has six rows");
            assert_eq!(
                art.iter().map(|l| l.chars().count()).max().unwrap() as u16,
                width,
                "{name} widest row = block width"
            );
            for (i, line) in art.iter().enumerate() {
                assert!(
                    line.chars().all(|c| (c as u32) <= 0xFFFF),
                    "BMP-only (no PUA) {name} row {i}"
                );
                assert!(
                    line.chars()
                        .all(|c| c == ' ' || ('\u{2500}'..='\u{259F}').contains(&c)),
                    "box/block glyphs only, {name} row {i}"
                );
                assert_eq!(*line, line.trim_end(), "rows are rstripped ({name} {i})");
            }
        }
        // The size ladder: full ≥ 80, medium ≥ 45, height ≥ 10, and
        // the ascii icon tier never shows art.
        assert_eq!(SplashTier::pick(80, 10, false), Some(SplashTier::Full));
        assert_eq!(SplashTier::pick(120, 30, false), Some(SplashTier::Full));
        assert_eq!(SplashTier::pick(79, 10, false), Some(SplashTier::Medium));
        assert_eq!(SplashTier::pick(45, 10, false), Some(SplashTier::Medium));
        assert_eq!(SplashTier::pick(44, 10, false), None);
        assert_eq!(SplashTier::pick(80, 9, false), None, "short → legacy");
        assert_eq!(
            SplashTier::pick(120, 30, true),
            None,
            "ascii icon tier → legacy"
        );
        // The shared hint copy rides both empty-state paths.
        assert_eq!(EMPTY_SESSION_HINT, "空会话 — 输入 prompt 开始,Enter 发送");
    }
}
