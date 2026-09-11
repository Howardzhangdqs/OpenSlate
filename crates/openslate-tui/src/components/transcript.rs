//! Transcript — the conversation view (chat main region).
//!
//! Data model and Action consumption were frozen in P2b; this module
//! implements the full P3 lane-a rendering:
//!
//! * entry rendering per the borderless spec (Wave 2 lane-a): user
//!   messages carry a Cyan `┃` bar over the block's full height with
//!   the body hanging-indented 2 columns (no `you:` text label —
//!   identity lives in the bar, opencode language), assistant text is
//!   default-foreground indented 2 (aligned with the user body column),
//!   reasoning keeps its dim `┆` gutter, tool lines are single-line
//!   entries `{icon} name(args≤40列)` with the opencode icon table —
//!   all Nerd Font PUA glyphs, width exactly 1 column (terminal
//!    shell /  read /  write-edit /  glob-grep /
//!    other) — + status + elapsed + bytes, ` agent` link-icon
//!   delegation markers (Magenta), approval outcome lines (Nerd
//!   hand icon), one-blank-line step separators
//!   ([`TranscriptComponent::step_end`]), and the ` model · Ns`
//!   cube-icon end-of-turn marker fed by
//!   [`TranscriptComponent::set_turn_meta`]. Spacing: block entries
//!   (user/assistant/turn marker) get one blank line before them;
//!   runs of single-line tool rows stay compact;
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
//! * scroll pinning: auto-follow at the bottom while content streams;
//!   any user scroll-up pins (new content stops following and a
//!   `↓ 新内容` hint appears at the bottom-right — CLICKABLE, see
//!   [`TranscriptComponent::hint_hit_rect`]); `G`/`End`/
//!   `ScrollBottom`/`Esc` release the pin, as does scrolling back down
//!   to the bottom;
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

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use ratatui::layout::{Alignment, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::status::SPINNER_FRAMES;
use super::{AppCtx, Component, Focus};
use crate::action::Action;
use crate::theme::Theme;
use openslate_core::types::{Message, MessageRole, Usage};

/// The live stats row's rate floor (fix-19): below this many seconds
/// since the first answer delta the estimated rate is noise (a
/// one-token block microseconds old would render a five-digit tok/s)
/// and the segment is suppressed until the clock makes it meaningful.
const LIVE_RATE_FLOOR_SECS: f64 = 0.3;

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
    /// path's Tool-role result messages back to this entry.
    ToolCall {
        name: String,
        args: String,
        status: ToolEntryStatus,
        call_id: Option<String>,
    },
    /// Approval outcome line.
    Approval { tool_name: String, decision: String },
    /// Delegation marker ` agent` (Nerd link icon, Magenta).
    Delegate { agent: String },
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
    /// `↓ 新内容 +N` hint (the hint's own row, its right-aligned span
    /// widened 2 columns each side); `None` whenever the hint did NOT
    /// render (following, nothing below, zero-height viewport). `handle`
    /// hit-tests [`Action::Click`] against it — a hit returns
    /// [`Action::ScrollBottom`] (the existing follow-restore action).
    /// Interior mutability like `viewport` (render takes `&self`).
    hint_hit_rect: Cell<Option<Rect>>,
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

    /// Live streaming buffer (answer deltas only).
    pub fn streaming_text(&self) -> &str {
        &self.streaming
    }

    /// Whether the view is pinned (not following the tail).
    pub fn is_pinned(&self) -> bool {
        !self.follow
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
        self.reasoning_started = None;
        self.answer_started = None;
        self.live_ttft = None;
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
        });
        self.running_since.insert(index, Instant::now());
        self.flush_pending_step_meta();
    }

    /// A tool finished: mark the LAST running entry with this name Done
    /// (A4: no failure state on the live path — the rebuild decides).
    pub fn tool_end(&mut self, name: &str, bytes: usize, truncated: bool) {
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
        self.reasoning_started = None;
        self.answer_started = None;
        if !reasoning.trim().is_empty() {
            self.entries
                .push(TranscriptEntry::Reasoning(trim_edge_blank_lines(
                    &reasoning,
                )));
        }
        let answer = std::mem::take(&mut self.streaming);
        if !answer.trim().is_empty() {
            self.entries
                .push(TranscriptEntry::Assistant(trim_edge_blank_lines(&answer)));
        }
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
        let answer_raw = std::mem::take(&mut self.streaming);
        self.answer_started = None;
        self.live_ttft = None;

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
                self.entries.push(TranscriptEntry::Reasoning(reasoning));
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
                    self.entries.push(TranscriptEntry::Reasoning(reasoning));
                    self.pending_step_meta = Some(meta_merged_line(chars, &usage, ttft, elapsed));
                }
                (true, None) => {
                    self.entries.push(TranscriptEntry::Reasoning(reasoning));
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
        self.reasoning_started = None;
        self.answer_started = None;
        self.live_ttft = None;
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
    /// entry to  Failed (the A4 heuristic).
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
    }

    /// Clear everything (`/new`). The turn marker goes too — a fresh
    /// session has no last turn to mark (a held step meta is dropped
    /// with it).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.clear_streaming();
        self.pending_step_meta = None;
        self.turn_meta = None;
        self.marker_at = None;
        self.scroll = 0;
        self.follow = true;
        self.running_since.clear();
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
        self.layout_lines_at(width, theme, spinner_frame, Instant::now())
    }

    /// [`Self::layout_lines`] with an injectable clock — the test
    /// seam for the time-derived rows (fix-19's live rate needs
    /// deterministic instants; production always passes `now`).
    fn layout_lines_at(
        &self,
        width: usize,
        theme: &Theme,
        spinner_frame: usize,
        now: Instant,
    ) -> Vec<Line<'static>> {
        let width = width.max(1);
        let mut out: Vec<Line<'static>> = Vec::new();

        let is_empty = self.entries.is_empty()
            && self.streaming.is_empty()
            && self.streaming_reasoning.is_empty()
            && self.turn_meta.is_none();
        if is_empty {
            out.push(Line::from(Span::styled(
                "空会话 — 输入 prompt 开始,Enter 发送",
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
            // The marker of the LAST finished turn renders at its
            // recorded position (merge_turn/rebuild) — above the
            // next turn's entries, never below them.
            if !emitted_marker && marker_at == Some(index) {
                emitted_marker = true;
                if let Some(TurnMeta {
                    model,
                    elapsed_secs: secs,
                    tokens,
                }) = &self.turn_meta
                {
                    gap_before_block(&mut out);
                    let mut text = format!(" {model} · {secs}秒");
                    if let Some((input, output)) = tokens {
                        text.push_str(&format!(" · ↑{input} ↓{output}"));
                    }
                    out.push(Line::from(vec![
                        Span::styled("".to_owned(), theme.turn_marker),
                        Span::styled(text, theme.muted),
                    ]));
                }
            }
            match entry {
                TranscriptEntry::User(text) => {
                    gap_before_block(&mut out);
                    // Cyan `┃` bar over the block's FULL height; the
                    // body hangs at column 2 (the bar replaces the old
                    // `you:` text label — opencode identity language).
                    push_gutter_block(
                        &mut out,
                        "┃ ",
                        theme.user_label,
                        text,
                        theme.assistant,
                        width,
                    );
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
                    push_markdown_block(&mut out, text, theme, width, None);
                }
                TranscriptEntry::Reasoning(text) => push_gutter_block(
                    &mut out,
                    "┆ ",
                    theme.reasoning,
                    text,
                    theme.reasoning,
                    width,
                ),
                TranscriptEntry::ToolCall {
                    name, args, status, ..
                } => {
                    let running_for = if matches!(status, ToolEntryStatus::Running) {
                        self.running_since
                            .get(&index)
                            .map(|t| now.duration_since(*t))
                    } else {
                        None
                    };
                    let spinner = SPINNER_FRAMES[spinner_frame % SPINNER_FRAMES.len()];
                    push_tool_entry(
                        &mut out,
                        &format!("{} {name}({args})", tool_icon(name)),
                        status,
                        running_for,
                        theme,
                        spinner,
                        width,
                    );
                }
                TranscriptEntry::Approval {
                    tool_name,
                    decision,
                } => push_approval_line(&mut out, tool_name, decision, theme, width),
                TranscriptEntry::Delegate { agent } => out.push(Line::from(Span::styled(
                    format!(" {agent}"),
                    theme.delegate,
                ))),
                // Step separator: one blank line (the borderless spec
                // retired the full-width `─` rule).
                TranscriptEntry::StepBreak => out.push(Line::from("")),
                // Per-request telemetry (dim, indented 2 — single-line
                // entry: no block gap).
                TranscriptEntry::Meta(text) => {
                    out.push(Line::from(Span::styled(format!("  {text}"), theme.muted)))
                }
            }
        }
        // End-of-turn marker, trailing fallback: renders when no
        // in-loop position emitted it yet (fresh transcript, or
        // set_turn_meta without merge_turn/rebuild). Nerd cube icon
        // Cyan, the rest muted, `·` (U+00B7) between model and
        // seconds; the aggregate token totals are the recovery
        // rebuild path's usage display (the merge path keeps the
        // per-request meta lines above this marker).
        if !emitted_marker {
            if let Some(TurnMeta {
                model,
                elapsed_secs: secs,
                tokens,
            }) = &self.turn_meta
            {
                gap_before_block(&mut out);
                let mut text = format!(" {model} · {secs}秒");
                if let Some((input, output)) = tokens {
                    text.push_str(&format!(" · ↑{input} ↓{output}"));
                }
                out.push(Line::from(vec![
                    Span::styled("".to_owned(), theme.turn_marker),
                    Span::styled(text, theme.muted),
                ]));
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
            push_gutter_block(
                &mut out,
                "┆ ",
                theme.reasoning,
                &reasoning_view,
                theme.reasoning,
                width,
            );
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
                out.push(Line::from(Span::styled(format!("  {stats}"), theme.muted)));
            }
            push_markdown_block(
                &mut out,
                &answer_view,
                theme,
                width,
                Some(theme.tool_running),
            );
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
                    out.push(TranscriptEntry::ToolCall {
                        name: tc.name.clone(),
                        args: args_preview(&tc.arguments.to_string()),
                        status: ToolEntryStatus::Running,
                        call_id: Some(tc.id.0.clone()),
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
///  Failed. Orphan results (no matching call) are dropped silently.
fn fold_tool_outcome(entries: &mut [TranscriptEntry], msg: &Message) {
    let Some(index) = find_tool_entry(entries, msg, MatchDone::No) else {
        return;
    };
    let status = tool_outcome_from_message(msg);
    if let TranscriptEntry::ToolCall { status: s, .. } = &mut entries[index] {
        *s = status;
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
    let mut plan: Vec<(usize, ToolEntryStatus)> = Vec::new();
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
        plan.push((index, status));
    }

    for (index, status) in plan {
        if let TranscriptEntry::ToolCall { status: s, .. } = &mut entries[index] {
            *s = status;
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
/// (no wrap); a horizontal rule expands to a dim full-width line;
/// pipe tables render as aligned columns ([`crate::md::table_lines`])
/// with the header row BOLD.
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
) {
    const INDENT: &str = "  ";
    let avail = width.saturating_sub(str_width(INDENT)).max(1);
    let styles = crate::md::MdStyles {
        text: theme.assistant,
        heading: theme.md_heading,
        code: theme.md_code,
        dim: theme.muted,
    };
    let mut last_was_code = false;
    for line in crate::md::parse(text, &styles) {
        match line {
            crate::md::MdLine::Flow(spans) => {
                last_was_code = false;
                // List/quote hanging indent: the `• `/`N. `/`> `
                // prefix pins row 0; wrapped rows align under the
                // BODY by indenting the prefix width.
                let hang = crate::md::hang_width(&spans);
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
                // Full-width dim rule: 2-col indent + (avail) rule
                // glyphs covers the whole content row.
                out.push(Line::from(Span::styled(
                    format!("{INDENT}{}", "─".repeat(avail)),
                    theme.muted,
                )));
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
                    last.spans.push(Span::styled("▍", style));
                }
            }
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

/// Gutter block: EVERY display line carries the gutter (`┃ ` user
/// bar, `┆ ` reasoning; the assistant's `"  "` indent is handled by
/// [`push_markdown_block`]) so multi-line blocks read as one visual
/// unit.
fn push_gutter_block(
    out: &mut Vec<Line<'static>>,
    gutter: &str,
    gutter_style: Style,
    body: &str,
    body_style: Style,
    width: usize,
) {
    let avail = width.saturating_sub(str_width(gutter)).max(1);
    for logical in body.split('\n') {
        for seg in wrap_to_width(logical, avail) {
            out.push(Line::from(vec![
                Span::styled(gutter.to_owned(), gutter_style),
                Span::styled(seg, body_style),
            ]));
        }
    }
}

/// Semantic icon for a tool line (opencode's table), all Nerd Font
/// PUA glyphs — width exactly 1 column, no emoji presentation
/// variants: terminal shell, eye read, pencil write/edit, search
/// glob/grep, cog everything else. Matched by substring so
/// prefixed/sibling names (`read_skill`, `filesystem_bash`, …) land
/// on the right glyph; `call_agent` never reaches here (it becomes a
/// delegation marker).
fn tool_icon(name: &str) -> &'static str {
    if name.contains("shell") || name.contains("bash") {
        ""
    } else if name.contains("read") {
        ""
    } else if name.contains("write") || name.contains("edit") {
        ""
    } else if name.contains("glob") || name.contains("grep") {
        ""
    } else {
        ""
    }
}

/// One tool entry line: `head` is `{icon} name(args)` (icon from the
/// semantic table, [`tool_icon`]). The head style follows the status —
/// Running → Yellow (icon + name + args all Yellow), Done → DarkGray
/// muted (the whole line recedes,  keeps its Green accent), Failed
/// → default head with  Red + summary. Status spans join the last head
/// line when they fit, else start their own indented line.
fn push_tool_entry(
    out: &mut Vec<Line<'static>>,
    head: &str,
    status: &ToolEntryStatus,
    running_for: Option<Duration>,
    theme: &Theme,
    spinner: char,
    width: usize,
) {
    let head_style = match status {
        ToolEntryStatus::Running => theme.tool_running,
        ToolEntryStatus::Done { .. } => theme.muted,
        ToolEntryStatus::Failed { .. } => theme.assistant,
    };
    let head_rows = wrap_to_width(head, width);
    let mut lines: Vec<Line<'static>> = head_rows
        .into_iter()
        .map(|row| Line::from(Span::styled(row, head_style)))
        .collect();
    let (status_spans, status_width) = status_spans(status, theme, spinner, running_for);
    let last_width = lines.last().map(|l| l.width()).unwrap_or(0);
    if last_width + 1 + status_width <= width {
        if let Some(last) = lines.last_mut() {
            last.spans.extend(status_spans);
        }
    } else {
        let mut line = Line::from(Span::raw("  "));
        line.spans.extend(status_spans);
        lines.push(line);
    }
    out.extend(lines);
}

/// Status spans for a tool entry: glyph (colored) + trailing detail
/// (muted). Returns the spans and their total display width (for the
/// fit check in [`push_tool_entry`]).
fn status_spans(
    status: &ToolEntryStatus,
    theme: &Theme,
    spinner: char,
    running_for: Option<Duration>,
) -> (Vec<Span<'static>>, usize) {
    match status {
        ToolEntryStatus::Running => {
            let glyph = format!(" {spinner}");
            let detail = format!(
                " {}",
                format_ms(running_for.map(|d| d.as_millis() as u64).unwrap_or(0))
            );
            let width = str_width(&glyph) + str_width(&detail);
            (
                vec![
                    Span::styled(glyph, theme.tool_running),
                    Span::styled(detail, theme.muted),
                ],
                width,
            )
        }
        ToolEntryStatus::Done {
            bytes,
            truncated,
            elapsed_ms,
        } => {
            let glyph = " ".to_owned();
            let mut detail_parts: Vec<String> = Vec::new();
            if let Some(ms) = elapsed_ms {
                detail_parts.push(format_ms(*ms));
            }
            detail_parts.push(format_bytes(*bytes));
            if *truncated {
                detail_parts.push("…".to_owned());
            }
            let detail = format!(" {}", detail_parts.join(" "));
            let width = str_width(&glyph) + str_width(&detail);
            (
                vec![
                    Span::styled(glyph, theme.tool_success),
                    Span::styled(detail, theme.muted),
                ],
                width,
            )
        }
        ToolEntryStatus::Failed { summary } => {
            let glyph = " ".to_owned();
            let detail = format!(" {summary}");
            let width = str_width(&glyph) + str_width(&detail);
            (
                vec![
                    Span::styled(glyph, theme.tool_failure),
                    Span::styled(detail, theme.tool_failure),
                ],
                width,
            )
        }
    }
}

/// Approval outcome line: ` tool — decision` (Nerd hand icon, width 1
/// — the old U+270B ✋ rendered 2-wide on emoji terminals and shifted
/// the wrap math), the decision tinted by semantics (approved green /
/// denied red / else approval yellow).
fn push_approval_line(
    out: &mut Vec<Line<'static>>,
    tool_name: &str,
    decision: &str,
    theme: &Theme,
    width: usize,
) {
    let decision_style = match decision {
        "approved" => theme.tool_success,
        "denied" => theme.tool_failure,
        _ => theme.approval,
    };
    let text = format!(" {tool_name} — {decision}");
    for (i, seg) in wrap_to_width(&text, width).into_iter().enumerate() {
        if i == 0 {
            if let Some(rest) = seg.strip_prefix(" ") {
                out.push(Line::from(vec![
                    Span::styled(" ".to_owned(), theme.approval),
                    Span::styled(rest.to_owned(), decision_style),
                ]));
                continue;
            }
        }
        out.push(Line::from(Span::styled(seg, decision_style)));
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
            Action::ScrollBottom | Action::DismissOverlay => {
                // Esc/G/End: cancel pinning, follow the tail again.
                self.follow = true;
            }
            Action::Click(column, row) => {
                // A click inside the last-rendered `↓ 新内容 +N`
                // hint rectangle jumps back to the bottom. The
                // returned ScrollBottom IS the existing follow-restore
                // action (the App feeds it straight back here) — the
                // hit test itself does not mutate the pin state. No
                // hint / following / miss → inert (positional clicks
                // never scroll or focus).
                return match self.hint_hit_rect.get() {
                    Some(rect)
                        if rect.x <= *column
                            && *column < rect.x + rect.width
                            && rect.y <= *row
                            && *row < rect.y + rect.height =>
                    {
                        Some(Action::ScrollBottom)
                    }
                    _ => None,
                };
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
        // Remember the geometry for the next handle() (pin math).
        self.viewport.set((inner_width, visible));

        let lines = self.layout_lines(inner_width as usize, &ctx.theme, ctx.run.spinner_frame);
        let total = lines.len();
        let max_scroll = total.saturating_sub(visible as usize);
        let scroll = if self.follow {
            max_scroll
        } else {
            (self.scroll as usize).min(max_scroll)
        };

        f.render_widget(Paragraph::new(lines).scroll((clamp_u16(scroll), 0)), area);

        // Pinned with content below the viewport → bottom-right hint.
        // The hint is CLICKABLE: its span (widened 2 columns each
        // side, clamped to the hint row) is recorded in
        // `hint_hit_rect` for `handle`'s Action::Click hit-testing —
        // cleared to `None` on every render that does not draw it.
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
                let hint_text = format!("↓ 新内容 +{below}");
                f.render_widget(
                    Paragraph::new(Span::styled(hint_text.clone(), ctx.theme.user_label))
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
            },
            config: ConfigSummary {
                model_alias: String::new(),
                model_id: String::new(),
                provider_name: String::new(),
                max_depth: 4,
                max_tool_calls: 20,
                run_id: None,
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
                agent: "researcher".into()
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
        assert_eq!(before, vec!["┆ first thought continued"]);

        // Answer deltas begin — the reasoning rows are untouched and
        // the answer appends BELOW (block gap + md-rendered rows; the
        // ▍ tail cursor marks the live row).
        t.push_delta("the answer");
        let after = layout_text(&t, 60, &ctx.theme);
        assert_eq!(
            after,
            vec!["┆ first thought continued", "", "  the answer▍"],
            "reasoning block stays intact above the answer block"
        );
        // More answer deltas only grow the answer block's tail.
        t.push_delta(" grows");
        assert_eq!(
            layout_text(&t, 60, &ctx.theme),
            vec!["┆ first thought continued", "", "  the answer grows▍"]
        );
    }

    /// Bug 1, CJK + wrap angle: multi-line reasoning keeps every
    /// wrapped row stable when the answer starts (the wrap/offset math
    /// of the two streaming buffers must not overlap).
    #[test]
    fn multiline_cjk_reasoning_rows_stay_stable_when_answer_starts() {
        let mut t = TranscriptComponent::new();
        t.begin_streaming();
        let reasoning = "思考第一行内容较长会自动折行处理".repeat(2);
        t.push_reasoning(&reasoning);
        let ctx = test_ctx();
        let before = layout_text(&t, 20, &ctx.theme); // forces wrapping
        assert!(before.len() > 1, "reasoning wraps: {before:?}");
        assert!(before.iter().all(|l| l.starts_with("┆")));

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
        assert_eq!(after[before.len() + 1], "  最终答案▍");
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
        let tool_row = rows
            .iter()
            .position(|r| r.contains("read_file("))
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
            }]
        );
        let ctx = test_ctx();
        let rows = layout_text(&t, 40, &ctx.theme);
        assert!(rows.iter().all(|r| !r.starts_with("┆") || r.trim() != "┆"));
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
        t.viewport.set((40, 4)); // 10 lines + 9 block gaps = 19 → max scroll 15
        let mut ctx = test_ctx();

        t.handle(&Action::ScrollUp, &mut ctx);
        assert!(t.is_pinned());
        assert_eq!(t.scroll, 14, "unpin starts one line above the bottom");
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
        assert_eq!(t.scroll, 14);

        // Streaming content arrives while pinned: the offset must not
        // move (the hint "↓ 新内容" appears instead — render-level).
        t.push_delta("brand new content");
        t.handle(&Action::ScrollUp, &mut ctx); // one more line up: 14 → 13
        assert_eq!(t.scroll, 13);

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
        t.handle(&Action::ScrollPageUp, &mut ctx); // page = 2 → scroll 15-2 = 13
        assert!(t.is_pinned());
        assert_eq!(t.scroll, 13);

        // ScrollDown twice: 13 → 14 (still pinned) → 15 == max → re-follow.
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
        // PageDown (page = 2) walks to the bottom: 0 → 2 → … → 14
        // (still pinned, one above max), then the press that reaches
        // max 15 re-follows.
        for expected in [2u16, 4, 6, 8, 10, 12, 14] {
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

    // ── Hint click (fix-17: `↓ 新内容 +N` is clickable) ────────────────

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
    /// `↓ 新内容 +1` (11 display cols) right-aligns into the row rect
    /// x=1..39 → text at cols 28..=38; the hit rect widens ±2 cols,
    /// clamped to the row → cols 26..=38 on the last row (y=5).
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
                x: 26,
                y: 5,
                width: 13,
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
        // Text starts at col 28: col 26 (+2 slack) still hits, col 25
        // (+3) misses; the right edge clamps to the row rect (col 38
        // is the text's last column and also the rect's).
        assert_eq!(
            t.handle(&Action::Click(26, 5), &mut ctx),
            Some(Action::ScrollBottom)
        );
        assert_eq!(t.handle(&Action::Click(25, 5), &mut ctx), None);
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
        t.push_user("one two three four five"); // width 20 → bar gutter + wrapped body
        t.viewport.set((22, 10));
        let ctx = test_ctx();
        let lines = t.layout_lines(20, &ctx.theme, 0);
        // "┃ " (2) + body avail 18: "one two three four" (18), "five"
        assert_eq!(lines.len(), 2);
        let total: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
            .collect();
        // The ┃ bar covers the block's FULL height (every display line).
        assert_eq!(total[0], "┃ one two three four");
        assert_eq!(total[1], "┃ five");
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
        // Icon table: echo → cog, grep → search (substring match).
        assert!(joined[0].starts_with(" echo({})"), "{}", joined[0]);
        assert!(joined[0].contains(" 2B"), "{}", joined[0]);
        assert!(joined[1].starts_with(" grep({})"), "{}", joined[1]);
        assert!(joined[1].contains(" Error: x"), "{}", joined[1]);
    }

    /// The opencode icon table (Nerd PUA: terminal shell / eye read /
    /// pencil write-edit / search glob-grep / cog other), matched by
    /// substring.
    #[test]
    fn tool_icon_semantic_table() {
        assert_eq!(tool_icon("bash"), "");
        assert_eq!(tool_icon("shell"), "");
        assert_eq!(tool_icon("filesystem_bash"), "");
        assert_eq!(tool_icon("read_file"), "");
        assert_eq!(tool_icon("read_skill"), "");
        assert_eq!(tool_icon("write_file"), "");
        assert_eq!(tool_icon("edit_file"), "");
        assert_eq!(tool_icon("glob"), "");
        assert_eq!(tool_icon("grep"), "");
        assert_eq!(tool_icon("run_code"), "");
        assert_eq!(tool_icon("current_time"), "");
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
        assert_eq!(
            joined,
            vec![
                "┃ go",
                " read_file({})  2B",
                " grep({})  2B",
                "",
                "  done",
                "",
                " main · 9秒",
            ]
        );
    }

    /// The ` model · Ns` end-of-turn marker: only with turn_meta
    /// set, cube themed, cleared by `/new`.
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
            .all(|l| !l.spans.iter().any(|s| s.content.contains(""))));

        t.set_turn_meta("fast".to_owned(), 3, None);
        let lines = t.layout_lines(40, &ctx.theme, 0);
        let last = lines.last().expect("marker line");
        assert_eq!(last.spans.len(), 2);
        assert_eq!(last.spans[0].content, "");
        assert_eq!(last.spans[0].style, ctx.theme.turn_marker);
        assert_eq!(last.spans[1].content, " fast · 3秒");
        assert_eq!(last.spans[1].style, ctx.theme.muted);

        // `/new` resets the marker along with everything else.
        t.clear();
        let cleared = t.layout_lines(40, &ctx.theme, 0);
        assert_eq!(cleared.len(), 1, "back to the empty-state hint");
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
        t.layout_lines_at(60, &ctx.theme, 0, now)
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
            vec!["┆ pondering deeply"],
            "no live row while only reasoning streams"
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
        let line = t.layout_lines_at(60, &ctx.theme, 0, t0 + Duration::from_secs(2))[0].clone();
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
            vec!["┆ 思考过程", "", "  最终答案▍"],
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
            vec!["  第一段", "  ", "  第二段▍"]
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
        assert_eq!(texts, vec!["  类别  要点", "  代码  说明内容"]);
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
        assert_eq!(rows[0].0, "  | a | b |▍");
        assert_eq!(rows[0].1, ctx.theme.assistant);
        t.push_delta("\n|---|---|\n| 1 | 2 |");
        let rows = styled_rows(&t, 20, &ctx.theme);
        assert_eq!(rows[0].0, "  a  b");
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
            vec!["  • 第一项", "    内容", "    很长", "  • 短项",]
        );
        // Ordered: "1. " hangs 3 → continuation indent = 2 + 3.
        let mut t2 = TranscriptComponent::new();
        t2.entries
            .push(TranscriptEntry::Assistant("1. 第一项内容很长".into()));
        let rows2 = layout_text(&t2, 11, &ctx.theme);
        assert_eq!(rows2, vec!["  1. 第一项", "     内容", "     很长"]);
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
        assert_eq!(rows[0], "  第一段");
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
                    .find(|s| !s.content.trim().is_empty())
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
                "  Title",
                "  ",
                "  plain bold code",
                "  ",
                "  • item one",
                "  ",
                "  > quoted",
            ]
        );
        // Heading row: md_heading (Cyan + BOLD).
        assert_eq!(rows[0].1, ctx.theme.md_heading);
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
        assert_eq!(texts, vec!["  abcde", "  世界"]);
        assert_eq!(rows[0].1, ctx.theme.md_code, "code block styled");
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
        assert_eq!(texts, vec!["  aaa bbb ccc", "  ddd eee"]);
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
        assert_eq!(rows[0].0, "  not a heading yet bold▍");
        assert_eq!(rows[0].1, ctx.theme.md_heading);

        // Committed (flushed) assistant text renders identically —
        // only the tail cursor disappears at the flush.
        t.tool_start("echo", "{}");
        let rows = styled_rows(&t, 60, &ctx.theme);
        assert_eq!(rows[0].0, "  not a heading yet bold");
        assert_eq!(rows[0].1, ctx.theme.md_heading);
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
        assert_eq!(rows[0].0, "  标▍");
        assert_eq!(rows[0].1, ctx.theme.md_heading);
        t.push_delta("题");
        let rows = styled_rows(&t, 40, &ctx.theme);
        assert_eq!(rows[0].0, "  标题▍");
        assert_eq!(rows[0].1, ctx.theme.md_heading);

        // No space after `#` → not a heading, literal body text.
        let mut t2 = TranscriptComponent::new();
        t2.begin_streaming();
        t2.push_delta("#nos");
        t2.push_delta("pace");
        let rows = styled_rows(&t2, 40, &ctx.theme);
        assert_eq!(rows[0].0, "  #nospace▍");
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
        assert_eq!(rows[0].0, "  **bo▍");
        assert_eq!(rows[0].1, ctx.theme.assistant);
        t.push_delta("ld**");
        let rows = styled_rows(&t, 40, &ctx.theme);
        assert_eq!(rows[0].0, "  bold▍");
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
        assert_eq!(rows[0].0, "  para");
        assert_eq!(rows[1].0, "  fn m");
        assert_eq!(rows[1].1, ctx.theme.md_code);
        assert!(!rows[1].0.contains('▍'), "no cursor inside code");

        // Bare opening fence, nothing streamed into it yet: the cursor
        // must not land on the paragraph above either.
        let mut t2 = TranscriptComponent::new();
        t2.begin_streaming();
        t2.push_delta("para\n```");
        let rows = styled_rows(&t2, 20, &ctx.theme);
        assert_eq!(rows.last().unwrap().0, "  para");

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
            vec!["  one two", "  three four", "  five six▍"],
            "one cursor, on the last row only"
        );

        // Exactly-full last row: the cursor is dropped rather than
        // clipped ("one two three" = 13 cols; +2 indent = width 15).
        let mut t2 = TranscriptComponent::new();
        t2.begin_streaming();
        t2.push_delta("one two three");
        let rows = layout_text(&t2, 15, &ctx.theme);
        assert_eq!(rows, vec!["  one two three"]);
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
        assert!(text.contains("main · 9秒 · ↑130 ↓15"), "marker: {text}");
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
        assert!(text.contains("fast · 3秒"), "plain form: {text}");
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
            .position(|r| r.contains("main · 5秒"))
            .expect("marker row");
        let q2 = rows.iter().position(|r| r.contains("q2")).expect("q2 row");
        let a1 = rows.iter().position(|r| r.contains("a1")).expect("a1 row");
        assert!(a1 < marker && marker < q2, "a1 < marker < q2: {rows:?}");
    }
}
