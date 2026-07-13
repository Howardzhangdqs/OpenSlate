//! Bidirectional conversion between OpenSlate core types and genai types.
//!
//! # Critical invariant (G1)
//!
//! An assistant turn carrying tool calls MUST be mapped to a genai
//! `ChatMessage::from(Vec<ToolCall>)` (which builds an assistant message with
//! `ContentPart::ToolCall` entries), NOT to `ChatMessage::assistant(content)`.
//! The plain-text form silently drops tool calls and breaks multi-step tool
//! loops — the model would see a tool result with no corresponding tool_use.
//! Likewise a tool-result message maps to `ChatMessage::from(ToolResponse { .. })`.

use openslate_core::provider::{GenerateRequest, ToolDefinition};
use openslate_core::types::{Message, MessageRole, ModelResponse, ToolCall, ToolCallId, Usage};

use genai::chat::{
    ChatMessage, ChatRequest, ChatResponse, StreamEnd, Tool as GenaiTool,
    ToolCall as GenaiToolCall, ToolResponse,
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
            // be flattened to plain text.
            if let Some(tcs) = msg.tool_calls.as_ref().filter(|v| !v.is_empty()) {
                let genai_tcs: Vec<GenaiToolCall> = tcs.iter().map(to_genai_tool_call).collect();
                ChatMessage::from(genai_tcs)
            } else {
                ChatMessage::assistant(msg.content.clone())
            }
        }
        MessageRole::Tool => {
            let call_id = msg
                .tool_call_id
                .as_ref()
                .map(|id| id.0.clone())
                .unwrap_or_default();
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

/// Convert a non-streaming genai [`ChatResponse`] into an OpenSlate
/// [`ModelResponse`].
///
/// `usage` fields in genai are `Option<i32>` (nullable, since OpenAI returns 0
/// for non-applicable counters and genai deserializes 0 as None); they are
/// clamped to `u32`.
pub(crate) fn from_chat_response(res: ChatResponse) -> ModelResponse {
    let content = res.first_text().map(str::to_owned);
    let usage = Usage {
        input_tokens: res.usage.prompt_tokens.unwrap_or(0).max(0) as u32,
        output_tokens: res.usage.completion_tokens.unwrap_or(0).max(0) as u32,
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
    });

    let content = end
        .captured_content
        .as_ref()
        .and_then(|c| c.first_text())
        .map(str::to_owned);

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
}
