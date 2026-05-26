//! Agent runtime loop.
//!
//! Executes a single agent: sends messages to the model, processes the response
//! (text or tool_calls), and loops until completion or limits are hit.
//! This is a SEQUENTIAL loop for v0.1-v0.4 (parallel execution comes later).

use crate::error::OpenSlateError;
use crate::provider::{GenerateRequest, ModelProvider};
use crate::types::*;

/// Configuration for a single agent run.
#[derive(Debug, Clone)]
pub struct RunConfig {
    pub run_id: RunId,
    pub agent_id: AgentId,
    pub model_alias: String,
    pub system_prompt: Option<String>,
    pub initial_messages: Vec<Message>,
    pub max_steps: u32,
    pub max_context_bytes: u32,
    pub max_output_bytes: u32,
}

/// Result of a completed agent run.
#[derive(Debug, Clone)]
pub struct RunResult {
    pub run_id: RunId,
    pub status: RunStatus,
    pub messages: Vec<Message>,
    pub total_steps: u32,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
}

/// A single step in the execution loop.
#[derive(Debug, Clone)]
pub struct StepResult {
    pub step_number: u32,
    pub model_response: ModelResponse,
    pub tool_outputs: Vec<ToolOutput>,
}

/// Execute an agent run to completion.
///
/// The loop:
/// 1. Build `GenerateRequest` from conversation history
/// 2. Call `ModelProvider::generate()`
/// 3. If response has content -> add assistant message, check for `tool_calls`
/// 4. If `tool_calls` -> execute each tool (via callback), add tool results as messages
/// 5. If no `tool_calls` and content present -> done (`Completed`)
/// 6. If `max_steps` reached -> `Interrupted`
/// 7. If `finish_reason` is `"stop"` -> `Completed`
///
/// Returns `RunResult` with final status and full conversation.
pub async fn execute_run(
    provider: &dyn ModelProvider,
    config: RunConfig,
    model_id: &str,
    tool_executor: &(dyn Fn(&str, &serde_json::Value) -> ToolOutput + Send + Sync),
) -> Result<RunResult, OpenSlateError> {
    let mut messages = config.initial_messages.clone();
    let mut total_steps = 0u32;
    let mut total_input_tokens = 0u64;
    let mut total_output_tokens = 0u64;

    loop {
        // Check step limit
        if total_steps >= config.max_steps {
            return Ok(RunResult {
                run_id: config.run_id,
                status: RunStatus::Interrupted,
                messages,
                total_steps,
                total_input_tokens,
                total_output_tokens,
            });
        }

        // Build request
        let request = GenerateRequest {
            model_id: model_id.to_owned(),
            system_prompt: config.system_prompt.clone(),
            messages: messages.clone(),
            tools: vec![], // Tool definitions will be added in Task 15
            max_tokens: None,
            temperature: None,
        };

        // Call model
        let response = provider.generate(request).await?;

        // Track usage
        if let Some(usage) = &response.usage {
            total_input_tokens += usage.input_tokens as u64;
            total_output_tokens += usage.output_tokens as u64;
        }

        total_steps += 1;

        // Add assistant message
        let assistant_content = response.content.clone().unwrap_or_default();
        messages.push(Message {
            role: MessageRole::Assistant,
            content: assistant_content,
            tool_call_id: None,
            name: None,
        });

        // Process tool calls
        if !response.tool_calls.is_empty() {
            for tc in &response.tool_calls {
                let output = tool_executor(&tc.name, &tc.arguments);
                messages.push(Message {
                    role: MessageRole::Tool,
                    content: output.content,
                    tool_call_id: Some(tc.id.clone()),
                    name: Some(tc.name.clone()),
                });
            }
            // Continue loop to send tool results back to model
            continue;
        }

        // No tool calls — check if we're done
        let is_done = response.finish_reason.as_deref() == Some("stop")
            || response.finish_reason.as_deref() == Some("end_turn");

        if is_done || response.content.is_some() {
            return Ok(RunResult {
                run_id: config.run_id,
                status: RunStatus::Completed,
                messages,
                total_steps,
                total_input_tokens,
                total_output_tokens,
            });
        }

        // No content and no tool calls — failed
        return Ok(RunResult {
            run_id: config.run_id,
            status: RunStatus::Failed,
            messages,
            total_steps,
            total_input_tokens,
            total_output_tokens,
        });
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
        async fn generate(&self, _request: GenerateRequest) -> Result<ModelResponse, ProviderError> {
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

    fn mock_tool_executor(name: &str, args: &serde_json::Value) -> ToolOutput {
        ToolOutput {
            content: format!("executed {name} with {args:?}"),
            bytes: 20,
            duration_ms: 10,
            status: ToolOutputStatus::Success,
        }
    }

    fn default_config() -> RunConfig {
        RunConfig {
            run_id: RunId("test-run".into()),
            agent_id: AgentId("test-agent".into()),
            model_alias: "test-model".into(),
            system_prompt: None,
            initial_messages: vec![Message {
                role: MessageRole::User,
                content: "hello".into(),
                tool_call_id: None,
                name: None,
            }],
            max_steps: 10,
            max_context_bytes: 100_000,
            max_output_bytes: 10_000,
        }
    }

    // -- Tests --

    #[tokio::test]
    async fn test_simple_single_turn() {
        let provider = MockProvider::new(vec![ModelResponse {
            content: Some("Hello!".into()),
            tool_calls: vec![],
            usage: None,
            finish_reason: Some("stop".into()),
        }]);

        let result = execute_run(&provider, default_config(), "m1", &mock_tool_executor)
            .await
            .expect("run should succeed");

        assert_eq!(result.status, RunStatus::Completed);
        assert_eq!(result.total_steps, 1);
        assert_eq!(result.run_id, RunId("test-run".into()));

        // 1 user + 1 assistant
        assert_eq!(result.messages.len(), 2);
        assert_eq!(result.messages[1].role, MessageRole::Assistant);
        assert_eq!(result.messages[1].content, "Hello!");
    }

    #[tokio::test]
    async fn test_multi_turn_with_tools() {
        let provider = MockProvider::new(vec![
            // Step 1: model requests a tool call
            ModelResponse {
                content: Some("Let me check that.".into()),
                tool_calls: vec![ToolCall {
                    id: ToolCallId("tc-1".into()),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command": "ls"}),
                }],
                usage: None,
                finish_reason: Some("tool_calls".into()),
            },
            // Step 2: model returns final text
            ModelResponse {
                content: Some("Here are the files.".into()),
                tool_calls: vec![],
                usage: None,
                finish_reason: Some("stop".into()),
            },
        ]);

        let result = execute_run(&provider, default_config(), "m1", &mock_tool_executor)
            .await
            .expect("run should succeed");

        assert_eq!(result.status, RunStatus::Completed);
        assert_eq!(result.total_steps, 2);

        // 1 user + 1 assistant(1st) + 1 tool + 1 assistant(2nd)
        assert_eq!(result.messages.len(), 4);
        assert_eq!(result.messages[2].role, MessageRole::Tool);
        assert_eq!(result.messages[2].tool_call_id, Some(ToolCallId("tc-1".into())));
        assert_eq!(result.messages[3].role, MessageRole::Assistant);
        assert_eq!(result.messages[3].content, "Here are the files.");
    }

    #[tokio::test]
    async fn test_max_steps_interrupted() {
        // Provider always returns tool_calls — never terminates naturally
        let provider = MockProvider::new(vec![
            ModelResponse {
                content: None,
                tool_calls: vec![ToolCall {
                    id: ToolCallId("tc-1".into()),
                    name: "loop".into(),
                    arguments: serde_json::json!({}),
                }],
                usage: None,
                finish_reason: Some("tool_calls".into()),
            };
            5 // more than enough
        ]);

        let mut config = default_config();
        config.max_steps = 2;

        let result = execute_run(&provider, config, "m1", &mock_tool_executor)
            .await
            .expect("run should succeed");

        assert_eq!(result.status, RunStatus::Interrupted);
        assert_eq!(result.total_steps, 2);
    }

    #[tokio::test]
    async fn test_provider_error() {
        struct ErrorProvider;

        #[async_trait::async_trait]
        impl ModelProvider for ErrorProvider {
            async fn generate(
                &self,
                _request: GenerateRequest,
            ) -> Result<ModelResponse, ProviderError> {
                Err(ProviderError::ServerError(503))
            }

            fn provider_name(&self) -> &str {
                "error"
            }
        }

        let result = execute_run(&ErrorProvider, default_config(), "m1", &mock_tool_executor).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, OpenSlateError::Provider(ProviderError::ServerError(503))),
            "expected ProviderError::ServerError(503), got {err:?}"
        );
    }

    #[tokio::test]
    async fn test_usage_tracking() {
        let provider = MockProvider::new(vec![
            // Step 1: tool call
            ModelResponse {
                content: None,
                tool_calls: vec![ToolCall {
                    id: ToolCallId("tc-1".into()),
                    name: "calc".into(),
                    arguments: serde_json::json!({}),
                }],
                usage: Some(Usage {
                    input_tokens: 100,
                    output_tokens: 20,
                }),
                finish_reason: Some("tool_calls".into()),
            },
            // Step 2: final answer
            ModelResponse {
                content: Some("42".into()),
                tool_calls: vec![],
                usage: Some(Usage {
                    input_tokens: 150,
                    output_tokens: 5,
                }),
                finish_reason: Some("stop".into()),
            },
        ]);

        let result = execute_run(&provider, default_config(), "m1", &mock_tool_executor)
            .await
            .expect("run should succeed");

        assert_eq!(result.total_input_tokens, 250); // 100 + 150
        assert_eq!(result.total_output_tokens, 25); // 20 + 5
        assert_eq!(result.total_steps, 2);
    }

    #[tokio::test]
    async fn test_empty_response_fails() {
        let provider = MockProvider::new(vec![ModelResponse {
            content: None,
            tool_calls: vec![],
            usage: None,
            finish_reason: None,
        }]);

        let result = execute_run(&provider, default_config(), "m1", &mock_tool_executor)
            .await
            .expect("run should succeed");

        assert_eq!(result.status, RunStatus::Failed);
        assert_eq!(result.total_steps, 1);
    }
}
