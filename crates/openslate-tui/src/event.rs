//! UI-event vocabulary — the display side of the openslate client (web-1).
//!
//! The TUI is a pure client: the engine, session state, approvals and
//! config persistence all live in `openslate-server`. [`TuiEvent`] remains
//! the App's single internal event type, but every variant is now display
//! data only:
//!
//! * the **streaming mirror** variants (`RequestStart`/`Delta`/
//!   `Reasoning`/`ToolStart`/…) map 1:1 from `ServerMsg` broadcast events
//!   ([`crate::client::to_tui`]);
//! * the **client-lifecycle** variants ([`TuiEvent::Snapshot`],
//!   [`TuiEvent::ConfigChanged`], [`TuiEvent::ModelChanged`],
//!   [`TuiEvent::SessionReset`], [`TuiEvent::Notice`],
//!   [`TuiEvent::ApprovalResolved`], [`TuiEvent::LinkState`]) carry the
//!   server's authoritative state (and the link's transport state) to the
//!   App;
//! * [`TuiEvent::TurnDone`] carries a manager-free [`TurnSummary`] — the
//!   RunManager round-trip is gone with the local engine.
//!
//! Frozen surface (P2b): the variant set. Payload types for the lifecycle
//! variants come from `openslate-protocol` (single source of truth — no
//! mirror structs to drift).

use openslate_core::types::{RunId, RunStatus, Usage};
use openslate_protocol::{ConfigViewDto, SnapshotDto};

/// A display-oriented summary of an approval request (the full request
/// is engine-side material; the UI only needs these fields for the
/// yellow banner). Built from the protocol's `ApprovalSummaryDto` —
/// the server clips the arguments preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalSummary {
    /// Tool that wants to run.
    pub tool_name: String,
    /// Arguments preview (compact JSON string, clipped server-side).
    pub arguments: String,
    /// Agent requesting execution (source of the banner).
    pub agent_id: String,
    /// Assessed risk: "low" | "medium" | "high".
    pub risk_level: String,
}

/// Everything the server reports about a finished turn. The protocol
/// mirror is `TurnSummaryDto`; the wire copy drops nothing the client
/// needs (execution trees stay server-side).
#[derive(Debug, Clone)]
pub struct TurnSummary {
    pub run_id: RunId,
    pub status: RunStatus,
    pub messages: Vec<openslate_core::types::Message>,
    pub total_steps: u32,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cost_usd: f64,
    pub model: String,
}

/// Everything that reaches the App's dispatch loop as
/// `Action::Engine(_)`. Frozen surface: the variant set (see module
/// docs); payload internals may evolve with the protocol.
pub enum TuiEvent {
    // ── Streaming mirror (ServerMsg broadcast events, 1:1) ─────────────
    /// A model request is being sent (step number, model id).
    RequestStart { step: u32, model: String },
    /// First content token of the current request arrived (TTFT).
    FirstToken,
    /// One content delta from the model.
    Delta(String),
    /// One reasoning/thinking delta from the model.
    Reasoning(String),
    /// Server-side input-token estimate for the streaming request
    /// (`↑~N` live row segment).
    InputEstimate(u32),
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
    /// A tool call needs user approval. `id` correlates the UI answer
    /// with the server-side blocked request (defense against crossed
    /// wires across clients).
    ApprovalRequested { id: u64, request: ApprovalSummary },
    /// The turn finished. Ok → summary (no manager — the server owns
    /// the session). Err → error message.
    TurnDone(Result<TurnSummary, String>),

    // ── Client lifecycle (server authority + link transport) ───────────
    /// Full-session snapshot (hello ack / reconnect / post-`new_session`).
    /// The App rebuilds transcript + mirrors from it wholesale.
    Snapshot(Box<SnapshotDto>),
    /// Config CRUD succeeded somewhere (any client); swap the config
    /// mirror + re-sync the model-management panel.
    ConfigChanged(Box<ConfigViewDto>),
    /// The session's active model alias changed (server broadcast).
    ModelChanged(String),
    /// A `new_session` landed: pending local state clears; a fresh
    /// [`TuiEvent::Snapshot`] follows immediately.
    SessionReset,
    /// A server-directed human-readable notice (failure of a
    /// client-initiated request, cross-client approval races, …).
    Notice(String),
    /// An approval was decided (any client). All clients clear the
    /// banner for `id` and record the decision entry. `choice` uses the
    /// wire vocabulary: `approve` | `deny` | `approve_all`.
    ApprovalResolved { id: u64, choice: String },
    /// Transport state change of the server link (see
    /// [`crate::client::LinkPhase`]).
    LinkState(crate::client::LinkPhase),
}

/// Manual `Debug`: keeps the historical shape (payload-light); the
/// snapshot/config arms print only the discriminant.
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
            Self::InputEstimate(n) => f.debug_tuple("InputEstimate").field(n).finish(),
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
            Self::Snapshot(_) => write!(f, "Snapshot"),
            Self::ConfigChanged(_) => write!(f, "ConfigChanged"),
            Self::ModelChanged(alias) => f.debug_tuple("ModelChanged").field(alias).finish(),
            Self::SessionReset => write!(f, "SessionReset"),
            Self::Notice(text) => f.debug_tuple("Notice").field(text).finish(),
            Self::ApprovalResolved { id, choice } => f
                .debug_struct("ApprovalResolved")
                .field("id", id)
                .field("choice", choice)
                .finish(),
            Self::LinkState(phase) => f.debug_tuple("LinkState").field(phase).finish(),
        }
    }
}

/// Manual `PartialEq`: `TurnDone`'s payload carries core `Message`s (no
/// `PartialEq`), so that arm compares by discriminant only; everything
/// else compares by value.
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
            | (TurnDone(Err(_)), TurnDone(Err(_)))
            | (SessionReset, SessionReset) => true,
            (Delta(a), Delta(b)) | (Reasoning(a), Reasoning(b)) => a == b,
            (InputEstimate(a), InputEstimate(b)) => a == b,
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
            (ModelChanged(a), ModelChanged(b)) => a == b,
            (Notice(a), Notice(b)) => a == b,
            (ApprovalResolved { id: a, choice: c }, ApprovalResolved { id: b, choice: d }) => {
                a == b && c == d
            }
            (LinkState(a), LinkState(b)) => a == b,
            (Snapshot(a), Snapshot(b)) => a == b,
            (ConfigChanged(a), ConfigChanged(b)) => a == b,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_done_partial_eq_compares_by_shape_only() {
        // The payload (core Message) has no PartialEq — the arm must stay
        // discriminant-only. Build two Ok payloads that differ in content.
        let a = TuiEvent::TurnDone(Ok(TurnSummary {
            run_id: RunId("r1".into()),
            status: RunStatus::Completed,
            messages: vec![],
            total_steps: 1,
            total_input_tokens: 2,
            total_output_tokens: 3,
            total_cost_usd: 0.0,
            model: "m".into(),
        }));
        let b = TuiEvent::TurnDone(Ok(TurnSummary {
            run_id: RunId("r2".into()),
            status: RunStatus::Failed,
            messages: vec![],
            total_steps: 9,
            total_input_tokens: 9,
            total_output_tokens: 9,
            total_cost_usd: 9.0,
            model: "z".into(),
        }));
        assert_eq!(a, b, "Ok payloads compare equal by shape");
        assert_ne!(
            a,
            TuiEvent::TurnDone(Err("boom".into())),
            "Ok and Err arms differ"
        );
    }

    #[test]
    fn input_estimate_and_notice_compare_by_value() {
        assert_eq!(TuiEvent::InputEstimate(7), TuiEvent::InputEstimate(7));
        assert_ne!(TuiEvent::InputEstimate(7), TuiEvent::InputEstimate(8));
        assert_eq!(TuiEvent::Notice("x".into()), TuiEvent::Notice("x".into()));
        assert_ne!(
            TuiEvent::ApprovalResolved {
                id: 1,
                choice: "approve".into()
            },
            TuiEvent::ApprovalResolved {
                id: 1,
                choice: "deny".into()
            }
        );
    }

    #[test]
    fn debug_covers_every_variant_without_panicking() {
        // Snapshot/ConfigChanged arms must not dereference the payload.
        let dto = SnapshotDto {
            proto: 1,
            session_id: "s".into(),
            session_label: "l".into(),
            transcript: vec![],
            running: false,
            depth_cur: 0,
            agents_running: 0,
            tool_calls_cur: 0,
            model_alias: "main".into(),
            pending_approval: None,
            config: snapshot_test_config(),
            session_stats: Default::default(),
        };
        let events = [
            TuiEvent::RequestStart {
                step: 1,
                model: "m".into(),
            },
            TuiEvent::FirstToken,
            TuiEvent::Delta("d".into()),
            TuiEvent::Reasoning("r".into()),
            TuiEvent::InputEstimate(5),
            TuiEvent::Usage(Usage {
                input_tokens: 1,
                output_tokens: 2,
                cached_input_tokens: None,
            }),
            TuiEvent::RequestEnd,
            TuiEvent::StepEnd,
            TuiEvent::ToolStart {
                name: "t".into(),
                args: "{}".into(),
            },
            TuiEvent::ToolEnd {
                name: "t".into(),
                bytes: 1,
                truncated: false,
            },
            TuiEvent::ApprovalRequested {
                id: 1,
                request: ApprovalSummary {
                    tool_name: "t".into(),
                    arguments: "{}".into(),
                    agent_id: "root".into(),
                    risk_level: "high".into(),
                },
            },
            TuiEvent::TurnDone(Err("e".into())),
            TuiEvent::Snapshot(Box::new(dto)),
            TuiEvent::ConfigChanged(Box::new(snapshot_test_config())),
            TuiEvent::ModelChanged("fast".into()),
            TuiEvent::SessionReset,
            TuiEvent::Notice("n".into()),
            TuiEvent::ApprovalResolved {
                id: 1,
                choice: "approve".into(),
            },
            TuiEvent::LinkState(crate::client::LinkPhase::Connected),
        ];
        for ev in &events {
            let s = format!("{ev:?}");
            assert!(!s.is_empty());
        }
    }

    /// Minimal valid `ConfigViewDto` for the Debug/PartialEq tests.
    fn snapshot_test_config() -> openslate_protocol::ConfigViewDto {
        openslate_protocol::ConfigViewDto {
            providers: Default::default(),
            models: Default::default(),
            levels: Default::default(),
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
            active_config: "/x/openslate.toml".into(),
            global_config: None,
            local_config: None,
        }
    }
}
