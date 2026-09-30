//! Server link — the client half of the web-1 split.
//!
//! One place owns everything between the WebSocket and the App:
//!
//! * [`ServerLink`] — the App's outbound message sink. Production is the
//!   WS writer queue ([`WsLink`]); tests record into an in-memory
//!   vector ([`MemLink`]) and inject `ServerMsg`s through the same
//!   conversion path.
//! * [`to_tui`] — the single `ServerMsg → TuiEvent` conversion (pure,
//!   order-preserving: WS frame order IS event order, which the
//!   protocol guarantees per-connection). Includes the `EntryDto →
//!   TranscriptEntry` transcript mapping (1:1 — the server already
//!   clips args/output previews, the client adds no truncation).
//! * [`connect`] — the transport task: initial ladder (0.5s/1s/2s ×3
//!   then a hard error so `main` can exit 1), runtime reconnect with
//!   the same ladder cycling, `hello` on every (re)connect, snapshot
//!   wholesale on success.
//!
//! serde decoding happens inline in the receive loop (single task, no
//! spawned stages): snapshots can be large and a parallel decode task
//! could reorder events — the ordering guarantee is the whole design.

use std::sync::Mutex;
use std::time::Duration;

use anyhow::{anyhow, Result};
use futures_util::{SinkExt, StreamExt};
use openslate_protocol::{
    ApprovalAnswerChoice, ApprovalSummaryDto, ClientMsg, ConfigViewDto, EntryDto, ServerMsg,
    SnapshotDto, ToolStatusDto, TurnSummaryDto, PROTOCOL_VERSION,
};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::action::ApprovalChoice;
use crate::components::transcript::{ToolEntryDetail, ToolEntryStatus, TranscriptEntry};
use crate::event::{ApprovalSummary, TuiEvent, TurnSummary};

// ---------------------------------------------------------------------------
// Transport state
// ---------------------------------------------------------------------------

/// Transport phase of the server link (drives the freeze gate + notices).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkPhase {
    /// Attempting the initial connection (pre-first-snapshot).
    Connecting,
    /// Transport up, hello accepted.
    Connected,
    /// An established link dropped; retrying. `attempt` is 1-based
    /// within the current ladder cycle (1/2/3 → 0.5s/1s/2s).
    Reconnecting { attempt: u8 },
}

/// The App's outbound message sink (see module docs).
pub trait ServerLink: Send + Sync {
    /// Queue one client→server message. Silently dropped when the link
    /// is down (the freeze gate in the App prevents sends in that state;
    /// races lose the message and surface via the link notice instead).
    fn send(&self, msg: ClientMsg);
}

// ---------------------------------------------------------------------------
// ServerMsg → TuiEvent
// ---------------------------------------------------------------------------

/// Convert one server message into zero or more UI events. Order
/// preserving; `Snapshot` yields a single boxed event.
pub fn to_tui(msg: ServerMsg) -> Vec<TuiEvent> {
    let event = match msg {
        ServerMsg::Snapshot { session } => TuiEvent::Snapshot(session),
        ServerMsg::RequestStart { step, model } => TuiEvent::RequestStart { step, model },
        ServerMsg::FirstToken => TuiEvent::FirstToken,
        ServerMsg::Delta { text } => TuiEvent::Delta(text),
        ServerMsg::Reasoning { text } => TuiEvent::Reasoning(text),
        ServerMsg::InputEstimate { tokens } => TuiEvent::InputEstimate(tokens),
        ServerMsg::Usage { usage } => TuiEvent::Usage(usage),
        ServerMsg::RequestEnd => TuiEvent::RequestEnd,
        ServerMsg::StepEnd => TuiEvent::StepEnd,
        ServerMsg::ToolStart { name, args } => TuiEvent::ToolStart { name, args },
        ServerMsg::ToolEnd {
            name,
            bytes,
            truncated,
        } => TuiEvent::ToolEnd {
            name,
            bytes,
            truncated,
        },
        ServerMsg::ApprovalRequested { id, summary } => TuiEvent::ApprovalRequested {
            id,
            request: ApprovalSummary::from(summary),
        },
        ServerMsg::ApprovalResolved { id, choice } => TuiEvent::ApprovalResolved { id, choice },
        ServerMsg::TurnOk { summary } => {
            return vec![TuiEvent::TurnDone(Ok(TurnSummary::from(*summary)))];
        }
        ServerMsg::TurnError { message } => return vec![TuiEvent::TurnDone(Err(message))],
        ServerMsg::ConfigChanged { config } => TuiEvent::ConfigChanged(config),
        ServerMsg::ModelChanged { alias } => TuiEvent::ModelChanged(alias),
        ServerMsg::SessionReset => TuiEvent::SessionReset,
        ServerMsg::Notice { text, .. } => TuiEvent::Notice(text),
        ServerMsg::Error { message, .. } => TuiEvent::Notice(message),
    };
    vec![event]
}

impl From<ApprovalSummaryDto> for ApprovalSummary {
    fn from(dto: ApprovalSummaryDto) -> Self {
        Self {
            tool_name: dto.tool_name,
            arguments: dto.arguments,
            agent_id: dto.agent_id,
            risk_level: dto.risk_level,
        }
    }
}

impl From<TurnSummaryDto> for TurnSummary {
    fn from(dto: TurnSummaryDto) -> Self {
        Self {
            run_id: dto.run_id,
            status: dto.status,
            messages: dto.messages,
            total_steps: dto.total_steps,
            total_input_tokens: dto.total_input_tokens,
            total_output_tokens: dto.total_output_tokens,
            total_cost_usd: dto.total_cost_usd,
            model: dto.model,
        }
    }
}

/// Map the server's transcript mirror onto the display entries (1:1 —
/// no re-truncation, no transformation; the server applies the same
/// caps the local transcript used to).
pub fn entries_from_dto(dtos: Vec<EntryDto>) -> Vec<TranscriptEntry> {
    dtos.into_iter().map(entry_from_dto).collect()
}

fn entry_from_dto(dto: EntryDto) -> TranscriptEntry {
    match dto {
        EntryDto::User { text } => TranscriptEntry::User(text),
        EntryDto::Assistant { text } => TranscriptEntry::Assistant(text),
        EntryDto::Reasoning { text } => TranscriptEntry::Reasoning(text),
        EntryDto::ToolCall {
            name,
            args,
            call_id,
            status,
            detail,
        } => TranscriptEntry::ToolCall {
            name,
            args,
            call_id,
            status: tool_status_from_dto(status),
            detail: ToolEntryDetail {
                args: detail.args,
                output: detail.output,
            },
        },
        EntryDto::Approval {
            tool_name,
            decision,
        } => TranscriptEntry::Approval {
            tool_name,
            decision,
        },
        EntryDto::Delegate { agent, done } => TranscriptEntry::Delegate { agent, done },
        EntryDto::StepBreak => TranscriptEntry::StepBreak,
        EntryDto::Meta { text } => TranscriptEntry::Meta(text),
    }
}

fn tool_status_from_dto(dto: ToolStatusDto) -> ToolEntryStatus {
    match dto {
        ToolStatusDto::Running => ToolEntryStatus::Running,
        ToolStatusDto::Done {
            bytes,
            truncated,
            elapsed_ms,
        } => ToolEntryStatus::Done {
            bytes,
            truncated,
            elapsed_ms,
        },
        ToolStatusDto::Failed { summary } => ToolEntryStatus::Failed { summary },
    }
}

/// The config mirror the App keeps for local display/UX decisions
/// (model panel, `/model` alias check, status limits). The server stays
/// the authority; `ConfigChanged` swaps this wholesale.
pub fn config_from_view(view: &ConfigViewDto) -> openslate_core::config::OpenSlateConfig {
    use openslate_core::config::{LimitsConfig, ModelConfig, OpenSlateConfig, ProviderConfig};
    use std::collections::HashMap;

    let providers: HashMap<String, ProviderConfig> = view
        .providers
        .iter()
        .map(|(k, v)| (k.clone(), ProviderConfig::from(v.clone())))
        .collect();
    let models: HashMap<String, ModelConfig> = view
        .models
        .iter()
        .map(|(k, v)| (k.clone(), ModelConfig::from(v.clone())))
        .collect();
    let levels: HashMap<String, String> = view.levels.clone().into_iter().collect();
    let limits = LimitsConfig {
        max_steps: view.limits.max_steps,
        max_depth: view.limits.max_depth,
        max_tool_calls: view.limits.max_tool_calls,
        max_child_agent_calls: view.limits.max_child_agent_calls,
        timeout_ms: view.limits.timeout_ms,
        max_context_messages: view.limits.max_context_messages,
        max_context_bytes: view.limits.max_context_bytes,
        max_output_bytes: view.limits.max_output_bytes,
        auto_compact: view.limits.auto_compact,
        parallel_tool_calls: view.limits.parallel_tool_calls,
    };
    OpenSlateConfig {
        providers,
        models,
        levels,
        limits: Some(limits),
        // Display-only sections the wire view intentionally omits; the
        // local `[tui]` read in `main` overlays the real icon overrides.
        project: None,
        database: None,
        prompts: None,
        trace: None,
        mcp: None,
        builtin_tools: Default::default(),
        skills: Default::default(),
        ptc: Default::default(),
        approval: None,
        tui: Default::default(),
    }
}

/// Test/bootstrap convenience: the INVERSE of [`config_from_view`] —
/// build a wire view from a config (hello-snapshot payloads for test
/// seeding; the protocol's per-entry `From` impls do the field work).
pub fn view_from_config(config: &openslate_core::config::OpenSlateConfig) -> ConfigViewDto {
    use openslate_protocol::{AgentNodeDto, LimitsDto, ModelDto, ProviderDto};
    let limits = config
        .limits
        .as_ref()
        .cloned()
        .unwrap_or(openslate_core::config::LimitsConfig {
            max_steps: 8,
            max_depth: 4,
            max_tool_calls: 20,
            max_child_agent_calls: 8,
            timeout_ms: 300000,
            max_context_messages: 200,
            max_context_bytes: 512000,
            max_output_bytes: 65536,
            auto_compact: true,
            parallel_tool_calls: true,
        });
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
            max_steps: limits.max_steps,
            max_depth: limits.max_depth,
            max_tool_calls: limits.max_tool_calls,
            max_child_agent_calls: limits.max_child_agent_calls,
            timeout_ms: limits.timeout_ms,
            max_context_messages: limits.max_context_messages,
            max_context_bytes: limits.max_context_bytes,
            max_output_bytes: limits.max_output_bytes,
            auto_compact: limits.auto_compact,
            parallel_tool_calls: limits.parallel_tool_calls,
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

/// Test/bootstrap convenience: a hello-shaped snapshot carrying a view
/// built from `config` (empty transcript, idle session).
pub fn snapshot_from_config(
    config: &openslate_core::config::OpenSlateConfig,
    model_alias: &str,
) -> SnapshotDto {
    SnapshotDto {
        proto: 1,
        session_id: "test-session".into(),
        session_label: "test".into(),
        transcript: vec![],
        running: false,
        depth_cur: 0,
        agents_running: 0,
        tool_calls_cur: 0,
        model_alias: model_alias.into(),
        pending_approval: None,
        config: view_from_config(config),
        session_stats: Default::default(),
    }
}

/// Snapshot hydration data the App pulls out of a `SnapshotDto` besides
/// the transcript itself (small-value mirror of the session state).
pub struct SessionView {
    pub session_id: String,
    pub session_label: String,
    pub model_alias: String,
    pub running: bool,
    pub depth_cur: u32,
    pub agents_running: u32,
    pub tool_calls_cur: u32,
}

impl From<&SnapshotDto> for SessionView {
    fn from(s: &SnapshotDto) -> Self {
        Self {
            session_id: s.session_id.clone(),
            session_label: s.session_label.clone(),
            model_alias: s.model_alias.clone(),
            running: s.running,
            depth_cur: s.depth_cur,
            agents_running: s.agents_running,
            tool_calls_cur: s.tool_calls_cur,
        }
    }
}

/// UI choice → wire vocabulary.
pub fn approval_choice_to_msg(choice: ApprovalChoice) -> ApprovalAnswerChoice {
    match choice {
        ApprovalChoice::Approve => ApprovalAnswerChoice::Approve,
        ApprovalChoice::Deny => ApprovalAnswerChoice::Deny,
        ApprovalChoice::ApproveAll => ApprovalAnswerChoice::ApproveAll,
    }
}

/// Wire choice vocabulary (approval_resolved broadcast) → the display
/// label used by transcript decision lines (`approved`/`denied`/
/// `approve-all`, matching `EntryDto::Approval.decision`).
pub fn approval_choice_label(choice: &str) -> &'static str {
    match choice {
        "approve" => "approved",
        "deny" => "denied",
        _ => "approve-all",
    }
}

// ---------------------------------------------------------------------------
// URL normalization
// ---------------------------------------------------------------------------

/// Normalize a `--server` value into a full WS URL:
///
/// * bare host[:port] → `ws://host[:port]`
/// * `http(s)://` → `ws(s)://`
/// * empty path (`/` or none) → `/api/ws` appended (the server's single
///   protocol endpoint); an explicit path is kept as typed.
pub fn normalize_server_url(input: &str) -> String {
    let trimmed = input.trim();
    let with_scheme = if trimmed.starts_with("ws://")
        || trimmed.starts_with("wss://")
        || trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
    {
        trimmed.to_owned()
    } else {
        format!("ws://{trimmed}")
    };
    let upgraded = with_scheme
        .replacen("http://", "ws://", 1)
        .replacen("https://", "wss://", 1);
    let path = upgraded
        .find("://")
        .map(|i| {
            upgraded[i + 3..]
                .find('/')
                .map(|j| upgraded[i + 3 + j..].to_owned())
                .unwrap_or_default()
        })
        .unwrap_or_default();
    if path.is_empty() || path == "/" {
        format!("{}/api/ws", upgraded.trim_end_matches('/'))
    } else {
        upgraded
    }
}

// ---------------------------------------------------------------------------
// Test seam
// ---------------------------------------------------------------------------

/// In-memory [`ServerLink`] for tests: records every sent `ClientMsg`
/// and can inject `ServerMsg`s into the App's event channel through the
/// SAME [`to_tui`] conversion the production transport uses.
pub struct MemLink {
    sent: Mutex<Vec<ClientMsg>>,
    events: UnboundedSender<TuiEvent>,
}

impl MemLink {
    /// Create the link together with the receiving end the test drives
    /// `App::run`'s engine-event arm with (`drain_engine_events`).
    pub fn pair() -> (std::sync::Arc<Self>, UnboundedReceiver<TuiEvent>) {
        let (tx, rx) = unbounded_channel();
        (
            std::sync::Arc::new(Self {
                sent: Mutex::new(Vec::new()),
                events: tx,
            }),
            rx,
        )
    }

    /// Wrap an EXISTING events channel (the App constructor owns one —
    /// tests that build the App first reuse its receiver).
    pub fn new(events: UnboundedSender<TuiEvent>) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            sent: Mutex::new(Vec::new()),
            events,
        })
    }

    /// All `ClientMsg`s sent so far, drained.
    pub fn take_sent(&self) -> Vec<ClientMsg> {
        self.sent
            .lock()
            .map(|mut v| std::mem::take(&mut *v))
            .unwrap_or_default()
    }

    /// Peek without draining.
    pub fn sent_snapshot(&self) -> Vec<ClientMsg> {
        self.sent.lock().map(|v| v.clone()).unwrap_or_default()
    }

    /// Inject a server message: converts through [`to_tui`] and pushes
    /// the resulting UI events into the App's channel.
    pub fn emit(&self, msg: ServerMsg) {
        for event in to_tui(msg) {
            let _ = self.events.send(event);
        }
    }

    /// Inject a raw UI event (e.g. [`TuiEvent::LinkState`]) straight
    /// into the App's channel — for events with no ServerMsg origin.
    pub fn emit_tui(&self, event: TuiEvent) {
        let _ = self.events.send(event);
    }
}

impl ServerLink for MemLink {
    fn send(&self, msg: ClientMsg) {
        if let Ok(mut v) = self.sent.lock() {
            v.push(msg);
        }
    }
}

// ---------------------------------------------------------------------------
// Production transport
// ---------------------------------------------------------------------------

/// Reconnect ladder (web-1): 0.5s / 1s / 2s. The INITIAL connect fails
/// after one full cycle (main exits 1); runtime drops cycle it forever.
const LADDER_MS: [u64; 3] = [500, 1000, 2000];

/// Production [`ServerLink`]: queues into the link task's writer side.
#[derive(Clone)]
pub struct WsLink {
    tx: UnboundedSender<ClientMsg>,
}

impl ServerLink for WsLink {
    fn send(&self, msg: ClientMsg) {
        // Channel-closed means the link task is gone (runtime teardown):
        // drop silently; the App's link-state notice already covers it.
        let _ = self.tx.send(msg);
    }
}

/// What [`connect`] hands back to `main`.
pub struct Connection {
    pub link: WsLink,
    /// The App's engine-event receiver (feeds `Action::Engine`).
    pub events: UnboundedReceiver<TuiEvent>,
}

/// Connect to the server with the INITIAL ladder (0.5s/1s/2s): returns
/// once the first snapshot arrived (link up + hello accepted), or
/// `Err` after the ladder exhausted (main exits 1). The transport task
/// then runs until the sender side drops (App teardown) or the runtime
/// shuts down; runtime drops reconnect with the same cycling ladder.
pub async fn connect(url: &str, token: Option<String>) -> Result<Connection> {
    let ws_url = normalize_server_url(url);
    let (events_tx, events_rx) = unbounded_channel::<TuiEvent>();
    let (client_tx, client_rx) = unbounded_channel::<ClientMsg>();
    let (ready_tx, ready_rx) = oneshot::channel::<Result<(), String>>();

    let _ = events_tx.send(TuiEvent::LinkState(LinkPhase::Connecting));

    let task_url = ws_url.clone();
    tokio::spawn(async move {
        link_task(task_url, token, events_tx, client_rx, ready_tx).await;
    });

    match ready_rx.await {
        Ok(Ok(())) => Ok(Connection {
            link: WsLink { tx: client_tx },
            events: events_rx,
        }),
        Ok(Err(e)) => Err(anyhow!(e)),
        Err(_) => Err(anyhow!("连接任务在收到首个快照前退出")),
    }
}

/// The transport loop: connect → hello → pump frames until the
/// connection dies → ladder → repeat. Exits when the App drops the
/// [`WsLink`] (send channel closed) or the INITIAL ladder exhausted
/// before the first snapshot (the `ready` signal then carries the Err).
async fn link_task(
    url: String,
    token: Option<String>,
    events: UnboundedSender<TuiEvent>,
    mut client_rx: UnboundedReceiver<ClientMsg>,
    ready: oneshot::Sender<Result<(), String>>,
) {
    // `Some` until the first snapshot passed through (the initial
    // handshake completing); runtime reconnects never re-arm it.
    let mut ready = Some(ready);
    loop {
        match establish(&url, &token, &events, &mut client_rx, &mut ready).await {
            PumpOutcome::AppGone => return,
            PumpOutcome::LinkDropped => {}
        }
        for (i, delay_ms) in LADDER_MS.iter().enumerate() {
            let attempt = (i as u8) + 1;
            // Pre-ready drops are still connecting (「重连」wording
            // would be wrong); post-ready ones are true reconnects.
            let phase = if ready.is_some() {
                LinkPhase::Connecting
            } else {
                LinkPhase::Reconnecting { attempt }
            };
            let _ = events.send(TuiEvent::LinkState(phase));
            tokio::time::sleep(Duration::from_millis(*delay_ms)).await;
            // Drain late sends that raced the drop so they fail fast
            // client-side instead of queueing behind the reconnect.
            while client_rx.try_recv().is_ok() {}
            tracing::warn!(attempt, url = %url, "server link down; retrying");
            match establish(&url, &token, &events, &mut client_rx, &mut ready).await {
                PumpOutcome::AppGone => return,
                PumpOutcome::LinkDropped => continue,
            }
        }
        // Ladder exhausted. Initial (never became ready) → report and
        // stop (main exits 1). Runtime → cycle the ladder forever.
        if let Some(sender) = ready.take() {
            let _ = sender.send(Err(
                "无法连接服务器（0.5s/1s/2s 重试均失败），请检查 --server 地址与服务器状态"
                    .to_owned(),
            ));
            return;
        }
    }
}

enum PumpOutcome {
    /// Connection lost (io error / close / handshake fail).
    LinkDropped,
    /// The App dropped the link handle — task exits.
    AppGone,
}

/// One connection attempt: TCP+HTTP upgrade, `hello`, then the frame
/// pump (reads decode inline; writes forward the App's queue). The
/// `ready` signal fires when the first `Snapshot` frame passes through
/// (the initial handshake's completion proof) and is consumed.
async fn establish(
    url: &str,
    token: &Option<String>,
    events: &UnboundedSender<TuiEvent>,
    client_rx: &mut UnboundedReceiver<ClientMsg>,
    ready: &mut Option<oneshot::Sender<Result<(), String>>>,
) -> PumpOutcome {
    let stream = match tokio_tungstenite::connect_async(url).await {
        Ok((s, _resp)) => s,
        Err(e) => {
            tracing::warn!(error = %e, url = %url, "WS connect failed");
            return PumpOutcome::LinkDropped;
        }
    };
    let (mut sink, mut stream) = stream.split();

    let hello = ClientMsg::Hello {
        proto: PROTOCOL_VERSION,
        token: token.clone(),
    };
    if send_msg(&mut sink, &hello).await.is_err() {
        return PumpOutcome::LinkDropped;
    }

    let _ = events.send(TuiEvent::LinkState(LinkPhase::Connected));

    loop {
        tokio::select! {
            frame = stream.next() => match frame {
                Some(Ok(WsMessage::Text(text))) => {
                    match serde_json::from_str::<ServerMsg>(&text) {
                        Ok(msg) => {
                            if matches!(msg, ServerMsg::Snapshot { .. }) {
                                if let Some(sender) = ready.take() {
                                    // Receiver already gone = main aborted:
                                    // keep pumping regardless (AppGone via
                                    // the send channel covers teardown).
                                    let _ = sender.send(Ok(()));
                                }
                            }
                            for event in to_tui(msg) {
                                let _ = events.send(event);
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "unparseable server frame dropped");
                            let _ = events.send(TuiEvent::Notice(format!(
                                "服务器消息解析失败: {e}"
                            )));
                        }
                    }
                }
                Some(Ok(WsMessage::Ping(payload))) => {
                    // Auto-pong: split halves may not flush the queued
                    // reply until the next write — answer explicitly.
                    if sink.send(WsMessage::Pong(payload)).await.is_err() {
                        return PumpOutcome::LinkDropped;
                    }
                }
                Some(Ok(_)) => {} // pong / binary: ignored
                Some(Err(e)) => {
                    tracing::warn!(error = %e, "WS read failed");
                    return PumpOutcome::LinkDropped;
                }
                None => {
                    tracing::warn!("server closed the link");
                    return PumpOutcome::LinkDropped;
                }
            },
            outbound = client_rx.recv() => match outbound {
                Some(msg) => {
                    if send_msg(&mut sink, &msg).await.is_err() {
                        return PumpOutcome::LinkDropped;
                    }
                }
                None => return PumpOutcome::AppGone,
            },
        }
    }
}

async fn send_msg<S>(sink: &mut S, msg: &ClientMsg) -> std::result::Result<(), ()>
where
    S: SinkExt<WsMessage> + Unpin,
    <S as futures_util::Sink<WsMessage>>::Error: std::fmt::Debug,
{
    let json = serde_json::to_string(msg).map_err(|e| {
        tracing::error!(error = %e, "ClientMsg serialization failed");
    })?;
    sink.send(WsMessage::Text(json.into()))
        .await
        .map_err(|e| tracing::warn!(error = ?e, "WS send failed"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use openslate_core::types::{Message, MessageRole, RunId, RunStatus};
    use openslate_protocol::ToolEntryDetailDto;

    fn summary_dto() -> TurnSummaryDto {
        TurnSummaryDto {
            run_id: RunId("run-9".into()),
            status: RunStatus::Completed,
            total_steps: 2,
            total_input_tokens: 11,
            total_output_tokens: 7,
            total_cost_usd: 0.002,
            model: "test-model".into(),
            messages: vec![Message {
                role: MessageRole::User,
                content: "hi".into(),
                tool_call_id: None,
                name: None,
                tool_calls: None,
            }],
        }
    }

    #[test]
    fn turn_ok_maps_to_turn_done_ok_with_fields() {
        let events = to_tui(ServerMsg::TurnOk {
            summary: Box::new(summary_dto()),
        });
        assert_eq!(events.len(), 1);
        let TuiEvent::TurnDone(Ok(summary)) = &events[0] else {
            panic!("expected TurnDone(Ok)");
        };
        assert_eq!(summary.run_id.0, "run-9");
        assert_eq!(summary.messages.len(), 1);
        assert_eq!(summary.total_input_tokens, 11);
    }

    #[test]
    fn turn_error_maps_to_turn_done_err() {
        let events = to_tui(ServerMsg::TurnError {
            message: "boom".into(),
        });
        assert_eq!(events, vec![TuiEvent::TurnDone(Err("boom".into()))]);
    }

    #[test]
    fn streaming_variants_map_one_to_one() {
        use openslate_core::types::Usage;
        assert_eq!(
            to_tui(ServerMsg::RequestStart {
                step: 3,
                model: "m".into()
            }),
            vec![TuiEvent::RequestStart {
                step: 3,
                model: "m".into()
            }]
        );
        assert_eq!(
            to_tui(ServerMsg::InputEstimate { tokens: 42 }),
            vec![TuiEvent::InputEstimate(42)]
        );
        assert_eq!(
            to_tui(ServerMsg::ToolEnd {
                name: "t".into(),
                bytes: 9,
                truncated: true
            }),
            vec![TuiEvent::ToolEnd {
                name: "t".into(),
                bytes: 9,
                truncated: true
            }]
        );
        assert_eq!(
            to_tui(ServerMsg::Usage {
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 2,
                    cached_input_tokens: None
                }
            })
            .len(),
            1
        );
    }

    #[test]
    fn error_and_notice_map_to_notice_text() {
        assert_eq!(
            to_tui(ServerMsg::Notice {
                text: "回合进行中".into(),
                level: openslate_protocol::NoticeLevel::Warn,
            }),
            vec![TuiEvent::Notice("回合进行中".into())]
        );
        assert_eq!(
            to_tui(ServerMsg::Error {
                code: "proto_mismatch".into(),
                message: "协议版本不符".into(),
            }),
            vec![TuiEvent::Notice("协议版本不符".into())]
        );
    }

    #[test]
    fn entry_dto_tool_call_maps_status_and_detail() {
        let entries = entries_from_dto(vec![
            EntryDto::ToolCall {
                name: "shell".into(),
                args: "{\"cmd\":\"ls\"}".into(),
                call_id: Some("tc-1".into()),
                status: ToolStatusDto::Done {
                    bytes: 128,
                    truncated: true,
                    elapsed_ms: Some(40),
                },
                detail: ToolEntryDetailDto {
                    args: "{\"cmd\": \"ls\"}".into(),
                    output: Some("file-a\nfile-b".into()),
                },
            },
            EntryDto::ToolCall {
                name: "boom".into(),
                args: String::new(),
                call_id: None,
                status: ToolStatusDto::Failed {
                    summary: "exit 1".into(),
                },
                detail: ToolEntryDetailDto {
                    args: String::new(),
                    output: None,
                },
            },
            EntryDto::Delegate {
                agent: "researcher".into(),
                done: false,
            },
            EntryDto::Approval {
                tool_name: "shell".into(),
                decision: "approved".into(),
            },
        ]);
        assert_eq!(entries.len(), 4);
        let TranscriptEntry::ToolCall {
            status,
            detail,
            call_id,
            ..
        } = &entries[0]
        else {
            panic!("tool call");
        };
        assert_eq!(
            status,
            &ToolEntryStatus::Done {
                bytes: 128,
                truncated: true,
                elapsed_ms: Some(40)
            }
        );
        assert_eq!(call_id.as_deref(), Some("tc-1"));
        assert_eq!(detail.output.as_deref(), Some("file-a\nfile-b"));
        assert!(matches!(
            &entries[1],
            TranscriptEntry::ToolCall {
                status: ToolEntryStatus::Failed { summary },
                ..
            } if summary == "exit 1"
        ));
        assert_eq!(
            entries[2],
            TranscriptEntry::Delegate {
                agent: "researcher".into(),
                done: false
            }
        );
        assert_eq!(
            entries[3],
            TranscriptEntry::Approval {
                tool_name: "shell".into(),
                decision: "approved".into()
            }
        );
    }

    #[test]
    fn url_normalization_cases() {
        assert_eq!(
            normalize_server_url("127.0.0.1:7800"),
            "ws://127.0.0.1:7800/api/ws"
        );
        assert_eq!(
            normalize_server_url("ws://127.0.0.1:7800"),
            "ws://127.0.0.1:7800/api/ws"
        );
        assert_eq!(normalize_server_url("http://h:1"), "ws://h:1/api/ws");
        assert_eq!(normalize_server_url("https://h:1"), "wss://h:1/api/ws");
        assert_eq!(normalize_server_url("ws://h:1/custom"), "ws://h:1/custom");
        assert_eq!(normalize_server_url("ws://h:1/"), "ws://h:1/api/ws");
    }

    #[test]
    fn approval_choice_mapping_and_labels() {
        assert_eq!(
            approval_choice_to_msg(ApprovalChoice::Approve),
            ApprovalAnswerChoice::Approve
        );
        assert_eq!(
            approval_choice_to_msg(ApprovalChoice::Deny),
            ApprovalAnswerChoice::Deny
        );
        assert_eq!(
            approval_choice_to_msg(ApprovalChoice::ApproveAll),
            ApprovalAnswerChoice::ApproveAll
        );
        assert_eq!(approval_choice_label("approve"), "approved");
        assert_eq!(approval_choice_label("deny"), "denied");
        assert_eq!(approval_choice_label("approve_all"), "approve-all");
        assert_eq!(approval_choice_label("garbage"), "approve-all");
    }

    #[test]
    fn mem_link_records_and_emits_through_to_tui() {
        let (link, mut rx) = MemLink::pair();
        link.send(ClientMsg::Cancel);
        link.emit(ServerMsg::ModelChanged {
            alias: "fast".into(),
        });
        assert_eq!(link.take_sent(), vec![ClientMsg::Cancel]);
        assert_eq!(
            rx.try_recv().unwrap(),
            TuiEvent::ModelChanged("fast".into())
        );
        assert!(rx.try_recv().is_err());
        assert!(link.take_sent().is_empty(), "take drains");
    }

    #[test]
    fn config_from_view_mirrors_tables_and_limits() {
        let view = openslate_protocol::ConfigViewDto {
            providers: [(
                "p".to_owned(),
                openslate_protocol::ProviderDto {
                    base_url: "https://x".into(),
                    api_key_env: "P_KEY".into(),
                    adapter: Some("openai".into()),
                    max_attempts: 2,
                    retry_base_ms: 100,
                },
            )]
            .into_iter()
            .collect(),
            models: [(
                "main".to_owned(),
                openslate_protocol::ModelDto {
                    provider: "p".into(),
                    model: "m1".into(),
                    max_context_tokens: Some(8192),
                    max_output_tokens: None,
                    supports_tool_call: true,
                    supports_vision: false,
                    supports_reasoning: false,
                    input_price_per_mtok: Some(1.0),
                    output_price_per_mtok: None,
                },
            )]
            .into_iter()
            .collect(),
            levels: [("main".to_owned(), "main".to_owned())]
                .into_iter()
                .collect(),
            limits: openslate_protocol::LimitsDto {
                max_steps: 8,
                max_depth: 4,
                max_tool_calls: 20,
                max_child_agent_calls: 8,
                timeout_ms: 300000,
                max_context_messages: 200,
                max_context_bytes: 512000,
                max_output_bytes: 65536,
                auto_compact: true,
                parallel_tool_calls: true,
            },
            agents: openslate_protocol::AgentNodeDto {
                id: "root".into(),
                name: "Root".into(),
                model: "main".into(),
                children: vec![],
            },
            skills: vec![],
            active_config: "/x.toml".into(),
            global_config: None,
            local_config: None,
        };
        let config = config_from_view(&view);
        assert_eq!(config.providers.len(), 1);
        assert_eq!(config.models["main"].model, "m1");
        assert_eq!(config.levels["main"], "main");
        let limits = config.limits.expect("limits mirrored");
        assert_eq!(limits.max_depth, 4);
        assert_eq!(limits.timeout_ms, 300000);
    }
}
