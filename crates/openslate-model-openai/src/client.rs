use std::time::Duration;

use openslate_core::error::ProviderError;
use openslate_core::provider::{GenerateRequest, ModelProvider, ToolDefinition};
use openslate_core::types::ModelResponse;

use crate::types::*;

/// Configuration for retry behavior on transient errors.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of retry attempts.
    pub max_retries: u32,
    /// Base delay in milliseconds for exponential backoff.
    pub retry_base_delay_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            retry_base_delay_ms: 1000,
        }
    }
}

#[derive(Debug, Clone)]
pub struct OpenAIProviderConfig {
    pub provider_name: String,
    pub base_url: String,
    pub api_key: String,
    pub timeout_secs: u64,
}

pub struct OpenAICompatibleProvider {
    pub(crate) config: OpenAIProviderConfig,
    pub(crate) client: reqwest::Client,
    retry_config: Option<RetryConfig>,
}

impl OpenAICompatibleProvider {
    pub fn new(config: OpenAIProviderConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .no_proxy()
            .build()
            .unwrap_or_default();
        Self {
            config,
            client,
            retry_config: None,
        }
    }

    /// Enable retry with the given configuration.
    pub fn with_retry(mut self, config: RetryConfig) -> Self {
        self.retry_config = Some(config);
        self
    }

    pub fn completions_url(&self) -> String {
        let base = self.config.base_url.trim_end_matches('/');
        format!("{}/chat/completions", base)
    }

    pub(crate) fn convert_messages(
        system_prompt: Option<&str>,
        messages: &[openslate_core::types::Message],
    ) -> Vec<ApiMessage> {
        let mut api_messages = Vec::new();
        if let Some(sys) = system_prompt {
            api_messages.push(ApiMessage::System {
                content: sys.to_owned(),
            });
        }
        for msg in messages {
            let api_msg = match msg.role {
                openslate_core::types::MessageRole::User => ApiMessage::User {
                    content: msg.content.clone(),
                },
                openslate_core::types::MessageRole::Assistant => ApiMessage::Assistant {
                    content: Some(msg.content.clone()),
                    tool_calls: msg.tool_calls.as_ref().map(|tcs| {
                        tcs.iter().map(|tc| ApiToolCall {
                            id: tc.id.0.clone(),
                            r#type: "function".to_owned(),
                            function: ApiFunctionCall {
                                name: tc.name.clone(),
                                arguments: serde_json::to_string(&tc.arguments).unwrap_or_default(),
                            },
                        }).collect()
                    }),
                },
                openslate_core::types::MessageRole::Tool => ApiMessage::Tool {
                    content: msg.content.clone(),
                    tool_call_id: msg
                        .tool_call_id
                        .as_ref()
                        .map(|id| id.0.clone())
                        .unwrap_or_default(),
                },
                openslate_core::types::MessageRole::System => ApiMessage::System {
                    content: msg.content.clone(),
                },
            };
            api_messages.push(api_msg);
        }
        api_messages
    }

    pub(crate) fn convert_tools(tools: &[ToolDefinition]) -> Vec<ApiTool> {
        tools
            .iter()
            .map(|t| ApiTool {
                r#type: "function".to_owned(),
                function: ApiFunction {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: t.parameters.clone(),
                },
            })
            .collect()
    }

    /// Execute a single API request attempt.
    async fn try_generate(
        &self,
        body: &ChatCompletionRequest,
        url: &str,
    ) -> Result<ModelResponse, ProviderError> {
        let response = self
            .client
            .post(url)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .header("Content-Type", "application/json")
            .json(body)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    ProviderError::Timeout
                } else {
                    ProviderError::ConnectionError(e.to_string())
                }
            })?;

        let status = response.status();
        let response_body = response
            .text()
            .await
            .map_err(|e| ProviderError::ConnectionError(e.to_string()))?;

        if !status.is_success() {
            let snippet = truncate_response_body(&response_body, 200);
            return Err(classify_http_error(status, &body.model, snippet));
        }

        let api_response: ChatCompletionResponse =
            serde_json::from_str(&response_body).map_err(|e| {
                ProviderError::MalformedResponse(format!("Failed to parse response: {}", e))
            })?;

        let choice = api_response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| ProviderError::MalformedResponse("No choices in response".into()))?;

        let tool_calls = choice
            .message
            .tool_calls
            .map(|calls| {
                calls
                    .into_iter()
                    .map(|tc| openslate_core::types::ToolCall {
                        id: openslate_core::types::ToolCallId(tc.id),
                        name: tc.function.name,
                        arguments: serde_json::from_str(&tc.function.arguments)
                            .unwrap_or(serde_json::Value::Null),
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let usage = api_response.usage.map(|u| openslate_core::types::Usage {
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
        });

        Ok(ModelResponse {
            content: choice.message.content,
            tool_calls,
            usage,
            finish_reason: choice.finish_reason,
        })
    }
}

/// Classify an HTTP error status into the appropriate ProviderError.
fn classify_http_error(
    status: reqwest::StatusCode,
    model_id: &str,
    body_snippet: &str,
) -> ProviderError {
    match status.as_u16() {
        429 => ProviderError::RateLimit,
        404 => ProviderError::NotFound(model_id.to_owned()),
        401 => ProviderError::AuthError(format!(
            "invalid API key (HTTP 401): {}",
            body_snippet
        )),
        403 => ProviderError::AuthError(format!(
            "forbidden (HTTP 403): {}",
            body_snippet
        )),
        s if s >= 500 => ProviderError::ServerError(s),
        _ => ProviderError::ServerError(status.as_u16()),
    }
}

/// Check if an error is transient and worth retrying.
fn is_retryable(error: &ProviderError) -> bool {
    match error {
        ProviderError::RateLimit => true,
        ProviderError::ServerError(status) => (500..=599).contains(status),
        ProviderError::Timeout => true,
        ProviderError::ConnectionError(_) => true,
        _ => false,
    }
}

/// Calculate exponential backoff delay with jitter.
///
/// `delay = base * 2^attempt + jitter`
fn calculate_backoff(base_delay_ms: u64, attempt: u32) -> Duration {
    let multiplier = 1u64.checked_shl(attempt).unwrap_or(u64::MAX);
    let base = base_delay_ms.saturating_mul(multiplier);
    // Deterministic jitter: varies by attempt, up to ~25% of base
    let jitter = if base > 0 {
        let factor = ((attempt as u64 * 31 + 17) % 4).max(1);
        (base / 4) * factor / 4
    } else {
        0
    };
    Duration::from_millis(base + jitter)
}

/// Truncate a string to at most `max_chars` bytes, respecting UTF-8 char boundaries.
fn truncate_response_body(s: &str, max_chars: usize) -> &str {
    if s.len() <= max_chars {
        return s;
    }
    let mut end = max_chars;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[async_trait::async_trait]
impl ModelProvider for OpenAICompatibleProvider {
    async fn generate(&self, request: GenerateRequest) -> Result<ModelResponse, ProviderError> {
        let api_messages =
            Self::convert_messages(request.system_prompt.as_deref(), &request.messages);
        // If any message is a tool result (role == Tool), omit tools from the request.
        // The MiniMax API may not recognize the tool_call_id if tools are re-sent
        // after the model has already selected a tool to call.
        let has_tool_result = request
            .messages
            .iter()
            .any(|m| m.role == openslate_core::types::MessageRole::Tool);
        let api_tools = if request.tools.is_empty() || has_tool_result {
            None
        } else {
            Some(Self::convert_tools(&request.tools))
        };

        let body = ChatCompletionRequest {
            model: request.model_id,
            messages: api_messages,
            tools: api_tools,
            max_tokens: request.max_tokens,
            temperature: request.temperature,
        };

        let url = self.completions_url();
        let max_retries = self.retry_config.as_ref().map_or(0, |c| c.max_retries);
        let base_delay_ms = self
            .retry_config
            .as_ref()
            .map_or(1000, |c| c.retry_base_delay_ms);

        for attempt in 0..=max_retries {
            match self.try_generate(&body, &url).await {
                Ok(response) => return Ok(response),
                Err(e) => {
                    let can_retry = attempt < max_retries && is_retryable(&e);
                    if can_retry {
                        tracing::warn!(
                            attempt = attempt + 1,
                            max_retries,
                            error = %e,
                            "retryable error, backing off before retry"
                        );
                        let delay = calculate_backoff(base_delay_ms, attempt);
                        tokio::time::sleep(delay).await;
                        continue;
                    }

                    // Final error: non-retryable or retries exhausted
                    if attempt > 0 {
                        return Err(ProviderError::ConnectionError(format!(
                            "request to {} failed after {} attempt(s), last error: {}",
                            url,
                            attempt + 1,
                            e
                        )));
                    }
                    return Err(e);
                }
            }
        }

        // Unreachable: loop always returns via the branches above
        Err(ProviderError::ConnectionError(
            "unexpected retry loop exit".into(),
        ))
    }

    fn provider_name(&self) -> &str {
        &self.config.provider_name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openslate_core::types::{Message, MessageRole, ToolCallId};

    // --- Test helpers ---

    fn make_provider(server_url: &str, timeout_secs: u64) -> OpenAICompatibleProvider {
        OpenAICompatibleProvider::new(OpenAIProviderConfig {
            provider_name: "test".into(),
            base_url: server_url.into(),
            api_key: "test-key".into(),
            timeout_secs,
        })
    }

    fn make_provider_with_retry(
        server_url: &str,
        timeout_secs: u64,
        retry: RetryConfig,
    ) -> OpenAICompatibleProvider {
        OpenAICompatibleProvider::new(OpenAIProviderConfig {
            provider_name: "test".into(),
            base_url: server_url.into(),
            api_key: "test-key".into(),
            timeout_secs,
        })
        .with_retry(retry)
    }

    fn make_request() -> GenerateRequest {
        GenerateRequest {
            model_id: "test-model".into(),
            system_prompt: Some("You are helpful.".into()),
            messages: vec![Message {
                role: MessageRole::User,
                content: "Hi".into(),
                tool_call_id: None,
                name: None,
                tool_calls: None,
            }],
            tools: vec![],
            max_tokens: None,
            temperature: None,
        }
    }

    fn success_body() -> &'static str {
        r#"{
                "choices": [{
                    "message": {"content": "Hello!", "tool_calls": null},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
            }"#
    }

    // --- Original tests (unchanged logic) ---

    #[tokio::test]
    async fn test_chat_completion_success() {
        let mut server = mockito::Server::new_async().await;

        let mock = server
            .mock("POST", "/chat/completions")
            .match_header("Authorization", "Bearer test-key")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                "choices": [{
                    "message": {"content": "Hello!", "tool_calls": null},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
            }"#,
            )
            .create_async()
            .await;

        let provider = make_provider(&server.url(), 5);
        let result = provider.generate(make_request()).await.unwrap();
        assert_eq!(result.content.as_deref(), Some("Hello!"));
        assert!(result.tool_calls.is_empty());
        assert!(result.usage.is_some());
        assert_eq!(result.usage.unwrap().input_tokens, 10);

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_tool_calls_response() {
        let mut server = mockito::Server::new_async().await;

        let mock = server
            .mock("POST", "/chat/completions")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                "choices": [{
                    "message": {
                        "content": null,
                        "tool_calls": [{
                            "id": "call_abc",
                            "type": "function",
                            "function": {
                                "name": "bash",
                                "arguments": "{\"command\":\"ls -la\"}"
                            }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }],
                "usage": {"prompt_tokens": 20, "completion_tokens": 10, "total_tokens": 30}
            }"#,
            )
            .create_async()
            .await;

        let provider = make_provider(&server.url(), 5);
        let result = provider.generate(make_request()).await.unwrap();
        assert!(result.content.is_none());
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].id.0, "call_abc");
        assert_eq!(result.tool_calls[0].name, "bash");
        assert_eq!(
            result.tool_calls[0].arguments["command"],
            "ls -la"
        );
        assert_eq!(result.finish_reason.as_deref(), Some("tool_calls"));

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_rate_limit_error() {
        let mut server = mockito::Server::new_async().await;

        let mock = server
            .mock("POST", "/chat/completions")
            .with_status(429)
            .create_async()
            .await;

        let provider = make_provider(&server.url(), 5);
        let result = provider.generate(make_request()).await;
        assert!(matches!(result, Err(ProviderError::RateLimit)));

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_server_error() {
        let mut server = mockito::Server::new_async().await;

        let mock = server
            .mock("POST", "/chat/completions")
            .with_status(500)
            .create_async()
            .await;

        let provider = make_provider(&server.url(), 5);
        let result = provider.generate(make_request()).await;
        assert!(matches!(result, Err(ProviderError::ServerError(500))));

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_auth_error() {
        let mut server = mockito::Server::new_async().await;

        let mock = server
            .mock("POST", "/chat/completions")
            .with_status(401)
            .create_async()
            .await;

        let provider = make_provider(&server.url(), 5);
        let result = provider.generate(make_request()).await;
        assert!(matches!(result, Err(ProviderError::AuthError(_))));

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_not_found_error() {
        let mut server = mockito::Server::new_async().await;

        let mock = server
            .mock("POST", "/chat/completions")
            .with_status(404)
            .create_async()
            .await;

        let provider = make_provider(&server.url(), 5);
        let result = provider.generate(make_request()).await;
        assert!(matches!(result, Err(ProviderError::NotFound(_))));

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_malformed_response() {
        let mut server = mockito::Server::new_async().await;

        let mock = server
            .mock("POST", "/chat/completions")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("this is not json")
            .create_async()
            .await;

        let provider = make_provider(&server.url(), 5);
        let result = provider.generate(make_request()).await;
        assert!(matches!(result, Err(ProviderError::MalformedResponse(_))));

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_empty_choices_error() {
        let mut server = mockito::Server::new_async().await;

        let mock = server
            .mock("POST", "/chat/completions")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"choices": [], "usage": null}"#)
            .create_async()
            .await;

        let provider = make_provider(&server.url(), 5);
        let result = provider.generate(make_request()).await;
        assert!(matches!(result, Err(ProviderError::MalformedResponse(_))));

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_timeout_error() {
        let config = OpenAIProviderConfig {
            provider_name: "test".into(),
            base_url: "http://10.255.255.1:1".into(),
            api_key: "test-key".into(),
            timeout_secs: 1,
        };
        let provider = OpenAICompatibleProvider::new(config);
        let result = provider.generate(make_request()).await;
        let err = result.unwrap_err();
        let is_timeout = matches!(err, ProviderError::Timeout);
        let is_connection = matches!(err, ProviderError::ConnectionError(_));
        assert!(is_timeout || is_connection, "expected Timeout or ConnectionError, got {:?}", err);
    }

    #[tokio::test]
    async fn test_request_format() {
        let mut server = mockito::Server::new_async().await;

        let mock = server
            .mock("POST", "/chat/completions")
            .match_header("Authorization", "Bearer test-key")
            .match_header("Content-Type", "application/json")
            .match_body(mockito::Matcher::JsonString(
                serde_json::json!({
                    "model": "test-model",
                    "messages": [
                        {"role": "system", "content": "You are helpful."},
                        {"role": "user", "content": "Hi"}
                    ],
                    "max_tokens": 50,
                    "temperature": 0.5
                })
                .to_string(),
            ))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                "choices": [{
                    "message": {"content": "Hey!", "tool_calls": null},
                    "finish_reason": "stop"
                }],
                "usage": null
            }"#,
            )
            .create_async()
            .await;

        let provider = make_provider(&server.url(), 5);
        let req = GenerateRequest {
            model_id: "test-model".into(),
            system_prompt: Some("You are helpful.".into()),
            messages: vec![Message {
                role: MessageRole::User,
                content: "Hi".into(),
                tool_call_id: None,
                name: None,
                tool_calls: None,
            }],
            tools: vec![],
            max_tokens: Some(50),
            temperature: Some(0.5),
        };
        let result = provider.generate(req).await.unwrap();
        assert_eq!(result.content.as_deref(), Some("Hey!"));

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_request_with_tools() {
        let mut server = mockito::Server::new_async().await;

        let mock = server
            .mock("POST", "/chat/completions")
            .match_body(mockito::Matcher::Regex(
                r#""tools":\[\{"type":"function","function":\{"name":"bash""#.to_owned(),
            ))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                "choices": [{
                    "message": {
                        "content": null,
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": {"name": "bash", "arguments": "{}"}
                        }]
                    },
                    "finish_reason": "tool_calls"
                }],
                "usage": null
            }"#,
            )
            .create_async()
            .await;

        let provider = make_provider(&server.url(), 5);
        let req = GenerateRequest {
            model_id: "test-model".into(),
            system_prompt: None,
            messages: vec![Message {
                role: MessageRole::User,
                content: "run ls".into(),
                tool_call_id: None,
                name: None,
                tool_calls: None,
            }],
            tools: vec![ToolDefinition {
                name: "bash".into(),
                description: "Run a shell command".into(),
                parameters: serde_json::json!({"type": "object"}),
            }],
            max_tokens: None,
            temperature: None,
        };
        let result = provider.generate(req).await.unwrap();
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "bash");

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_tool_message_conversion() {
        let mut server = mockito::Server::new_async().await;

        let mock = server
            .mock("POST", "/chat/completions")
            .match_body(mockito::Matcher::Regex(
                r#"\\?"role\\?":\\?"tool\\?""#.to_owned(),
            ))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                "choices": [{
                    "message": {"content": "Done!", "tool_calls": null},
                    "finish_reason": "stop"
                }],
                "usage": null
            }"#,
            )
            .create_async()
            .await;

        let provider = make_provider(&server.url(), 5);
        let req = GenerateRequest {
            model_id: "test-model".into(),
            system_prompt: None,
            messages: vec![Message {
                role: MessageRole::Tool,
                content: "result data".into(),
                tool_call_id: Some(ToolCallId("tc-42".into())),
                name: None,
                tool_calls: None,
            }],
            tools: vec![],
            max_tokens: None,
            temperature: None,
        };
        let result = provider.generate(req).await.unwrap();
        assert_eq!(result.content.as_deref(), Some("Done!"));

        mock.assert_async().await;
    }

    #[test]
    fn test_completions_url_trims_trailing_slash() {
        let config = OpenAIProviderConfig {
            provider_name: "test".into(),
            base_url: "https://api.example.com/".into(),
            api_key: "key".into(),
            timeout_secs: 30,
        };
        let provider = OpenAICompatibleProvider::new(config);
        assert_eq!(
            provider.completions_url(),
            "https://api.example.com/chat/completions"
        );
    }

    #[test]
    fn test_provider_name_returns_config_value() {
        let provider = make_provider("http://localhost", 5);
        assert_eq!(provider.provider_name(), "test");
    }

    // --- Retry tests ---

    #[test]
    fn test_retry_config_default() {
        let config = RetryConfig::default();
        assert_eq!(config.max_retries, 3);
        assert_eq!(config.retry_base_delay_ms, 1000);
    }

    #[test]
    fn test_is_retryable() {
        assert!(is_retryable(&ProviderError::RateLimit));
        assert!(is_retryable(&ProviderError::ServerError(500)));
        assert!(is_retryable(&ProviderError::ServerError(502)));
        assert!(is_retryable(&ProviderError::ServerError(503)));
        assert!(is_retryable(&ProviderError::ServerError(504)));
        assert!(is_retryable(&ProviderError::Timeout));
        assert!(is_retryable(&ProviderError::ConnectionError("refused".into())));

        // Non-retryable
        assert!(!is_retryable(&ProviderError::AuthError("bad key".into())));
        assert!(!is_retryable(&ProviderError::NotFound("model".into())));
        assert!(!is_retryable(&ProviderError::MalformedResponse("bad json".into())));
        assert!(!is_retryable(&ProviderError::ServerError(400)));
    }

    #[test]
    fn test_calculate_backoff_exponential() {
        let d0 = calculate_backoff(1000, 0);
        let d1 = calculate_backoff(1000, 1);
        let d2 = calculate_backoff(1000, 2);
        let d3 = calculate_backoff(1000, 3);

        // Base delays: ~1000, ~2000, ~4000, ~8000 (with jitter up to 25%)
        assert!(
            d0.as_millis() >= 1000 && d0.as_millis() <= 1300,
            "d0: {:?}",
            d0
        );
        assert!(
            d1.as_millis() >= 2000 && d1.as_millis() <= 2600,
            "d1: {:?}",
            d1
        );
        assert!(
            d2.as_millis() >= 4000 && d2.as_millis() <= 5200,
            "d2: {:?}",
            d2
        );
        assert!(
            d3.as_millis() >= 8000 && d3.as_millis() <= 10400,
            "d3: {:?}",
            d3
        );

        // Verify exponential growth
        assert!(
            d1.as_millis() >= d0.as_millis(),
            "d1 ({:?}) should be >= d0 ({:?})",
            d1,
            d0
        );
        assert!(
            d2.as_millis() >= d1.as_millis(),
            "d2 ({:?}) should be >= d1 ({:?})",
            d2,
            d1
        );
    }

    #[test]
    fn test_calculate_backoff_zero_base() {
        let d = calculate_backoff(0, 3);
        assert_eq!(d.as_millis(), 0);
    }

    #[test]
    fn test_truncate_response_body_short() {
        assert_eq!(truncate_response_body("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_response_body_long() {
        let long = "a".repeat(300);
        let truncated = truncate_response_body(&long, 200);
        assert_eq!(truncated.len(), 200);
    }

    #[test]
    fn test_truncate_response_body_empty() {
        assert_eq!(truncate_response_body("", 200), "");
    }

    #[tokio::test]
    async fn test_retry_success_on_first_try() {
        let mut server = mockito::Server::new_async().await;

        let mock = server
            .mock("POST", "/chat/completions")
            .expect(1)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(success_body())
            .create_async()
            .await;

        let provider = make_provider_with_retry(
            &server.url(),
            5,
            RetryConfig {
                max_retries: 3,
                retry_base_delay_ms: 1,
            },
        );

        let result = provider.generate(make_request()).await.unwrap();
        assert_eq!(result.content.as_deref(), Some("Hello!"));

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_retry_429_then_success() {
        let mut server = mockito::Server::new_async().await;

        // Error mock — created first, matched first (FIFO)
        let mock_error = server
            .mock("POST", "/chat/completions")
            .expect(1)
            .with_status(429)
            .create_async()
            .await;

        // Success mock — matched after error mock is exhausted
        let mock_success = server
            .mock("POST", "/chat/completions")
            .expect(1)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(success_body())
            .create_async()
            .await;

        let provider = make_provider_with_retry(
            &server.url(),
            5,
            RetryConfig {
                max_retries: 3,
                retry_base_delay_ms: 1,
            },
        );

        let result = provider.generate(make_request()).await.unwrap();
        assert_eq!(result.content.as_deref(), Some("Hello!"));

        mock_error.assert_async().await;
        mock_success.assert_async().await;
    }

    #[tokio::test]
    async fn test_retry_500_then_success() {
        let mut server = mockito::Server::new_async().await;

        let mock_error = server
            .mock("POST", "/chat/completions")
            .expect(1)
            .with_status(500)
            .with_body("Internal Server Error")
            .create_async()
            .await;

        let mock_success = server
            .mock("POST", "/chat/completions")
            .expect(1)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(success_body())
            .create_async()
            .await;

        let provider = make_provider_with_retry(
            &server.url(),
            5,
            RetryConfig {
                max_retries: 3,
                retry_base_delay_ms: 1,
            },
        );

        let result = provider.generate(make_request()).await.unwrap();
        assert_eq!(result.content.as_deref(), Some("Hello!"));

        mock_error.assert_async().await;
        mock_success.assert_async().await;
    }

    #[tokio::test]
    async fn test_retry_401_no_retry() {
        let mut server = mockito::Server::new_async().await;

        let mock = server
            .mock("POST", "/chat/completions")
            .expect(1) // Exactly one request — no retries
            .with_status(401)
            .with_body(r#"{"error":{"message":"bad key"}}"#)
            .create_async()
            .await;

        let provider = make_provider_with_retry(
            &server.url(),
            5,
            RetryConfig {
                max_retries: 3,
                retry_base_delay_ms: 1,
            },
        );

        let result = provider.generate(make_request()).await;
        // Auth errors return immediately without retry wrapper since attempt == 0
        assert!(matches!(result, Err(ProviderError::AuthError(_))));

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_retry_403_no_retry() {
        let mut server = mockito::Server::new_async().await;

        let mock = server
            .mock("POST", "/chat/completions")
            .expect(1)
            .with_status(403)
            .with_body("forbidden")
            .create_async()
            .await;

        let provider = make_provider_with_retry(
            &server.url(),
            5,
            RetryConfig {
                max_retries: 3,
                retry_base_delay_ms: 1,
            },
        );

        let result = provider.generate(make_request()).await;
        assert!(matches!(result, Err(ProviderError::AuthError(_))));

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_retry_exhausted_all_429() {
        let mut server = mockito::Server::new_async().await;

        let mock = server
            .mock("POST", "/chat/completions")
            .expect(4) // 1 initial + 3 retries = 4 total attempts
            .with_status(429)
            .create_async()
            .await;

        let provider = make_provider_with_retry(
            &server.url(),
            5,
            RetryConfig {
                max_retries: 3,
                retry_base_delay_ms: 1,
            },
        );

        let result = provider.generate(make_request()).await;
        match result {
            Err(ProviderError::ConnectionError(msg)) => {
                assert!(
                    msg.contains("failed after 4 attempt(s)"),
                    "unexpected message: {}",
                    msg
                );
                assert!(
                    msg.contains("rate limit exceeded"),
                    "should mention the last error: {}",
                    msg
                );
            }
            other => panic!("expected ConnectionError, got: {:?}", other),
        }

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_retry_exhausted_all_500() {
        let mut server = mockito::Server::new_async().await;

        let mock = server
            .mock("POST", "/chat/completions")
            .expect(4)
            .with_status(503)
            .with_body("service unavailable")
            .create_async()
            .await;

        let provider = make_provider_with_retry(
            &server.url(),
            5,
            RetryConfig {
                max_retries: 3,
                retry_base_delay_ms: 1,
            },
        );

        let result = provider.generate(make_request()).await;
        match result {
            Err(ProviderError::ConnectionError(msg)) => {
                assert!(msg.contains("failed after 4 attempt(s)"), "msg: {}", msg);
                assert!(msg.contains("503"), "should include status code: {}", msg);
            }
            other => panic!("expected ConnectionError, got: {:?}", other),
        }

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_retry_mixed_errors_then_success() {
        let mut server = mockito::Server::new_async().await;

        let mock_429 = server
            .mock("POST", "/chat/completions")
            .expect(1)
            .with_status(429)
            .create_async()
            .await;

        let mock_500 = server
            .mock("POST", "/chat/completions")
            .expect(1)
            .with_status(500)
            .create_async()
            .await;

        let mock_success = server
            .mock("POST", "/chat/completions")
            .expect(1)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(success_body())
            .create_async()
            .await;

        let provider = make_provider_with_retry(
            &server.url(),
            5,
            RetryConfig {
                max_retries: 3,
                retry_base_delay_ms: 1,
            },
        );

        let result = provider.generate(make_request()).await.unwrap();
        assert_eq!(result.content.as_deref(), Some("Hello!"));

        mock_429.assert_async().await;
        mock_500.assert_async().await;
        mock_success.assert_async().await;
    }

    #[tokio::test]
    async fn test_retry_non_retryable_stops_early() {
        let mut server = mockito::Server::new_async().await;

        let mock_500 = server
            .mock("POST", "/chat/completions")
            .expect(1)
            .with_status(500)
            .create_async()
            .await;

        let mock_401 = server
            .mock("POST", "/chat/completions")
            .expect(1)
            .with_status(401)
            .create_async()
            .await;

        let provider = make_provider_with_retry(
            &server.url(),
            5,
            RetryConfig {
                max_retries: 3,
                retry_base_delay_ms: 1,
            },
        );

        let result = provider.generate(make_request()).await;
        // After 500 (retryable, attempt 0), 401 (non-retryable, attempt 1)
        // Since attempt > 0, wrapped in ConnectionError with retry context
        match result {
            Err(ProviderError::ConnectionError(msg)) => {
                assert!(msg.contains("failed after 2 attempt(s)"), "msg: {}", msg);
                assert!(msg.contains("authentication error"), "should mention auth error: {}", msg);
            }
            other => panic!("expected ConnectionError with retry context, got: {:?}", other),
        }

        mock_500.assert_async().await;
        mock_401.assert_async().await;
    }

    #[tokio::test]
    async fn test_retry_with_zero_max_retries() {
        let mut server = mockito::Server::new_async().await;

        let mock = server
            .mock("POST", "/chat/completions")
            .expect(1)
            .with_status(500)
            .create_async()
            .await;

        let provider = make_provider_with_retry(
            &server.url(),
            5,
            RetryConfig {
                max_retries: 0,
                retry_base_delay_ms: 1,
            },
        );

        let result = provider.generate(make_request()).await;
        // With max_retries=0, should return raw error (no retry wrapping)
        assert!(matches!(result, Err(ProviderError::ServerError(500))));

        mock.assert_async().await;
    }

    #[test]
    fn test_with_retry_sets_config() {
        let provider = make_provider("http://localhost", 5).with_retry(RetryConfig {
            max_retries: 5,
            retry_base_delay_ms: 500,
        });
        assert_eq!(provider.provider_name(), "test");
        assert!(provider.retry_config.is_some());
        let rc = provider.retry_config.as_ref().unwrap();
        assert_eq!(rc.max_retries, 5);
        assert_eq!(rc.retry_base_delay_ms, 500);
    }

    #[test]
    fn test_without_retry_config_is_none() {
        let provider = make_provider("http://localhost", 5);
        assert!(provider.retry_config.is_none());
    }
}
