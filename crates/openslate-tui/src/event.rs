//! Engine bridge — connects the core runtime to the TUI event loop.
//!
//! Design (architecture spec v2, frozen):
//!
//! * [`EngineBridge`] implements the core [`ProgressCallback`] trait and
//!   `try_send`s every progress hook into an unbounded
//!   `tokio::sync::mpsc` channel as [`TuiEvent`]s. The App's `select!`
//!   loop turns them into `Action::Engine` and dispatches.
//! * [`ApprovalBridge`] implements the **synchronous** core
//!   [`ApprovalCallback`]: `decide()` publishes an
//!   [`TuiEvent::ApprovalRequested`] and then blocks on a
//!   `std::sync::mpsc` receiver until the UI answers (y/n/a). The blocking
//!   happens on a worker thread of the multi-thread runtime — this is why
//!   the turn future must NEVER be inlined into the UI `select!` loop
//!   ([`spawn_turn`] owns it) and why `current_thread` runtimes are
//!   forbidden. If the UI side drops or closes the channel (shutdown),
//!   `recv()` errors and the request is denied.
//! * Turn ownership: the App holds `Option<RunManager>`; on submit it
//!   `take()`s the manager, moves it into [`spawn_turn`]'s task, and the
//!   resulting [`TuiEvent::TurnDone`] carries the manager BACK (in both
//!   the Ok and Err arms) so the App can re-fill its slot. The per-turn
//!   `message_sink` mutation needs exclusive access — the manager cannot
//!   be `Arc`ed.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use openslate_core::approval::{ApprovalCallback, ApprovalDecision, ApprovalRequest};
use openslate_core::execution::ExecutionTree;
use openslate_core::provider::ProgressCallback;
use openslate_core::run_manager::{ManagedRunResult, RunManager};
use openslate_core::runtime::CancellationToken;
use openslate_core::types::{Message, RunId, RunStatus, Usage};
use tokio::sync::mpsc::UnboundedSender;

use crate::action::ApprovalChoice;

/// A display-oriented summary of an [`ApprovalRequest`] (the full request
/// is not `Send`-friendly display material; the UI only needs these
/// fields for the yellow banner).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalSummary {
    /// Tool that wants to run.
    pub tool_name: String,
    /// Arguments preview (compact JSON string).
    pub arguments: String,
    /// Agent requesting execution (source of the banner).
    pub agent_id: String,
    /// Assessed risk: "low" | "medium" | "high".
    pub risk_level: String,
}

impl ApprovalSummary {
    fn truncate(s: &str, max_len: usize) -> String {
        if s.len() <= max_len {
            s.to_owned()
        } else {
            // Walk back to a UTF-8 char boundary (CJK-safe).
            let mut end = max_len.saturating_sub(1);
            while end > 0 && !s.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}…", &s[..end])
        }
    }
}

impl From<&ApprovalRequest> for ApprovalSummary {
    fn from(req: &ApprovalRequest) -> Self {
        let args = req.arguments.to_string();
        Self {
            tool_name: req.tool_name.clone(),
            arguments: Self::truncate(args.trim_matches('"'), 120),
            agent_id: req.agent_id.clone(),
            risk_level: req.risk_level.to_string(),
        }
    }
}

/// Everything the engine task reports to the UI. Frozen surface: variant
/// set matches the P3 needs; internals of `TurnDone` carry the manager
/// back to the App (both arms — see module docs).
// TurnDone carries the RunManager by design (ownership round-trip);
// boxing would change the frozen shape for no dispatch-frequency gain.
#[allow(clippy::large_enum_variant)]
pub enum TuiEvent {
    /// A model request is being sent (step number, model id).
    RequestStart { step: u32, model: String },
    /// First content token of the current request arrived (TTFT).
    FirstToken,
    /// One content delta from the model.
    Delta(String),
    /// One reasoning/thinking delta from the model.
    Reasoning(String),
    /// Token usage for the current request (may arrive at stream end).
    Usage(Usage),
    /// The model response was fully received.
    RequestEnd,
    /// A step fully finished (response + its tool calls executed).
    StepEnd,
    /// A tool is about to execute (name + args display string).
    ToolStart { name: String, args: String },
    /// A tool finished (name, output bytes, whether output was truncated).
    ToolEnd {
        name: String,
        bytes: usize,
        truncated: bool,
    },
    /// A tool call needs user approval. `id` correlates the UI answer with
    /// the blocked `decide()` call (defense against crossed wires).
    ApprovalRequested { id: u64, request: ApprovalSummary },
    /// The turn finished. Ok → summary + the manager (returned for the next
    /// turn). Err → error message + the manager (when the task still owns
    /// it; `None` only if the manager was somehow lost).
    TurnDone(Result<(TurnSummary, RunManager), (String, Option<RunManager>)>),
}

/// Manual `Debug`: `TurnDone` carries a non-`Debug` `RunManager`, so its
/// arm prints only the outcome discriminant.
impl std::fmt::Debug for TuiEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RequestStart { step, model } => f
                .debug_struct("RequestStart")
                .field("step", step)
                .field("model", model)
                .finish(),
            Self::FirstToken => write!(f, "FirstToken"),
            Self::Delta(text) => f.debug_tuple("Delta").field(text).finish(),
            Self::Reasoning(text) => f.debug_tuple("Reasoning").field(text).finish(),
            Self::Usage(usage) => f.debug_tuple("Usage").field(usage).finish(),
            Self::RequestEnd => write!(f, "RequestEnd"),
            Self::StepEnd => write!(f, "StepEnd"),
            Self::ToolStart { name, args } => f
                .debug_struct("ToolStart")
                .field("name", name)
                .field("args", args)
                .finish(),
            Self::ToolEnd {
                name,
                bytes,
                truncated,
            } => f
                .debug_struct("ToolEnd")
                .field("name", name)
                .field("bytes", bytes)
                .field("truncated", truncated)
                .finish(),
            Self::ApprovalRequested { id, request } => f
                .debug_struct("ApprovalRequested")
                .field("id", id)
                .field("request", request)
                .finish(),
            Self::TurnDone(Ok(_)) => write!(f, "TurnDone(Ok)"),
            Self::TurnDone(Err(_)) => write!(f, "TurnDone(Err)"),
        }
    }
}

/// Manual `PartialEq` (the `TurnDone` payload contains a non-comparable
/// `RunManager`): arms compare by discriminant; simple payloads compare
/// by value; `TurnDone` compares only its Ok/Err shape.
impl PartialEq for TuiEvent {
    fn eq(&self, other: &Self) -> bool {
        use TuiEvent::*;
        match (self, other) {
            (RequestStart { step: a, model: b }, RequestStart { step: c, model: d }) => {
                a == c && b == d
            }
            (FirstToken, FirstToken)
            | (RequestEnd, RequestEnd)
            | (StepEnd, StepEnd)
            | (TurnDone(Ok(_)), TurnDone(Ok(_)))
            | (TurnDone(Err(_)), TurnDone(Err(_))) => true,
            (Delta(a), Delta(b)) | (Reasoning(a), Reasoning(b)) => a == b,
            (Usage(a), Usage(b)) => a == b,
            (ToolStart { name: a, args: b }, ToolStart { name: c, args: d }) => a == c && b == d,
            (
                ToolEnd {
                    name: a,
                    bytes: b,
                    truncated: t,
                },
                ToolEnd {
                    name: c,
                    bytes: d,
                    truncated: u,
                },
            ) => a == c && b == d && t == u,
            (ApprovalRequested { id: a, .. }, ApprovalRequested { id: b, .. }) => a == b,
            _ => false,
        }
    }
}

/// Extracted, `Clone`-able view of a finished turn ([`ManagedRunResult`]
/// without its non-Clone `TraceCollector`, which the TUI discards in v1).
#[derive(Debug, Clone)]
pub struct TurnSummary {
    pub run_id: RunId,
    pub status: RunStatus,
    pub messages: Vec<Message>,
    pub total_steps: u32,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cost_usd: f64,
    pub execution_tree: ExecutionTree,
    pub model: String,
}

impl From<ManagedRunResult> for TurnSummary {
    fn from(r: ManagedRunResult) -> Self {
        // TraceCollector intentionally dropped (persistence is a later
        // concern; it would have to be written to the store before the
        // engine task returns).
        Self {
            run_id: r.run_id,
            status: r.status,
            messages: r.messages,
            total_steps: r.total_steps,
            total_input_tokens: r.total_input_tokens,
            total_output_tokens: r.total_output_tokens,
            total_cost_usd: r.total_cost_usd,
            execution_tree: r.execution_tree,
            model: r.model,
        }
    }
}

/// Progress-callback bridge: forwards every runtime hook into the UI's
/// unbounded event channel. `try_send` only fails when the channel is
/// closed (UI gone) — the engine keeps running, events are dropped.
pub struct EngineBridge {
    tx: UnboundedSender<TuiEvent>,
}

impl EngineBridge {
    /// Create a bridge publishing into `tx`.
    pub fn new(tx: UnboundedSender<TuiEvent>) -> Self {
        Self { tx }
    }

    fn send(&self, event: TuiEvent) {
        let _ = self.tx.send(event);
    }
}

impl ProgressCallback for EngineBridge {
    fn on_request_start(&mut self, step: u32, model_id: &str) {
        self.send(TuiEvent::RequestStart {
            step,
            model: model_id.to_owned(),
        });
    }

    fn on_first_token(&mut self) {
        self.send(TuiEvent::FirstToken);
    }

    fn on_delta(&mut self, text: &str) {
        self.send(TuiEvent::Delta(text.to_owned()));
    }

    fn on_reasoning(&mut self, text: &str) {
        self.send(TuiEvent::Reasoning(text.to_owned()));
    }

    fn on_usage(&mut self, usage: Usage) {
        self.send(TuiEvent::Usage(usage));
    }

    fn on_request_end(&mut self) {
        self.send(TuiEvent::RequestEnd);
    }

    fn on_tool_start(&mut self, name: &str, args: &str) {
        self.send(TuiEvent::ToolStart {
            name: name.to_owned(),
            args: args.to_owned(),
        });
    }

    fn on_tool_end(&mut self, name: &str, bytes: usize, truncated: bool) {
        self.send(TuiEvent::ToolEnd {
            name: name.to_owned(),
            bytes,
            truncated,
        });
    }

    fn on_step_end(&mut self) {
        self.send(TuiEvent::StepEnd);
    }
}

/// One blocked approval request: the id and the channel the UI answers on.
struct PendingApproval {
    id: u64,
    responder: std::sync::mpsc::Sender<ApprovalChoice>,
}

#[derive(Default)]
struct ApprovalInner {
    pending: Vec<PendingApproval>,
    /// Session allowlist fed by `a` (approve-all): later calls to the same
    /// tool stop prompting, exactly like the REPL.
    allowlist: HashSet<String>,
}

/// The session-scoped approval callback installed on the `RunManager`.
///
/// `decide()` blocks on a std-channel recv; see the module docs for why
/// that is safe (multi-thread runtime, turn spawned off the UI loop) and
/// what happens on UI shutdown (recv error → deny).
pub struct ApprovalBridge {
    tx: UnboundedSender<TuiEvent>,
    next_id: AtomicU64,
    inner: Mutex<ApprovalInner>,
}

impl ApprovalBridge {
    /// Create a bridge publishing approval requests into `tx`.
    pub fn new(tx: UnboundedSender<TuiEvent>) -> Self {
        Self {
            tx,
            next_id: AtomicU64::new(1),
            inner: Mutex::new(ApprovalInner::default()),
        }
    }

    /// UI side: answer the request with the given id. Returns `true` when a
    /// matching pending request was found and answered.
    pub fn respond(&self, id: u64, choice: ApprovalChoice) -> bool {
        let mut inner = Self::lock(&self.inner);
        if let Some(pos) = inner.pending.iter().position(|p| p.id == id) {
            let pending = inner.pending.remove(pos);
            // Sending can only fail if decide() gave up (receiver dropped) —
            // the answer is then simply lost, which is fine.
            return pending.responder.send(choice).is_ok();
        }
        false
    }

    /// UI shutdown: deny every still-blocked request so the engine task can
    /// unwind promptly instead of waiting for recv errors.
    pub fn deny_all(&self) {
        let mut inner = Self::lock(&self.inner);
        for pending in inner.pending.drain(..) {
            let _ = pending.responder.send(ApprovalChoice::Deny);
        }
    }

    /// Number of requests waiting for an answer (UI may show a queue).
    pub fn pending_count(&self) -> usize {
        Self::lock(&self.inner).pending.len()
    }

    /// Poison-tolerant lock helper (a panicking decide must not wedge the
    /// whole session).
    fn lock(inner: &Mutex<ApprovalInner>) -> std::sync::MutexGuard<'_, ApprovalInner> {
        inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn choice_to_decision(
        inner: &mut ApprovalInner,
        req: &ApprovalRequest,
        choice: ApprovalChoice,
    ) -> ApprovalDecision {
        match choice {
            ApprovalChoice::Approve => ApprovalDecision::Approved,
            ApprovalChoice::ApproveAll => {
                inner.allowlist.insert(req.tool_name.clone());
                ApprovalDecision::Approved
            }
            ApprovalChoice::Deny => {
                ApprovalDecision::Denied("用户在 TUI 审批横幅上选择了拒绝".to_owned())
            }
        }
    }
}

impl ApprovalCallback for ApprovalBridge {
    fn decide(&self, req: &ApprovalRequest) -> ApprovalDecision {
        // Session-allowlist fast path (populated by previous `a` answers).
        if Self::lock(&self.inner).allowlist.contains(&req.tool_name) {
            tracing::debug!(
                target: "openslate_approval",
                "session allowlist approved tool '{}'",
                req.tool_name
            );
            return ApprovalDecision::Approved;
        }

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = std::sync::mpsc::channel();
        {
            let mut inner = Self::lock(&self.inner);
            inner.pending.push(PendingApproval { id, responder: tx });
        }

        let sent = self
            .tx
            .send(TuiEvent::ApprovalRequested {
                id,
                request: ApprovalSummary::from(req),
            })
            .is_ok();

        let decision = if !sent {
            // UI is gone: deny rather than block forever.
            ApprovalDecision::Denied("TUI 事件通道已关闭,默认拒绝".to_owned())
        } else {
            match rx.recv() {
                Ok(choice) => {
                    let mut inner = Self::lock(&self.inner);
                    Self::choice_to_decision(&mut inner, req, choice)
                }
                Err(_) => ApprovalDecision::Denied("审批通道已关闭(TUI 退出),默认拒绝".to_owned()),
            }
        };

        // Drop our bookkeeping entry if it is still there (respond() may
        // have already removed it).
        let mut inner = Self::lock(&self.inner);
        inner.pending.retain(|p| p.id != id);
        decision
    }
}

/// Spawn one agent turn on its own task, publishing progress into `events`.
///
/// The manager is MOVED in and MOVED BACK OUT inside
/// [`TuiEvent::TurnDone`] (both arms) — the caller re-fills its
/// `Option<RunManager>` slot from that event. Never `.await` this future
/// on the UI task: a pending approval blocks inside `decide()` and would
/// freeze the event loop.
pub fn spawn_turn(
    manager: RunManager,
    run_id: RunId,
    provider: Box<dyn openslate_core::provider::ModelProvider>,
    history: Vec<Message>,
    cancel: CancellationToken,
    events: UnboundedSender<TuiEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut bridge = EngineBridge::new(events.clone());
        let result = manager
            .execute_with_run_id(
                run_id,
                provider.as_ref(),
                &history,
                cancel,
                Some(&mut bridge),
            )
            .await;
        let payload = match result {
            Ok(r) => Ok((TurnSummary::from(r), manager)),
            Err(e) => Err((e.to_string(), Some(manager))),
        };
        let _ = events.send(TuiEvent::TurnDone(payload));
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use openslate_core::execution::ExecutionTree;
    use openslate_core::trace::TraceCollector;
    use openslate_core::types::{Message, MessageRole, ModelResponse, ToolCall, ToolCallId};
    use std::sync::Arc;

    fn sample_result() -> ManagedRunResult {
        let run_id = RunId("run-1".into());
        let tree = ExecutionTree::new(
            run_id.clone(),
            openslate_core::types::AgentId("root".into()),
        );
        ManagedRunResult {
            run_id,
            status: RunStatus::Completed,
            messages: vec![
                Message {
                    role: MessageRole::User,
                    content: "hi".into(),
                    tool_call_id: None,
                    name: None,
                    tool_calls: None,
                },
                Message {
                    role: MessageRole::Assistant,
                    content: "hello".into(),
                    tool_call_id: None,
                    name: None,
                    tool_calls: None,
                },
            ],
            total_steps: 1,
            total_input_tokens: 10,
            total_output_tokens: 5,
            total_cost_usd: 0.001,
            execution_tree: tree,
            model: "test-model".into(),
            trace: TraceCollector::new(1, 1),
        }
    }

    #[test]
    fn turn_summary_extracts_all_display_fields() {
        let summary = TurnSummary::from(sample_result());
        assert_eq!(summary.run_id.0, "run-1");
        assert_eq!(summary.status, RunStatus::Completed);
        assert_eq!(summary.messages.len(), 2);
        assert_eq!(summary.total_steps, 1);
        assert_eq!(summary.total_input_tokens, 10);
        assert_eq!(summary.total_output_tokens, 5);
        assert!((summary.total_cost_usd - 0.001).abs() < 1e-12);
        assert_eq!(summary.model, "test-model");
        assert_eq!(summary.execution_tree.node_count(), 1);
    }

    #[test]
    fn approval_summary_truncates_and_strips_quotes() {
        let req = ApprovalRequest {
            tool_name: "shell".into(),
            arguments: serde_json::json!({"cmd": "echo hello world"}),
            agent_id: "root".into(),
            risk_level: openslate_core::approval::RiskLevel::High,
        };
        let summary = ApprovalSummary::from(&req);
        assert_eq!(summary.tool_name, "shell");
        assert_eq!(summary.agent_id, "root");
        assert_eq!(summary.risk_level, "high");
        assert!(summary.arguments.contains("cmd"));

        let long = ApprovalSummary::truncate(&"x".repeat(300), 120);
        assert!(
            long.chars().count() <= 120 && long.ends_with('…'),
            "clamped to ≤120 display chars"
        );
        // CJK boundary safety: never splits a multi-byte char.
        let cjk = ApprovalSummary::truncate(&"世".repeat(100), 10);
        assert!(cjk.ends_with('…'));
    }

    #[tokio::test]
    async fn approval_bridge_roundtrip_and_deny_on_drop() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let bridge = Arc::new(ApprovalBridge::new(tx));

        // Answering from another thread (the UI role).
        let responder = bridge.clone();
        let answerer = tokio::task::spawn_blocking(move || {
            let ev = rx.blocking_recv().expect("event arrives");
            match ev {
                TuiEvent::ApprovalRequested { id, request } => {
                    assert_eq!(request.tool_name, "shell");
                    assert!(responder.respond(id, ApprovalChoice::ApproveAll));
                    // Unknown ids report false (no cross-talk).
                    assert!(!responder.respond(id + 100, ApprovalChoice::Deny));
                }
                other => panic!("unexpected event: {other:?}"),
            }
        });

        let req = ApprovalRequest {
            tool_name: "shell".into(),
            arguments: serde_json::json!({"cmd": "ls"}),
            agent_id: "root".into(),
            risk_level: openslate_core::approval::RiskLevel::High,
        };
        let decision = bridge.decide(&req);
        assert_eq!(decision, ApprovalDecision::Approved);
        answerer.await.expect("answerer");

        // ApproveAll populated the session allowlist → instant approve.
        assert_eq!(bridge.decide(&req), ApprovalDecision::Approved);
        assert_eq!(bridge.pending_count(), 0);
    }

    #[test]
    fn approval_bridge_deny_all_unblocks() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let bridge = Arc::new(ApprovalBridge::new(tx));

        let b = bridge.clone();
        let decide = std::thread::spawn(move || {
            let req = ApprovalRequest {
                tool_name: "write_file".into(),
                arguments: serde_json::json!({"path": "/tmp/x"}),
                agent_id: "root".into(),
                risk_level: openslate_core::approval::RiskLevel::High,
            };
            b.decide(&req)
        });

        // Wait for the request to be registered, then deny everything.
        while bridge.pending_count() == 0 {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        bridge.deny_all();
        let decision = decide.join().expect("decide returns");
        assert!(matches!(decision, ApprovalDecision::Denied(_)));
    }

    #[test]
    fn engine_bridge_forwards_all_hooks() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut bridge = EngineBridge::new(tx);

        bridge.on_request_start(1, "m1");
        bridge.on_first_token();
        bridge.on_delta("he");
        bridge.on_delta("llo");
        bridge.on_reasoning("hmm");
        bridge.on_usage(Usage {
            input_tokens: 3,
            output_tokens: 2,
            cached_input_tokens: None,
        });
        bridge.on_request_end();
        bridge.on_tool_start("echo", "{}");
        bridge.on_tool_end("echo", 7, false);
        bridge.on_step_end();

        let kinds: Vec<String> = {
            let mut v = Vec::new();
            while let Ok(ev) = rx.try_recv() {
                v.push(
                    match ev {
                        TuiEvent::RequestStart { .. } => "request_start",
                        TuiEvent::FirstToken => "first_token",
                        TuiEvent::Delta(_) => "delta",
                        TuiEvent::Reasoning(_) => "reasoning",
                        TuiEvent::Usage(_) => "usage",
                        TuiEvent::RequestEnd => "request_end",
                        TuiEvent::ToolStart { .. } => "tool_start",
                        TuiEvent::ToolEnd { .. } => "tool_end",
                        TuiEvent::StepEnd => "step_end",
                        TuiEvent::ApprovalRequested { .. } => "approval",
                        TuiEvent::TurnDone(_) => "turn_done",
                    }
                    .to_owned(),
                );
            }
            v
        };
        assert_eq!(
            kinds,
            vec![
                "request_start",
                "first_token",
                "delta",
                "delta",
                "reasoning",
                "usage",
                "request_end",
                "tool_start",
                "tool_end",
                "step_end",
            ]
        );
    }

    /// Sanity: TurnSummary keeps tool_call structure for the transcript
    /// rebuild (assistant message with tool_calls).
    #[test]
    fn turn_summary_preserves_tool_calls() {
        let mut result = sample_result();
        result.messages[1].tool_calls = Some(vec![ToolCall {
            id: ToolCallId("tc-1".into()),
            name: "echo".into(),
            arguments: serde_json::json!({"text": "hi"}),
        }]);
        let summary = TurnSummary::from(result);
        let assistant = &summary.messages[1];
        assert!(assistant.tool_calls.as_ref().is_some_and(|t| !t.is_empty()));
        // ModelResponse shape compiles against the same types the scripted
        // integration provider uses.
        let _ = ModelResponse {
            content: Some("ok".into()),
            tool_calls: vec![],
            usage: None,
            finish_reason: None,
        };
    }
}
