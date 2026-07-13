//! Agent runtime loop.
//!
//! Executes a single agent: sends messages to the model, processes the response
//! (text or tool_calls), and loops until completion or limits are hit.
//! This is a SEQUENTIAL loop for v0.1-v0.4 (parallel execution comes later).

use std::panic::AssertUnwindSafe;
use std::time::{Duration, Instant};

use futures_util::FutureExt;

use crate::error::{OpenSlateError, ProviderError, RuntimeError};
use crate::provider::{GenerateRequest, ModelProvider, ProgressCallback};
use crate::tool::ToolExecutor;
use crate::types::*;

/// Default maximum number of consecutive empty turns before failing.
pub const DEFAULT_MAX_EMPTY_TURNS: u32 = 3;

/// Runtime limits for a single agent run.
#[derive(Debug, Clone)]
pub struct RuntimeLimits {
    pub max_steps: u32,
    pub max_depth: u32,
    pub max_tool_calls: u32,
    pub max_child_agent_calls: u32,
    pub timeout_ms: u64,
    pub max_context_bytes: u32,
    pub max_output_bytes: u32,
    pub max_empty_turns: u32,
}

impl Default for RuntimeLimits {
    fn default() -> Self {
        Self {
            max_steps: 0,
            max_depth: 4,
            max_tool_calls: 20,
            max_child_agent_calls: 8,
            timeout_ms: 60_000,
            max_context_bytes: 64_000,
            max_output_bytes: 65_536,
            max_empty_turns: DEFAULT_MAX_EMPTY_TURNS,
        }
    }
}

/// Check if runtime limits are exceeded. Returns `Ok(())` if within limits.
pub fn check_limits(
    limits: &RuntimeLimits,
    current_steps: u32,
    current_depth: u32,
    current_tool_calls: u32,
    current_child_calls: u32,
) -> Result<(), RuntimeError> {
    if limits.max_steps > 0 && current_steps >= limits.max_steps {
        return Err(RuntimeError::MaxStepsExceeded {
            max: limits.max_steps,
        });
    }
    if current_depth >= limits.max_depth {
        return Err(RuntimeError::MaxDepthExceeded {
            max: limits.max_depth,
        });
    }
    if current_tool_calls >= limits.max_tool_calls {
        return Err(RuntimeError::MaxToolCallsExceeded {
            max: limits.max_tool_calls,
        });
    }
    if current_child_calls >= limits.max_child_agent_calls {
        return Err(RuntimeError::MaxChildAgentCallsExceeded {
            max: limits.max_child_agent_calls,
        });
    }
    Ok(())
}

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
    pub max_empty_turns: u32,
    pub tool_definitions: Vec<crate::provider::ToolDefinition>,
    /// Wall-clock timeout for the entire run in milliseconds.
    /// If exceeded, the run returns `RuntimeError::Timeout`.
    pub timeout_ms: u64,
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
/// Edge cases handled:
/// - Empty model response (no content, no tool_calls) counts toward max empty turns
/// - Unknown tool names produce a clear error message in tool output
/// - Malformed tool arguments (non-object) produce an error message in tool output
/// - Context exceeding max bytes is truncated to continue
/// - Tool execution panics are caught and reported as errors
/// - Multiple consecutive empty responses stop after max_empty_turns
///
/// Returns `RunResult` with final status and full conversation.
pub async fn execute_run(
    provider: &dyn ModelProvider,
    config: RunConfig,
    model_id: &str,
    tool_executor: &dyn ToolExecutor,
    mut progress: Option<&mut dyn ProgressCallback>,
) -> Result<RunResult, OpenSlateError> {
    let mut messages = config.initial_messages.clone();
    let mut total_steps = 0u32;
    let mut total_input_tokens = 0u64;
    let mut total_output_tokens = 0u64;
    let mut consecutive_empty_turns = 0u32;

    let deadline = tokio::time::Instant::now() + Duration::from_millis(config.timeout_ms);

    loop {
        if config.max_steps > 0 && total_steps >= config.max_steps {
            return Ok(RunResult {
                run_id: config.run_id.clone(),
                status: RunStatus::Interrupted,
                messages,
                total_steps,
                total_input_tokens,
                total_output_tokens,
            });
        }

        // Check remaining time budget before each model call.
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(OpenSlateError::Runtime(RuntimeError::Timeout {
                timeout_ms: config.timeout_ms,
            }));
        }
        let remaining = deadline - now;

        truncate_context_if_needed(
            &mut messages,
            config.max_context_bytes,
        );

        let request = GenerateRequest {
            model_id: model_id.to_owned(),
            system_prompt: config.system_prompt.clone(),
            messages: messages.clone(),
            tools: config.tool_definitions.clone(),
            max_tokens: None,
            temperature: None,
        };

        let response = if let Some(cb) = progress.as_mut() {
            // --- Streaming path with progress callbacks ---
            cb.on_request_start(total_steps + 1, model_id);
            // Early input-token estimate so the UI can show ↑N during streaming,
            // before the provider's real usage arrives at stream end.
            cb.on_input_estimate(estimate_input_tokens(&request));

            let mut rx = provider.generate_stream(request).await;
            let mut assembled: Option<ModelResponse> = None;

            let timeout_result = tokio::time::timeout(remaining, async {
                let mut first_token = true;
                while let Some(event) = rx.recv().await {
                    match event {
                        Ok(ModelStreamEvent::Delta(text)) => {
                            if first_token {
                                first_token = false;
                                cb.on_first_token();
                            }
                            cb.on_delta(&text);
                        }
                        Ok(ModelStreamEvent::Reasoning(text)) => {
                            cb.on_reasoning(&text);
                        }
                        Ok(ModelStreamEvent::Usage(usage)) => {
                            cb.on_usage(usage);
                        }
                        Ok(ModelStreamEvent::Done(resp)) => {
                            assembled = Some(resp);
                        }
                        Err(e) => return Err(e),
                    }
                }
                Ok::<_, ProviderError>(())
            })
            .await;

            cb.on_request_end();

            match timeout_result {
                Ok(Ok(())) => assembled.unwrap_or(ModelResponse {
                    content: None,
                    tool_calls: vec![],
                    usage: None,
                    finish_reason: None,
                }),
                Ok(Err(e)) => return Err(OpenSlateError::from(e)),
                Err(_) => {
                    return Err(OpenSlateError::Runtime(RuntimeError::Timeout {
                        timeout_ms: config.timeout_ms,
                    }))
                }
            }
        } else {
            // --- Non-streaming path with tracing logs ---
            if config.max_steps > 0 {
                tracing::info!(
                    "Step {}/{}: requesting {}...",
                    total_steps + 1,
                    config.max_steps,
                    model_id
                );
            } else {
                tracing::info!("Step {}: requesting {}...", total_steps + 1, model_id);
            }

            let call_start = Instant::now();
            let resp = tokio::time::timeout(remaining, provider.generate(request))
                .await
                .map_err(|_| {
                    OpenSlateError::Runtime(RuntimeError::Timeout {
                        timeout_ms: config.timeout_ms,
                    })
                })??;
            let call_elapsed = call_start.elapsed();

            if let Some(usage) = &resp.usage {
                tracing::info!(
                    "  [{}ms · {}in/{}out]",
                    call_elapsed.as_millis(),
                    usage.input_tokens,
                    usage.output_tokens
                );
            } else {
                tracing::info!("  [{}ms]", call_elapsed.as_millis());
            }
            resp
        };

        if let Some(usage) = &response.usage {
            total_input_tokens += usage.input_tokens as u64;
            total_output_tokens += usage.output_tokens as u64;
        }

        total_steps += 1;

        let has_tool_calls = !response.tool_calls.is_empty();
        let assistant_content = if has_tool_calls {
            String::new()
        } else {
            response.content.clone().unwrap_or_default()
        };
        let has_content = !assistant_content.is_empty();

        messages.push(Message {
            role: MessageRole::Assistant,
            content: assistant_content,
            tool_call_id: None,
            name: None,
            tool_calls: if has_tool_calls {
                Some(response.tool_calls.clone())
            } else {
                None
            },
        });

        if has_tool_calls {
            consecutive_empty_turns = 0;
            for tc in &response.tool_calls {
                let args_str = tc.arguments.to_string();
                let args_display = truncate_str(&args_str, 120);

                if let Some(cb) = progress.as_mut() {
                    cb.on_tool_start(&tc.name, args_display);
                } else {
                    tracing::info!("  -> {}({})", tc.name, args_display);
                }

                validate_tool_arguments(&tc.arguments)?;

                let output = execute_tool_safely(
                    tool_executor,
                    &tc.name,
                    &tc.arguments,
                )
                .await;

                let truncated = output.bytes > 80;
                if let Some(cb) = progress.as_mut() {
                    cb.on_tool_end(&tc.name, output.bytes, truncated);
                } else {
                    let result_preview = truncate_str(&output.content, 80);
                    tracing::info!(
                        "  <- {} [{} bytes] {}",
                        tc.name,
                        output.bytes,
                        if truncated { format!("\"{}\"...", result_preview) } else { format!("\"{}\"", result_preview) }
                    );
                }

                messages.push(Message {
                    role: MessageRole::Tool,
                    content: output.content,
                    tool_call_id: Some(tc.id.clone()),
                    name: Some(tc.name.clone()),
                    tool_calls: None,
                });
            }
            continue;
        }

        if has_content {
            return Ok(RunResult {
                run_id: config.run_id.clone(),
                status: RunStatus::Completed,
                messages,
                total_steps,
                total_input_tokens,
                total_output_tokens,
            });
        }

        let is_done = response.finish_reason.as_deref() == Some("stop")
            || response.finish_reason.as_deref() == Some("end_turn");

        if is_done {
            return Ok(RunResult {
                run_id: config.run_id.clone(),
                status: RunStatus::Completed,
                messages,
                total_steps,
                total_input_tokens,
                total_output_tokens,
            });
        }

        consecutive_empty_turns += 1;
        if consecutive_empty_turns >= config.max_empty_turns {
            return Err(OpenSlateError::Runtime(RuntimeError::MaxEmptyTurnsExceeded {
                count: consecutive_empty_turns,
                step: total_steps,
                agent_id: config.agent_id.0.clone(),
                model_alias: config.model_alias.clone(),
            }));
        }
    }
}

/// Truncate a string to at most `max_chars` characters, appending "..." if truncated.
fn truncate_str(s: &str, max_chars: usize) -> &str {
    if s.len() <= max_chars {
        return s;
    }
    let mut end = max_chars;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Rough estimate of input tokens for a request (~4 chars/token), so the UI can
/// show an `↑N` hint during streaming before the provider's real usage arrives.
fn estimate_input_tokens(req: &GenerateRequest) -> u32 {
    let mut chars: usize = req.system_prompt.as_ref().map_or(0, String::len);
    for m in &req.messages {
        chars += m.content.len();
        if let Some(name) = m.name.as_ref() {
            chars += name.len();
        }
        if let Some(tcs) = m.tool_calls.as_ref() {
            for tc in tcs {
                chars += tc.name.len() + tc.arguments.to_string().len();
            }
        }
    }
    for t in &req.tools {
        chars += t.name.len() + t.description.len() + t.parameters.to_string().len();
    }
    (chars as u32).max(1) / 4
}

/// Validate that tool call arguments are a JSON object. Returns `Ok(())` if valid,
/// or a `RuntimeError::ToolArgumentError` if the arguments are malformed.
pub fn validate_tool_arguments(args: &serde_json::Value) -> Result<(), RuntimeError> {
    match args {
        serde_json::Value::Object(_) => Ok(()),
        serde_json::Value::Null => Ok(()),
        other => Err(RuntimeError::ToolArgumentError {
            tool_name: String::new(),
            step: 0,
            agent_id: String::new(),
            details: format!("expected object or null, got {}", json_type_name(other)),
        }),
    }
}

/// Get a human-readable type name for a JSON value.
fn json_type_name(val: &serde_json::Value) -> &'static str {
    match val {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// Execute a tool with panic protection.
///
/// Wraps the tool executor call in `catch_unwind` to catch panics
/// and convert them into error `ToolOutput`s.
async fn execute_tool_safely(
    executor: &dyn ToolExecutor,
    name: &str,
    args: &serde_json::Value,
) -> ToolOutput {
    let name_owned = name.to_owned();
    let args_owned = args.clone();

    let result = AssertUnwindSafe(executor.execute(&name_owned, &args_owned))
        .catch_unwind()
        .await;

    match result {
        Ok(output) => output,
        Err(panic_payload) => {
            let reason = match panic_payload.downcast_ref::<&str>() {
                Some(s) => s.to_string(),
                None => match panic_payload.downcast_ref::<String>() {
                    Some(s) => s.clone(),
                    None => "unknown panic".to_string(),
                },
            };
            ToolOutput {
                content: format!("Tool '{}' panicked: {}", name, reason),
                bytes: 0,
                duration_ms: 0,
                status: ToolOutputStatus::Error,
            }
        }
    }
}

/// Estimate the byte size of the message context.
fn estimate_context_bytes(messages: &[Message]) -> usize {
    messages.iter().map(|m| m.content.len()).sum()
}

/// Truncate conversation context if it exceeds the maximum byte limit.
///
/// Preserves the first message (typically the user's input) and the most recent
/// messages. Middle messages are dropped to bring the total under the limit.
fn truncate_context_if_needed(messages: &mut Vec<Message>, max_bytes: u32) {
    let max = max_bytes as usize;
    if estimate_context_bytes(messages) <= max {
        return;
    }

    if messages.len() <= 2 {
        return;
    }

    let keep_recent = 2.min(messages.len());
    let first = messages.first().cloned();

    let mut trimmed = Vec::new();
    if let Some(first_msg) = first {
        trimmed.push(first_msg);
    }

    let notice = Message {
        role: MessageRole::System,
        content: "[Context truncated: older messages removed to stay within limit]".into(),
        tool_call_id: None,
        name: None,
        tool_calls: None,
    };
    trimmed.push(notice);

    let recent_start = messages.len().saturating_sub(keep_recent);
    for msg in messages.iter().skip(recent_start) {
        trimmed.push(msg.clone());
    }

    while estimate_context_bytes(&trimmed) > max && trimmed.len() > 3 {
        let mid = 1 + (trimmed.len() - 1) / 2;
        trimmed.remove(mid.min(trimmed.len() - 2).max(1));
    }

    *messages = trimmed;
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

    // -- Mock tool executor --

    struct MockToolExecutor;

    #[async_trait::async_trait]
    impl crate::tool::ToolExecutor for MockToolExecutor {
        async fn execute(&self, name: &str, args: &serde_json::Value) -> ToolOutput {
            ToolOutput {
                content: format!("executed {name} with {args:?}"),
                bytes: 20,
                duration_ms: 10,
                status: ToolOutputStatus::Success,
            }
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
                tool_calls: None,
            }],
            max_steps: 10,
            max_context_bytes: 100_000,
            max_output_bytes: 10_000,
            max_empty_turns: DEFAULT_MAX_EMPTY_TURNS,
            tool_definitions: vec![],
            timeout_ms: 60_000,
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

        let result = execute_run(&provider, default_config(), "m1", &MockToolExecutor, None)
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

        let result = execute_run(&provider, default_config(), "m1", &MockToolExecutor, None)
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

        let result = execute_run(&provider, config, "m1", &MockToolExecutor, None)
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

        let result = execute_run(&ErrorProvider, default_config(), "m1", &MockToolExecutor, None).await;

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

        let result = execute_run(&provider, default_config(), "m1", &MockToolExecutor, None)
            .await
            .expect("run should succeed");

        assert_eq!(result.total_input_tokens, 250); // 100 + 150
        assert_eq!(result.total_output_tokens, 25); // 20 + 5
        assert_eq!(result.total_steps, 2);
    }

    #[tokio::test]
    async fn test_empty_response_counts_toward_max_empty_turns() {
        let provider = MockProvider::new(vec![
            ModelResponse {
                content: None,
                tool_calls: vec![],
                usage: None,
                finish_reason: None,
            };
            5
        ]);

        let mut config = default_config();
        config.max_empty_turns = 3;

        let result = execute_run(&provider, config, "m1", &MockToolExecutor, None).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        match &err {
            OpenSlateError::Runtime(RuntimeError::MaxEmptyTurnsExceeded {
                count,
                step,
                agent_id,
                model_alias,
            }) => {
                assert_eq!(*count, 3);
                assert_eq!(*step, 3);
                assert_eq!(agent_id, "test-agent");
                assert_eq!(model_alias, "test-model");
            }
            other => panic!("expected MaxEmptyTurnsExceeded, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_single_empty_response_then_content() {
        let provider = MockProvider::new(vec![
            ModelResponse {
                content: None,
                tool_calls: vec![],
                usage: None,
                finish_reason: None,
            },
            ModelResponse {
                content: Some("Finally!".into()),
                tool_calls: vec![],
                usage: None,
                finish_reason: Some("stop".into()),
            },
        ]);

        let mut config = default_config();
        config.max_empty_turns = 3;

        let result = execute_run(&provider, config, "m1", &MockToolExecutor, None)
            .await
            .expect("run should succeed");

        assert_eq!(result.status, RunStatus::Completed);
        assert_eq!(result.total_steps, 2);
    }

    #[tokio::test]
    async fn test_unknown_tool_produces_error_output() {
        struct ErrorToolExecutor;

        #[async_trait::async_trait]
        impl crate::tool::ToolExecutor for ErrorToolExecutor {
            async fn execute(&self, name: &str, _args: &serde_json::Value) -> ToolOutput {
                ToolOutput {
                    content: format!("Error: tool '{}' not found", name),
                    bytes: 0,
                    duration_ms: 0,
                    status: ToolOutputStatus::Error,
                }
            }
        }

        let provider = MockProvider::new(vec![
            ModelResponse {
                content: Some("calling unknown tool".into()),
                tool_calls: vec![ToolCall {
                    id: ToolCallId("tc-1".into()),
                    name: "nonexistent_tool".into(),
                    arguments: serde_json::json!({}),
                }],
                usage: None,
                finish_reason: Some("tool_calls".into()),
            },
            ModelResponse {
                content: Some("Done after error".into()),
                tool_calls: vec![],
                usage: None,
                finish_reason: Some("stop".into()),
            },
        ]);

        let result = execute_run(&provider, default_config(), "m1", &ErrorToolExecutor, None)
            .await
            .expect("run should succeed");

        assert_eq!(result.status, RunStatus::Completed);
        assert_eq!(result.total_steps, 2);
        assert_eq!(result.messages[2].role, MessageRole::Tool);
        assert!(result.messages[2].content.contains("nonexistent_tool"));
        assert!(result.messages[2].content.contains("not found"));
    }

    #[tokio::test]
    async fn test_malformed_tool_arguments() {
        let result = validate_tool_arguments(&serde_json::json!({"key": "value"}));
        assert!(result.is_ok());

        let result = validate_tool_arguments(&serde_json::Value::Null);
        assert!(result.is_ok());

        let result = validate_tool_arguments(&serde_json::json!("not an object"));
        assert!(result.is_err());
        match result.unwrap_err() {
            RuntimeError::ToolArgumentError { details, .. } => {
                assert!(details.contains("expected object or null"));
                assert!(details.contains("string"));
            }
            other => panic!("expected ToolArgumentError, got {other:?}"),
        }

        let result = validate_tool_arguments(&serde_json::json!(42));
        assert!(result.is_err());

        let result = validate_tool_arguments(&serde_json::json!([1, 2, 3]));
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_tool_panic_caught() {
        struct PanickingExecutor;

        #[async_trait::async_trait]
        impl crate::tool::ToolExecutor for PanickingExecutor {
            async fn execute(&self, _name: &str, _args: &serde_json::Value) -> ToolOutput {
                panic!("intentional test panic");
            }
        }

        let output = execute_tool_safely(&PanickingExecutor, "test_tool", &serde_json::json!({}))
            .await;
        assert_eq!(output.status, ToolOutputStatus::Error);
        assert!(output.content.contains("test_tool"));
        assert!(output.content.contains("panicked"));
        assert!(output.content.contains("intentional test panic"));
    }

    #[tokio::test]
    async fn test_tool_panic_with_string_message() {
        struct StringPanicExecutor;

        #[async_trait::async_trait]
        impl crate::tool::ToolExecutor for StringPanicExecutor {
            async fn execute(&self, _name: &str, _args: &serde_json::Value) -> ToolOutput {
                panic!("string panic message");
            }
        }

        let output =
            execute_tool_safely(&StringPanicExecutor, "my_tool", &serde_json::json!({})).await;
        assert_eq!(output.status, ToolOutputStatus::Error);
        assert!(output.content.contains("my_tool"));
        assert!(output.content.contains("string panic message"));
    }

    #[tokio::test]
    async fn test_tool_panic_with_unknown_payload() {
        struct UnknownPanicExecutor;

        #[async_trait::async_trait]
        impl crate::tool::ToolExecutor for UnknownPanicExecutor {
            async fn execute(&self, _name: &str, _args: &serde_json::Value) -> ToolOutput {
                std::panic::panic_any(42i32); // non-string payload
            }
        }

        let output =
            execute_tool_safely(&UnknownPanicExecutor, "weird_tool", &serde_json::json!({})).await;
        assert_eq!(output.status, ToolOutputStatus::Error);
        assert!(output.content.contains("weird_tool"));
        assert!(output.content.contains("unknown panic"));
    }

    #[test]
    fn test_context_truncation() {
        let mut messages = vec![
            Message {
                role: MessageRole::User,
                content: "hello".repeat(1000),
                tool_call_id: None,
                name: None,
                tool_calls: None,
            },
            Message {
                role: MessageRole::Assistant,
                content: "response1".repeat(1000),
                tool_call_id: None,
                name: None,
                tool_calls: None,
            },
            Message {
                role: MessageRole::User,
                content: "followup".repeat(1000),
                tool_call_id: None,
                name: None,
                tool_calls: None,
            },
            Message {
                role: MessageRole::Assistant,
                content: "response2".repeat(1000),
                tool_call_id: None,
                name: None,
                tool_calls: None,
            },
            Message {
                role: MessageRole::User,
                content: "final".repeat(100),
                tool_call_id: None,
                name: None,
                tool_calls: None,
            },
        ];

        let total_before: usize = messages.iter().map(|m| m.content.len()).sum();
        assert!(total_before > 2000);

        truncate_context_if_needed(&mut messages, 2000);

        let total_after: usize = messages.iter().map(|m| m.content.len()).sum();
        assert!(total_after < total_before);
        assert!(messages.len() >= 2);
        assert_eq!(messages[0].role, MessageRole::User);
    }

    #[test]
    fn test_context_no_truncation_when_under_limit() {
        let mut messages = vec![
            Message {
                role: MessageRole::User,
                content: "hello".into(),
                tool_call_id: None,
                name: None,
                tool_calls: None,
            },
            Message {
                role: MessageRole::Assistant,
                content: "hi".into(),
                tool_call_id: None,
                name: None,
                tool_calls: None,
            },
        ];

        let original_len = messages.len();
        truncate_context_if_needed(&mut messages, 100_000);
        assert_eq!(messages.len(), original_len);
    }

    #[test]
    fn test_error_messages_include_context() {
        let err = RuntimeError::EmptyResponse {
            step: 5,
            agent_id: "my-agent".into(),
            model_alias: "gpt-4o".into(),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("step 5"));
        assert!(msg.contains("my-agent"));
        assert!(msg.contains("gpt-4o"));

        let err = RuntimeError::UnknownTool {
            tool_name: "bad_tool".into(),
            step: 3,
            agent_id: "root".into(),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("bad_tool"));
        assert!(msg.contains("step 3"));
        assert!(msg.contains("root"));

        let err = RuntimeError::ToolExecutionError {
            tool_name: "bash".into(),
            step: 7,
            agent_id: "worker".into(),
            reason: "segfault".into(),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("bash"));
        assert!(msg.contains("step 7"));
        assert!(msg.contains("worker"));
        assert!(msg.contains("segfault"));
    }

    // -- RuntimeLimits + check_limits tests --

    #[test]
    fn test_max_steps_exceeded() {
        let limits = RuntimeLimits {
            max_steps: 5,
            ..Default::default()
        };
        let err = check_limits(&limits, 5, 0, 0, 0).unwrap_err();
        assert!(
            matches!(err, RuntimeError::MaxStepsExceeded { max: 5 }),
            "expected MaxStepsExceeded, got {err:?}"
        );
    }

    #[test]
    fn test_max_steps_zero_means_unlimited() {
        let limits = RuntimeLimits {
            max_steps: 0,
            ..Default::default()
        };
        // Should NOT trigger MaxStepsExceeded even with a huge step count
        check_limits(&limits, 999_999, 0, 0, 0).expect("max_steps=0 should be unlimited");
    }

    #[test]
    fn test_max_depth_exceeded() {
        let limits = RuntimeLimits::default();
        let err = check_limits(&limits, 0, limits.max_depth, 0, 0).unwrap_err();
        assert!(
            matches!(err, RuntimeError::MaxDepthExceeded { max: 4 }),
            "expected MaxDepthExceeded, got {err:?}"
        );
    }

    #[test]
    fn test_max_tool_calls_exceeded() {
        let limits = RuntimeLimits::default();
        let err = check_limits(&limits, 0, 0, limits.max_tool_calls, 0).unwrap_err();
        assert!(
            matches!(err, RuntimeError::MaxToolCallsExceeded { max: 20 }),
            "expected MaxToolCallsExceeded, got {err:?}"
        );
    }

    #[test]
    fn test_within_limits() {
        let limits = RuntimeLimits::default();
        check_limits(&limits, 0, 0, 0, 0).expect("should be within limits");
        check_limits(&limits, 11, 3, 19, 7).expect("should be within limits (one under each)");
    }

    #[test]
    fn test_default_limits() {
        let limits = RuntimeLimits::default();
        assert_eq!(limits.max_steps, 0); // 0 = unlimited
        assert_eq!(limits.max_depth, 4);
        assert_eq!(limits.max_tool_calls, 20);
        assert_eq!(limits.max_child_agent_calls, 8);
        assert_eq!(limits.timeout_ms, 60_000);
        assert_eq!(limits.max_context_bytes, 64_000);
        assert_eq!(limits.max_output_bytes, 65_536);
        assert_eq!(limits.max_empty_turns, DEFAULT_MAX_EMPTY_TURNS);
    }

    // ── Timeout enforcement tests ──

    #[tokio::test]
    async fn test_timeout_fires_on_slow_provider() {
        struct SlowProvider;

        #[async_trait::async_trait]
        impl ModelProvider for SlowProvider {
            async fn generate(
                &self,
                _request: GenerateRequest,
            ) -> Result<ModelResponse, ProviderError> {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                Ok(ModelResponse {
                    content: Some("too slow".into()),
                    tool_calls: vec![],
                    usage: None,
                    finish_reason: Some("stop".into()),
                })
            }
            fn provider_name(&self) -> &str {
                "slow"
            }
        }

        let mut config = default_config();
        config.timeout_ms = 50; // 50 ms budget

        let result = execute_run(&SlowProvider, config, "m1", &MockToolExecutor, None).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            OpenSlateError::Runtime(RuntimeError::Timeout { timeout_ms }) => {
                assert_eq!(timeout_ms, 50);
            }
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_normal_execution_within_timeout() {
        let provider = MockProvider::new(vec![ModelResponse {
            content: Some("fast enough".into()),
            tool_calls: vec![],
            usage: None,
            finish_reason: Some("stop".into()),
        }]);

        let result = execute_run(&provider, default_config(), "m1", &MockToolExecutor, None)
            .await
            .expect("should complete well within timeout");
        assert_eq!(result.status, RunStatus::Completed);
    }

    #[tokio::test]
    async fn test_timeout_allows_multi_step_within_budget() {
        let provider = MockProvider::new(vec![
            ModelResponse {
                content: None,
                tool_calls: vec![ToolCall {
                    id: ToolCallId("tc-1".into()),
                    name: "step1".into(),
                    arguments: serde_json::json!({}),
                }],
                usage: None,
                finish_reason: Some("tool_calls".into()),
            },
            ModelResponse {
                content: Some("done".into()),
                tool_calls: vec![],
                usage: None,
                finish_reason: Some("stop".into()),
            },
        ]);

        // Generous timeout — should complete both steps.
        let mut config = default_config();
        config.timeout_ms = 5_000;

        let result = execute_run(&provider, config, "m1", &MockToolExecutor, None)
            .await
            .expect("should complete within timeout");
        assert_eq!(result.status, RunStatus::Completed);
        assert_eq!(result.total_steps, 2);
    }

    #[tokio::test]
    async fn test_timeout_zero_ms_immediately_fires() {
        let provider = MockProvider::new(vec![ModelResponse {
            content: Some("never".into()),
            tool_calls: vec![],
            usage: None,
            finish_reason: Some("stop".into()),
        }]);

        let mut config = default_config();
        config.timeout_ms = 0; // zero → immediate timeout

        let result = execute_run(&provider, config, "m1", &MockToolExecutor, None).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            OpenSlateError::Runtime(RuntimeError::Timeout { timeout_ms }) => {
                assert_eq!(timeout_ms, 0);
            }
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    // ── Async tool execution tests ──

    #[tokio::test]
    async fn test_async_tool_executes_without_nested_runtime() {
        /// A tool executor that performs real async work (sleeps).
        /// This would deadlock/fail with the old nested-runtime approach
        /// when called from within a multi-threaded runtime context.
        struct AsyncSleepExecutor;

        #[async_trait::async_trait]
        impl crate::tool::ToolExecutor for AsyncSleepExecutor {
            async fn execute(&self, name: &str, args: &serde_json::Value) -> ToolOutput {
                // Perform genuine async I/O to prove we're on a real async runtime.
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                ToolOutput {
                    content: format!("async tool {name} done, args={args}"),
                    bytes: 30,
                    duration_ms: 5,
                    status: ToolOutputStatus::Success,
                }
            }
        }

        let provider = MockProvider::new(vec![
            ModelResponse {
                content: None,
                tool_calls: vec![ToolCall {
                    id: ToolCallId("tc-1".into()),
                    name: "async_op".into(),
                    arguments: serde_json::json!({"key": "val"}),
                }],
                usage: None,
                finish_reason: Some("tool_calls".into()),
            },
            ModelResponse {
                content: Some("all done".into()),
                tool_calls: vec![],
                usage: None,
                finish_reason: Some("stop".into()),
            },
        ]);

        let result =
            execute_run(&provider, default_config(), "m1", &AsyncSleepExecutor, None)
                .await
                .expect("run should succeed");

        assert_eq!(result.status, RunStatus::Completed);
        assert_eq!(result.total_steps, 2);
        assert_eq!(result.messages[2].role, MessageRole::Tool);
        assert!(result.messages[2].content.contains("async_op"));
    }
}
