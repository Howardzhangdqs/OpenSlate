//! Bridge integration — a full scripted turn through the REAL engine.
//!
//! No network: a hand-written `ScriptedProvider` (core
//! integration_run.rs pattern) drives
//! [`spawn_turn`] end-to-end while an [`App`] (built from a real
//! `build_app_context` on a temp project) consumes the resulting events,
//! proving:
//!
//! * the `TuiEvent` sequence for a tool-calling turn
//!   (RequestStart → deltas → RequestEnd → ToolStart/ToolEnd → … →
//!   TurnDone(Ok));
//! * the transcript rebuild from `result.messages` (the fixed merge
//!   point) and the run-state fall back to `Idle`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use openslate_app::wiring::build_app_context;
use openslate_core::agent_tree::AgentTree;
use openslate_core::approval::{ApprovalCallback, ApprovalDecision, ApprovalRequest, RiskLevel};
use openslate_core::config::parse_openslate_toml;
use openslate_core::error::ProviderError;
use openslate_core::provider::{GenerateRequest, ModelProvider};
use openslate_core::run_manager::RunManager;
use openslate_core::skills::SkillsCatalog;
use openslate_core::tool::{Tool, ToolRegistry};
use openslate_core::types::{
    AgentConfig, AgentId, Message, MessageRole, ModelResponse, ModelStreamEvent, RunId, ToolCall,
    ToolCallId, ToolOutput, ToolOutputStatus, Usage,
};

use openslate_tui::action::{Action, ApprovalChoice};
use openslate_tui::app::{coalesce_deltas, App, DispatchOutcome};
use openslate_tui::components::transcript::{ToolEntryStatus, TranscriptEntry};
use openslate_tui::components::RunState;
use openslate_tui::event::{self, ApprovalBridge, ApprovalSummary, TuiEvent};

// ─── Scripted provider (streams deltas, then requests a tool) ─────────────

struct ScriptedProvider {
    responses: Vec<ModelResponse>,
    call_count: AtomicUsize,
}

impl ScriptedProvider {
    fn new(responses: Vec<ModelResponse>) -> Self {
        Self {
            responses,
            call_count: AtomicUsize::new(0),
        }
    }

    fn scripted_turn() -> Vec<ModelResponse> {
        vec![
            // Step 1: stream some text, then ask for the echo tool.
            ModelResponse {
                content: Some("Let me look at the code first.".into()),
                tool_calls: vec![ToolCall {
                    id: ToolCallId("tc-1".into()),
                    name: "echo".into(),
                    arguments: serde_json::json!({"text": "hello world"}),
                }],
                usage: Some(Usage {
                    input_tokens: 50,
                    output_tokens: 10,
                    cached_input_tokens: None,
                }),
                finish_reason: Some("tool_calls".into()),
            },
            // Step 2: final answer.
            ModelResponse {
                content: Some("All done!".into()),
                tool_calls: vec![],
                usage: Some(Usage {
                    input_tokens: 80,
                    output_tokens: 5,
                    cached_input_tokens: None,
                }),
                finish_reason: Some("stop".into()),
            },
        ]
    }
}

#[async_trait]
impl ModelProvider for ScriptedProvider {
    async fn generate(&self, _request: GenerateRequest) -> Result<ModelResponse, ProviderError> {
        let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
        self.responses
            .get(idx)
            .cloned()
            .ok_or(ProviderError::ServerError(500))
    }

    /// Stream the scripted response as explicit Delta events so the
    /// bridge's on_first_token/on_delta paths actually fire.
    async fn generate_stream(
        &self,
        request: GenerateRequest,
    ) -> tokio::sync::mpsc::Receiver<Result<ModelStreamEvent, ProviderError>> {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let result = self.generate(request).await;
        tokio::spawn(async move {
            match result {
                Ok(response) => {
                    if let Some(usage) = response.usage {
                        let _ = tx.send(Ok(ModelStreamEvent::Usage(usage))).await;
                    }
                    if let Some(content) = &response.content {
                        // Two deltas to exercise coalescing-adjacent paths.
                        let mid = content.len() / 2;
                        let (a, b) = content.split_at(mid.max(1));
                        let _ = tx.send(Ok(ModelStreamEvent::Delta(a.to_owned()))).await;
                        let _ = tx.send(Ok(ModelStreamEvent::Delta(b.to_owned()))).await;
                    }
                    let _ = tx.send(Ok(ModelStreamEvent::Done(response))).await;
                }
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                }
            }
        });
        rx
    }

    fn provider_name(&self) -> &str {
        "scripted-mock"
    }
}

// ─── Fixture helpers ───────────────────────────────────────────────────────

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "Echo back the input"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"text": {"type": "string"}}
        })
    }

    async fn execute(
        &self,
        args: &serde_json::Value,
    ) -> Result<ToolOutput, openslate_core::error::ToolError> {
        let text = args["text"].as_str().unwrap_or("");
        Ok(ToolOutput {
            content: text.to_owned(),
            bytes: text.len(),
            duration_ms: 1,
            status: ToolOutputStatus::Success,
        })
    }
}

fn test_config() -> openslate_core::config::OpenSlateConfig {
    let toml = r#"
[providers.mock]
base_url = "http://localhost"
api_key_env = "TUI_TEST_KEY"

[models.main]
provider = "mock"
model = "mock-model"

[models.fast]
provider = "mock"
model = "mock-model"

[limits]
max_steps = 10
max_depth = 4
max_context_bytes = 100_000
max_output_bytes = 10_000
"#;
    parse_openslate_toml(toml).expect("test config should parse")
}

fn test_manager() -> RunManager {
    let agents = vec![AgentConfig {
        id: AgentId("root".into()),
        name: "Root Agent".into(),
        model: "main".into(),
        children: vec![],
        tools: vec!["echo".into()],
        default_prompt: "You are a test agent.".into(),
    }];
    let tree = AgentTree::from_configs(&agents).expect("agent tree should build");
    let mut registry = ToolRegistry::new();
    registry.register(EchoTool);
    RunManager::new(test_config(), tree, registry, SkillsCatalog::default())
}

/// Real temp project for `build_app_context` (config + agents + store).
/// The `[database]` path is ABSOLUTE inside the tempdir — a relative path
/// would be resolved against the user's global data dir, leaking test
/// runs into the real store.
fn temp_project() -> tempfile::TempDir {
    temp_project_with_limits(100_000)
}

/// [`temp_project`] with a configurable `max_context_bytes` (drive the
/// auto-compact threshold in tests).
fn temp_project_with_limits(max_context_bytes: u32) -> tempfile::TempDir {
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
max_context_bytes = {max_context_bytes}
max_output_bytes = 10_000
"#
        ),
    )
    .expect("write toml");
    let agents_dir = openslate_dir.join("agents");
    std::fs::create_dir(&agents_dir).expect("create agents dir");
    // `read_file` is a registered builtin tool (validation knows it);
    // the scripted turn's `echo` tool is only exercised through the
    // manually-built manager in the other tests.
    std::fs::write(
        agents_dir.join("root.md"),
        "---\nid: root\nname: Root Agent\nmodel: main\ntools:\n  - read_file\n---\nYou are the root agent.\n",
    )
    .expect("write root.md");
    tmp
}

fn user_message(text: &str) -> Message {
    Message {
        role: MessageRole::User,
        content: text.to_owned(),
        tool_call_id: None,
        name: None,
        tool_calls: None,
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

/// Full turn through the real engine + bridge: assert the event sequence
/// and that `TurnDone` carries a usable `TurnSummary` + the manager back.
#[tokio::test]
async fn full_turn_streams_expected_event_sequence() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<TuiEvent>();
    let manager = test_manager();
    let handle = event::spawn_turn(
        manager,
        RunId("integration-run".into()),
        Box::new(ScriptedProvider::new(ScriptedProvider::scripted_turn())),
        vec![user_message("echo hello world")],
        openslate_core::runtime::CancellationToken::new(),
        tx,
    );
    handle.await.expect("engine task joins cleanly");

    let mut kinds: Vec<&'static str> = Vec::new();
    let mut turn_summary = None;
    while let Some(event) = rx.recv().await {
        match event {
            TuiEvent::RequestStart { .. } => kinds.push("request_start"),
            TuiEvent::FirstToken => kinds.push("first_token"),
            TuiEvent::Delta(_) => kinds.push("delta"),
            TuiEvent::Reasoning(_) => kinds.push("reasoning"),
            TuiEvent::Usage(_) => kinds.push("usage"),
            TuiEvent::RequestEnd => kinds.push("request_end"),
            TuiEvent::StepEnd => kinds.push("step_end"),
            TuiEvent::ToolStart { .. } => kinds.push("tool_start"),
            TuiEvent::ToolEnd { .. } => kinds.push("tool_end"),
            TuiEvent::ApprovalRequested { .. } => kinds.push("approval"),
            TuiEvent::TurnDone(payload) => {
                kinds.push("turn_done");
                // `RunManager` is not Debug, so no `.expect` — match.
                let (summary, _manager) = match payload {
                    Ok(pair) => pair,
                    Err(_) => panic!("scripted turn should succeed"),
                };
                turn_summary = Some(summary);
            }
        }
    }

    // Step 1: request → stream → usage → request_end → tool → step_end
    // Step 2 (final content step): request → stream → usage →
    // request_end — the runtime emits NO step_end for the final step
    // (core runtime.rs:903) — then TurnDone.
    let expected = vec![
        "request_start",
        "usage",
        "first_token",
        "delta",
        "delta",
        "request_end",
        "tool_start",
        "tool_end",
        "step_end",
        "request_start",
        "usage",
        "first_token",
        "delta",
        "delta",
        "request_end",
        "turn_done",
    ];
    assert_eq!(kinds, expected, "full event sequence must match");

    let summary = turn_summary.expect("TurnDone carried a summary");
    assert_eq!(summary.run_id.0, "integration-run");
    assert_eq!(summary.status, openslate_core::types::RunStatus::Completed);
    assert_eq!(summary.total_steps, 2);
    // user + assistant(tool_calls) + tool result + final assistant
    assert_eq!(summary.messages.len(), 4);
    assert_eq!(summary.total_input_tokens, 130);
    assert_eq!(summary.total_output_tokens, 15);
}

/// The App side: engine events drive the transcript streaming area, the
/// tool entry lifecycle, and the TurnDone merge + status fall-back.
#[tokio::test]
async fn app_consumes_turn_events_and_merges_transcript() {
    let tmp = temp_project();
    let config_path = tmp.path().join(".openslate/openslate.toml");
    let ctx = build_app_context(config_path.to_str())
        .await
        .expect("build app context");
    let mut app = App::new(ctx);

    // A spare manager to hand back inside TurnDone(Ok) — the App stores
    // it for the next turn (ownership round-trip).
    let spare_manager = test_manager();

    let summary = openslate_tui::event::TurnSummary {
        run_id: RunId("app-run".into()),
        status: openslate_core::types::RunStatus::Completed,
        messages: vec![
            user_message("go"),
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                tool_call_id: None,
                name: None,
                tool_calls: Some(vec![ToolCall {
                    id: ToolCallId("tc-1".into()),
                    name: "echo".into(),
                    arguments: serde_json::json!({"text": "hi"}),
                }]),
            },
            Message {
                role: MessageRole::Tool,
                content: "hi".into(),
                tool_call_id: Some(ToolCallId("tc-1".into())),
                name: Some("echo".into()),
                tool_calls: None,
            },
            Message {
                role: MessageRole::Assistant,
                content: "All done!".into(),
                tool_call_id: None,
                name: None,
                tool_calls: None,
            },
        ],
        total_steps: 2,
        total_input_tokens: 100,
        total_output_tokens: 20,
        total_cost_usd: 0.002,
        execution_tree: openslate_core::execution::ExecutionTree::new(
            RunId("app-run".into()),
            AgentId("root".into()),
        ),
        model: "mock-model".into(),
    };

    // ── streaming phase ──
    app.dispatch(Action::Engine(TuiEvent::RequestStart {
        step: 1,
        model: "mock-model".into(),
    }))
    .await;
    assert_eq!(*app.run_state(), RunState::Thinking);

    app.dispatch(Action::Engine(TuiEvent::Delta("Hel".into())))
        .await;
    app.dispatch(Action::Engine(TuiEvent::Delta("lo".into())))
        .await;
    assert_eq!(
        app.transcript_entries().len(),
        0,
        "deltas live in the streaming area"
    );

    // ── tool phase ──
    app.dispatch(Action::Engine(TuiEvent::ToolStart {
        name: "echo".into(),
        args: r#"{"text":"hi"}"#.into(),
    }))
    .await;
    assert!(!app.transcript_entries().is_empty());
    assert_eq!(
        *app.run_state(),
        RunState::ToolRunning {
            name: "echo".into()
        }
    );
    app.dispatch(Action::Engine(TuiEvent::ToolEnd {
        name: "echo".into(),
        bytes: 2,
        truncated: false,
    }))
    .await;
    assert!(matches!(
        app.transcript_entries().last(),
        Some(TranscriptEntry::ToolCall {
            status: ToolEntryStatus::Done { bytes, .. },
            ..
        }) if *bytes == 2
    ));

    // ── TurnDone: merge + status fall-back ──
    app.dispatch(Action::Engine(TuiEvent::TurnDone(Ok((
        summary,
        spare_manager,
    )))))
    .await;
    assert_eq!(*app.run_state(), RunState::Idle);
    assert!(!app.is_running());
    // The merge path keeps the event-ordered live view and only adds:
    // the live-flushed "Hello" (streamed before the tool call) stays,
    // the tool result folds into the row, and the messages' "All
    // done!" — never streamed here — appends. (No User entry exists:
    // this test never dispatched StartTurn, and the merge has no
    // user-message guard — the live path owns that push.)
    assert_eq!(app.transcript_entries().len(), 3);
    assert!(matches!(
        app.transcript_entries().first(),
        Some(TranscriptEntry::Assistant(text)) if text == "Hello"
    ));
    assert!(matches!(
        &app.transcript_entries()[1],
        TranscriptEntry::ToolCall { status: ToolEntryStatus::Done { bytes, .. }, .. }
            if *bytes == 2
    ));
    assert!(matches!(
        app.transcript_entries().last(),
        Some(TranscriptEntry::Assistant(text)) if text == "All done!"
    ));
    // History = full message list (the engine-side authority).
    assert_eq!(app.history().len(), 4);
}

/// StartTurn through the injected scripted provider: a REAL end-to-end
/// turn driven by dispatch only (store persistence included), polled to
/// completion through the engine-event drain seam.
#[tokio::test]
async fn start_turn_via_dispatch_runs_real_engine() {
    let tmp = temp_project();
    let config_path = tmp.path().join(".openslate/openslate.toml");
    let ctx = build_app_context(config_path.to_str())
        .await
        .expect("build app context");
    let factory = move |_config: &openslate_core::config::OpenSlateConfig, _alias: &str| {
        let responses = ScriptedProvider::scripted_turn();
        Ok::<Box<dyn ModelProvider>, anyhow::Error>(Box::new(ScriptedProvider::new(responses)))
    };
    let mut app = App::with_provider_factory(ctx, std::sync::Arc::new(factory));

    assert_eq!(
        app.dispatch(Action::StartTurn("echo hello world".into()))
            .await,
        DispatchOutcome::Continue
    );
    assert!(app.is_running(), "engine task spawned");
    assert!(app.session_run_id().is_some(), "session run opened");

    // Pump engine events (the run loop's select! role) until TurnDone.
    let mut guard = 0;
    while app.is_running() && guard < 500 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        app.drain_engine_events().await;
        guard += 1;
    }
    app.drain_engine_events().await;
    assert!(!app.is_running(), "engine task must finish");

    // Fixed merge point: the transcript was rebuilt from the engine's
    // own result.messages (user + assistant(tool_calls) + final).
    assert_eq!(*app.run_state(), RunState::Idle);
    assert_eq!(app.history().len(), 4);
    assert!(app.transcript_entries().iter().any(|e| matches!(
        e,
        TranscriptEntry::ToolCall { name, .. } if name == "echo"
    )));
}

/// Per-request telemetry through the App's dispatch: RequestStart
/// starts the clock (and resets the TTFT mark), FirstToken records
/// the TTFT numerator (and freezes the live stats row's ttft,
/// fix-19), Usage stores the counts, RequestEnd flushes the streamed
/// blocks — the reasoning estimate at the reasoning block's tail —
/// and HOLDS the exact usage line (fix-19: tools of the step execute
/// after RequestEnd, so the line waits for a boundary; the next
/// RequestStart proves the step had no tools and lands it at the
/// answer's tail). The `TurnDone(Ok)` merge then KEEPS those lines
/// (event-ordered entries) — only the recovery rebuilds drop them.
#[tokio::test]
async fn request_usage_meta_lines_flow_through_dispatch() {
    let tmp = temp_project();
    let config_path = tmp.path().join(".openslate/openslate.toml");
    let ctx = build_app_context(config_path.to_str())
        .await
        .expect("build app context");
    let mut app = App::new(ctx);
    let spare_manager = test_manager();

    app.dispatch(Action::Engine(TuiEvent::RequestStart {
        step: 1,
        model: "mock-model".into(),
    }))
    .await;
    // FirstToken before the content: the request's ttft is measurable.
    app.dispatch(Action::Engine(TuiEvent::FirstToken)).await;
    app.dispatch(Action::Engine(TuiEvent::Reasoning(
        "thinking about it".into(),
    )))
    .await;
    app.dispatch(Action::Engine(TuiEvent::Delta("the answer".into())))
        .await;
    // Usage may arrive before or after deltas (provider-dependent);
    // RequestEnd assembles the meta regardless.
    app.dispatch(Action::Engine(TuiEvent::Usage(Usage {
        input_tokens: 50,
        output_tokens: 10,
        cached_input_tokens: Some(3),
    })))
    .await;
    app.dispatch(Action::Engine(TuiEvent::RequestEnd)).await;

    let entries = app.transcript_entries();
    let kinds: Vec<String> = entries
        .iter()
        .map(|e| match e {
            TranscriptEntry::Reasoning(_) => "reasoning".into(),
            TranscriptEntry::Meta(t) => {
                if t.starts_with('~') {
                    "meta_est".into()
                } else {
                    format!("meta_usage:{t}")
                }
            }
            TranscriptEntry::Assistant(_) => "assistant".into(),
            TranscriptEntry::StepBreak => "sep".into(),
            other => format!("other:{other:?}"),
        })
        .collect();
    assert!(
        kinds.iter().any(|k| k == "reasoning"),
        "reasoning flushed: {kinds:?}"
    );
    assert!(
        kinds.iter().any(|k| k == "meta_est"),
        "reasoning estimate line: {kinds:?}"
    );
    assert!(
        kinds.iter().any(|k| k == "assistant"),
        "answer flushed: {kinds:?}"
    );
    // fix-19: the exact usage line is HELD at RequestEnd — no tools
    // have started yet, so its position is not yet known.
    assert!(
        !kinds.iter().any(|k| k.starts_with("meta_usage:")),
        "usage line held, not committed: {kinds:?}"
    );

    // The NEXT RequestStart proves the step had no tools — the hold
    // lands at the answer's tail.
    app.dispatch(Action::Engine(TuiEvent::RequestStart {
        step: 2,
        model: "mock-model".into(),
    }))
    .await;
    let entries = app.transcript_entries();
    let kinds: Vec<String> = entries
        .iter()
        .map(|e| match e {
            TranscriptEntry::Reasoning(_) => "reasoning".into(),
            TranscriptEntry::Meta(t) => {
                if t.starts_with('~') {
                    "meta_est".into()
                } else {
                    format!("meta_usage:{t}")
                }
            }
            TranscriptEntry::Assistant(_) => "assistant".into(),
            TranscriptEntry::StepBreak => "sep".into(),
            other => format!("other:{other:?}"),
        })
        .collect();
    let usage_line = kinds
        .iter()
        .find(|k| k.starts_with("meta_usage:"))
        .expect("exact usage meta line flushed at the next RequestStart");
    assert!(
        usage_line.contains("↑50 ↓10 ⎓3"),
        "cached segment present: {usage_line}"
    );
    assert!(
        usage_line.contains("ttft "),
        "ttft segment present (FirstToken was dispatched): {usage_line}"
    );
    // Event order is visual order: estimate above answer above usage.
    let idx = |name: &str| kinds.iter().position(|k| k == name).unwrap();
    assert!(idx("reasoning") < idx("meta_est"));
    assert!(idx("meta_est") < idx("assistant"));
    assert!(
        kinds
            .iter()
            .position(|k| k.starts_with("meta_usage:"))
            .unwrap()
            > idx("assistant")
    );

    // TurnDone(Ok) MERGE: the live meta lines survive as part of the
    // event-ordered record; the turn marker's aggregates join them.
    let summary = openslate_tui::event::TurnSummary {
        run_id: RunId("app-run".into()),
        status: openslate_core::types::RunStatus::Completed,
        messages: vec![
            user_message("go"),
            Message {
                role: MessageRole::Assistant,
                content: "the answer".into(),
                tool_call_id: None,
                name: None,
                tool_calls: None,
            },
        ],
        total_steps: 1,
        total_input_tokens: 50,
        total_output_tokens: 10,
        total_cost_usd: 0.001,
        execution_tree: openslate_core::execution::ExecutionTree::new(
            RunId("app-run".into()),
            AgentId("root".into()),
        ),
        model: "mock-model".into(),
    };
    app.dispatch(Action::Engine(TuiEvent::TurnDone(Ok((
        summary,
        spare_manager,
    )))))
    .await;
    assert!(
        app.transcript_entries()
            .iter()
            .any(|e| matches!(e, TranscriptEntry::Meta(_))),
        "meta lines survive the merge path"
    );
    // History stays the engine-side message authority.
    assert_eq!(app.history().len(), 2);
}

/// fix-19 ① (App wiring): `FirstToken` freezes the precise ttft into
/// the transcript; while the ANSWER streams, the live stats row
/// (`ttft S.Ss · ~Rtok/s`) renders at the answer head (muted, no
/// rate segment under the floor — a µs-old buffer shows ttft only).
#[tokio::test]
async fn first_token_feeds_the_live_stats_row_during_answer_streaming() {
    let tmp = temp_project();
    let config_path = tmp.path().join(".openslate/openslate.toml");
    let ctx = build_app_context(config_path.to_str())
        .await
        .expect("build app context");
    let mut app = App::new(ctx);

    app.dispatch(Action::Engine(TuiEvent::RequestStart {
        step: 1,
        model: "mock-model".into(),
    }))
    .await;
    // Reasoning streams first: no live row yet (answer area only).
    app.dispatch(Action::Engine(TuiEvent::FirstToken)).await;
    app.dispatch(Action::Engine(TuiEvent::Reasoning("thinking".into())))
        .await;
    app.dispatch(Action::Engine(TuiEvent::Delta("partial".into())))
        .await;

    let theme = openslate_tui::theme::Theme::new();
    let render_ctx = openslate_tui::components::AppCtx {
        theme,
        focus: openslate_tui::components::Focus::Transcript,
        run: openslate_tui::components::RunInfo {
            state: RunState::Thinking,
            spinner_frame: 0,
            model_label: "mock-model".into(),
            tokens_in: 0,
            tokens_out: 0,
            cost_usd: 0.0,
            elapsed: None,
            tool_calls_cur: 0,
            depth_cur: 0,
        },
        config: openslate_tui::components::ConfigSummary {
            model_alias: "main".into(),
            model_id: "mock-model".into(),
            provider_name: "mock".into(),
            max_depth: 4,
            max_tool_calls: 20,
            run_id: None,
        },
        size: (30, 5),
        notice: None,
    };
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(30, 5)).expect("test terminal");
    terminal
        .draw(|f| {
            use openslate_tui::components::Component;
            app.transcript().render(f, f.area(), &render_ctx)
        })
        .expect("draw frame");
    let screen: Vec<String> = (0..terminal.backend().buffer().area.height)
        .map(|y| {
            (0..terminal.backend().buffer().area.width)
                .map(|x| {
                    terminal
                        .backend()
                        .buffer()
                        .cell((x, y))
                        .map(|c| c.symbol().to_string())
                        .unwrap_or_default()
                })
                .collect()
        })
        .collect();
    let stats_row = screen
        .iter()
        .find(|r| r.contains("ttft "))
        .expect("live stats row at the answer head");
    assert!(
        stats_row.contains("ttft 0."),
        "precise ttft frozen at FirstToken: {stats_row}"
    );
    assert!(
        !stats_row.contains("tok/s"),
        "no rate segment under the floor: {stats_row}"
    );
    // The stats row is dim (muted) like the meta rows.
    let y = screen.iter().position(|r| r.contains("ttft ")).unwrap() as u16;
    let x = stats_row.find('t').unwrap() as u16;
    assert_eq!(
        terminal.backend().buffer().cell((x, y)).unwrap().style().fg,
        Some(ratatui::style::Color::DarkGray)
    );
}

/// fix-19 ② (App wiring): a TOOL step's held usage line lands below
/// the tool row — `RequestEnd` holds it, `ToolStart` consumes it.
#[tokio::test]
async fn tool_step_usage_meta_lands_below_the_tool_row() {
    let tmp = temp_project();
    let config_path = tmp.path().join(".openslate/openslate.toml");
    let ctx = build_app_context(config_path.to_str())
        .await
        .expect("build app context");
    let mut app = App::new(ctx);

    app.dispatch(Action::Engine(TuiEvent::RequestStart {
        step: 1,
        model: "mock-model".into(),
    }))
    .await;
    app.dispatch(Action::Engine(TuiEvent::FirstToken)).await;
    app.dispatch(Action::Engine(TuiEvent::Delta("checking".into())))
        .await;
    app.dispatch(Action::Engine(TuiEvent::Usage(Usage {
        input_tokens: 50,
        output_tokens: 10,
        cached_input_tokens: None,
    })))
    .await;
    app.dispatch(Action::Engine(TuiEvent::RequestEnd)).await;
    // Held: no usage meta among the entries yet.
    assert!(app
        .transcript_entries()
        .iter()
        .all(|e| { !matches!(e, TranscriptEntry::Meta(m) if m.starts_with("↑50")) }));
    app.dispatch(Action::Engine(TuiEvent::ToolStart {
        name: "echo".into(),
        args: "{}".into(),
    }))
    .await;

    let entries = app.transcript_entries();
    let tool = entries
        .iter()
        .position(|e| matches!(e, TranscriptEntry::ToolCall { .. }))
        .expect("tool entry");
    let meta = entries
        .iter()
        .position(|e| matches!(e, TranscriptEntry::Meta(m) if m.starts_with("↑50")))
        .expect("held usage meta landed below the tool row");
    assert!(meta > tool, "meta AFTER the tool row: {tool} vs {meta}");
}

// ─── ora-2 dispatch-level tests (frozen-revision window) ───────────────────

/// A provider that always fails — drives the TurnDone(Err) path.
struct FailingProvider;

#[async_trait]
impl ModelProvider for FailingProvider {
    async fn generate(&self, _request: GenerateRequest) -> Result<ModelResponse, ProviderError> {
        Err(ProviderError::ServerError(500))
    }

    fn provider_name(&self) -> &str {
        "failing"
    }
}

/// Err path (review item 4a): the engine turn fails → the error surfaces
/// as `RunState::Error`, the in-memory history is RELOADED from the store
/// (exactly the persisted user message), and input submitted while the
/// turn ran is RESTORED to the editor instead of auto-retried.
#[tokio::test]
async fn turn_error_restores_pending_input_and_reloads_store() {
    let tmp = temp_project();
    let config_path = tmp.path().join(".openslate/openslate.toml");
    let ctx = build_app_context(config_path.to_str())
        .await
        .expect("build app context");
    let factory = |_config: &openslate_core::config::OpenSlateConfig, _alias: &str| {
        Ok::<Box<dyn ModelProvider>, anyhow::Error>(Box::new(FailingProvider))
    };
    let mut app = App::with_provider_factory(ctx, std::sync::Arc::new(factory));

    app.dispatch(Action::StartTurn("hello".into())).await;
    assert!(app.is_running(), "engine task spawned");
    assert!(app.session_run_id().is_some(), "session run opened");

    // Submit while the turn is running → queued as pending input (never
    // reaches the editor yet).
    app.dispatch(Action::StartTurn("queued follow-up".into()))
        .await;
    assert_eq!(
        app.input_text(),
        "",
        "queued text is not typed into the editor"
    );

    // Pump engine events until TurnDone(Err) is dispatched.
    let mut guard = 0;
    while app.is_running() && guard < 500 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        app.drain_engine_events().await;
        guard += 1;
    }
    app.drain_engine_events().await;
    assert!(!app.is_running(), "engine task must finish");

    // The engine error surfaces (session stays alive — not a Quit).
    assert!(matches!(app.run_state(), RunState::Error(_)));

    // Pending input restored to the editor (no auto-retry storms).
    assert_eq!(app.input_text(), "queued follow-up");

    // History reloaded from the store: exactly the persisted user
    // message (the engine failed before any assistant output).
    assert_eq!(app.history().len(), 1);
    assert_eq!(app.history()[0].content, "hello");
}

/// Approval modal preemption (review item 4b): while ApprovalActive, y/n/a
/// answer the queue and EVERYTHING else — including input-box characters
/// and submit — is swallowed; normal typing resumes once the queue drains.
#[tokio::test]
async fn approval_modal_preempts_input_keys() {
    let tmp = temp_project();
    let config_path = tmp.path().join(".openslate/openslate.toml");
    let ctx = build_app_context(config_path.to_str())
        .await
        .expect("build app context");
    // Production factory — never invoked (no turn runs in this test).
    let mut app = App::new(ctx);

    let summary = ApprovalSummary {
        tool_name: "shell".into(),
        arguments: r#"{"cmd":"ls"}"#.into(),
        agent_id: "root".into(),
        risk_level: "high".into(),
    };
    for id in 1..=3u64 {
        app.dispatch(Action::Engine(TuiEvent::ApprovalRequested {
            id,
            request: summary.clone(),
        }))
        .await;
    }
    assert_eq!(*app.run_state(), RunState::ApprovalPending);

    // Ordinary characters and submit are swallowed while approval is up.
    app.dispatch(Action::InputChar('h')).await;
    app.dispatch(Action::InputChar('i')).await;
    app.dispatch(Action::SubmitInput).await;
    assert_eq!(app.input_text(), "");

    // `y` answers the front request; the queue keeps preemption active.
    app.dispatch(Action::InputChar('y')).await;
    app.dispatch(Action::InputChar('q')).await; // still pending → swallowed
    assert_eq!(app.input_text(), "");
    // `n` and `a` drain the remaining two.
    app.dispatch(Action::InputChar('n')).await;
    app.dispatch(Action::InputChar('a')).await;

    // Queue empty → preemption over: characters type into the editor
    // again (also proves the layer derives from the queue, not a stale
    // modal flag).
    app.dispatch(Action::InputChar('x')).await;
    assert_eq!(app.input_text(), "x");
}

/// Delta coalescing (review item 4c): adjacent `Engine(Delta)` actions in
/// one drained batch merge; anything between them breaks the run. Exposed
/// via the pub `coalesce_deltas` (dispatch itself does not coalesce —
/// coalescing happens between the drain and the dispatch loop).
#[test]
fn coalesce_deltas_merges_only_adjacent_deltas() {
    let mut batch = vec![
        Action::Engine(TuiEvent::Delta("Hel".into())),
        Action::Engine(TuiEvent::Delta("lo".into())),
        Action::Redraw, // separator: no merge across it
        Action::Engine(TuiEvent::Delta("a".into())),
        Action::Tick, // separator
        Action::Engine(TuiEvent::Delta("b".into())),
        Action::Engine(TuiEvent::Delta("c".into())),
    ];
    coalesce_deltas(&mut batch);

    let deltas: Vec<String> = batch
        .iter()
        .filter_map(|action| match action {
            Action::Engine(TuiEvent::Delta(text)) => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, vec!["Hello".to_owned(), "a".into(), "bc".into()]);
    // Non-delta actions survive in order:
    // [Delta("Hello"), Redraw, Delta("a"), Tick, Delta("bc")].
    assert_eq!(batch.len(), 5);
    assert!(matches!(batch[1], Action::Redraw));
    assert!(matches!(batch[3], Action::Tick));
}

/// Deferred auto-compact (review item 7): when `needs_compact` fires,
/// `StartTurn` returns WITHOUT spawning the engine (the ` compacting…`
/// badge paints on the draw between dispatch rounds) and the blocking
/// summary call + turn spawn run on the NEXT dispatched action — never a
/// silent freeze on a slow provider.
#[tokio::test]
async fn auto_compact_defers_to_next_dispatch_with_badge_visible() {
    // Tiny byte limit → the single user message already crosses the 0.8
    // compaction threshold.
    let tmp = temp_project_with_limits(64);
    let config_path = tmp.path().join(".openslate/openslate.toml");
    let ctx = build_app_context(config_path.to_str())
        .await
        .expect("build app context");
    let factory = move |_config: &openslate_core::config::OpenSlateConfig, _alias: &str| {
        let responses = ScriptedProvider::scripted_turn();
        Ok::<Box<dyn ModelProvider>, anyhow::Error>(Box::new(ScriptedProvider::new(responses)))
    };
    let mut app = App::with_provider_factory(ctx, std::sync::Arc::new(factory));

    let long_prompt = "x".repeat(120); // > 0.8 × 64 bytes
    app.dispatch(Action::StartTurn(long_prompt.clone())).await;

    // Deferred: no engine task yet, badge state set — this is exactly the
    // state the run loop's draw paints between dispatch rounds.
    assert!(!app.is_running(), "engine not spawned while compacting");
    assert_eq!(*app.run_state(), RunState::Compacting);

    // The next dispatched action (a Tick in the real loop) runs the
    // compaction and then continues the turn spawn.
    app.dispatch(Action::Tick).await;
    assert!(app.is_running(), "turn spawned after the compact");
    assert_eq!(*app.run_state(), RunState::Thinking);

    // Pump to TurnDone and verify the turn completed end-to-end.
    let mut guard = 0;
    while app.is_running() && guard < 500 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        app.drain_engine_events().await;
        guard += 1;
    }
    app.drain_engine_events().await;
    assert_eq!(*app.run_state(), RunState::Idle);
    assert_eq!(app.history().len(), 4);
}

/// CancelTurn under ApprovalActive (ora-3 review item 2): the pending
/// request is denied (queue drains, preemption lifts) AND the turn's
/// cancel token flips — the R3 hazard was "Deny alone lets the engine
/// keep running".
#[tokio::test]
async fn cancel_turn_while_approval_pending_denies_and_cancels() {
    let tmp = temp_project();
    let config_path = tmp.path().join(".openslate/openslate.toml");
    let ctx = build_app_context(config_path.to_str())
        .await
        .expect("build app context");
    let mut app = App::new(ctx);

    let summary = ApprovalSummary {
        tool_name: "shell".into(),
        arguments: r#"{"cmd":"ls"}"#.into(),
        agent_id: "root".into(),
        risk_level: "high".into(),
    };
    app.dispatch(Action::Engine(TuiEvent::ApprovalRequested {
        id: 1,
        request: summary,
    }))
    .await;
    assert_eq!(*app.run_state(), RunState::ApprovalPending);
    assert!(!app.is_cancelled());

    app.dispatch(Action::CancelTurn).await;

    // The Deny consumed the pending request (preemption lifted) …
    assert_ne!(*app.run_state(), RunState::ApprovalPending);
    // … and the running turn is cancelled — a lone Deny would not be.
    assert!(app.is_cancelled());
}

/// Bridge-level deny round-trip (ora-3 review item 2, engine side): a
/// blocked `decide` is released by `respond(id, Deny)` and returns
/// `Denied` — the same std-channel path the CancelTurn branch and the
/// shutdown `deny_all` rely on.
#[tokio::test]
async fn approval_bridge_deny_round_trip() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<TuiEvent>();
    let bridge = Arc::new(ApprovalBridge::new(tx));

    let worker = std::thread::spawn({
        let bridge = Arc::clone(&bridge);
        move || {
            let request = ApprovalRequest {
                tool_name: "shell".into(),
                arguments: serde_json::json!({"cmd": "ls"}),
                agent_id: "root".into(),
                risk_level: RiskLevel::High,
            };
            bridge.decide(&request)
        }
    });

    // decide registers the request and blocks on the std responder.
    let id = match rx.recv().await {
        Some(TuiEvent::ApprovalRequested { id, .. }) => id,
        _ => panic!("expected ApprovalRequested from the bridge"),
    };
    assert_eq!(bridge.pending_count(), 1);
    assert!(bridge.respond(id, ApprovalChoice::Deny));

    let decision = worker.join().expect("decide thread to finish");
    assert!(matches!(decision, ApprovalDecision::Denied(_)));
    assert_eq!(bridge.pending_count(), 0);
}

/// Exit-confirmation semantics (user request): a second Ctrl+C inside
/// the modal CONFIRMS the quit; Esc dismisses without quitting.
#[tokio::test]
async fn second_ctrl_c_confirms_exit_and_esc_cancels() {
    let tmp = temp_project();
    let config_path = tmp.path().join(".openslate/openslate.toml");
    let ctx = build_app_context(config_path.to_str())
        .await
        .expect("build app context");
    let mut app = App::new(ctx);

    // Idle Ctrl+C escalates to the exit-confirmation modal (no quit yet).
    assert_eq!(
        app.dispatch(Action::CancelTurn).await,
        DispatchOutcome::Continue
    );

    // A second Ctrl+C inside the modal confirms the quit.
    assert_eq!(
        app.dispatch(Action::CancelTurn).await,
        DispatchOutcome::Quit
    );

    // Esc path: reopen the modal, then Esc dismisses without quitting.
    // (Fresh App so the quit above doesn't poison the loop state.)
    let tmp2 = temp_project();
    let config2 = tmp2.path().join(".openslate/openslate.toml");
    let ctx2 = build_app_context(config2.to_str())
        .await
        .expect("build app context 2");
    let mut app2 = App::new(ctx2);
    assert_eq!(
        app2.dispatch(Action::CancelTurn).await,
        DispatchOutcome::Continue
    );
    assert_eq!(
        app2.dispatch(Action::DismissOverlay).await,
        DispatchOutcome::Continue
    );
    // Modal closed → ordinary input flows again (no lingering preemption).
    app2.dispatch(Action::InputChar('x')).await;
    assert_eq!(app2.input_text(), "x");
}

/// Mouse wheel routing: a wheel notch ALWAYS scrolls the chat transcript,
/// never the input-box history — even while the input box holds focus
/// (where keyboard ↑/↓ switch history). Wheel-down back to the bottom
/// re-follows the live tail.
#[tokio::test]
async fn wheel_scroll_targets_transcript_not_input_history() {
    let tmp = temp_project();
    let config_path = tmp.path().join(".openslate/openslate.toml");
    let ctx = build_app_context(config_path.to_str())
        .await
        .expect("build app context");
    let mut app = App::new(ctx);

    // Default focus is the input box; type a char so any accidental
    // history navigation would be observable through `input_text()`.
    app.dispatch(Action::InputChar('a')).await;
    assert!(!app.transcript_pinned());

    // A wheel notch up pins the transcript (scrolls the chat) …
    app.dispatch(Action::WheelScrollUp).await;
    assert!(app.transcript_pinned());

    // … and leaves the input editor (and history) untouched.
    assert_eq!(app.input_text(), "a");

    // Wheel back down reaches the bottom and re-follows the live tail.
    app.dispatch(Action::WheelScrollDown).await;
    app.dispatch(Action::WheelScrollDown).await;
    assert!(!app.transcript_pinned());
}

// ─── fix-11: streaming order + mouse-capture toggle ────────────────────────

/// Streaming order (user report 1+3): reasoning that streamed BEFORE a
/// tool call must sit ABOVE the tool line in the live transcript —
/// event order is visual order — and the reasoning that streams after
/// the tool lands below it, above the answer. The `TurnDone(Ok)`
/// merge then KEEPS the live view in that order (reasoning survives;
/// the tool result folds in).
#[tokio::test]
async fn reasoning_and_tool_line_keep_event_order_in_live_view() {
    let tmp = temp_project();
    let config_path = tmp.path().join(".openslate/openslate.toml");
    let ctx = build_app_context(config_path.to_str())
        .await
        .expect("build app context");
    let mut app = App::new(ctx);

    app.dispatch(Action::StartTurn("go".into())).await;

    // Step 1: reasoning streams, then a tool fires mid-stream.
    app.dispatch(Action::Engine(TuiEvent::Reasoning("think A".into())))
        .await;
    app.dispatch(Action::Engine(TuiEvent::ToolStart {
        name: "echo".into(),
        args: "{}".into(),
    }))
    .await;
    // The reasoning was flushed into entries ABOVE the tool call.
    assert!(matches!(
        app.transcript_entries(),
        [TranscriptEntry::User(_), TranscriptEntry::Reasoning(text), TranscriptEntry::ToolCall { name, .. }]
            if text == "think A" && name == "echo"
    ));

    // Step 2: more reasoning + the answer — both stay in the streaming
    // area (below the tool line) until the next boundary/turn end.
    app.dispatch(Action::Engine(TuiEvent::ToolEnd {
        name: "echo".into(),
        bytes: 2,
        truncated: false,
    }))
    .await;
    app.dispatch(Action::Engine(TuiEvent::Reasoning("think B".into())))
        .await;
    app.dispatch(Action::Engine(TuiEvent::Delta("final".into())))
        .await;
    assert_eq!(
        app.transcript_entries().len(),
        3,
        "no new entries mid-stream"
    );

    // TurnDone(Ok) MERGE: the live view is kept in event order — the
    // reasoning blocks survive (core never persists reasoning; the
    // live entries are the only record), the straggler "think B"/
    // "final" buffers commit, and the tool result folds into the row.
    let spare = test_manager();
    let summary = openslate_tui::event::TurnSummary {
        run_id: RunId("order-run".into()),
        status: openslate_core::types::RunStatus::Completed,
        messages: vec![
            user_message("go"),
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                tool_call_id: None,
                name: None,
                tool_calls: Some(vec![ToolCall {
                    id: ToolCallId("tc-1".into()),
                    name: "echo".into(),
                    arguments: serde_json::json!({}),
                }]),
            },
            Message {
                role: MessageRole::Tool,
                content: "ok".into(),
                tool_call_id: Some(ToolCallId("tc-1".into())),
                name: Some("echo".into()),
                tool_calls: None,
            },
            Message {
                role: MessageRole::Assistant,
                content: "final".into(),
                tool_call_id: None,
                name: None,
                tool_calls: None,
            },
        ],
        total_steps: 2,
        total_input_tokens: 10,
        total_output_tokens: 5,
        total_cost_usd: 0.001,
        execution_tree: openslate_core::execution::ExecutionTree::new(
            RunId("order-run".into()),
            AgentId("root".into()),
        ),
        model: "mock-model".into(),
    };
    app.dispatch(Action::Engine(TuiEvent::TurnDone(Ok((summary, spare)))))
        .await;
    assert!(matches!(
        app.transcript_entries(),
        [
            TranscriptEntry::User(_),
            TranscriptEntry::Reasoning(a),
            TranscriptEntry::ToolCall {
                status: ToolEntryStatus::Done { bytes, .. },
                ..
            },
            TranscriptEntry::Reasoning(b),
            TranscriptEntry::Assistant(f),
        ] if a == "think A" && *bytes == 2 && b == "think B" && f == "final"
    ));
}

/// Mouse-capture runtime toggle (user report 2): Ctrl+M flips the App
/// flag with a notice explaining the trade-off; `/mouse` is the slash
/// alias. Default ON (wheel scrolls); OFF = native selection works.
#[tokio::test]
async fn toggle_mouse_capture_flips_state_and_notice() {
    let tmp = temp_project();
    let config_path = tmp.path().join(".openslate/openslate.toml");
    let ctx = build_app_context(config_path.to_str())
        .await
        .expect("build app context");
    let mut app = App::new(ctx);

    assert!(app.mouse_capture(), "capture on by default (wheel)");

    // Ctrl+M → off, with the selection/wheel trade-off notice.
    app.dispatch(Action::ToggleMouseCapture).await;
    assert!(!app.mouse_capture());
    let notice = app.status_notice().expect("notice set").to_owned();
    assert!(notice.contains("off"), "notice names the state: {notice}");
    assert!(
        notice.contains("selectable"),
        "notice explains the selection win: {notice}"
    );
    assert!(
        notice.contains("arrow"),
        "notice explains the wheel degradation: {notice}"
    );
    assert!(
        notice.is_ascii(),
        "notices render in the REVERSED status bar (ASCII-only): {notice}"
    );

    // `/mouse` — the slash alias flips back on.
    app.dispatch(Action::StartTurn("/mouse".into())).await;
    assert!(app.mouse_capture());
    let notice = app.status_notice().expect("notice set").to_owned();
    assert!(notice.contains("on"), "notice names the state: {notice}");
    assert!(
        notice.contains("wheel"),
        "notice names the wheel win: {notice}"
    );
    assert!(notice.is_ascii());

    // Ctrl+M round-trips and the toggle works mid-modal too? — No:
    // modal guards swallow it (same as Ctrl+T); assert the non-modal
    // path only, plus idempotent round-trip.
    app.dispatch(Action::ToggleMouseCapture).await;
    assert!(!app.mouse_capture());
    app.dispatch(Action::ToggleMouseCapture).await;
    assert!(app.mouse_capture());
}

// ─── fix-18: streaming markdown renders live ───────────────────────────────

/// Dispatch level: a Delta carrying markdown paints its styles into
/// the rendered buffer IMMEDIATELY — the heading accent exists in the
/// frame while the text still lives in the streaming area (entries
/// stay empty; nothing waited for a flush boundary).
#[tokio::test]
async fn streaming_delta_renders_markdown_styles_before_flush() {
    let tmp = temp_project();
    let config_path = tmp.path().join(".openslate/openslate.toml");
    let ctx = build_app_context(config_path.to_str())
        .await
        .expect("build app context");
    let mut app = App::new(ctx);

    app.dispatch(Action::Engine(TuiEvent::RequestStart {
        step: 1,
        model: "mock-model".into(),
    }))
    .await;
    app.dispatch(Action::Engine(TuiEvent::Delta("# Live heading".into())))
        .await;
    assert!(
        app.transcript_entries().is_empty(),
        "the delta still lives in the streaming area"
    );

    // Draw the transcript exactly like a frame would.
    let theme = openslate_tui::theme::Theme::new();
    let render_ctx = openslate_tui::components::AppCtx {
        theme,
        focus: openslate_tui::components::Focus::Transcript,
        run: openslate_tui::components::RunInfo {
            state: RunState::Thinking,
            spinner_frame: 0,
            model_label: "mock-model".into(),
            tokens_in: 0,
            tokens_out: 0,
            cost_usd: 0.0,
            elapsed: None,
            tool_calls_cur: 0,
            depth_cur: 0,
        },
        config: openslate_tui::components::ConfigSummary {
            model_alias: "main".into(),
            model_id: "mock-model".into(),
            provider_name: "mock".into(),
            max_depth: 4,
            max_tool_calls: 20,
            run_id: None,
        },
        size: (30, 4),
        notice: None,
    };
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(30, 4)).expect("test terminal");
    terminal
        .draw(|f| {
            use openslate_tui::components::Component;
            app.transcript().render(f, f.area(), &render_ctx)
        })
        .expect("draw frame");
    let buf = terminal.backend().buffer().clone();

    // The heading text carries the accent (Cyan) + BOLD mid-stream…
    let mut found = None;
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            if buf.cell((x, y)).is_some_and(|c| c.symbol() == "L") {
                found = buf.cell((x, y)).map(|c| (x, y, c.style()));
            }
        }
    }
    let (x, y, style) = found.expect("'L' of Live rendered");
    let _ = (x, y);
    assert_eq!(style.fg, Some(ratatui::style::Color::Cyan));
    assert!(style.add_modifier.contains(ratatui::style::Modifier::BOLD));
    // …and the live tail cursor (▍, running yellow) closes the row.
    let mut cursor = None;
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            if buf.cell((x, y)).is_some_and(|c| c.symbol() == "▍") {
                cursor = buf.cell((x, y)).map(|c| c.style());
            }
        }
    }
    assert_eq!(
        cursor.expect("tail cursor rendered").fg,
        Some(ratatui::style::Color::Yellow)
    );
}

// ─── merge batch: TurnDone(Ok) merge + TurnDone(Err) rebuild ────────────────

/// The multi-step App-level contract: a reasoning → tool → reasoning →
/// answer turn ends in `TurnDone(Ok)` and the merge keeps EVERYTHING
/// in event order — both thinking blocks, both requests' meta lines
/// (request 1 with a ttft segment — FirstToken dispatched; request 2
/// without — none dispatched), the folded tool row — while `history`
/// stays the engine-side message authority.
#[tokio::test]
async fn turn_done_ok_merge_keeps_thinking_meta_and_tool_rows() {
    let tmp = temp_project();
    let config_path = tmp.path().join(".openslate/openslate.toml");
    let ctx = build_app_context(config_path.to_str())
        .await
        .expect("build app context");
    let mut app = App::new(ctx);
    let spare_manager = test_manager();

    app.dispatch(Action::StartTurn("go".into())).await;

    // Request 1: reasoning, a pre-tool answer, usage, end.
    app.dispatch(Action::Engine(TuiEvent::RequestStart {
        step: 1,
        model: "mock-model".into(),
    }))
    .await;
    app.dispatch(Action::Engine(TuiEvent::FirstToken)).await;
    app.dispatch(Action::Engine(TuiEvent::Reasoning("think 1".into())))
        .await;
    app.dispatch(Action::Engine(TuiEvent::Delta("checking".into())))
        .await;
    app.dispatch(Action::Engine(TuiEvent::Usage(Usage {
        input_tokens: 50,
        output_tokens: 10,
        cached_input_tokens: None,
    })))
    .await;
    app.dispatch(Action::Engine(TuiEvent::RequestEnd)).await;

    // Tool round-trip between the requests.
    app.dispatch(Action::Engine(TuiEvent::ToolStart {
        name: "echo".into(),
        args: r#"{"text":"hi"}"#.into(),
    }))
    .await;
    app.dispatch(Action::Engine(TuiEvent::ToolEnd {
        name: "echo".into(),
        bytes: 2,
        truncated: false,
    }))
    .await;

    // Request 2: reasoning, the final answer (NO FirstToken → no ttft).
    app.dispatch(Action::Engine(TuiEvent::RequestStart {
        step: 2,
        model: "mock-model".into(),
    }))
    .await;
    app.dispatch(Action::Engine(TuiEvent::Reasoning("think 2".into())))
        .await;
    app.dispatch(Action::Engine(TuiEvent::Delta("final".into())))
        .await;
    app.dispatch(Action::Engine(TuiEvent::Usage(Usage {
        input_tokens: 80,
        output_tokens: 5,
        cached_input_tokens: None,
    })))
    .await;
    app.dispatch(Action::Engine(TuiEvent::RequestEnd)).await;

    let summary = openslate_tui::event::TurnSummary {
        run_id: RunId("merge-run".into()),
        status: openslate_core::types::RunStatus::Completed,
        messages: vec![
            user_message("go"),
            Message {
                role: MessageRole::Assistant,
                content: "checking".into(),
                tool_call_id: None,
                name: None,
                tool_calls: Some(vec![ToolCall {
                    id: ToolCallId("tc-1".into()),
                    name: "echo".into(),
                    arguments: serde_json::json!({"text": "hi"}),
                }]),
            },
            Message {
                role: MessageRole::Tool,
                content: "hi".into(),
                tool_call_id: Some(ToolCallId("tc-1".into())),
                name: Some("echo".into()),
                tool_calls: None,
            },
            Message {
                role: MessageRole::Assistant,
                content: "final".into(),
                tool_call_id: None,
                name: None,
                tool_calls: None,
            },
        ],
        total_steps: 2,
        total_input_tokens: 130,
        total_output_tokens: 15,
        total_cost_usd: 0.002,
        execution_tree: openslate_core::execution::ExecutionTree::new(
            RunId("merge-run".into()),
            AgentId("root".into()),
        ),
        model: "mock-model".into(),
    };
    app.dispatch(Action::Engine(TuiEvent::TurnDone(Ok((
        summary,
        spare_manager,
    )))))
    .await;
    assert_eq!(*app.run_state(), RunState::Idle);

    let entries = app.transcript_entries();
    let kind = |e: &TranscriptEntry| -> &'static str {
        match e {
            TranscriptEntry::User(_) => "user",
            TranscriptEntry::Reasoning(_) => "reasoning",
            TranscriptEntry::Meta(m) if m.starts_with('~') => "meta_est",
            TranscriptEntry::Meta(_) => "meta_usage",
            TranscriptEntry::Assistant(_) => "assistant",
            TranscriptEntry::ToolCall { .. } => "tool",
            TranscriptEntry::Approval { .. } | TranscriptEntry::Delegate { .. } => "other",
            TranscriptEntry::StepBreak => "sep",
        }
    };
    let kinds: Vec<&str> = entries.iter().map(kind).collect();
    let reasoning1 = kinds.iter().position(|k| *k == "reasoning").unwrap();
    let tool = kinds.iter().position(|k| *k == "tool").unwrap();
    let reasoning2 = kinds
        .iter()
        .enumerate()
        .filter(|(_, k)| **k == "reasoning")
        .nth(1)
        .expect("second reasoning block")
        .0;
    assert!(
        reasoning1 < tool && tool < reasoning2,
        "event order kept: {kinds:?}"
    );
    // Both thinking blocks in place, texts untouched.
    assert!(matches!(
        &entries[reasoning1],
        TranscriptEntry::Reasoning(t) if t == "think 1"
    ));
    assert!(matches!(
        &entries[reasoning2],
        TranscriptEntry::Reasoning(t) if t == "think 2"
    ));
    // Both requests' meta lines survived the turn end — request 1's
    // usage line carries the ttft segment (its FirstToken was
    // dispatched), request 2's does not.
    let usage_lines: Vec<&str> = entries
        .iter()
        .filter_map(|e| match e {
            TranscriptEntry::Meta(m) if m.starts_with("↑") => Some(m.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        usage_lines.len(),
        2,
        "one usage meta per request: {kinds:?}"
    );
    assert!(usage_lines[0].starts_with("↑50 ↓10"), "{}", usage_lines[0]);
    assert!(
        usage_lines[0].contains("ttft "),
        "request 1 observed FirstToken: {}",
        usage_lines[0]
    );
    assert!(usage_lines[1].starts_with("↑80 ↓5"), "{}", usage_lines[1]);
    assert!(
        !usage_lines[1].contains("ttft"),
        "request 2 saw no FirstToken: {}",
        usage_lines[1]
    );
    // The tool row folded the Tool message ("hi" → 2 bytes).
    assert!(matches!(
        &entries[tool],
        TranscriptEntry::ToolCall { status: ToolEntryStatus::Done { bytes, .. }, .. }
            if *bytes == 2
    ));
    // History stays the engine-side message authority.
    assert_eq!(app.history().len(), 4);
}

/// The Err-path fallback: after the store reload the transcript is
/// REBUILT from the persisted messages — the live-only reasoning/tool
/// entries are dropped (acceptable loss on the recovery path) and
/// display == persisted truth for the next turn.
#[tokio::test]
async fn turn_done_err_rebuilds_from_store_dropping_live_entries() {
    let tmp = temp_project();
    let config_path = tmp.path().join(".openslate/openslate.toml");
    let ctx = build_app_context(config_path.to_str())
        .await
        .expect("build app context");
    let factory = |_config: &openslate_core::config::OpenSlateConfig, _alias: &str| {
        Ok::<Box<dyn ModelProvider>, anyhow::Error>(Box::new(FailingProvider))
    };
    let mut app = App::with_provider_factory(ctx, std::sync::Arc::new(factory));

    app.dispatch(Action::StartTurn("hello".into())).await;
    assert!(app.is_running(), "engine task spawned");
    assert!(app.session_run_id().is_some(), "session run opened (store)");

    // Live-only entries exist when the turn dies: a reasoning block
    // flushed above a tool line (synthetic events — the failing
    // provider streams nothing itself).
    app.dispatch(Action::Engine(TuiEvent::Reasoning("dying thought".into())))
        .await;
    app.dispatch(Action::Engine(TuiEvent::ToolStart {
        name: "echo".into(),
        args: "{}".into(),
    }))
    .await;
    assert!(
        app.transcript_entries()
            .iter()
            .any(|e| matches!(e, TranscriptEntry::Reasoning(_))),
        "reasoning visible before the failure"
    );

    // Pump engine events until TurnDone(Err) is dispatched.
    let mut guard = 0;
    while app.is_running() && guard < 500 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        app.drain_engine_events().await;
        guard += 1;
    }
    app.drain_engine_events().await;
    assert!(!app.is_running(), "engine task must finish");
    assert!(matches!(app.run_state(), RunState::Error(_)));

    // The store reload REBUILT the transcript from the persisted
    // messages: only the user message survives (reasoning loss is
    // accepted on the recovery path).
    assert!(matches!(
        app.transcript_entries(),
        [TranscriptEntry::User(text)] if text == "hello"
    ));
    assert_eq!(app.history().len(), 1);
    assert_eq!(app.history()[0].content, "hello");
}

// ─── fix-13: OSC 52 clipboard copy ─────────────────────────────────────────

use openslate_tui::clipboard;

/// A capturing clipboard sink: records every payload written and
/// reports success (the injectable/capturable write seam).
fn capturing_sink() -> (Arc<std::sync::Mutex<Vec<String>>>, clipboard::ClipboardSink) {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = {
        let seen = Arc::clone(&seen);
        Box::new(move |payload: &str| {
            seen.lock().expect("sink lock").push(payload.to_owned());
            true
        }) as clipboard::ClipboardSink
    };
    (seen, sink)
}

/// Snapshot of everything the sink wrote (ends the MutexGuard within
/// the statement — never held across an await).
fn written_payloads(seen: &Arc<std::sync::Mutex<Vec<String>>>) -> Vec<String> {
    seen.lock().expect("sink lock").clone()
}

/// Fresh app on a temp project with the capturing sink installed.
async fn copy_test_app() -> (App, Arc<std::sync::Mutex<Vec<String>>>, tempfile::TempDir) {
    let tmp = temp_project();
    let config_path = tmp.path().join(".openslate/openslate.toml");
    let ctx = build_app_context(config_path.to_str())
        .await
        .expect("build app context");
    let mut app = App::new(ctx);
    let (seen, sink) = capturing_sink();
    app.set_clipboard_sink(sink);
    (app, seen, tmp)
}

/// Commit an assistant entry through the engine-event seam: a Delta
/// followed by RequestEnd flushes the streamed text into entries
/// (no Usage → no meta line), exactly like a live request boundary.
async fn commit_assistant(app: &mut App, text: String) {
    app.dispatch(Action::Engine(TuiEvent::Delta(text))).await;
    app.dispatch(Action::Engine(TuiEvent::RequestEnd)).await;
}

/// Empty state: Ctrl+Y with no assistant output posts the
/// `nothing to copy` notice and never touches the write channel.
#[tokio::test]
async fn copy_last_without_assistant_output_notices_nothing() {
    let (mut app, seen, _tmp) = copy_test_app().await;
    app.dispatch(Action::CopyLast).await;
    assert_eq!(app.status_notice(), Some("nothing to copy"));
    assert!(
        written_payloads(&seen).is_empty(),
        "no payload written without assistant output"
    );

    // A user message alone is not assistant output either.
    app.dispatch(Action::StartTurn("/copy".into())).await; // no-op copy
    assert_eq!(app.status_notice(), Some("nothing to copy"));
    assert!(written_payloads(&seen).is_empty());
}

/// Happy path: Ctrl+Y writes the OSC 52 payload of the last assistant
/// entry's RAW markdown (asterisks intact — entries store the original
/// text; markdown rendering happens at draw time) and confirms with an
/// ASCII notice. `/copy` takes the same path.
#[tokio::test]
async fn copy_last_sends_osc52_payload_and_notices() {
    let (mut app, seen, _tmp) = copy_test_app().await;
    commit_assistant(&mut app, "Hello **world**".into()).await;
    // A later reasoning block must NOT shadow the assistant entry.
    app.dispatch(Action::Engine(TuiEvent::Reasoning("musing".into())))
        .await;

    app.dispatch(Action::CopyLast).await;
    let written = written_payloads(&seen);
    assert_eq!(written.len(), 1, "exactly one write");
    assert_eq!(
        written[0],
        clipboard::osc52_payload("Hello **world**"),
        "payload carries the raw markdown"
    );
    assert_eq!(app.status_notice(), Some("copied 15 chars"));

    // `/copy` — the slash alias writes the same payload again.
    let (seen2, sink2) = capturing_sink();
    app.set_clipboard_sink(sink2);
    app.dispatch(Action::StartTurn("/copy".into())).await;
    assert_eq!(
        written_payloads(&seen2),
        vec![clipboard::osc52_payload("Hello **world**")],
        "/copy aliases Ctrl+Y"
    );
    assert_eq!(app.status_notice(), Some("copied 15 chars"));
}

/// The last entry WINS: a second assistant block replaces the copy
/// source.
#[tokio::test]
async fn copy_last_takes_the_most_recent_assistant_block() {
    let (mut app, seen, _tmp) = copy_test_app().await;
    commit_assistant(&mut app, "first answer".into()).await;
    commit_assistant(&mut app, "second **answer**".into()).await;

    app.dispatch(Action::CopyLast).await;
    assert_eq!(
        written_payloads(&seen),
        vec![clipboard::osc52_payload("second **answer**")]
    );
    assert_eq!(app.status_notice(), Some("copied 17 chars"));
}

/// Length cap: >32 KiB assistant text truncates to 32 KiB and the
/// notice says how much was taken of how much existed. A CJK char
/// straddling the cap boundary is never split (char-boundary-safe
/// walk-back shows up as N of M chars too).
#[tokio::test]
async fn copy_last_truncates_at_32_kib() {
    let (mut app, seen, _tmp) = copy_test_app().await;
    commit_assistant(&mut app, "y".repeat(clipboard::COPY_MAX_BYTES + 5)).await;
    app.dispatch(Action::CopyLast).await;
    let expected = "y".repeat(clipboard::COPY_MAX_BYTES);
    assert_eq!(
        written_payloads(&seen),
        vec![clipboard::osc52_payload(&expected)]
    );
    assert_eq!(
        app.status_notice(),
        Some("copied 32768 of 32773 chars"),
        "truncation notice reports N of M"
    );

    // CJK straddle: 32767 ASCII bytes + two 3-byte chars = 32769 chars;
    // the cut walks back to 32767 bytes (no partial char).
    let (mut app, seen, _tmp) = copy_test_app().await;
    let mut text = "a".repeat(clipboard::COPY_MAX_BYTES - 1);
    text.push_str("中中");
    commit_assistant(&mut app, text).await;
    app.dispatch(Action::CopyLast).await;
    let expected = "a".repeat(clipboard::COPY_MAX_BYTES - 1);
    assert_eq!(
        written_payloads(&seen),
        vec![clipboard::osc52_payload(&expected)]
    );
    assert_eq!(app.status_notice(), Some("copied 32767 of 32769 chars"));
}

/// A failing sink write must not claim success.
#[tokio::test]
async fn copy_last_reports_write_failures() {
    let (mut app, _seen, _tmp) = copy_test_app().await;
    commit_assistant(&mut app, "answer".into()).await;
    app.set_clipboard_sink(Box::new(|_| false));
    app.dispatch(Action::CopyLast).await;
    assert_eq!(app.status_notice(), Some("clipboard write failed"));
}
