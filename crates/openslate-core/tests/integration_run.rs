//! Integration tests: end-to-end RunManager flow with mock provider and real tools.
//!
//! These tests exercise the full pipeline:
//!   config → agent tree → RunManager → runtime loop → mock provider → tool execution → result
//!
//! They verify the C1 fix (no nested runtime) and C2 fix (workspace confinement)
//! work correctly through the public API.

use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;

use openslate_core::agent_tree::AgentTree;
use openslate_core::config::parse_openslate_toml;
use openslate_core::config::BuiltinToolsConfig;
use openslate_core::error::ProviderError;
use openslate_core::mcp::connect_builtin_servers;
use openslate_core::provider::{GenerateRequest, ModelProvider};
use openslate_core::run_manager::RunManager;
use openslate_core::tool::ToolRegistry;
use openslate_core::types::*;

// ─── Mock Provider ───────────────────────────────────────────────────────

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

    fn provider_name(&self) -> &str {
        "scripted-mock"
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────

fn test_config() -> openslate_core::config::OpenSlateConfig {
    let toml = r#"
[providers.mock]
base_url = "http://localhost"
api_key_env = "MOCK_KEY"

[models.main]
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

fn test_agent_tree(tools: Vec<String>) -> AgentTree {
    let agents = vec![AgentConfig {
        id: AgentId("root".into()),
        name: "Root Agent".into(),
        model: "main".into(),
        children: vec![],
        tools,
        default_prompt: "You are a test agent.".into(),
    }];
    AgentTree::from_configs(&agents).expect("agent tree should build")
}

/// Like [`test_config`] but with a tiny `max_output_bytes` so the global
/// output cap kicks in on any reasonably sized tool output.
fn test_config_small_output_cap() -> openslate_core::config::OpenSlateConfig {
    let toml = r#"
[providers.mock]
base_url = "http://localhost"
api_key_env = "MOCK_KEY"

[models.main]
provider = "mock"
model = "mock-model"

[limits]
max_steps = 10
max_depth = 4
max_context_bytes = 100_000
max_output_bytes = 200
"#;
    parse_openslate_toml(toml).expect("test config should parse")
}

/// Test tool whose output (10 KB) far exceeds the configured cap.
struct HugeOutputTool;

#[async_trait]
impl openslate_core::tool::Tool for HugeOutputTool {
    fn name(&self) -> &str {
        "huge"
    }
    fn description(&self) -> &str {
        "Returns a huge string"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(
        &self,
        _args: &serde_json::Value,
    ) -> Result<ToolOutput, openslate_core::error::ToolError> {
        let content = "z".repeat(10_000);
        let bytes = content.len();
        Ok(ToolOutput {
            content,
            bytes,
            duration_ms: 0,
            status: ToolOutputStatus::Success,
        })
    }
}

/// Build a `ToolRegistry` from the in-process builtin MCP servers rooted at
/// `root` (all builtin tools enabled). This is the production wiring in
/// miniature — the returned connections must outlive the registry, so hold
/// the second tuple element until the run finishes.
async fn builtin_mcp_registry(
    root: &std::path::Path,
) -> (
    ToolRegistry,
    Vec<rmcp::service::RunningService<rmcp::service::RoleClient, rmcp::model::ClientInfo>>,
) {
    let (tools, services) = connect_builtin_servers(root, &BuiltinToolsConfig::default())
        .await
        .expect("builtin servers should connect");
    let mut registry = ToolRegistry::new();
    for tool in tools {
        registry
            .try_register(tool)
            .expect("builtin tool names never conflict");
    }
    (registry, services)
}

// ─── Tests ───────────────────────────────────────────────────────────────

/// Simple single-turn: user says hello, model responds, run completes.
#[tokio::test]
async fn integration_simple_single_turn() {
    let provider = ScriptedProvider::new(vec![ModelResponse {
        content: Some("Hello from the model!".into()),
        tool_calls: vec![],
        usage: Some(Usage {
            input_tokens: 10,
            output_tokens: 5,
        }),
        finish_reason: Some("stop".into()),
    }]);

    let manager = RunManager::new(test_config(), test_agent_tree(vec![]), ToolRegistry::new());
    let result = manager
        .execute(&provider, "hello", None)
        .await
        .expect("run should succeed");

    assert_eq!(result.status, RunStatus::Completed);
    assert_eq!(result.total_steps, 1);
    assert_eq!(result.messages.len(), 2); // user + assistant
    assert_eq!(result.messages[1].content, "Hello from the model!");
    assert_eq!(result.total_input_tokens, 10);
    assert_eq!(result.total_output_tokens, 5);
}

/// Multi-turn with a real async tool: model requests write_file, tool writes
/// the file, model produces final answer.  This exercises the full async tool
/// execution path (C1 fix — no nested runtime).
#[tokio::test]
async fn integration_run_with_real_tool() {
    let workspace = tempfile::TempDir::new().unwrap();
    let (registry, _mcp_services) = builtin_mcp_registry(workspace.path()).await;

    let provider = ScriptedProvider::new(vec![
        // Step 1: model calls write_file
        ModelResponse {
            content: None,
            tool_calls: vec![ToolCall {
                id: ToolCallId("tc-1".into()),
                name: "write_file".into(),
                arguments: serde_json::json!({
                    "path": "output.txt",
                    "content": "integration test content"
                }),
            }],
            usage: Some(Usage {
                input_tokens: 50,
                output_tokens: 20,
            }),
            finish_reason: Some("tool_calls".into()),
        },
        // Step 2: model returns final text
        ModelResponse {
            content: Some("File written successfully!".into()),
            tool_calls: vec![],
            usage: Some(Usage {
                input_tokens: 80,
                output_tokens: 10,
            }),
            finish_reason: Some("stop".into()),
        },
    ]);

    let manager = RunManager::new(
        test_config(),
        test_agent_tree(vec![
            "write_file".into(),
            "read_file".into(),
        ]),
        registry,
    );
    let result = manager
        .execute(&provider, "write a file", None)
        .await
        .expect("run should succeed");

    assert_eq!(result.status, RunStatus::Completed);
    assert_eq!(result.total_steps, 2);
    // user + assistant(tool_call) + tool + assistant(final)
    assert_eq!(result.messages.len(), 4);
    assert_eq!(result.messages[2].role, MessageRole::Tool);

    // Verify the file was actually written
    let written = std::fs::read_to_string(workspace.path().join("output.txt")).unwrap();
    assert_eq!(written, "integration test content");
}

/// Verify read_file tool works through the full pipeline.
#[tokio::test]
async fn integration_run_with_read_file_tool() {
    let workspace = tempfile::TempDir::new().unwrap();
    std::fs::write(workspace.path().join("data.txt"), "secret data").unwrap();

    let (registry, _mcp_services) = builtin_mcp_registry(workspace.path()).await;

    let provider = ScriptedProvider::new(vec![
        ModelResponse {
            content: None,
            tool_calls: vec![ToolCall {
                id: ToolCallId("tc-read".into()),
                name: "read_file".into(),
                arguments: serde_json::json!({"path": "data.txt"}),
            }],
            usage: None,
            finish_reason: Some("tool_calls".into()),
        },
        ModelResponse {
            content: Some("I read the data".into()),
            tool_calls: vec![],
            usage: None,
            finish_reason: Some("stop".into()),
        },
    ]);

    let manager = RunManager::new(
        test_config(),
        test_agent_tree(vec!["read_file".into()]),
        registry,
    );
    let result = manager
        .execute(&provider, "read the file", None)
        .await
        .expect("run should succeed");

    assert_eq!(result.status, RunStatus::Completed);
    // The tool output should contain the file content
    assert_eq!(result.messages[2].role, MessageRole::Tool);
    assert!(result.messages[2].content.contains("secret data"));
}

/// Verify that workspace confinement (C2) is enforced through the tool registry:
/// a model requesting to read /etc/hostname should get an error, not the file contents.
#[tokio::test]
async fn integration_tool_rejects_path_outside_workspace() {
    let workspace = tempfile::TempDir::new().unwrap();
    let (registry, _mcp_services) = builtin_mcp_registry(workspace.path()).await;

    let provider = ScriptedProvider::new(vec![
        ModelResponse {
            content: None,
            tool_calls: vec![ToolCall {
                id: ToolCallId("tc-evil".into()),
                name: "read_file".into(),
                arguments: serde_json::json!({"path": "/etc/hostname"}),
            }],
            usage: None,
            finish_reason: Some("tool_calls".into()),
        },
        ModelResponse {
            content: Some("ok".into()),
            tool_calls: vec![],
            usage: None,
            finish_reason: Some("stop".into()),
        },
    ]);

    let manager = RunManager::new(
        test_config(),
        test_agent_tree(vec!["read_file".into()]),
        registry,
    );
    let result = manager
        .execute(&provider, "read /etc/hostname", None)
        .await
        .expect("run should succeed");

    assert_eq!(result.status, RunStatus::Completed);
    // The tool message must carry the fs server's sandbox rejection text —
    // asserted on the stable message, not a broad "Error" disjunction, so a
    // regression that swaps the is_error-with-content path for an adapter Err
    // (or vice versa) cannot pass by accident.
    let tool_output = &result.messages[2].content;
    assert!(
        tool_output.contains("outside workspace"),
        "expected sandbox rejection text, got: {tool_output}"
    );
}

/// Verify that path traversal is blocked through the registry.
#[tokio::test]
async fn integration_tool_rejects_path_traversal() {
    let workspace = tempfile::TempDir::new().unwrap();
    let (registry, _mcp_services) = builtin_mcp_registry(workspace.path()).await;

    // Unique outside-workspace target derived from the workspace's own random
    // temp name, so a leftover file from a previous run can never make the
    // non-creation assertion flaky.
    let unique = format!(
        "evil-{}.txt",
        workspace.path().file_name().unwrap().to_string_lossy()
    );
    let outside_target = workspace.path().parent().unwrap().join(&unique);
    assert!(
        !outside_target.exists(),
        "precondition: target must not pre-exist"
    );

    let provider = ScriptedProvider::new(vec![
        ModelResponse {
            content: None,
            tool_calls: vec![ToolCall {
                id: ToolCallId("tc-trav".into()),
                name: "write_file".into(),
                arguments: serde_json::json!({
                    "path": format!("../{unique}"),
                    "content": "pwned"
                }),
            }],
            usage: None,
            finish_reason: Some("tool_calls".into()),
        },
        ModelResponse {
            content: Some("ok".into()),
            tool_calls: vec![],
            usage: None,
            finish_reason: Some("stop".into()),
        },
    ]);

    let manager = RunManager::new(
        test_config(),
        test_agent_tree(vec!["write_file".into()]),
        registry,
    );
    let result = manager
        .execute(&provider, "write outside workspace", None)
        .await
        .expect("run should succeed");

    assert_eq!(result.status, RunStatus::Completed);
    // Stable rejection text from the fs server (see the outside-workspace
    // test above for why the assertion is exact).
    let tool_output = &result.messages[2].content;
    assert!(
        tool_output.contains("traversal"),
        "expected path traversal error, got: {tool_output}"
    );

    // Ensure the file was NOT created outside workspace
    assert!(
        !outside_target.exists(),
        "sandbox escape: {} was created",
        outside_target.display()
    );
}

/// Verify `limits.max_output_bytes` caps tool output end to end: the Tool
/// message that enters the conversation (and therefore the second LLM request,
/// which is built from these messages) carries the truncation marker defined
/// by `limit_tool_output` instead of the full 10 KB payload.
#[tokio::test]
async fn integration_tool_output_capped_by_max_output_bytes() {
    let mut registry = ToolRegistry::new();
    registry.register(HugeOutputTool);

    let provider = ScriptedProvider::new(vec![
        ModelResponse {
            content: None,
            tool_calls: vec![ToolCall {
                id: ToolCallId("tc-huge".into()),
                name: "huge".into(),
                arguments: serde_json::json!({}),
            }],
            usage: None,
            finish_reason: Some("tool_calls".into()),
        },
        ModelResponse {
            content: Some("got it".into()),
            tool_calls: vec![],
            usage: None,
            finish_reason: Some("stop".into()),
        },
    ]);

    let manager = RunManager::new(
        test_config_small_output_cap(),
        test_agent_tree(vec!["huge".into()]),
        registry,
    );
    let result = manager
        .execute(&provider, "call the huge tool", None)
        .await
        .expect("run should succeed");

    assert_eq!(result.status, RunStatus::Completed);
    assert_eq!(result.total_steps, 2);

    let tool_msg = result
        .messages
        .iter()
        .find(|m| m.role == MessageRole::Tool)
        .expect("tool message should be present");
    assert!(
        tool_msg
            .content
            .contains("[TRUNCATED: original 10000 bytes"),
        "expected truncation marker, got: {}",
        tool_msg.content
    );
    assert!(
        tool_msg.content.len() < 2_000,
        "tool message must stay far below the original 10000 bytes, got {}",
        tool_msg.content.len()
    );
}

/// Verify execution tree is properly built and root is marked Completed.
#[tokio::test]
async fn integration_execution_tree_built() {
    let provider = ScriptedProvider::new(vec![ModelResponse {
        content: Some("done".into()),
        tool_calls: vec![],
        usage: None,
        finish_reason: Some("stop".into()),
    }]);

    let manager = RunManager::new(test_config(), test_agent_tree(vec![]), ToolRegistry::new());
    let result = manager
        .execute(&provider, "test", None)
        .await
        .expect("run should succeed");

    let root = result.execution_tree.root();
    assert_eq!(root.agent_id.0, "root");
    assert_eq!(root.depth, 0);
    assert_eq!(root.status, openslate_core::execution::ExecutionStatus::Completed);
}

/// Verify model is correctly resolved from config.
#[tokio::test]
async fn integration_model_resolved() {
    let provider = ScriptedProvider::new(vec![ModelResponse {
        content: Some("ok".into()),
        tool_calls: vec![],
        usage: None,
        finish_reason: Some("stop".into()),
    }]);

    let manager = RunManager::new(test_config(), test_agent_tree(vec![]), ToolRegistry::new());
    let result = manager
        .execute(&provider, "test", None)
        .await
        .expect("run should succeed");

    assert_eq!(result.model, "mock-model");
}

// ─── Multi-agent delegation (subagent / call_agent) ──────────────────────

fn two_agent_tree() -> AgentTree {
    let agents = vec![
        AgentConfig {
            id: AgentId("root".into()),
            name: "Root".into(),
            model: "main".into(),
            children: vec![AgentId("child".into())],
            tools: vec![],
            default_prompt: "You coordinate and delegate.".into(),
        },
        AgentConfig {
            id: AgentId("child".into()),
            name: "Child".into(),
            model: "main".into(),
            children: vec![],
            tools: vec![],
            default_prompt: "You perform sub-tasks.".into(),
        },
    ];
    AgentTree::from_configs(&agents).expect("two-agent tree should build")
}

/// End-to-end: root agent delegates to a child agent via `call_agent`, the
/// child runs to completion, and its answer flows back through the parent.
/// This exercises the full RunManager -> AgentRunner -> execute_run ->
/// interception -> recursion path through the public API.
#[tokio::test]
async fn integration_delegates_root_to_child() {
    let provider = ScriptedProvider::new(vec![
        // root step 1: delegate
        ModelResponse {
            content: None,
            tool_calls: vec![ToolCall {
                id: ToolCallId("ca-1".into()),
                name: "call_agent".into(),
                arguments: serde_json::json!({"agent_id": "child", "task": "compute 2+2"}),
            }],
            usage: Some(Usage { input_tokens: 10, output_tokens: 5 }),
            finish_reason: Some("tool_calls".into()),
        },
        // child step 1: answer
        ModelResponse {
            content: Some("4".into()),
            tool_calls: vec![],
            usage: Some(Usage { input_tokens: 20, output_tokens: 3 }),
            finish_reason: Some("stop".into()),
        },
        // root step 2: final summary using the child's reply
        ModelResponse {
            content: Some("The answer is 4".into()),
            tool_calls: vec![],
            usage: Some(Usage { input_tokens: 30, output_tokens: 8 }),
            finish_reason: Some("stop".into()),
        },
    ]);

    let manager = RunManager::new(test_config(), two_agent_tree(), ToolRegistry::new());
    let result = manager
        .execute(&provider, "compute via child", None)
        .await
        .expect("run should succeed");

    assert_eq!(result.status, RunStatus::Completed);
    // Execution tree grew a child node at depth 1.
    assert_eq!(result.execution_tree.node_count(), 2);
    // The child's answer surfaced back to the root as a tool message.
    let tool_msg = result
        .messages
        .iter()
        .find(|m| m.role == MessageRole::Tool)
        .expect("a call_agent tool message should be present");
    assert!(
        tool_msg.content.contains("4"),
        "child answer should reach root, got: {}",
        tool_msg.content
    );
    // Tokens aggregated across both layers: 10+20+30 in, 5+3+8 out.
    assert_eq!(result.total_input_tokens, 60);
    assert_eq!(result.total_output_tokens, 16);
}
