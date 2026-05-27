use openslate_core::error::ProviderError;
use openslate_core::provider::{GenerateRequest, ModelProvider, ToolDefinition};
use openslate_core::types::ModelResponse;

use crate::types::*;

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
}

impl OpenAICompatibleProvider {
    pub fn new(config: OpenAIProviderConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .build()
            .unwrap_or_default();
        Self { config, client }
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
                    tool_calls: None,
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
}

#[async_trait::async_trait]
impl ModelProvider for OpenAICompatibleProvider {
    async fn generate(&self, request: GenerateRequest) -> Result<ModelResponse, ProviderError> {
        let model_id = request.model_id.clone();
        let api_messages =
            Self::convert_messages(request.system_prompt.as_deref(), &request.messages);
        let api_tools = if request.tools.is_empty() {
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

        let response = self
            .client
            .post(self.completions_url())
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
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
        if status.as_u16() == 429 {
            return Err(ProviderError::RateLimit);
        }
        if status.as_u16() >= 500 {
            return Err(ProviderError::ServerError(status.as_u16()));
        }
        if status.as_u16() == 404 {
            return Err(ProviderError::NotFound(model_id));
        }
        if status.as_u16() == 401 {
            return Err(ProviderError::AuthError("invalid API key".into()));
        }
        if !status.is_success() {
            return Err(ProviderError::ServerError(status.as_u16()));
        }

        let body = response
            .text()
            .await
            .map_err(|e| ProviderError::ConnectionError(e.to_string()))?;

        let api_response: ChatCompletionResponse = serde_json::from_str(&body).map_err(|e| {
            ProviderError::MalformedResponse(format!("Failed to parse response: {}", e))
        })?;

        let choice = api_response.choices.into_iter().next().ok_or_else(|| {
            ProviderError::MalformedResponse("No choices in response".into())
        })?;

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

    fn provider_name(&self) -> &str {
        &self.config.provider_name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openslate_core::types::{Message, MessageRole, ToolCallId};

    fn make_provider(server_url: &str, timeout_secs: u64) -> OpenAICompatibleProvider {
        OpenAICompatibleProvider::new(OpenAIProviderConfig {
            provider_name: "test".into(),
            base_url: server_url.into(),
            api_key: "test-key".into(),
            timeout_secs,
        })
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
            }],
            tools: vec![],
            max_tokens: None,
            temperature: None,
        }
    }

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
}
