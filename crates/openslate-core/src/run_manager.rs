//! Run Manager — orchestrates complete agent runs.
//!
//! Ties together: config loading, model resolution, agent tree,
//! execution tree, tool registry, runtime loop.

use std::sync::Arc;

use crate::agent_tree::AgentTree;
use crate::approval::ApprovalManager;
use crate::config::OpenSlateConfig;
use crate::error::OpenSlateError;
use crate::execution::ExecutionTree;
use crate::model_config::resolve_model;
use crate::provider::{ModelProvider, ProgressCallback};
use crate::runner::AgentRunner;
use crate::runtime::{CancellationToken, MessageSink, RuntimeLimits};
use crate::skills::SkillsCatalog;
use crate::tool::ToolRegistry;
use crate::trace::TraceCollector;
use crate::types::*;

/// Orchestrates a complete agent run from start to finish.
pub struct RunManager {
    pub config: OpenSlateConfig,
    pub agent_tree: AgentTree,
    pub tool_registry: Arc<ToolRegistry>,
    pub skills: SkillsCatalog,
    pub limits: RuntimeLimits,
    /// Approval gate (Phase 1), initialized from `[approval]` config and
    /// snapshotted into each run's AgentRunner. The CLI layer overrides it
    /// with the effective policy (`--yes` > config policy > interactive
    /// default) and attaches the mode-appropriate callback (interactive
    /// prompt / non-interactive high-risk gate).
    pub approval: ApprovalManager,
    /// Per-run message persistence sink (Phase 3). When set, the root
    /// agent's assistant messages and tool results are appended to durable
    /// storage the moment they enter the conversation (see
    /// [`MessageSink`]). The CLI layer assigns a `RunRecorder` here before
    /// execution; REPL sessions keep one recorder across turns so every
    /// turn lands under the same run id.
    pub message_sink: Option<Arc<dyn MessageSink>>,
}

/// Result of a complete managed run.
#[derive(Debug)]
pub struct ManagedRunResult {
    pub run_id: RunId,
    pub status: RunStatus,
    pub messages: Vec<Message>,
    pub total_steps: u32,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    /// Run-wide cost in USD (P2-3): every layer's model usage priced with
    /// its own model's `[models.X]` pricing and aggregated by the runner.
    /// 0.0 when no pricing is configured.
    pub total_cost_usd: f64,
    pub execution_tree: ExecutionTree,
    /// The model ID used for this run (e.g., "glm-4").
    pub model: String,
    /// Trace collector with recorded spans for this run.
    pub trace: TraceCollector,
}

impl RunManager {
    /// Create a new RunManager with the given configuration.
    pub fn new(
        config: OpenSlateConfig,
        agent_tree: AgentTree,
        tool_registry: ToolRegistry,
        skills: SkillsCatalog,
    ) -> Self {
        let limits = RuntimeLimits::from_config(&config);
        let approval = ApprovalManager::new(
            config
                .approval
                .as_ref()
                .map(|a| a.to_policy())
                .unwrap_or_default(),
        );
        Self {
            config,
            agent_tree,
            tool_registry: Arc::new(tool_registry),
            skills,
            limits,
            approval,
            message_sink: None,
        }
    }

    /// Mint a fresh run id (UUID v4). Callers that need the id BEFORE
    /// execution (e.g. to insert the run row for lifecycle persistence)
    /// use this plus [`Self::execute_with_run_id`].
    pub fn new_run_id() -> RunId {
        RunId(uuid::Uuid::new_v4().to_string())
    }

    /// Execute a complete agent run with prior conversation history, under a
    /// caller-chosen run id (Phase 3 resume / persistence entry point).
    ///
    /// `prior_messages` is used as the `initial_messages` for the run directly —
    /// no additional user message is prepended. The caller is responsible for
    /// ensuring the current user message is already included in `prior_messages`.
    ///
    /// `cancel` (Phase 4) is the run's cooperative cancellation token — the
    /// CLI layer cancels it on Ctrl-C and every layer of the run observes it
    /// at the execute_run checkpoints (returning
    /// `Ok(RunStatus::Interrupted)` with the partial transcript). A token
    /// already cancelled on entry surfaces `RuntimeError::Cancelled` before
    /// the run starts.
    ///
    /// This:
    /// 1. Resolves the root agent
    /// 2. Creates an execution tree
    /// 3. Resolves the model
    /// 4. Runs the agent loop with the provided conversation history
    /// 5. Returns the result
    pub async fn execute_with_run_id(
        &self,
        run_id: RunId,
        provider: &dyn ModelProvider,
        prior_messages: &[Message],
        cancel: CancellationToken,
        progress: Option<&mut dyn ProgressCallback>,
    ) -> Result<ManagedRunResult, OpenSlateError> {
        let root_agent = self.agent_tree.get_root();

        let mut trace = TraceCollector::new(std::process::id() as i32, 1);
        let run_span = trace.begin_span("run", "runtime");

        let agent_span = trace.begin_span_with_args(
            "agent_exec",
            "runtime",
            std::collections::HashMap::from([(
                "agent_id".to_owned(),
                serde_json::Value::String(root_agent.id.0.clone()),
            )]),
        );

        // The runner is the ToolExecutor seam: it intercepts `call_agent`
        // calls and (recursively, in later phases) runs child agents, while
        // delegating ordinary tools to the registry. Keeping `execute_run`
        // unmodified lets the single-agent loop stay generic.
        let runner = AgentRunner::new(
            provider,
            &self.agent_tree,
            &self.tool_registry,
            &self.skills,
            &self.config,
            self.limits.clone(),
            run_id.clone(),
        )
        .with_approval(self.approval.clone())
        .with_message_sink(self.message_sink.clone())
        .with_cancel_token(cancel);

        let result = runner.run_root(prior_messages.to_vec(), progress).await?;

        trace.end_span(agent_span);
        trace.end_span(run_span);

        let resolved_model = resolve_model(&self.config, &root_agent.model_alias)?;

        Ok(ManagedRunResult {
            run_id,
            status: result.status,
            messages: result.messages,
            total_steps: result.total_steps,
            // Tokens accumulated across all layers by the runner.
            total_input_tokens: runner.total_input_tokens(),
            total_output_tokens: runner.total_output_tokens(),
            // Cost accumulated across all layers by the runner (P2-3),
            // each layer priced with its own model's pricing.
            total_cost_usd: runner.total_cost_usd(),
            execution_tree: runner.execution_tree(),
            model: resolved_model.model_id,
            trace,
        })
    }

    /// Execute a complete agent run with prior conversation history.
    ///
    /// Convenience wrapper around [`Self::execute_with_run_id`] that mints a
    /// fresh run id and a fresh (never-cancelled) cancellation token;
    /// callers that need either upfront (lifecycle persistence, Ctrl-C)
    /// call [`Self::execute_with_run_id`] directly.
    pub async fn execute_with_history(
        &self,
        provider: &dyn ModelProvider,
        prior_messages: &[Message],
        progress: Option<&mut dyn ProgressCallback>,
    ) -> Result<ManagedRunResult, OpenSlateError> {
        self.execute_with_run_id(
            Self::new_run_id(),
            provider,
            prior_messages,
            CancellationToken::new(),
            progress,
        )
        .await
    }

    /// Execute a complete agent run with a single user message (no prior history).
    ///
    /// Convenience wrapper around [`execute_with_history`] that wraps `input`
    /// into a single `User` message. Existing callers remain unchanged.
    pub async fn execute(
        &self,
        provider: &dyn ModelProvider,
        input: &str,
        progress: Option<&mut dyn ProgressCallback>,
    ) -> Result<ManagedRunResult, OpenSlateError> {
        let messages = vec![Message {
            role: MessageRole::User,
            content: input.to_owned(),
            tool_call_id: None,
            name: None,
            tool_calls: None,
            reasoning_content: None,
        }];
        self.execute_with_history(provider, &messages, progress)
            .await
    }
}

impl RuntimeLimits {
    /// Build `RuntimeLimits` from an `OpenSlateConfig`.
    pub fn from_config(config: &OpenSlateConfig) -> Self {
        config
            .limits
            .as_ref()
            .map(|l| Self {
                max_steps: l.max_steps,
                max_depth: l.max_depth,
                max_tool_calls: l.max_tool_calls,
                max_child_agent_calls: l.max_child_agent_calls,
                timeout_ms: l.timeout_ms,
                max_context_bytes: l.max_context_bytes,
                max_output_bytes: l.max_output_bytes,
                max_empty_turns: crate::runtime::DEFAULT_MAX_EMPTY_TURNS,
                parallel_tool_calls: l.parallel_tool_calls,
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ProviderError;
    use crate::execution::ExecutionStatus;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // -- Mock provider --

    struct MockProvider {
        responses: Vec<ModelResponse>,
        call_count: AtomicUsize,
    }

    impl MockProvider {
        fn new(responses: Vec<ModelResponse>) -> Self {
            Self {
                responses,
                call_count: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl ModelProvider for MockProvider {
        async fn generate(
            &self,
            _request: crate::provider::GenerateRequest,
        ) -> Result<ModelResponse, ProviderError> {
            let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
            self.responses
                .get(idx)
                .cloned()
                .ok_or(ProviderError::ServerError(500))
        }

        fn provider_name(&self) -> &str {
            "mock"
        }
    }

    // -- Test helpers --

    fn test_config() -> OpenSlateConfig {
        let toml = r#"
[providers.zhipu]
base_url = "https://open.bigmodel.cn/api/paas/v4"
api_key_env = "ZHIPU_API_KEY"

[models.main]
provider = "zhipu"
model = "glm-5.1"

[limits]
max_steps = 10
max_depth = 4
max_context_bytes = 100_000
max_output_bytes = 10_000
"#;
        crate::config::parse_openslate_toml(toml).expect("test config should parse")
    }

    fn test_agent_tree() -> AgentTree {
        let agents = vec![AgentConfig {
            id: AgentId("root".into()),
            name: "Root Agent".into(),
            model: "main".into(),
            children: vec![],
            tools: vec!["echo".into()],
            default_prompt: "You are a test agent.".into(),
        }];
        AgentTree::from_configs(&agents).expect("test tree should build")
    }

    // -- Tests --

    #[tokio::test]
    async fn test_simple_managed_run() {
        let provider = MockProvider::new(vec![ModelResponse {
            content: Some("Hello from agent!".into()),
            tool_calls: vec![],
            reasoning_content: None,
            usage: Some(Usage {
                input_tokens: 50,
                output_tokens: 10,
                cached_input_tokens: None,
                reasoning_tokens: None,
            }),
            finish_reason: Some("stop".into()),
        }]);

        let manager = RunManager::new(
            test_config(),
            test_agent_tree(),
            ToolRegistry::new(),
            SkillsCatalog::default(),
        );
        let result = manager
            .execute(&provider, "hello", None)
            .await
            .expect("run should succeed");

        assert_eq!(result.status, RunStatus::Completed);
        assert_eq!(result.total_steps, 1);
        assert_eq!(result.messages.len(), 2); // user + assistant
        assert_eq!(result.messages[1].content, "Hello from agent!");
        assert_eq!(result.total_input_tokens, 50);
        assert_eq!(result.total_output_tokens, 10);
    }

    #[tokio::test]
    async fn test_managed_run_with_tools() {
        // Register an echo tool
        struct EchoTool;

        #[async_trait::async_trait]
        impl crate::tool::Tool for EchoTool {
            fn name(&self) -> &str {
                "echo"
            }
            fn description(&self) -> &str {
                "Echo back the input"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "text": { "type": "string" }
                    }
                })
            }
            async fn execute(
                &self,
                args: &serde_json::Value,
            ) -> Result<ToolOutput, crate::error::ToolError> {
                let text = args["text"].as_str().unwrap_or("");
                Ok(ToolOutput {
                    content: text.to_owned(),
                    bytes: text.len(),
                    duration_ms: 1,
                    status: ToolOutputStatus::Success,
                })
            }
        }

        let mut registry = ToolRegistry::new();
        registry.register(EchoTool);

        let provider = MockProvider::new(vec![
            // Step 1: model requests echo tool
            ModelResponse {
            reasoning_content: None,
                content: Some("Let me echo that.".into()),
                tool_calls: vec![ToolCall {
                    id: ToolCallId("tc-1".into()),
                    name: "echo".into(),
                    arguments: serde_json::json!({"text": "hello world"}),
                }],
                usage: Some(Usage {
                    input_tokens: 100,
                    output_tokens: 20,
                    cached_input_tokens: None,
                    reasoning_tokens: None,
                }),
                finish_reason: Some("tool_calls".into()),
            },
            // Step 2: model returns final text
            ModelResponse {
                content: Some("Done!".into()),
                tool_calls: vec![],
                reasoning_content: None,
                usage: Some(Usage {
                    input_tokens: 120,
                    output_tokens: 5,
                    cached_input_tokens: None,
                    reasoning_tokens: None,
                }),
                finish_reason: Some("stop".into()),
            },
        ]);

        let manager = RunManager::new(
            test_config(),
            test_agent_tree(),
            registry,
            SkillsCatalog::default(),
        );
        let result = manager
            .execute(&provider, "echo hello world", None)
            .await
            .expect("run should succeed");

        assert_eq!(result.status, RunStatus::Completed);
        assert_eq!(result.total_steps, 2);
        // 1 user + 1 assistant + 1 tool + 1 assistant
        assert_eq!(result.messages.len(), 4);
        assert_eq!(result.messages[2].role, MessageRole::Tool);
        assert_eq!(result.messages[2].content, "hello world");
        assert_eq!(result.total_input_tokens, 220);
        assert_eq!(result.total_output_tokens, 25);
    }

    #[tokio::test]
    async fn test_managed_run_creates_execution_tree() {
        let provider = MockProvider::new(vec![ModelResponse {
            content: Some("Done".into()),
            tool_calls: vec![],
            reasoning_content: None,
            usage: None,
            finish_reason: Some("stop".into()),
        }]);

        let manager = RunManager::new(
            test_config(),
            test_agent_tree(),
            ToolRegistry::new(),
            SkillsCatalog::default(),
        );
        let result = manager
            .execute(&provider, "test", None)
            .await
            .expect("run should succeed");

        // Execution tree should have a root node for the root agent
        let root = result.execution_tree.root();
        assert_eq!(root.agent_id.0, "root");
        assert_eq!(root.depth, 0);
        assert_eq!(root.status, ExecutionStatus::Completed);
    }

    #[tokio::test]
    async fn test_managed_run_tracks_tokens() {
        let provider = MockProvider::new(vec![
            ModelResponse {
            reasoning_content: None,
                content: None,
                tool_calls: vec![ToolCall {
                    id: ToolCallId("tc-1".into()),
                    name: "echo".into(),
                    arguments: serde_json::json!({}),
                }],
                usage: Some(Usage {
                    input_tokens: 200,
                    output_tokens: 50,
                    cached_input_tokens: None,
                    reasoning_tokens: None,
                }),
                finish_reason: Some("tool_calls".into()),
            },
            ModelResponse {
                content: Some("Final answer".into()),
                tool_calls: vec![],
                reasoning_content: None,
                usage: Some(Usage {
                    input_tokens: 300,
                    output_tokens: 100,
                    cached_input_tokens: None,
                    reasoning_tokens: None,
                }),
                finish_reason: Some("stop".into()),
            },
        ]);

        let manager = RunManager::new(
            test_config(),
            test_agent_tree(),
            ToolRegistry::new(),
            SkillsCatalog::default(),
        );
        let result = manager
            .execute(&provider, "test", None)
            .await
            .expect("run should succeed");

        assert_eq!(result.total_input_tokens, 500);
        assert_eq!(result.total_output_tokens, 150);
        assert_eq!(result.total_steps, 2);
    }

    // ── Cost passthrough (P2-3) ─────────────────────────────────────────

    #[tokio::test]
    async fn test_managed_run_passes_cost_through() {
        // Two steps of 500k in / 100k out at $1/M in + $2/M out
        // → per step 0.5 + 0.2 = 0.7 → run total 1.4.
        let toml = r#"
[providers.zhipu]
base_url = "https://example.com"
api_key_env = "ZHIPU_API_KEY"

[models.main]
provider = "zhipu"
model = "glm-5.1"
input_price_per_mtok = 1.0
output_price_per_mtok = 2.0
"#;
        let config = crate::config::parse_openslate_toml(toml).expect("parse");

        let provider = MockProvider::new(vec![
            ModelResponse {
            reasoning_content: None,
                content: None,
                tool_calls: vec![ToolCall {
                    id: ToolCallId("tc-1".into()),
                    name: "echo".into(),
                    arguments: serde_json::json!({}),
                }],
                usage: Some(Usage {
                    input_tokens: 500_000,
                    output_tokens: 100_000,
                    cached_input_tokens: None,
                    reasoning_tokens: None,
                }),
                finish_reason: Some("tool_calls".into()),
            },
            ModelResponse {
                content: Some("done".into()),
                tool_calls: vec![],
                reasoning_content: None,
                usage: Some(Usage {
                    input_tokens: 500_000,
                    output_tokens: 100_000,
                    cached_input_tokens: None,
                    reasoning_tokens: None,
                }),
                finish_reason: Some("stop".into()),
            },
        ]);

        let mut registry = ToolRegistry::new();
        registry.register(SimpleEchoTool);

        let manager = RunManager::new(
            config,
            test_agent_tree(),
            registry,
            SkillsCatalog::default(),
        );
        let result = manager
            .execute(&provider, "test", None)
            .await
            .expect("run should succeed");

        assert_eq!(result.total_input_tokens, 1_000_000);
        assert_eq!(result.total_output_tokens, 200_000);
        assert!(
            (result.total_cost_usd - 1.4f64).abs() < 1e-9,
            "cost must flow usage→pricing→ManagedRunResult, got {}",
            result.total_cost_usd
        );
    }

    #[tokio::test]
    async fn test_managed_run_cost_zero_without_pricing() {
        let provider = MockProvider::new(vec![ModelResponse {
            content: Some("answer".into()),
            tool_calls: vec![],
            reasoning_content: None,
            usage: Some(Usage {
                input_tokens: 42,
                output_tokens: 7,
                cached_input_tokens: None,
                reasoning_tokens: None,
            }),
            finish_reason: Some("stop".into()),
        }]);

        let manager = RunManager::new(
            test_config(),
            test_agent_tree(),
            ToolRegistry::new(),
            SkillsCatalog::default(),
        );
        let result = manager
            .execute(&provider, "test", None)
            .await
            .expect("run should succeed");
        assert_eq!(result.total_cost_usd, 0.0);
    }

    #[test]
    fn test_runtime_limits_from_config() {
        let config = test_config();
        let limits = RuntimeLimits::from_config(&config);
        assert_eq!(limits.max_steps, 10);
        assert_eq!(limits.max_depth, 4);
        assert_eq!(limits.max_context_bytes, 100_000);
        assert_eq!(limits.max_output_bytes, 10_000);
        // P2-1 flag threads through to the runtime limits (absent field
        // defaults to on).
        assert!(limits.parallel_tool_calls);
    }

    #[test]
    fn test_runtime_limits_default_when_no_limits_section() {
        let toml = r#"
[providers.zhipu]
base_url = "https://example.com"
api_key_env = "KEY"

[models.main]
provider = "zhipu"
model = "m"
"#;
        let config = crate::config::parse_openslate_toml(toml).expect("should parse");
        let limits = RuntimeLimits::from_config(&config);
        let default = RuntimeLimits::default();
        assert_eq!(limits.max_steps, default.max_steps);
        assert_eq!(limits.max_depth, default.max_depth);
        assert_eq!(limits.max_context_bytes, default.max_context_bytes);
    }

    // ── Phase 3: persistence sink + resume ──────────────────────────────

    /// Module-scope echo tool for the persistence/resume tests (the
    /// `test_managed_run_with_tools` one is fn-scoped).
    struct SimpleEchoTool;

    #[async_trait::async_trait]
    impl crate::tool::Tool for SimpleEchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Echo back the input"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(
            &self,
            args: &serde_json::Value,
        ) -> Result<ToolOutput, crate::error::ToolError> {
            let text = args["text"].as_str().unwrap_or("");
            Ok(ToolOutput {
                content: text.to_owned(),
                bytes: text.len(),
                duration_ms: 1,
                status: ToolOutputStatus::Success,
            })
        }
    }

    /// Recording sink shared with the RunManager under test.
    struct RecordingSink(std::sync::Mutex<Vec<Message>>);

    #[async_trait::async_trait]
    impl crate::runtime::MessageSink for RecordingSink {
        async fn append(&self, message: &Message) {
            self.0.lock().expect("sink poisoned").push(message.clone());
        }
    }

    /// Provider that records the message history of every GenerateRequest.
    struct CapturingProvider {
        responses: Vec<ModelResponse>,
        call_count: AtomicUsize,
        histories: std::sync::Mutex<Vec<Vec<Message>>>,
    }

    impl CapturingProvider {
        fn new(responses: Vec<ModelResponse>) -> Self {
            Self {
                responses,
                call_count: AtomicUsize::new(0),
                histories: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl ModelProvider for CapturingProvider {
        async fn generate(
            &self,
            request: crate::provider::GenerateRequest,
        ) -> Result<ModelResponse, ProviderError> {
            self.histories
                .lock()
                .expect("histories")
                .push(request.messages);
            let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
            self.responses
                .get(idx)
                .cloned()
                .ok_or(ProviderError::ServerError(500))
        }
        fn provider_name(&self) -> &str {
            "capturing"
        }
    }

    fn tool_call_response(id: &str) -> ModelResponse {
        ModelResponse {
        reasoning_content: None,
            content: None,
            tool_calls: vec![ToolCall {
                id: ToolCallId(id.into()),
                name: "echo".into(),
                arguments: serde_json::json!({"text": "hi"}),
            }],
            usage: None,
            finish_reason: Some("tool_calls".into()),
        }
    }

    fn final_response(text: &str) -> ModelResponse {
        ModelResponse {
            content: Some(text.into()),
            tool_calls: vec![],
            reasoning_content: None,
            usage: None,
            finish_reason: Some("stop".into()),
        }
    }

    #[tokio::test]
    async fn test_execute_with_run_id_honors_given_id_and_drives_sink() {
        let provider = MockProvider::new(vec![final_response("answer")]);
        let sink = std::sync::Arc::new(RecordingSink(std::sync::Mutex::new(Vec::new())));

        let mut manager = RunManager::new(
            test_config(),
            test_agent_tree(),
            ToolRegistry::new(),
            SkillsCatalog::default(),
        );
        manager.message_sink = Some(sink.clone());
        let run_id = RunId("caller-chosen-id".into());

        let result = manager
            .execute_with_run_id(
                run_id.clone(),
                &provider,
                &[Message {
                    role: MessageRole::User,
                    content: "hello".into(),
                    tool_call_id: None,
                    name: None,
                    tool_calls: None,
                    reasoning_content: None,
                }],
                CancellationToken::new(),
                None,
            )
            .await
            .expect("run should succeed");

        assert_eq!(result.run_id, run_id, "the caller-chosen id must be used");
        let observed = sink.0.lock().expect("sink poisoned");
        assert_eq!(observed.len(), 1, "final assistant message persisted");
        assert_eq!(observed[0].content, "answer");
    }

    #[tokio::test]
    async fn test_resume_feeds_restored_history_to_provider() {
        // Turn 1: a tool-calling conversation recorded through the sink —
        // simulating exactly what a RunRecorder persisted to the store.
        let provider1 = CapturingProvider::new(vec![
            tool_call_response("tc-1"),
            final_response("first answer"),
        ]);
        let sink = std::sync::Arc::new(RecordingSink(std::sync::Mutex::new(Vec::new())));

        let mut registry = ToolRegistry::new();
        registry.register(SimpleEchoTool);
        let mut manager = RunManager::new(
            test_config(),
            test_agent_tree(),
            registry,
            SkillsCatalog::default(),
        );
        manager.message_sink = Some(sink.clone());

        let first = manager
            .execute_with_history(
                &provider1,
                &[Message {
                    role: MessageRole::User,
                    content: "echo hi".into(),
                    tool_call_id: None,
                    name: None,
                    tool_calls: None,
                    reasoning_content: None,
                }],
                None,
            )
            .await
            .expect("first run");
        assert_eq!(first.status, RunStatus::Completed);

        // The persisted stream is exactly what the sink observed, prefixed
        // by the user message the CLI layer writes directly.
        let restored: Vec<Message> = std::iter::once(Message {
            role: MessageRole::User,
            content: "echo hi".into(),
            tool_call_id: None,
            name: None,
            tool_calls: None,
            reasoning_content: None,
        })
        .chain(sink.0.lock().expect("sink poisoned").iter().cloned())
        .collect();

        // Turn 2 (resume): same run id, restored history — the provider's
        // request must carry the full restored transcript.
        let provider2 = CapturingProvider::new(vec![final_response("resumed answer")]);
        let second = manager
            .execute_with_run_id(
                first.run_id.clone(),
                &provider2,
                &restored,
                CancellationToken::new(),
                None,
            )
            .await
            .expect("resume run");
        assert_eq!(second.run_id, first.run_id, "resume continues the same run");

        let histories = provider2.histories.lock().expect("histories");
        assert_eq!(histories.len(), 1, "one request in the resumed turn");
        let req = &histories[0];
        // user + assistant(tool_calls) + tool + final — the pairing survived.
        assert_eq!(req.len(), restored.len(), "full restored history sent");
        assert_eq!(req[0].role, MessageRole::User);
        assert_eq!(req[0].content, "echo hi");
        assert_eq!(req[1].role, MessageRole::Assistant);
        let tcs = req[1].tool_calls.as_ref().expect("tool_calls restored");
        assert_eq!(tcs[0].id, ToolCallId("tc-1".into()));
        assert_eq!(req[2].role, MessageRole::Tool);
        assert_eq!(req[2].tool_call_id, Some(ToolCallId("tc-1".into())));
    }

    #[tokio::test]
    async fn test_interrupted_run_partial_transcript_is_resumable() {
        // max_steps=1 with a tool-calling model → Interrupted right after
        // the assistant's tool_calls + tool result landed (no final answer).
        let toml = r#"
[providers.zhipu]
base_url = "https://example.com"
api_key_env = "KEY"

[models.main]
provider = "zhipu"
model = "m1"

[limits]
max_steps = 1
max_depth = 4
max_context_bytes = 100_000
max_output_bytes = 10_000
"#;
        let config = crate::config::parse_openslate_toml(toml).expect("parse");

        let mut registry = ToolRegistry::new();
        registry.register(SimpleEchoTool);
        let manager = RunManager::new(
            config,
            test_agent_tree(),
            registry,
            SkillsCatalog::default(),
        );

        let provider = CapturingProvider::new(vec![tool_call_response("tc-9")]);
        let run_id = RunId("interrupted-run".into());
        let result = manager
            .execute_with_run_id(
                run_id.clone(),
                &provider,
                &[Message {
                    role: MessageRole::User,
                    content: "go".into(),
                    tool_call_id: None,
                    name: None,
                    tool_calls: None,
                    reasoning_content: None,
                }],
                CancellationToken::new(),
                None,
            )
            .await
            .expect("interrupted run still returns Ok");

        assert_eq!(result.status, RunStatus::Interrupted);
        // Partial transcript: user + assistant(tool_calls) + tool result.
        assert_eq!(result.messages.len(), 3);
        assert_eq!(
            result.messages[1].tool_calls.as_ref().expect("tcs").len(),
            1
        );
        assert_eq!(result.messages[2].role, MessageRole::Tool);

        // Resume: feed the partial transcript back; the model sees the tool
        // transcript and completes.
        let provider2 = CapturingProvider::new(vec![final_response("finished after resume")]);
        let resumed = manager
            .execute_with_run_id(
                run_id,
                &provider2,
                &result.messages,
                CancellationToken::new(),
                None,
            )
            .await
            .expect("resume completes");
        assert_eq!(resumed.status, RunStatus::Completed);

        let histories = provider2.histories.lock().expect("histories");
        assert_eq!(histories[0].len(), 3, "partial transcript carried over");
        assert_eq!(histories[0][2].role, MessageRole::Tool);
    }

    // ── Cancellation (Phase 4) ────────────────────────────────────────────

    /// Provider that returns a scripted tool-call response on its first call
    /// and cancels the run's token while doing so (a deterministic
    /// "Ctrl-C lands during the first model turn"); later calls answer with
    /// a final response.
    struct CancelAfterFirstProvider {
        token: CancellationToken,
    }

    #[async_trait::async_trait]
    impl ModelProvider for CancelAfterFirstProvider {
        async fn generate(
            &self,
            _request: crate::provider::GenerateRequest,
        ) -> Result<ModelResponse, ProviderError> {
            self.token.cancel();
            Ok(tool_call_response("tc-c"))
        }
        fn provider_name(&self) -> &str {
            "cancel-after-first"
        }
    }

    #[tokio::test]
    async fn test_cancel_mid_run_returns_interrupted_with_persisted_partial() {
        let token = CancellationToken::new();
        let provider = CancelAfterFirstProvider {
            token: token.clone(),
        };
        let sink = std::sync::Arc::new(RecordingSink(std::sync::Mutex::new(Vec::new())));

        let mut registry = ToolRegistry::new();
        registry.register(SimpleEchoTool);
        let mut manager = RunManager::new(
            test_config(),
            test_agent_tree(),
            registry,
            SkillsCatalog::default(),
        );
        manager.message_sink = Some(sink.clone());

        let result = manager
            .execute_with_run_id(
                RunId("cancelled-run".into()),
                &provider,
                &[Message {
                    role: MessageRole::User,
                    content: "go".into(),
                    tool_call_id: None,
                    name: None,
                    tool_calls: None,
                    reasoning_content: None,
                }],
                token,
                None,
            )
            .await
            .expect("cancelled run returns Ok(Interrupted)");

        // The completed step's transcript survives and is exactly what the
        // sink persisted — the run stays resumable from storage.
        assert_eq!(result.status, RunStatus::Interrupted);
        assert_eq!(result.messages.len(), 3, "user + assistant(tc) + tool");
        let observed = sink.0.lock().expect("sink poisoned").clone();
        assert_eq!(observed.len(), 2, "assistant + tool result persisted");
        assert_eq!(observed[0].role, MessageRole::Assistant);
        assert_eq!(observed[1].role, MessageRole::Tool);

        // Resume: the partial transcript feeds the next request in full.
        let provider2 = CapturingProvider::new(vec![final_response("resumed answer")]);
        let resumed = manager
            .execute_with_run_id(
                RunId("cancelled-run".into()),
                &provider2,
                &result.messages,
                CancellationToken::new(),
                None,
            )
            .await
            .expect("resume completes");
        assert_eq!(resumed.status, RunStatus::Completed);
        let histories = provider2.histories.lock().expect("histories");
        assert_eq!(histories[0].len(), 3, "partial transcript carried over");
        assert_eq!(histories[0][2].role, MessageRole::Tool);
    }
}
