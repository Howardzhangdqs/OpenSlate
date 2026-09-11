//! Bidirectional conversion between OpenSlate core types and genai types.
//!
//! # Critical invariant (G1)
//!
//! An assistant turn carrying tool calls MUST be mapped to a genai assistant
//! message whose content contains `ContentPart::ToolCall` entries (built via
//! `ChatMessage::assistant(MessageContent::from_parts(...))`), NOT to
//! `ChatMessage::assistant(plain_text)`. The plain-text form silently drops
//! tool calls and breaks multi-step tool loops — the model would see a tool
//! result with no corresponding tool_use. When the turn carries BOTH text and
//! tool calls, both must be preserved (text part first, then tool-call parts;
//! genai's adapters map Text parts to `content` and ToolCall parts to
//! `tool_calls` on the wire). Likewise a tool-result message maps to
//! `ChatMessage::from(ToolResponse { .. })`.

use openslate_core::provider::{GenerateRequest, ToolDefinition};
use openslate_core::types::{Message, MessageRole, ModelResponse, ToolCall, ToolCallId, Usage};

use genai::chat::{
    ChatMessage, ChatRequest, ChatResponse, ContentPart, MessageContent, StreamEnd,
    Tool as GenaiTool, ToolCall as GenaiToolCall, ToolResponse,
};

// ---------------------------------------------------------------------------
// GenerateRequest  ->  ChatRequest
// ---------------------------------------------------------------------------

/// Convert an OpenSlate [`GenerateRequest`] into a genai [`ChatRequest`].
pub(crate) fn to_chat_request(req: &GenerateRequest) -> ChatRequest {
    let messages: Vec<ChatMessage> = req.messages.iter().map(to_chat_message).collect();

    let mut chat_req = ChatRequest::new(messages);

    // Top-level system prompt (Anthropic-style). genai also accepts system-role
    // messages, so both this and any System-role messages in `messages` survive.
    if let Some(sys) = req.system_prompt.as_ref().filter(|s| !s.is_empty()) {
        chat_req = chat_req.with_system(sys.clone());
    }

    if !req.tools.is_empty() {
        let tools: Vec<GenaiTool> = req.tools.iter().map(to_genai_tool).collect();
        chat_req = chat_req.with_tools(tools);
    }

    chat_req
}

fn to_chat_message(msg: &Message) -> ChatMessage {
    match msg.role {
        MessageRole::System => ChatMessage::system(msg.content.clone()),
        MessageRole::User => ChatMessage::user(msg.content.clone()),
        MessageRole::Assistant => {
            // G1: assistant turns with tool calls must carry the tool calls, not
            // be flattened to plain text. Any accompanying text must survive
            // too — genai maps Text parts to `content` and ToolCall parts to
            // `tool_calls`, so a parts-based message keeps both on the wire.
            if let Some(tcs) = msg.tool_calls.as_ref().filter(|v| !v.is_empty()) {
                let genai_tcs: Vec<GenaiToolCall> = tcs.iter().map(to_genai_tool_call).collect();
                let mut parts: Vec<ContentPart> =
                    Vec::with_capacity(usize::from(!msg.content.is_empty()) + genai_tcs.len());
                if !msg.content.is_empty() {
                    parts.push(ContentPart::Text(msg.content.clone()));
                }
                parts.extend(genai_tcs.into_iter().map(ContentPart::ToolCall));
                ChatMessage::assistant(MessageContent::from_parts(parts))
            } else {
                ChatMessage::assistant(msg.content.clone())
            }
        }
        MessageRole::Tool => {
            // A missing call_id cannot be correlated by the model; warn (rather
            // than error) to avoid breaking runs, and keep the empty-string
            // mapping for wire compatibility.
            let call_id = msg
                .tool_call_id
                .as_ref()
                .map(|id| id.0.clone())
                .unwrap_or_else(|| {
                    tracing::warn!(
                        name = msg.name.as_deref().unwrap_or("<unknown>"),
                        "tool result message is missing tool_call_id; mapping to empty call_id"
                    );
                    String::new()
                });
            ChatMessage::from(ToolResponse {
                call_id,
                fn_name: msg.name.clone(),
                content: msg.content.clone(),
            })
        }
    }
}

fn to_genai_tool_call(tc: &ToolCall) -> GenaiToolCall {
    GenaiToolCall {
        call_id: tc.id.0.clone(),
        fn_name: tc.name.clone(),
        fn_arguments: tc.arguments.clone(),
        thought_signatures: None,
    }
}

fn to_genai_tool(td: &ToolDefinition) -> GenaiTool {
    let mut tool = GenaiTool::new(td.name.clone()).with_description(td.description.clone());
    // Only attach a schema if it is a real JSON value (not null). genai stores
    // schema as Option<Value>; null would be misleading.
    if !td.parameters.is_null() {
        tool = tool.with_schema(td.parameters.clone());
    }
    tool
}

// ---------------------------------------------------------------------------
// ChatResponse  ->  ModelResponse
// ---------------------------------------------------------------------------

/// Join all text parts of a genai [`MessageContent`] with newlines.
///
/// `first_text()` would silently drop every text part after the first; models
/// (and streaming accumulators) can legitimately emit multiple text parts.
/// Returns `None` only when there are no text parts at all.
fn joined_text_parts(content: &MessageContent) -> Option<String> {
    let texts = content.texts();
    if texts.is_empty() {
        None
    } else {
        Some(texts.join("\n"))
    }
}

/// Extract the cached-input-token count from a genai usage struct
/// (normalized across adapters: OpenAI `prompt_tokens_details.cached_tokens`,
/// Anthropic `cache_read_input_tokens`). genai deserializes 0 as `None`
/// (its `zero_as_none` convention), so `Some` means the server actually
/// reported a non-zero cache read; anything else stays `None`.
fn cached_input_tokens(u: &genai::chat::Usage) -> Option<u32> {
    u.prompt_tokens_details
        .as_ref()
        .and_then(|d| d.cached_tokens)
        .filter(|t| *t > 0)
        .map(|t| t.max(0) as u32)
}

/// Convert a non-streaming genai [`ChatResponse`] into an OpenSlate
/// [`ModelResponse`].
///
/// `usage` fields in genai are `Option<i32>` (nullable, since OpenAI returns 0
/// for non-applicable counters and genai deserializes 0 as None); they are
/// clamped to `u32`.
pub(crate) fn from_chat_response(res: ChatResponse) -> ModelResponse {
    // All text parts, newline-joined (first_text() would drop the rest).
    let content = joined_text_parts(&res.content);
    let usage = Usage {
        input_tokens: res.usage.prompt_tokens.unwrap_or(0).max(0) as u32,
        output_tokens: res.usage.completion_tokens.unwrap_or(0).max(0) as u32,
        cached_input_tokens: cached_input_tokens(&res.usage),
    };
    let finish_reason = res.stop_reason.as_ref().map(|sr| sr.raw().to_string());

    // into_tool_calls() consumes `res`; call it last, after the borrows above.
    let tool_calls: Vec<ToolCall> = res
        .into_tool_calls()
        .into_iter()
        .map(|tc| ToolCall {
            id: ToolCallId(tc.call_id),
            name: tc.fn_name,
            arguments: tc.fn_arguments,
        })
        .collect();

    ModelResponse {
        content,
        tool_calls,
        usage: Some(usage),
        finish_reason,
    }
}

// ---------------------------------------------------------------------------
// StreamEnd  ->  (Option<Usage>, ModelResponse)
// ---------------------------------------------------------------------------

/// Convert a streaming [`StreamEnd`] into the optional usage event and the
/// terminal [`ModelResponse`].
///
/// Requires `capture_content` / `capture_tool_calls` / `capture_usage` to have
/// been set on the `ChatOptions` — otherwise the `captured_*` fields are `None`
/// and the returned `ModelResponse` is empty.
pub(crate) fn from_stream_end(end: StreamEnd) -> (Option<Usage>, ModelResponse) {
    let usage = end.captured_usage.map(|u| Usage {
        input_tokens: u.prompt_tokens.unwrap_or(0).max(0) as u32,
        output_tokens: u.completion_tokens.unwrap_or(0).max(0) as u32,
        cached_input_tokens: cached_input_tokens(&u),
    });

    // All text parts, newline-joined (first_text() would drop the rest).
    let content = end.captured_content.as_ref().and_then(joined_text_parts);

    let tool_calls: Vec<ToolCall> = end
        .captured_content
        .as_ref()
        .map(|c| {
            c.tool_calls()
                .into_iter()
                .map(|tc| ToolCall {
                    id: ToolCallId(tc.call_id.clone()),
                    name: tc.fn_name.clone(),
                    arguments: tc.fn_arguments.clone(),
                })
                .collect()
        })
        .unwrap_or_default();

    let finish_reason = end
        .captured_stop_reason
        .as_ref()
        .map(|sr| sr.raw().to_string());

    let response = ModelResponse {
        content,
        tool_calls,
        usage,
        finish_reason,
    };

    // usage is returned separately so the bridge can emit a `Usage` stream event
    // ahead of `Done` (matching the OpenAI provider's event order); it is also
    // embedded in `response.usage` for consumers that only read `Done`.
    let usage_event = response.usage;
    (usage_event, response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use openslate_core::types::{Message, MessageRole, ToolCall, ToolCallId};
    use serde_json::json;

    /// G1 regression: an assistant message carrying tool calls must convert to a
    /// genai ChatMessage whose content contains tool-call parts (not plain text).
    #[test]
    fn assistant_with_tool_calls_keeps_tool_calls() {
        let msg = Message {
            role: MessageRole::Assistant,
            content: String::new(),
            tool_call_id: None,
            name: None,
            tool_calls: Some(vec![ToolCall {
                id: ToolCallId("call_1".into()),
                name: "search".into(),
                arguments: json!({"q": "rust"}),
            }]),
        };

        let genai_msg = to_chat_message(&msg);

        // An assistant message built from tool calls has tool-call parts; a
        // plain-text assistant message has none. Assert via the serialized form:
        // genai tool-call parts carry the function name.
        let serialized = serde_json::to_string(&genai_msg).expect("serialize");
        assert!(
            serialized.contains("search"),
            "tool call name 'search' must survive conversion; got: {serialized}"
        );
        assert!(
            serialized.contains("call_1"),
            "tool call id 'call_1' must survive conversion; got: {serialized}"
        );
    }

    /// A tool-result message must carry the originating `call_id` so the model
    /// can correlate the result with its tool_use.
    #[test]
    fn tool_result_carries_call_id() {
        let msg = Message {
            role: MessageRole::Tool,
            content: "42 results".into(),
            tool_call_id: Some(ToolCallId("call_1".into())),
            name: Some("search".into()),
            tool_calls: None,
        };

        let genai_msg = to_chat_message(&msg);
        let serialized = serde_json::to_string(&genai_msg).expect("serialize");
        assert!(
            serialized.contains("call_1"),
            "tool result must carry call_id 'call_1'; got: {serialized}"
        );
    }

    /// Plain assistant text (no tool calls) maps to a normal assistant message.
    #[test]
    fn assistant_text_maps_to_text() {
        let msg = Message {
            role: MessageRole::Assistant,
            content: "hello".into(),
            tool_call_id: None,
            name: None,
            tool_calls: None,
        };
        let genai_msg = to_chat_message(&msg);
        let serialized = serde_json::to_string(&genai_msg).expect("serialize");
        assert!(serialized.contains("hello"));
        // Should NOT look like a tool-call message (no "tool_calls" content part).
        assert!(
            !serialized.contains("\"call_id\""),
            "plain assistant text should not produce tool-call parts; got: {serialized}"
        );
    }

    #[test]
    fn null_tool_schema_is_omitted() {
        let td = ToolDefinition {
            name: "t".into(),
            description: "d".into(),
            parameters: serde_json::Value::Null,
        };
        let tool = to_genai_tool(&td);
        assert!(tool.schema.is_none(), "null schema should be omitted");
    }

    #[test]
    fn object_tool_schema_is_attached() {
        let td = ToolDefinition {
            name: "t".into(),
            description: "d".into(),
            parameters: json!({"type": "object"}),
        };
        let tool = to_genai_tool(&td);
        assert!(tool.schema.is_some());
    }

    /// Regression (P1): an assistant turn carrying BOTH text and tool calls
    /// must preserve both — the old `ChatMessage::from(Vec<ToolCall>)` mapping
    /// silently dropped the text.
    #[test]
    fn assistant_text_and_tool_calls_both_preserved() {
        let msg = Message {
            role: MessageRole::Assistant,
            content: "Let me look that up.".into(),
            tool_call_id: None,
            name: None,
            tool_calls: Some(vec![ToolCall {
                id: ToolCallId("call_1".into()),
                name: "search".into(),
                arguments: json!({"q": "rust"}),
            }]),
        };

        let genai_msg = to_chat_message(&msg);
        let serialized = serde_json::to_string(&genai_msg).expect("serialize");
        assert!(
            serialized.contains("Let me look that up."),
            "assistant text must survive alongside tool calls; got: {serialized}"
        );
        assert!(
            serialized.contains("search") && serialized.contains("call_1"),
            "tool calls must survive alongside text; got: {serialized}"
        );
    }

    /// Regression (P1): multiple text parts must be newline-joined, both for
    /// non-streaming responses and stream ends — `first_text()` silently
    /// dropped every part after the first.
    #[test]
    fn multiple_text_parts_are_joined() {
        let parts = || {
            MessageContent::from_parts(vec![
                ContentPart::Text("part one".into()),
                ContentPart::Text("part two".into()),
            ])
        };

        // Non-streaming path.
        let model_iden = genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "test-model");
        let res = ChatResponse {
            content: parts(),
            reasoning_content: None,
            model_iden: model_iden.clone(),
            provider_model_iden: model_iden,
            stop_reason: None,
            usage: genai::chat::Usage {
                prompt_tokens: None,
                prompt_tokens_details: None,
                completion_tokens: None,
                completion_tokens_details: None,
                total_tokens: None,
            },
            captured_raw_body: None,
            response_id: None,
        };
        let mr = from_chat_response(res);
        assert_eq!(
            mr.content.as_deref(),
            Some("part one\npart two"),
            "from_chat_response must join all text parts"
        );

        // Streaming path.
        let end = StreamEnd {
            captured_usage: None,
            captured_stop_reason: None,
            captured_content: Some(parts()),
            captured_reasoning_content: None,
            captured_response_id: None,
        };
        let (_, mr) = from_stream_end(end);
        assert_eq!(
            mr.content.as_deref(),
            Some("part one\npart two"),
            "from_stream_end must join all text parts"
        );
    }

    /// Regression: a tool-result message without `tool_call_id` takes the warn
    /// path — it must not panic and still maps to an empty call_id.
    #[test]
    fn missing_tool_call_id_warns_and_maps_empty() {
        let msg = Message {
            role: MessageRole::Tool,
            content: "42 results".into(),
            tool_call_id: None,
            name: Some("search".into()),
            tool_calls: None,
        };

        let genai_msg = to_chat_message(&msg);
        let serialized = serde_json::to_string(&genai_msg).expect("serialize");
        assert!(
            serialized.contains("\"call_id\":\"\""),
            "missing call_id should map to empty string; got: {serialized}"
        );
    }

    /// Cached input tokens survive both conversion paths when the provider
    /// reports `prompt_tokens_details.cached_tokens` (genai normalizes
    /// Anthropic's `cache_read_input_tokens` into the same field).
    #[test]
    fn cached_input_tokens_parsed_on_both_paths() {
        let genai_usage = || genai::chat::Usage {
            prompt_tokens: Some(10),
            prompt_tokens_details: Some(genai::chat::PromptTokensDetails {
                cached_tokens: Some(4),
                ..Default::default()
            }),
            completion_tokens: Some(2),
            ..Default::default()
        };

        // Non-streaming path.
        let model_iden = genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "m");
        let res = ChatResponse {
            content: MessageContent::from_text("hi"),
            reasoning_content: None,
            model_iden: model_iden.clone(),
            provider_model_iden: model_iden,
            stop_reason: None,
            usage: genai_usage(),
            captured_raw_body: None,
            response_id: None,
        };
        let mr = from_chat_response(res);
        let usage = mr.usage.expect("usage present");
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 2);
        assert_eq!(usage.cached_input_tokens, Some(4));

        // Streaming path: the usage event AND the embedded response usage.
        let end = StreamEnd {
            captured_usage: Some(genai_usage()),
            captured_stop_reason: None,
            captured_content: Some(MessageContent::from_text("hi")),
            captured_reasoning_content: None,
            captured_response_id: None,
        };
        let (usage_event, mr) = from_stream_end(end);
        assert_eq!(
            usage_event.expect("usage event").cached_input_tokens,
            Some(4)
        );
        assert_eq!(mr.usage.expect("usage").cached_input_tokens, Some(4));
    }

    /// When the server does not report cache details the field stays `None`
    /// (absent details object, details without `cached_tokens`, and a
    /// reported 0 — which genai's `zero_as_none` already folds to None).
    #[test]
    fn cached_input_tokens_absent_when_unreported() {
        let cases = [
            genai::chat::Usage {
                prompt_tokens: Some(5),
                completion_tokens: Some(1),
                ..Default::default()
            },
            genai::chat::Usage {
                prompt_tokens: Some(5),
                prompt_tokens_details: Some(Default::default()),
                completion_tokens: Some(1),
                ..Default::default()
            },
            genai::chat::Usage {
                prompt_tokens: Some(5),
                prompt_tokens_details: Some(genai::chat::PromptTokensDetails {
                    cached_tokens: None,
                    ..Default::default()
                }),
                completion_tokens: Some(1),
                ..Default::default()
            },
        ];
        for usage in cases {
            let model_iden = genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "m");
            let res = ChatResponse {
                content: MessageContent::from_text("hi"),
                reasoning_content: None,
                model_iden: model_iden.clone(),
                provider_model_iden: model_iden,
                stop_reason: None,
                usage,
                captured_raw_body: None,
                response_id: None,
            };
            let mr = from_chat_response(res);
            assert_eq!(
                mr.usage.expect("usage").cached_input_tokens,
                None,
                "unreported cache must stay None"
            );
        }
    }
}
