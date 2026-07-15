//! Run Manager — orchestrates complete agent runs.
//!
//! Ties together: config loading, model resolution, agent tree,
//! execution tree, tool registry, runtime loop.

use std::sync::Arc;

use crate::agent_tree::AgentTree;
use crate::config::OpenSlateConfig;
use crate::error::OpenSlateError;
use crate::execution::{ExecutionStatus, ExecutionTree};
use crate::model_config::resolve_model;
use crate::provider::{ModelProvider, ProgressCallback};
use crate::runtime::{RunConfig, RuntimeLimits, execute_run};
use crate::tool::ToolRegistry;
use crate::trace::TraceCollector;
use crate::types::*;

/// Orchestrates a complete agent run from start to finish.
pub struct RunManager {
    pub config: OpenSlateConfig,
    pub agent_tree: AgentTree,
    pub tool_registry: Arc<ToolRegistry>,
    pub limits: RuntimeLimits,
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
    ) -> Self {
        let limits = RuntimeLimits::from_config(&config);
        Self {
            config,
            agent_tree,
            tool_registry: Arc::new(tool_registry),
            limits,
        }
    }

    /// Execute a complete agent run with prior conversation history.
    ///
    /// `prior_messages` is used as the `initial_messages` for the run directly —
    /// no additional user message is prepended. The caller is responsible for
    /// ensuring the current user message is already included in `prior_messages`.
    ///
    /// This:
    /// 1. Resolves the root agent
    /// 2. Creates an execution tree
    /// 3. Resolves the model
    /// 4. Runs the agent loop with the provided conversation history
    /// 5. Returns the result
    pub async fn execute_with_history(
        &self,
        provider: &dyn ModelProvider,
        prior_messages: &[Message],
        progress: Option<&mut dyn ProgressCallback>,
    ) -> Result<ManagedRunResult, OpenSlateError> {
        let run_id = RunId(uuid::Uuid::new_v4().to_string());
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

        let mut execution_tree =
            ExecutionTree::new(run_id.clone(), root_agent.id.clone());

        let resolved_model = resolve_model(&self.config, &root_agent.model_alias)?;

        let run_config = RunConfig {
            run_id: run_id.clone(),
            agent_id: root_agent.id.clone(),
            model_alias: root_agent.model_alias.clone(),
            system_prompt: Some(root_agent.default_prompt.clone()),
            initial_messages: prior_messages.to_vec(),
            max_steps: self.limits.max_steps,
            max_context_bytes: self.limits.max_context_bytes,
            max_output_bytes: self.limits.max_output_bytes,
            max_empty_turns: self.limits.max_empty_turns,
            tool_definitions: if root_agent.tools.is_empty() {
                // No `tools:` whitelist in the agent frontmatter → expose every
                // registered tool (builtins + all MCP tools). Listing tools
                // explicitly still acts as a whitelist.
                self.tool_registry.definitions()
            } else {
                self.tool_registry.definitions_for(&root_agent.tools)
            },
            timeout_ms: self.limits.timeout_ms,
        };

        // ToolRegistry implements ToolExecutor — pass it directly to the
        // async runtime loop.  No nested runtime / thread spawning needed.
        let result =
            execute_run(provider, run_config, &resolved_model.model_id, self.tool_registry.as_ref(), progress)
                .await?;

        trace.end_span(agent_span);
        trace.end_span(run_span);

        let root_en_id = execution_tree.root().id.clone();
        execution_tree.update_status(&root_en_id, ExecutionStatus::Completed);

        Ok(ManagedRunResult {
            run_id,
            status: result.status,
            messages: result.messages,
            total_steps: result.total_steps,
            total_input_tokens: result.total_input_tokens,
            total_output_tokens: result.total_output_tokens,
            execution_tree,
            model: resolved_model.model_id,
            trace,
        })
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
        }];
        self.execute_with_history(provider, &messages, progress).await
    }
}

impl RuntimeLimits {
    /// Build `RuntimeLimits` from an `OpenSlateConfig`.
    pub fn from_config(config: &OpenSlateConfig) -> Self {
        config.limits.as_ref().map(|l| Self {
            max_steps: l.max_steps,
            max_depth: l.max_depth,
            max_tool_calls: l.max_tool_calls,
            max_child_agent_calls: l.max_child_agent_calls,
            timeout_ms: l.timeout_ms,
            max_context_bytes: l.max_context_bytes,
            max_output_bytes: l.max_output_bytes,
            max_empty_turns: crate::runtime::DEFAULT_MAX_EMPTY_TURNS,
        }).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ProviderError;
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
            usage: Some(Usage {
                input_tokens: 50,
                output_tokens: 10,
            }),
            finish_reason: Some("stop".into()),
        }]);

        let manager =
            RunManager::new(test_config(), test_agent_tree(), ToolRegistry::new());
        let result = manager.execute(&provider, "hello", None).await.expect("run should succeed");

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
                content: Some("Let me echo that.".into()),
                tool_calls: vec![ToolCall {
                    id: ToolCallId("tc-1".into()),
                    name: "echo".into(),
                    arguments: serde_json::json!({"text": "hello world"}),
                }],
                usage: Some(Usage {
                    input_tokens: 100,
                    output_tokens: 20,
                }),
                finish_reason: Some("tool_calls".into()),
            },
            // Step 2: model returns final text
            ModelResponse {
                content: Some("Done!".into()),
                tool_calls: vec![],
                usage: Some(Usage {
                    input_tokens: 120,
                    output_tokens: 5,
                }),
                finish_reason: Some("stop".into()),
            },
        ]);

        let manager =
            RunManager::new(test_config(), test_agent_tree(), registry);
        let result =
            manager.execute(&provider, "echo hello world", None).await.expect("run should succeed");

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
            usage: None,
            finish_reason: Some("stop".into()),
        }]);

        let manager =
            RunManager::new(test_config(), test_agent_tree(), ToolRegistry::new());
        let result =
            manager.execute(&provider, "test", None).await.expect("run should succeed");

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
                content: None,
                tool_calls: vec![ToolCall {
                    id: ToolCallId("tc-1".into()),
                    name: "echo".into(),
                    arguments: serde_json::json!({}),
                }],
                usage: Some(Usage {
                    input_tokens: 200,
                    output_tokens: 50,
                }),
                finish_reason: Some("tool_calls".into()),
            },
            ModelResponse {
                content: Some("Final answer".into()),
                tool_calls: vec![],
                usage: Some(Usage {
                    input_tokens: 300,
                    output_tokens: 100,
                }),
                finish_reason: Some("stop".into()),
            },
        ]);

        let manager =
            RunManager::new(test_config(), test_agent_tree(), ToolRegistry::new());
        let result = manager.execute(&provider, "test", None).await.expect("run should succeed");

        assert_eq!(result.total_input_tokens, 500);
        assert_eq!(result.total_output_tokens, 150);
        assert_eq!(result.total_steps, 2);
    }

    #[test]
    fn test_runtime_limits_from_config() {
        let config = test_config();
        let limits = RuntimeLimits::from_config(&config);
        assert_eq!(limits.max_steps, 10);
        assert_eq!(limits.max_depth, 4);
        assert_eq!(limits.max_context_bytes, 100_000);
        assert_eq!(limits.max_output_bytes, 10_000);
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
        let config =
            crate::config::parse_openslate_toml(toml).expect("should parse");
        let limits = RuntimeLimits::from_config(&config);
        let default = RuntimeLimits::default();
        assert_eq!(limits.max_steps, default.max_steps);
        assert_eq!(limits.max_depth, default.max_depth);
        assert_eq!(limits.max_context_bytes, default.max_context_bytes);
    }
}
