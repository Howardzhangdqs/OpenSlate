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
//!
//! # Tool 附件 1:N 展开（读图链路，2026-10）
//!
//! genai 的 `ToolResponse.content` 纯 String——tool 通道无法携带 Binary。
//! 带 `attachments` 的 Tool 消息由 [`to_chat_request`] 经
//! `expand_chat_message` 展开为两条：ToolResponse（正文照发）+ 紧随的
//! user 消息（`Text("[read_file 返回的图片：{mime}]")` + Binary parts）。
//! 无附件 / 附件全为引用（data=None）的 Tool 消息保持单条 wire 不变。

use openslate_core::provider::{GenerateRequest, ToolDefinition};
use openslate_core::types::{Message, MessageRole, ModelResponse, ToolCall, ToolCallId, Usage};

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use genai::chat::{
    Binary, ChatMessage, ChatRequest, ChatResponse, ContentPart, MessageContent, StreamEnd,
    Tool as GenaiTool, ToolCall as GenaiToolCall, ToolResponse,
};

/// 单请求图像附件兜底上限（智谱 GLM-5.3-Flash 事实：≤50 张）。主门控在
/// mobile 层 Submit 处，这里只是防直接 API 调用路径的兜底——超出部分
/// 跳过并 warn，不报错。
const MAX_IMAGES_PER_REQUEST: usize = 50;

// ---------------------------------------------------------------------------
// GenerateRequest  ->  ChatRequest
// ---------------------------------------------------------------------------

/// Convert an OpenSlate [`GenerateRequest`] into a genai [`ChatRequest`].
///
/// 消息映射是 **1:N 展开**（read_file 读图链路，2026-10）：带附件的 Tool
/// 消息展开为 ToolResponse + 紧随的 user Binary parts 两条（genai 的
/// `ToolResponse.content` 纯 String，tool 通道无法携带 Binary——wire 标
/// 准做法 = 工具结果正文照发，图片字节走紧随的 user 消息）；其余消息
/// 1:1（见 [`expand_chat_message`]）。
pub(crate) fn to_chat_request(req: &GenerateRequest) -> ChatRequest {
    let messages: Vec<ChatMessage> = req.messages.iter().flat_map(expand_chat_message).collect();

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

/// 1:N 消息展开（读图链路）：Tool 消息带非空 `attachments` 时 →
/// ① ToolResponse（content 原文，call_id 语义不变）② 紧随一条 user
/// 消息携带附件 Binary parts（`Text("[read_file 返回的图片：{mime}]")`
/// 打头 + 每个 data=Some 的附件一个 Binary，base64 内联）。data=None
/// 的引用（resume 后未回填 / 请求路径超封顶被丢弃）跳过 + warn，同
/// User 分支语义（模型丢图不丢文本）；全部跳过时**不**追加空壳 user
/// 消息（纯文本 ToolResponse 照旧，wire 不变）。其余消息 1:1。
fn expand_chat_message(msg: &Message) -> Vec<ChatMessage> {
    if msg.role != MessageRole::Tool {
        return vec![to_chat_message(msg)];
    }
    let Some(attachments) = msg.attachments.as_ref().filter(|v| !v.is_empty()) else {
        return vec![to_chat_message(msg)];
    };

    let mut binaries: Vec<ContentPart> = Vec::with_capacity(attachments.len());
    let mut mimes: Vec<&str> = Vec::with_capacity(attachments.len());
    let mut images = 0usize;
    for att in attachments {
        let Some(data) = att.data.as_ref() else {
            tracing::warn!(
                attachment_id = %att.id,
                mime = %att.mime,
                "tool attachment has no inline data (not backfilled or over cap); skipping"
            );
            continue;
        };
        if att.mime.starts_with("image/") {
            images += 1;
            if images > MAX_IMAGES_PER_REQUEST {
                tracing::warn!(
                    attachment_id = %att.id,
                    limit = MAX_IMAGES_PER_REQUEST,
                    "too many image attachments on tool result; dropping extra"
                );
                continue;
            }
        }
        mimes.push(att.mime.as_str());
        binaries.push(ContentPart::Binary(Binary::from_base64(
            att.mime.clone(),
            BASE64_STANDARD.encode(&**data),
            att.filename.clone(),
        )));
    }
    if binaries.is_empty() {
        // 没有可发字节：不追加空壳 user 消息（ToolResponse 单条照旧）。
        return vec![to_chat_message(msg)];
    }

    let label = if mimes.len() == 1 {
        format!("read_file 返回的图片：{}", mimes[0])
    } else {
        format!("read_file 返回的图片 ×{}：{}", mimes.len(), mimes.join(", "))
    };
    let mut parts: Vec<ContentPart> = Vec::with_capacity(1 + binaries.len());
    // 文本永远第一（provider 对 parts 顺序敏感，同 User 分支）。
    parts.push(ContentPart::Text(format!("[{label}]")));
    parts.extend(binaries);

    vec![
        to_chat_message(msg), // ① ToolResponse（content 原文）
        ChatMessage::user(MessageContent::from_parts(parts)), // ② 图片字节
    ]
}

fn to_chat_message(msg: &Message) -> ChatMessage {
    match msg.role {
        MessageRole::System => ChatMessage::system(msg.content.clone()),
        // User 分支（多模态输入侧，L3）：
        // - 无附件 → 纯文本（wire 字节级不变，历史测试锁定）。
        // - 有附件 → parts：Text 永远第一，随后每个 data=Some 的附件一个
        //   Binary part（base64 内联，vendor adapter 产 image_url/video_url
        //   块）。data=None 的引用（未回填的旧附件）跳过 + warn，绝不发
        //   空 Binary。
        // 智谱约束（GLM-5.3-Flash 官方文档，2026-10）：file 块与
        // image_url/video_url 互斥；图/视频不混发（本轮附件纯图或纯
        // 视频）。本层永不构造 file 块；图/视频混发的门控在 mobile 层
        // Submit 处（设计 §3.4，单点常量，实测可放开）。
        MessageRole::User => match msg.attachments.as_ref().filter(|v| !v.is_empty()) {
            None => ChatMessage::user(msg.content.clone()),
            Some(attachments) => {
                let mut parts: Vec<ContentPart> = Vec::with_capacity(1 + attachments.len());
                // 文本永远第一（provider 对 parts 顺序敏感）。
                parts.push(ContentPart::Text(msg.content.clone()));
                let mut images = 0usize;
                for att in attachments {
                    let Some(data) = att.data.as_ref() else {
                        tracing::warn!(
                            attachment_id = %att.id,
                            mime = %att.mime,
                            "user attachment has no inline data (not backfilled); skipping"
                        );
                        continue;
                    };
                    if att.mime.starts_with("image/") {
                        images += 1;
                        if images > MAX_IMAGES_PER_REQUEST {
                            tracing::warn!(
                                attachment_id = %att.id,
                                limit = MAX_IMAGES_PER_REQUEST,
                                "too many image attachments in one request; dropping extra"
                            );
                            continue;
                        }
                    }
                    parts.push(ContentPart::Binary(Binary::from_base64(
                        att.mime.clone(),
                        BASE64_STANDARD.encode(&**data),
                        att.filename.clone(),
                    )));
                }
                ChatMessage::user(MessageContent::from_parts(parts))
            }
        },
        MessageRole::Assistant => {
            // G1: assistant turns with tool calls must carry the tool calls, not
            // be flattened to plain text. Any accompanying text must survive
            // too — genai maps Text parts to `content` and ToolCall parts to
            // `tool_calls`, so a parts-based message keeps both on the wire.
            // 思维链回传（Anthropic 多轮语义：历史 thinking 块随 assistant
            // 消息原样回传；genai anthropic adapter 输出为 thinking block）。
            let reasoning = msg
                .reasoning_content
                .as_ref()
                .filter(|r| !r.is_empty())
                .map(|r| ContentPart::ReasoningContent(r.clone()));
            if let Some(tcs) = msg.tool_calls.as_ref().filter(|v| !v.is_empty()) {
                let genai_tcs: Vec<GenaiToolCall> = tcs.iter().map(to_genai_tool_call).collect();
                let mut parts: Vec<ContentPart> = Vec::with_capacity(
                    usize::from(!msg.content.is_empty()) + usize::from(reasoning.is_some())
                        + genai_tcs.len(),
                );
                if let Some(rc) = reasoning {
                    parts.push(rc);
                }
                if !msg.content.is_empty() {
                    parts.push(ContentPart::Text(msg.content.clone()));
                }
                parts.extend(genai_tcs.into_iter().map(ContentPart::ToolCall));
                ChatMessage::assistant(MessageContent::from_parts(parts))
            } else if let Some(rc) = reasoning {
                let mut parts: Vec<ContentPart> =
                    Vec::with_capacity(1 + usize::from(!msg.content.is_empty()));
                parts.push(rc);
                if !msg.content.is_empty() {
                    parts.push(ContentPart::Text(msg.content.clone()));
                }
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

/// Extract the reasoning-token breakdown from a genai usage struct (normalized
/// `completion_tokens_details.reasoning_tokens`, filled by the OpenAI/Gemini
/// adapters and — via our vendor patch — by Anthropic-compatible gateways that
/// inject a split). `None` when the provider reports only a combined output
/// count; genai's `zero_as_none` convention already folds a reported 0.
fn reasoning_tokens(u: &genai::chat::Usage) -> Option<u32> {
    u.completion_tokens_details
        .as_ref()
        .and_then(|d| d.reasoning_tokens)
        .filter(|t| *t > 0)
        .map(|t| t.max(0) as u32)
}

/// Image input tokens（vendor patch #3：OpenAI 系
/// `prompt_tokens_details.image_tokens` → genai 顶层归一化计数
/// `image_input_tokens`）。`None` = provider 不分模态（智谱 chat usage
/// 恒 None——附件 chip 回退文件大小，符合预期）。正数才透传（genai 侧
/// 已按 zero_as_none 折 0）。
fn image_tokens(u: &genai::chat::Usage) -> Option<u64> {
    u.image_input_tokens
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
        reasoning_tokens: reasoning_tokens(&res.usage),
        image_tokens: image_tokens(&res.usage),
    };
    let finish_reason = res.stop_reason.as_ref().map(|sr| sr.raw().to_string());
    let reasoning = res.reasoning_content.clone().filter(|r| !r.is_empty());

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
        reasoning_content: reasoning,
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
#[cfg(not(target_os = "android"))]
fn openslate_mobile_compat_log(
    _finish: &Option<String>,
    _content: &Option<String>,
    _tools: &[ToolCall],
) {
}

#[cfg(target_os = "android")]
fn openslate_mobile_compat_log(
    finish: &Option<String>,
    content: &Option<String>,
    tools: &[ToolCall],
) {
    // 最小依赖：直写 liblog，避免 crate 间耦合。
    use std::ffi::CString;
    unsafe extern "C" {
        #[link_name = "__android_log_print"]
        fn alog(prio: i32, tag: *const std::os::raw::c_char, fmt: *const std::os::raw::c_char, ...)
            -> i32;
    }
    let msg = format!(
        "stream_end: finish={:?} content_len={} tools={}",
        finish,
        content.as_ref().map(|c| c.len()).unwrap_or(0),
        tools.len()
    );
    let tag = CString::new("OpenSlateRust").unwrap();
    let fmt = CString::new("%s").unwrap();
    let m = CString::new(msg).unwrap();
    unsafe { alog(4, tag.as_ptr(), fmt.as_ptr(), m.as_ptr()) };
}

pub(crate) fn from_stream_end(end: StreamEnd) -> (Option<Usage>, ModelResponse) {
    let usage = end.captured_usage.map(|u| Usage {
        input_tokens: u.prompt_tokens.unwrap_or(0).max(0) as u32,
        output_tokens: u.completion_tokens.unwrap_or(0).max(0) as u32,
        cached_input_tokens: cached_input_tokens(&u),
        reasoning_tokens: reasoning_tokens(&u),
        image_tokens: image_tokens(&u),
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

    // 诊断可见性（logcat）：每步结束打一行——空回合排查的关键锚点。
    openslate_mobile_compat_log(&finish_reason, &content, &tool_calls);

    let response = ModelResponse {
        content,
        // 思维链全文随响应走（engine 侧写入 assistant 消息并回传）。
        reasoning_content: end
            .captured_reasoning_content
            .clone()
            .filter(|r| !r.is_empty()),
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
        attachments: None,
        reasoning_content: None,
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
            attachments: None,
            role: MessageRole::Tool,
            content: "42 results".into(),
            tool_call_id: Some(ToolCallId("call_1".into())),
            name: Some("search".into()),
            tool_calls: None,
            reasoning_content: None,
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
            attachments: None,
            role: MessageRole::Assistant,
            content: "hello".into(),
            tool_call_id: None,
            name: None,
            tool_calls: None,
            reasoning_content: None,
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
        attachments: None,
        reasoning_content: None,
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
                image_input_tokens: None,
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
            attachments: None,
            role: MessageRole::Tool,
            content: "42 results".into(),
            tool_call_id: None,
            name: Some("search".into()),
            tool_calls: None,
            reasoning_content: None,
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

    /// Reasoning tokens survive both conversion paths when the provider (or an
    /// Anthropic-compatible gateway, via the vendor patch) reports
    /// `completion_tokens_details.reasoning_tokens`.
    #[test]
    fn reasoning_tokens_parsed_on_both_paths() {
        let genai_usage = || genai::chat::Usage {
            prompt_tokens: Some(10),
            completion_tokens: Some(9),
            completion_tokens_details: Some(genai::chat::CompletionTokensDetails {
                reasoning_tokens: Some(6),
                ..Default::default()
            }),
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
        assert_eq!(mr.usage.expect("usage present").reasoning_tokens, Some(6));

        // Streaming path: the usage event AND the embedded response usage.
        let end = StreamEnd {
            captured_usage: Some(genai_usage()),
            captured_stop_reason: None,
            captured_content: Some(MessageContent::from_text("hi")),
            captured_reasoning_content: None,
            captured_response_id: None,
        };
        let (usage_event, mr) = from_stream_end(end);
        assert_eq!(usage_event.expect("usage event").reasoning_tokens, Some(6));
        assert_eq!(mr.usage.expect("usage").reasoning_tokens, Some(6));
    }

    /// Without a reported split the field stays `None` (absent details object,
    /// details without `reasoning_tokens`, and a reported 0).
    #[test]
    fn reasoning_tokens_absent_when_unreported() {
        let cases = [
            genai::chat::Usage {
                prompt_tokens: Some(5),
                completion_tokens: Some(1),
                ..Default::default()
            },
            genai::chat::Usage {
                prompt_tokens: Some(5),
                completion_tokens: Some(1),
                completion_tokens_details: Some(Default::default()),
                ..Default::default()
            },
            genai::chat::Usage {
                prompt_tokens: Some(5),
                completion_tokens: Some(1),
                completion_tokens_details: Some(genai::chat::CompletionTokensDetails {
                    reasoning_tokens: None,
                    ..Default::default()
                }),
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
                mr.usage.expect("usage").reasoning_tokens,
                None,
                "unreported split must stay None"
            );
        }
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

    // ── 多模态输入侧（L3）：User 分支附件 ───────────────────────────

    use openslate_core::types::Attachment;
    use std::sync::Arc;

    fn att(id: &str, mime: &str, filename: Option<&str>, data: Option<&[u8]>) -> Attachment {
        Attachment {
            id: id.into(),
            mime: mime.into(),
            filename: filename.map(str::to_owned),
            data: data.map(Arc::from),
        }
    }

    /// 带 data 的附件转为 Binary part：Text 永远第一、mime 透传、
    /// base64 为标准字母表编码、filename 透传。顺序（text → png → mp4）
    /// 与附件顺序一致。
    #[test]
    fn user_attachments_map_to_binary_parts_text_first() {
        let msg = Message {
            role: MessageRole::User,
            content: "describe these".into(),
            attachments: Some(vec![
                att("att-1", "image/png", Some("shot.png"), Some(b"hello")),
                att("att-2", "video/mp4", None, Some(b"vid")),
            ]),
            tool_call_id: None,
            name: None,
            tool_calls: None,
            reasoning_content: None,
        };

        let genai_msg = to_chat_message(&msg);
        let v = serde_json::to_value(&genai_msg).expect("serialize");
        let content = v["content"].as_array().expect("content is a parts array");

        assert_eq!(content.len(), 3, "text + two binaries");
        // 文本永远第一。
        assert_eq!(content[0], json!({"Text": "describe these"}));
        // image：mime 透传 + 标准 base64（b"hello" → aGVsbG8=）+ filename 透传。
        assert_eq!(content[1]["Binary"]["content_type"], "image/png");
        assert_eq!(content[1]["Binary"]["source"]["Base64"], "aGVsbG8=");
        assert_eq!(content[1]["Binary"]["name"], "shot.png");
        // video：同型（无 filename → null）。
        assert_eq!(content[2]["Binary"]["content_type"], "video/mp4");
        assert_eq!(content[2]["Binary"]["source"]["Base64"], "dmlk");
        assert_eq!(content[2]["Binary"]["name"], serde_json::Value::Null);
    }

    /// 无附件 → wire 字节级不变（与直接 ChatMessage::user 等价，包括
    /// 空附件 vec 与 attachments: None 两种形态）。
    #[test]
    fn user_without_attachments_wire_unchanged() {
        let mk = |attachments| Message {
            role: MessageRole::User,
            content: "plain question".into(),
            attachments,
            tool_call_id: None,
            name: None,
            tool_calls: None,
            reasoning_content: None,
        };
        let expected = serde_json::to_string(&ChatMessage::user("plain question")).unwrap();
        assert_eq!(
            serde_json::to_string(&to_chat_message(&mk(None))).unwrap(),
            expected,
            "attachments: None must keep the plain-text wire form"
        );
        assert_eq!(
            serde_json::to_string(&to_chat_message(&mk(Some(vec![])))).unwrap(),
            expected,
            "empty attachments vec must keep the plain-text wire form"
        );
    }

    /// data=None 的引用（未回填的旧附件）跳过，绝不发空 Binary；剩余
    /// 有 data 的附件正常发送。
    #[test]
    fn user_attachment_without_data_is_skipped() {
        let msg = Message {
            role: MessageRole::User,
            content: "one image attached".into(),
            attachments: Some(vec![
                att("att-stale", "image/jpeg", Some("old.jpg"), None),
                att("att-live", "image/jpeg", None, Some(b"ok")),
            ]),
            tool_call_id: None,
            name: None,
            tool_calls: None,
            reasoning_content: None,
        };

        let genai_msg = to_chat_message(&msg);
        let v = serde_json::to_value(&genai_msg).expect("serialize");
        let content = v["content"].as_array().expect("content is a parts array");
        assert_eq!(content.len(), 2, "text + only the backfilled binary");
        assert_eq!(content[1]["Binary"]["source"]["Base64"], "b2s=");
    }

    /// 图像 >50 兜底：超出上限的图像附件跳过（门控在 mobile 层，此处
    /// 防直接 API 调用）。视频不占图像配额。
    #[test]
    fn user_more_than_50_images_drops_extras() {
        let attachments: Vec<Attachment> = (0..52)
            .map(|i| att(&format!("att-img-{i}"), "image/png", None, Some(b"x")))
            .chain(std::iter::once(att(
                "att-vid",
                "video/mp4",
                None,
                Some(b"v"),
            )))
            .collect();
        let msg = Message {
            role: MessageRole::User,
            content: "many".into(),
            attachments: Some(attachments),
            tool_call_id: None,
            name: None,
            tool_calls: None,
            reasoning_content: None,
        };

        let genai_msg = to_chat_message(&msg);
        let v = serde_json::to_value(&genai_msg).expect("serialize");
        let content = v["content"].as_array().expect("content is a parts array");
        // 1 text + 前 50 张图（索引 1..=50）+ 1 视频（索引 51，不占图像配额）。
        assert_eq!(content.len(), 1 + 50 + 1);
        assert_eq!(content[1]["Binary"]["content_type"], "image/png");
        assert_eq!(content[50]["Binary"]["content_type"], "image/png");
        assert_eq!(content[51]["Binary"]["content_type"], "video/mp4");
    }

    // ── Tool 附件 1:N 展开（读图链路，2026-10）────────────────────

    fn tool_msg_with(attachments: Option<Vec<Attachment>>) -> Message {
        Message {
            role: MessageRole::Tool,
            content: "[image file: shot.png, image/png, 3000 bytes]".into(),
            attachments,
            tool_call_id: Some(ToolCallId("call_9".into())),
            name: Some("termux_read_file".into()),
            tool_calls: None,
            reasoning_content: None,
        }
    }

    fn req_of(messages: Vec<Message>) -> GenerateRequest {
        GenerateRequest {
            model_id: "test-model".into(),
            messages,
            tools: Vec::new(),
            system_prompt: None,
            max_tokens: None,
            temperature: None,
        }
    }

    /// Tool + 附件 → ToolResponse + 紧随 user Binary 两条：第一条 role=
    /// tool、content 原文、call_id 保留；第二条 role=user、parts =
    /// [Text 说明, Binary(base64)]。
    #[test]
    fn tool_attachments_expand_to_response_plus_user_binary() {
        let attachment = att("tool-0", "image/png", Some("shot.png"), Some(b"png!"));
        let req = req_of(vec![tool_msg_with(Some(vec![attachment]))]);

        let chat_req = to_chat_request(&req);
        assert_eq!(chat_req.messages.len(), 2, "1:N 展开");

        let first = serde_json::to_value(&chat_req.messages[0]).unwrap();
        assert_eq!(first["role"], "Tool", "{first}");
        assert!(
            first.to_string().contains("[image file: shot.png"),
            "ToolResponse content 原文：{first}"
        );
        assert!(
            first.to_string().contains("call_9"),
            "call_id 保留：{first}"
        );

        let second = serde_json::to_value(&chat_req.messages[1]).unwrap();
        assert_eq!(second["role"], "User", "{second}");
        let content = second["content"].as_array().expect("parts array");
        assert_eq!(content.len(), 2, "Text + Binary");
        let text = content[0]["Text"].as_str().expect("text part first");
        assert!(
            text.contains("read_file 返回的图片：image/png"),
            "说明文本打头：{text}"
        );
        assert_eq!(content[1]["Binary"]["content_type"], "image/png");
        assert_eq!(content[1]["Binary"]["source"]["Base64"], "cG5nIQ==");
        assert_eq!(content[1]["Binary"]["name"], "shot.png");
    }

    /// 多附件：Text 说明聚合 ×N 与 mime 清单，Binary 顺序与附件一致。
    #[test]
    fn tool_multiple_attachments_aggregate_label() {
        let req = req_of(vec![tool_msg_with(Some(vec![
            att("tool-0", "image/png", None, Some(b"a")),
            att("tool-1", "image/jpeg", None, Some(b"b")),
        ]))]);

        let chat_req = to_chat_request(&req);
        assert_eq!(chat_req.messages.len(), 2);
        let second = serde_json::to_value(&chat_req.messages[1]).unwrap();
        let content = second["content"].as_array().unwrap();
        assert_eq!(content.len(), 3, "Text + 2 Binary");
        let text = content[0]["Text"].as_str().unwrap();
        assert!(
            text.contains("×2") && text.contains("image/png, image/jpeg"),
            "{text}"
        );
        assert_eq!(content[1]["Binary"]["content_type"], "image/png");
        assert_eq!(content[2]["Binary"]["content_type"], "image/jpeg");
    }

    /// Tool 无附件 / 空附件 / 附件全为引用（data=None）→ wire 单条不变
    /// （不追加空壳 user 消息）。
    #[test]
    fn tool_without_live_attachments_wire_unchanged() {
        let mk = |attachments| {
            serde_json::to_string(&to_chat_request(&req_of(vec![tool_msg_with(attachments)])))
                .unwrap()
        };
        let baseline = mk(None);
        assert_eq!(mk(Some(vec![])), baseline, "空附件 vec 单条不变");
        // data=None 引用：跳过 + 不追加 user 消息（消息数锁定为 1）。
        let stale = mk(Some(vec![att("tool-0", "image/png", None, None)]));
        assert_eq!(stale, baseline, "全引用附件单条不变：{stale}");
    }

    /// 展开不打扰相邻消息：user → tool(带图) → 展开后顺序 =
    /// user, ToolResponse, user(Binary)。
    #[test]
    fn expansion_preserves_surrounding_order() {
        let req = req_of(vec![
            Message {
                role: MessageRole::User,
                content: "look".into(),
                attachments: None,
                tool_call_id: None,
                name: None,
                tool_calls: None,
                reasoning_content: None,
            },
            tool_msg_with(Some(vec![att("tool-0", "image/png", None, Some(b"z"))])),
        ]);

        let chat_req = to_chat_request(&req);
        assert_eq!(chat_req.messages.len(), 3);
        let roles: Vec<String> = chat_req
            .messages
            .iter()
            .map(|m| {
                serde_json::to_value(m).unwrap()["role"].as_str().unwrap().to_owned()
            })
            .collect();
        assert_eq!(
            roles,
            vec!["User".to_string(), "Tool".into(), "User".into()],
            "{roles:?}"
        );
    }
}
