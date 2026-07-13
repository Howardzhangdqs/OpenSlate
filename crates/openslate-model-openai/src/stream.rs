//! SSE streaming support for OpenAI-compatible providers.
//!
//! Parses Server-Sent Events (SSE) from chat completion streaming responses
//! and yields `ModelStreamEvent`s via a `tokio::sync::mpsc` channel.

use openslate_core::error::ProviderError;
use openslate_core::provider::GenerateRequest;
use openslate_core::types::{ModelResponse, ToolCall, ToolCallId, Usage};

use crate::client::OpenAICompatibleProvider;
use crate::types::*;

// Re-export for backward compatibility
pub use openslate_core::types::ModelStreamEvent;

/// Streaming request body (same as ChatCompletionRequest but with `stream: true`).
#[derive(Debug, serde::Serialize)]
struct StreamCompletionRequest {
    pub model: String,
    pub messages: Vec<ApiMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ApiTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    pub stream: bool,
}

/// A single SSE chunk from the streaming API.
#[derive(Debug, serde::Deserialize)]
struct StreamChunk {
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<ApiUsage>,
}

#[derive(Debug, serde::Deserialize)]
struct StreamChoice {
    delta: StreamDelta,
    finish_reason: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<StreamToolCall>>,
}

#[derive(Debug, serde::Deserialize)]
struct StreamToolCall {
    index: Option<u32>,
    id: Option<String>,
    #[allow(dead_code)]
    r#type: Option<String>,
    function: Option<StreamFunctionDelta>,
}

#[derive(Debug, serde::Deserialize)]
struct StreamFunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

/// Parse SSE lines from a raw byte buffer, returning complete events.
///
/// Returns (events, remaining_bytes). Incomplete lines are buffered.
pub fn parse_sse_chunks(input: &str) -> Vec<String> {
    let mut events = Vec::new();
    for line in input.lines() {
        let trimmed = line.trim();
        if let Some(data) = trimmed.strip_prefix("data: ") {
            let data = data.trim();
            if data == "[DONE]" {
                break;
            }
            if !data.is_empty() {
                events.push(data.to_owned());
            }
        }
    }
    events
}

/// Accumulates streaming deltas into a full response.
#[derive(Debug, Default)]
struct StreamAccumulator {
    content: String,
    tool_calls: Vec<AccumulatedToolCall>,
    finish_reason: Option<String>,
    usage: Option<Usage>,
}

#[derive(Debug, Default)]
struct AccumulatedToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl StreamAccumulator {
    fn process_chunk(&mut self, chunk: &StreamChunk) {
        if let Some(usage) = &chunk.usage {
            self.usage = Some(Usage {
                input_tokens: usage.prompt_tokens,
                output_tokens: usage.completion_tokens,
            });
        }
        if let Some(choice) = chunk.choices.first() {
            if let Some(ref content) = choice.delta.content {
                self.content.push_str(content);
            }
            if let Some(ref reason) = choice.finish_reason {
                self.finish_reason = Some(reason.clone());
            }
            if let Some(ref tool_calls) = choice.delta.tool_calls {
                for tc in tool_calls {
                    let idx = tc.index.unwrap_or(0) as usize;
                    // Ensure we have enough slots
                    while self.tool_calls.len() <= idx {
                        self.tool_calls.push(AccumulatedToolCall::default());
                    }
                    let entry = &mut self.tool_calls[idx];
                    if let Some(ref id) = tc.id {
                        entry.id = id.clone();
                    }
                    if let Some(ref func) = tc.function {
                        if let Some(ref name) = func.name {
                            entry.name = name.clone();
                        }
                        if let Some(ref args) = func.arguments {
                            entry.arguments.push_str(args);
                        }
                    }
                }
            }
        }
    }

    fn into_response(self) -> ModelResponse {
        let content = if self.content.is_empty() {
            None
        } else {
            Some(self.content)
        };
        let tool_calls: Vec<ToolCall> = self
            .tool_calls
            .into_iter()
            .filter(|tc| !tc.name.is_empty() || !tc.id.is_empty())
            .map(|tc| ToolCall {
                id: ToolCallId(tc.id),
                name: tc.name,
                arguments: serde_json::from_str(&tc.arguments)
                    .unwrap_or(serde_json::Value::Null),
            })
            .collect();
        ModelResponse {
            content,
            tool_calls,
            usage: self.usage,
            finish_reason: self.finish_reason,
        }
    }
}

impl OpenAICompatibleProvider {
    /// Execute a streaming chat completion request.
    ///
    /// Returns a receiver that yields `ModelStreamEvent`s as they arrive.
    /// The final event will always be `ModelStreamEvent::Done(..)` containing
    /// the fully assembled response.
    pub fn start_stream(
        &self,
        request: GenerateRequest,
    ) -> tokio::sync::mpsc::Receiver<Result<ModelStreamEvent, ProviderError>> {
        let (tx, rx) = tokio::sync::mpsc::channel(64);

        let client = self.client.clone();
        let url = self.completions_url();
        let api_key = self.config.api_key.clone();

        let api_messages =
            Self::convert_messages(request.system_prompt.as_deref(), &request.messages);
        let api_tools = if request.tools.is_empty() {
            None
        } else {
            Some(Self::convert_tools(&request.tools))
        };

        let body = StreamCompletionRequest {
            model: request.model_id,
            messages: api_messages,
            tools: api_tools,
            max_tokens: request.max_tokens,
            temperature: request.temperature,
            stream: true,
        };

        tokio::spawn(async move {
            if let Err(e) = run_stream(client, url, api_key, body, &tx).await {
                let _ = tx.send(Err(e)).await;
            }
        });

        rx
    }
}

async fn run_stream(
    client: reqwest::Client,
    url: String,
    api_key: String,
    body: StreamCompletionRequest,
    tx: &tokio::sync::mpsc::Sender<Result<ModelStreamEvent, ProviderError>>,
) -> Result<(), ProviderError> {
    let response = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", api_key))
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
        return Err(ProviderError::NotFound(body.model.clone()));
    }
    if status.as_u16() == 401 {
        return Err(ProviderError::AuthError("invalid API key".into()));
    }
    if !status.is_success() {
        return Err(ProviderError::ServerError(status.as_u16()));
    }

    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut accumulator = StreamAccumulator::default();

    use futures_util::StreamExt;

    while let Some(chunk_result) = stream.next().await {
        let chunk = chunk_result.map_err(|e| ProviderError::ConnectionError(e.to_string()))?;
        let text = String::from_utf8_lossy(&chunk);
        buffer.push_str(&text);

        // Parse complete SSE events from the buffer
        let events = parse_sse_chunks(&buffer);
        let mut consumed = 0usize;
        for event_json in &events {
            consumed += calculate_event_bytes(&buffer[consumed..], event_json);
            let parsed: serde_json::Result<StreamChunk> = serde_json::from_str(event_json);
            match parsed {
                Ok(chunk) => {
                    // Emit delta event for content
                    if let Some(choice) = chunk.choices.first() {
                        if let Some(ref content) = choice.delta.content {
                            if !content.is_empty() {
                                let _ = tx
                                    .send(Ok(ModelStreamEvent::Delta(content.clone())))
                                    .await;
                            }
                        }
                    }
                    // Emit usage event if present
                    if let Some(ref usage) = chunk.usage {
                        let _ = tx
                            .send(Ok(ModelStreamEvent::Usage(Usage {
                                input_tokens: usage.prompt_tokens,
                                output_tokens: usage.completion_tokens,
                            })))
                            .await;
                    }
                    accumulator.process_chunk(&chunk);
                }
                Err(e) => {
                    tracing::debug!("Failed to parse SSE chunk: {} — data: {}", e, event_json);
                }
            }
        }

        // Keep only unprocessed data in the buffer
        if consumed > 0 {
            buffer = buffer[consumed..].to_owned();
        }
    }

    // Parse any remaining events in buffer
    let remaining_events = parse_sse_chunks(&buffer);
    for event_json in &remaining_events {
        let parsed: serde_json::Result<StreamChunk> = serde_json::from_str(event_json);
        if let Ok(chunk) = parsed {
            if let Some(choice) = chunk.choices.first() {
                if let Some(ref content) = choice.delta.content {
                    if !content.is_empty() {
                        let _ = tx.send(Ok(ModelStreamEvent::Delta(content.clone()))).await;
                    }
                }
            }
            if let Some(ref usage) = chunk.usage {
                let _ = tx
                    .send(Ok(ModelStreamEvent::Usage(Usage {
                        input_tokens: usage.prompt_tokens,
                        output_tokens: usage.completion_tokens,
                    })))
                    .await;
            }
            accumulator.process_chunk(&chunk);
        }
    }

    // Send the final assembled response
    let response = accumulator.into_response();
    let _ = tx.send(Ok(ModelStreamEvent::Done(response))).await;

    Ok(())
}

/// Calculate how many bytes in the buffer correspond to a parsed event.
fn calculate_event_bytes(buffer: &str, event_data: &str) -> usize {
    // Find "data: {event_data}" in the buffer
    let search = format!("data: {}", event_data);
    if let Some(pos) = buffer.find(&search) {
        let end = pos + search.len();
        // Skip trailing whitespace/newlines
        let remaining = &buffer[end..];
        let skip = remaining.chars().take_while(|c| *c == '\n' || *c == '\r').count();
        end + skip
    } else {
        // Fallback: approximate
        event_data.len() + 8 // "data: " + "\n\n"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_sse_chunks_single_event() {
        let input = "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n";
        let events = parse_sse_chunks(input);
        assert_eq!(events.len(), 1);
        assert!(events[0].contains("\"delta\""));
    }

    #[test]
    fn test_parse_sse_chunks_multiple_events() {
        let input = "\
data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\" there\"}}]}\n\n\
data: [DONE]\n\n";
        let events = parse_sse_chunks(input);
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn test_parse_sse_chunks_done_terminator() {
        let input = "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\ndata: [DONE]\n\n";
        let events = parse_sse_chunks(input);
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn test_parse_sse_chunks_empty_data_lines() {
        let input = "data: \n\ndata: {\"choices\":[]}\n\n";
        let events = parse_sse_chunks(input);
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn test_stream_accumulator_basic() {
        let mut acc = StreamAccumulator::default();
        acc.process_chunk(&StreamChunk {
            choices: vec![StreamChoice {
                delta: StreamDelta {
                    content: Some("Hello".into()),
                    tool_calls: None,
                },
                finish_reason: None,
            }],
            usage: None,
        });
        acc.process_chunk(&StreamChunk {
            choices: vec![StreamChoice {
                delta: StreamDelta {
                    content: Some(" world".into()),
                    tool_calls: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Some(ApiUsage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
            }),
        });

        let resp = acc.into_response();
        assert_eq!(resp.content.as_deref(), Some("Hello world"));
        assert_eq!(resp.finish_reason.as_deref(), Some("stop"));
        assert!(resp.usage.is_some());
        assert_eq!(resp.usage.unwrap().input_tokens, 10);
    }

    #[test]
    fn test_stream_accumulator_tool_calls() {
        let mut acc = StreamAccumulator::default();
        acc.process_chunk(&StreamChunk {
            choices: vec![StreamChoice {
                delta: StreamDelta {
                    content: None,
                    tool_calls: Some(vec![StreamToolCall {
                        index: Some(0),
                        id: Some("call_1".into()),
                        r#type: Some("function".into()),
                        function: Some(StreamFunctionDelta {
                            name: Some("bash".into()),
                            arguments: None,
                        }),
                    }]),
                },
                finish_reason: None,
            }],
            usage: None,
        });
        acc.process_chunk(&StreamChunk {
            choices: vec![StreamChoice {
                delta: StreamDelta {
                    content: None,
                    tool_calls: Some(vec![StreamToolCall {
                        index: Some(0),
                        id: None,
                        r#type: None,
                        function: Some(StreamFunctionDelta {
                            name: None,
                            arguments: Some("{\"cmd\":\"ls\"}".into()),
                        }),
                    }]),
                },
                finish_reason: Some("tool_calls".into()),
            }],
            usage: None,
        });

        let resp = acc.into_response();
        assert!(resp.content.is_none());
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].id.0, "call_1");
        assert_eq!(resp.tool_calls[0].name, "bash");
        assert_eq!(resp.tool_calls[0].arguments["cmd"], "ls");
    }

    #[test]
    fn test_stream_accumulator_empty() {
        let acc = StreamAccumulator::default();
        let resp = acc.into_response();
        assert!(resp.content.is_none());
        assert!(resp.tool_calls.is_empty());
        assert!(resp.usage.is_none());
        assert!(resp.finish_reason.is_none());
    }

    #[test]
    fn test_parse_sse_chunks_no_events() {
        let events = parse_sse_chunks("");
        assert!(events.is_empty());
    }

    #[test]
    fn test_stream_accumulator_multiple_tool_calls() {
        let mut acc = StreamAccumulator::default();
        // First tool call at index 0
        acc.process_chunk(&StreamChunk {
            choices: vec![StreamChoice {
                delta: StreamDelta {
                    content: None,
                    tool_calls: Some(vec![
                        StreamToolCall {
                            index: Some(0),
                            id: Some("call_a".into()),
                            r#type: Some("function".into()),
                            function: Some(StreamFunctionDelta {
                                name: Some("read".into()),
                                arguments: None,
                            }),
                        },
                        StreamToolCall {
                            index: Some(1),
                            id: Some("call_b".into()),
                            r#type: Some("function".into()),
                            function: Some(StreamFunctionDelta {
                                name: Some("write".into()),
                                arguments: None,
                            }),
                        },
                    ]),
                },
                finish_reason: Some("tool_calls".into()),
            }],
            usage: None,
        });

        let resp = acc.into_response();
        assert_eq!(resp.tool_calls.len(), 2);
        assert_eq!(resp.tool_calls[0].name, "read");
        assert_eq!(resp.tool_calls[1].name, "write");
    }

    #[test]
    fn test_calculate_event_bytes() {
        let buffer = "data: {\"test\":1}\n\nother stuff";
        let result = calculate_event_bytes(buffer, "{\"test\":1}");
        assert!(result > 0);
        assert!(result <= buffer.len());
    }

    #[tokio::test]
    async fn test_generate_stream_with_mock() {
        use crate::client::OpenAIProviderConfig;

        let mut server = mockito::Server::new_async().await;

        let sse_body = "\
data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"},\"finish_reason\":null}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\"lo!\"},\"finish_reason\":null}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":null},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2,\"total_tokens\":12}}\n\n\
data: [DONE]\n\n";

        let mock = server
            .mock("POST", "/chat/completions")
            .match_header("Authorization", "Bearer test-key")
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body(sse_body)
            .create_async()
            .await;

        let provider = OpenAICompatibleProvider::new(OpenAIProviderConfig {
            provider_name: "test".into(),
            base_url: server.url(),
            api_key: "test-key".into(),
            timeout_secs: 5,
        });

        let request = GenerateRequest {
            model_id: "test-model".into(),
            system_prompt: None,
            messages: vec![openslate_core::types::Message {
                role: openslate_core::types::MessageRole::User,
                content: "Hi".into(),
                tool_call_id: None,
                name: None,
                tool_calls: None,
            }],
            tools: vec![],
            max_tokens: None,
            temperature: None,
        };

        let mut rx = provider.start_stream(request);

        let mut deltas = Vec::new();
        let mut got_done = false;
        while let Some(event) = rx.recv().await {
            match event {
                Ok(ModelStreamEvent::Delta(text)) => deltas.push(text),
                Ok(ModelStreamEvent::Usage(u)) => {
                    assert_eq!(u.input_tokens, 10);
                    assert_eq!(u.output_tokens, 2);
                }
                Ok(ModelStreamEvent::Done(resp)) => {
                    assert_eq!(resp.content.as_deref(), Some("Hello!"));
                    assert_eq!(resp.finish_reason.as_deref(), Some("stop"));
                    got_done = true;
                }
                // The OpenAI-compatible client never emits reasoning; ignore it
                // if some other layer forwards one.
                Ok(ModelStreamEvent::Reasoning(_)) => {}
                Err(e) => panic!("Stream error: {:?}", e),
            }
        }

        assert!(got_done, "Should receive Done event");
        assert_eq!(deltas, vec!["Hel", "lo!"]);

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_generate_stream_error_status() {
        use crate::client::OpenAIProviderConfig;

        let mut server = mockito::Server::new_async().await;

        let mock = server
            .mock("POST", "/chat/completions")
            .with_status(429)
            .create_async()
            .await;

        let provider = OpenAICompatibleProvider::new(OpenAIProviderConfig {
            provider_name: "test".into(),
            base_url: server.url(),
            api_key: "test-key".into(),
            timeout_secs: 5,
        });

        let request = GenerateRequest {
            model_id: "test-model".into(),
            system_prompt: None,
            messages: vec![],
            tools: vec![],
            max_tokens: None,
            temperature: None,
        };

        let mut rx = provider.start_stream(request);
        let event = rx.recv().await;
        match event {
            Some(Err(ProviderError::RateLimit)) => {}
            other => panic!("Expected RateLimit error, got: {:?}", other),
        }

        mock.assert_async().await;
    }
}
