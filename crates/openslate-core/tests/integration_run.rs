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
use openslate_core::error::ProviderError;
use openslate_core::provider::{GenerateRequest, ModelProvider};
use openslate_core::run_manager::RunManager;
use openslate_core::tool::{builtin_registry_with_workspace, ToolRegistry};
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
kind = "openai_compatible"
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
    let registry = builtin_registry_with_workspace(workspace.path().to_path_buf());

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
            "list_dir".into(),
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

    let registry = builtin_registry_with_workspace(workspace.path().to_path_buf());

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
/// a model requesting to read /etc/passwd should get an error, not the file contents.
#[tokio::test]
async fn integration_tool_rejects_path_outside_workspace() {
    let workspace = tempfile::TempDir::new().unwrap();
    let registry = builtin_registry_with_workspace(workspace.path().to_path_buf());

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
    // The tool output should contain an error, not the hostname
    let tool_output = &result.messages[2].content;
    assert!(
        tool_output.contains("Error") || tool_output.contains("outside workspace"),
        "expected security error in tool output, got: {tool_output}"
    );
}

/// Verify that path traversal is blocked through the registry.
#[tokio::test]
async fn integration_tool_rejects_path_traversal() {
    let workspace = tempfile::TempDir::new().unwrap();
    let registry = builtin_registry_with_workspace(workspace.path().to_path_buf());

    let provider = ScriptedProvider::new(vec![
        ModelResponse {
            content: None,
            tool_calls: vec![ToolCall {
                id: ToolCallId("tc-trav".into()),
                name: "write_file".into(),
                arguments: serde_json::json!({
                    "path": "../../../tmp/evil.txt",
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
    let tool_output = &result.messages[2].content;
    assert!(
        tool_output.contains("traversal") || tool_output.contains("Error"),
        "expected path traversal error, got: {tool_output}"
    );

    // Ensure the file was NOT created outside workspace
    assert!(!std::path::Path::new("/tmp/evil.txt").exists());
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
