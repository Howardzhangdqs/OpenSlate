//! Core ID types and data structures for OpenSlate.

use std::fmt;
use std::str::FromStr;

macro_rules! id_type {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
        pub struct $name(pub String);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl FromStr for $name {
            type Err = std::convert::Infallible;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(Self(s.to_owned()))
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_owned())
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

id_type!(AgentId, "Unique identifier for an agent definition.");
id_type!(RunId, "Unique identifier for a single agent run.");
id_type!(StepId, "Unique identifier for a step within a run.");
id_type!(
    ExecutionNodeId,
    "Unique identifier for a node in the execution tree."
);
id_type!(
    ToolCallId,
    "Unique identifier for a tool call within a step."
);

/// The role of a conversation message.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

/// A single message in a conversation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Message {
    pub role: MessageRole,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<ToolCallId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// 模型思维链（extended thinking）。多轮对话按 Anthropic 语义应随
    /// assistant 消息回传（历史 thinking 块）；持久化自动携带。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// 消息附件（多模态输入侧，本期仅 user 消息使用）。additive 字段：
    /// 旧 payload（无此字段）反序列化为 `None`，序列化时 `None` 省略，
    /// 存量 wire 行为不变。条目只持久化引用形状，见 [`Attachment`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<Attachment>>,
}

/// 消息附件。持久化形状 = {id, mime, filename}（引用，DB/协议 wire 都是这个
/// 形状）；`data` 仅供内存中直达 provider（ingest 层填充、#[serde(skip)]
/// 永不序列化），resume 恢复时由 mobile 层可选回填。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Attachment {
    pub id: String,
    pub mime: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(skip, default)]
    pub data: Option<std::sync::Arc<[u8]>>,
}

/// Static configuration for a single agent.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AgentConfig {
    pub id: AgentId,
    pub name: String,
    pub model: String,
    #[serde(default)]
    pub children: Vec<AgentId>,
    #[serde(default)]
    pub tools: Vec<String>,
    /// 工具隐藏名单（与 `tools` 同一套 glob 匹配：无 `*`/`?` 精确名，
    /// 否则通配）。生效集合 =（`tools` 为空 → 全部注册工具；否则白名
    /// 单匹配）减去 `hidden_tools` 匹配项。默认空 = 行为不变。
    #[serde(default)]
    pub hidden_tools: Vec<String>,
    pub default_prompt: String,
}

/// Status of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Running,
    Completed,
    Failed,
    Interrupted,
    Cancelled,
}

/// Discriminator for the kind of step recorded.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StepKind(pub String);

impl StepKind {
    /// A call to an LLM model.
    pub const MODEL_CALL: &'static str = "model_call";
    /// A call to a tool.
    pub const TOOL_CALL: &'static str = "tool_call";
    /// A call to a child agent.
    pub const CHILD_AGENT_CALL: &'static str = "child_agent_call";
}

/// The response returned by an LLM model call.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ModelResponse {
    pub content: Option<String>,
    /// 本步思维链全文（流式 ReasoningChunk 聚合）。
    #[serde(default)]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
    pub finish_reason: Option<String>,
}

/// A single tool call requested by the model.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolCall {
    pub id: ToolCallId,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// Token usage statistics from a model call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    /// Input tokens served from the provider's prompt cache (OpenAI's
    /// `usage.prompt_tokens_details.cached_tokens`, Anthropic's
    /// `cache_read_input_tokens`). `None` when the provider does not
    /// report cache information. Additive field: older serialized
    /// payloads (without it) deserialize to `None`, and cost
    /// computations deliberately ignore it.
    #[serde(default)]
    pub cached_input_tokens: Option<u32>,
    /// Reasoning (thinking) tokens included in `output_tokens`, when the
    /// provider reports the split — e.g. OpenAI
    /// `completion_tokens_details.reasoning_tokens`, or an
    /// Anthropic-compatible gateway injecting the same. `None` when only the
    /// combined `output_tokens` is reported. Additive field: older serialized
    /// payloads (without it) deserialize to `None`, cost computations
    /// deliberately ignore it (already counted in `output_tokens`).
    #[serde(default)]
    pub reasoning_tokens: Option<u32>,
    /// Image input tokens included in `input_tokens`, when the provider
    /// reports the per-modality split — e.g. OpenAI chat usage
    /// `prompt_tokens_details.image_tokens`. `None` when the provider does
    /// not break input tokens down by modality (Zhipu chat usage has no
    /// such field — the attachment chip falls back to file size there).
    /// Additive field: older serialized payloads (without it) deserialize
    /// to `None`; cost computations deliberately ignore it (already counted
    /// in `input_tokens`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_tokens: Option<u64>,
}

/// 单条消息的采集元数据（assistant 消息专属；落 messages.meta_json，
/// 不进 Message 本体——Message 是 provider 回放单元，混入 usage 会在
/// convert.rs / resume 回放路径泄漏给 provider）。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct MessageMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
}

/// Events emitted during a streaming chat completion.
#[derive(Debug, Clone)]
pub enum ModelStreamEvent {
    /// Partial content delta from the model (the final answer text).
    Delta(String),
    /// Partial reasoning/thinking delta from the model (e.g. DeepSeek-style
    /// `reasoning_content`). Surfaced separately from [`Delta`] so callers can
    /// display it distinctly (dimmed, folded, etc.). Providers that don't expose
    /// reasoning never emit this variant.
    Reasoning(String),
    /// Token usage update (may arrive in final chunk).
    Usage(Usage),
    /// Stream has completed. Contains the full assembled response.
    Done(ModelResponse),
}

/// The output produced by a tool execution.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolOutput {
    pub content: String,
    pub bytes: usize,
    pub duration_ms: u64,
    pub status: ToolOutputStatus,
    /// 工具产出附件（读图链路，2026-10）：MCP 工具返回 Image content
    /// block 时，core 把解码后的字节收集为附件（id 形如 `tool-{n}`，
    /// 不落 attachments 表——与用户 ingest 附件的 DB 行无关）。
    /// runtime 把它装进 Tool Message 的 `attachments`，convert 层
    /// 1:N 展开为 ToolResponse + 紧随的 user Binary parts。additive：
    /// 旧 payload（无此字段）反序列化为 `None`，序列化时 `None` 省略。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<Attachment>>,
}

/// Status of a tool output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolOutputStatus {
    Success,
    Error,
    Truncated,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_id_display() {
        let id = AgentId("root".into());
        assert_eq!(format!("{id}"), "root");
    }

    #[test]
    fn agent_id_from_str() {
        let id: AgentId = "root".parse().unwrap();
        assert_eq!(id.0, "root");
    }

    #[test]
    fn agent_id_from_string() {
        let id = AgentId::from(String::from("root"));
        assert_eq!(id.0, "root");
    }

    #[test]
    fn agent_id_from_ref_str() {
        let id = AgentId::from("root");
        assert_eq!(id.0, "root");
    }

    #[test]
    fn agent_id_equality_and_hash() {
        use std::collections::HashSet;
        let a = AgentId("x".into());
        let b = AgentId("x".into());
        let c = AgentId("y".into());
        assert_eq!(a, b);
        let mut set = HashSet::new();
        set.insert(a.clone());
        assert!(set.contains(&b));
        assert!(!set.contains(&c));
    }

    #[test]
    fn run_id_display_and_from_str() {
        let id = RunId("r-001".into());
        assert_eq!(format!("{id}"), "r-001");
        let parsed: RunId = "r-001".parse().unwrap();
        assert_eq!(parsed, id);
    }

    #[test]
    fn step_id_roundtrip() {
        let id = StepId::from("s-42");
        assert_eq!(id.as_ref(), "s-42");
    }

    #[test]
    fn execution_node_id_roundtrip() {
        let id = ExecutionNodeId::from("en-7");
        assert_eq!(id.as_ref(), "en-7");
    }

    #[test]
    fn tool_call_id_roundtrip() {
        let id = ToolCallId::from("tc-99");
        assert_eq!(id.as_ref(), "tc-99");
    }

    #[test]
    fn message_construction() {
        let msg = Message {
            attachments: None,
            role: MessageRole::User,
            content: "hello".into(),
            tool_call_id: None,
            name: None,
            tool_calls: None,
            reasoning_content: None,
        };
        assert_eq!(msg.role, MessageRole::User);
        assert_eq!(msg.content, "hello");
    }

    #[test]
    fn message_with_tool_fields() {
        let msg = Message {
            attachments: None,
            role: MessageRole::Tool,
            content: "result".into(),
            tool_call_id: Some(ToolCallId::from("tc-1")),
            name: Some("bash".into()),
            tool_calls: None,
            reasoning_content: None,
        };
        assert!(msg.tool_call_id.is_some());
        assert_eq!(msg.name.as_deref(), Some("bash"));
    }

    #[test]
    fn message_old_payload_without_attachments() {
        // 旧 payload（attachments 字段加入前的 Message JSON）反序列化为 None。
        let json = r#"{"role":"user","content":"hello"}"#;
        let msg: Message = serde_json::from_str(json).unwrap();
        assert_eq!(msg.role, MessageRole::User);
        assert!(msg.attachments.is_none());
    }

    #[test]
    fn message_attachments_roundtrip() {
        let msg = Message {
            attachments: Some(vec![Attachment {
                id: "att-1".into(),
                mime: "image/png".into(),
                filename: Some("shot.png".into()),
                data: None,
            }]),
            role: MessageRole::User,
            content: "look at this".into(),
            tool_call_id: None,
            name: None,
            tool_calls: None,
            reasoning_content: None,
        };
        let value = serde_json::to_value(&msg).unwrap();
        let back: Message = serde_json::from_value(value.clone()).unwrap();
        // Message 无 PartialEq：以 serde Value 往返相等为准。
        assert_eq!(serde_json::to_value(&back).unwrap(), value);
        let att = back
            .attachments
            .expect("attachments survive")
            .pop()
            .unwrap();
        assert_eq!(att.id, "att-1");
        assert_eq!(att.mime, "image/png");
        assert_eq!(att.filename.as_deref(), Some("shot.png"));
        // `data` 被 serde(skip)：即使 payload 里出现该键，反序列化也回落 None。
        assert_eq!(att.data, None);
    }

    #[test]
    fn attachment_data_never_serialized() {
        // data=Some 时序列化输出也只有 {id, mime, filename} 引用形状。
        let att = Attachment {
            id: "att-2".into(),
            mime: "image/jpeg".into(),
            filename: None,
            data: Some(std::sync::Arc::from(&b"fake-jpeg-bytes"[..])),
        };
        let value = serde_json::to_value(&att).unwrap();
        assert!(
            value.get("data").is_none(),
            "data must not serialize: {value}"
        );
        assert_eq!(value.get("id").and_then(|v| v.as_str()), Some("att-2"));
        assert_eq!(
            value.get("mime").and_then(|v| v.as_str()),
            Some("image/jpeg")
        );
        assert!(value.get("filename").is_none());

        // 嵌在 Message 里同样不出现。
        let msg = Message {
            attachments: Some(vec![att]),
            role: MessageRole::User,
            content: "hi".into(),
            tool_call_id: None,
            name: None,
            tool_calls: None,
            reasoning_content: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(
            !json.contains("data"),
            "data must stay off the wire: {json}"
        );
    }

    #[test]
    fn message_meta_serde_roundtrip() {
        // 空 payload（旧/无 meta 行）→ 全 None；字段各自可缺省。
        let meta: MessageMeta = serde_json::from_str("{}").unwrap();
        assert_eq!(meta.model_id, None);
        assert!(meta.usage.is_none());
        assert_eq!(meta.finish_reason, None);
        assert_eq!(meta.latency_ms, None);

        // 全字段往返；None 字段序列化时省略。
        let meta = MessageMeta {
            model_id: Some("glm-5.3-flash".into()),
            usage: Some(Usage {
                input_tokens: 10,
                output_tokens: 5,
                cached_input_tokens: None,
                reasoning_tokens: None,
                image_tokens: None,
            }),
            finish_reason: Some("stop".into()),
            latency_ms: Some(812),
        };
        let value = serde_json::to_value(&meta).unwrap();
        assert_eq!(
            value.get("model_id").and_then(|v| v.as_str()),
            Some("glm-5.3-flash")
        );
        let back: MessageMeta = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(&back).unwrap(), value);
        assert_eq!(back.latency_ms, Some(812));
        assert_eq!(back.usage.expect("usage survives").output_tokens, 5);
    }

    /// `Usage.image_tokens`（additive）：旧 payload（无此键）→ None；
    /// None 序列化省键；有值往返保留（响应侧 image tokens 回填，v5）。
    #[test]
    fn usage_image_tokens_serde_compat() {
        let usage: Usage = serde_json::from_str(
            r#"{"input_tokens":10,"output_tokens":5,"cached_input_tokens":3}"#,
        )
        .unwrap();
        assert_eq!(usage.image_tokens, None, "旧 payload 缺键 → None");

        let json = serde_json::to_string(&usage).unwrap();
        assert!(
            !json.contains("image_tokens"),
            "None 省键（存量字节不变）：{json}"
        );

        let usage = Usage {
            input_tokens: 10,
            output_tokens: 5,
            cached_input_tokens: None,
            reasoning_tokens: None,
            image_tokens: Some(728),
        };
        let value = serde_json::to_value(&usage).unwrap();
        assert_eq!(value.get("image_tokens").and_then(|v| v.as_u64()), Some(728));
        let back: Usage = serde_json::from_value(value).unwrap();
        assert_eq!(back.image_tokens, Some(728));
    }

    #[test]
    fn agent_config_serde_roundtrip() {
        let config = AgentConfig {
            hidden_tools: Vec::new(),
            id: AgentId::from("root"),
            name: "Root Agent".into(),
            model: "gpt-4".into(),
            children: vec![AgentId::from("child-1")],
            tools: vec!["bash".into()],
            default_prompt: "You are helpful.".into(),
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: AgentConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, config.id);
        assert_eq!(back.name, config.name);
        assert_eq!(back.model, config.model);
        assert_eq!(back.children, config.children);
        assert_eq!(back.tools, config.tools);
        assert_eq!(back.default_prompt, config.default_prompt);
    }

    #[test]
    fn agent_config_deserialize_missing_optionals() {
        let json = r#"{
            "id": "root",
            "name": "Root",
            "model": "gpt-4",
            "default_prompt": "hi"
        }"#;
        let config: AgentConfig = serde_json::from_str(json).unwrap();
        assert!(config.children.is_empty());
        assert!(config.tools.is_empty());
    }

    #[test]
    fn model_response_deserialize() {
        let json = r#"{
            "content": "Hello!",
            "tool_calls": [],
            "usage": { "input_tokens": 10, "output_tokens": 5 },
            "finish_reason": "stop"
        }"#;
        let resp: ModelResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.content.as_deref(), Some("Hello!"));
        assert!(resp.tool_calls.is_empty());
        let usage = resp.usage.unwrap();
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 5);
        // The cached field is additive: payloads without it → None.
        assert_eq!(usage.cached_input_tokens, None);
        assert_eq!(resp.finish_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn usage_cached_input_tokens_roundtrip() {
        // Old-shape payload (no cached field) → None.
        let usage: Usage = serde_json::from_str(r#"{"input_tokens":3,"output_tokens":4}"#).unwrap();
        assert_eq!(usage.cached_input_tokens, None);
        // With the field → Some.
        let usage: Usage =
            serde_json::from_str(r#"{"input_tokens":3,"output_tokens":4,"cached_input_tokens":2}"#)
                .unwrap();
        assert_eq!(usage.cached_input_tokens, Some(2));
        // Round-trip keeps it.
        let back: Usage = serde_json::from_str(&serde_json::to_string(&usage).unwrap()).unwrap();
        assert_eq!(back, usage);
    }

    #[test]
    fn usage_reasoning_tokens_roundtrip() {
        // Old-shape payload (no reasoning field) → None.
        let usage: Usage = serde_json::from_str(r#"{"input_tokens":3,"output_tokens":4}"#).unwrap();
        assert_eq!(usage.reasoning_tokens, None);
        // With the field → Some, and it survives a round trip.
        let usage: Usage =
            serde_json::from_str(r#"{"input_tokens":3,"output_tokens":9,"reasoning_tokens":6}"#)
                .unwrap();
        assert_eq!(usage.reasoning_tokens, Some(6));
        let back: Usage = serde_json::from_str(&serde_json::to_string(&usage).unwrap()).unwrap();
        assert_eq!(back, usage);
    }

    #[test]
    fn model_response_with_tool_calls() {
        let json = r#"{
            "content": null,
            "tool_calls": [
                {
                    "id": "tc-1",
                    "name": "bash",
                    "arguments": {"command": "ls"}
                }
            ],
            "usage": null,
            "finish_reason": "tool_calls"
        }"#;
        let resp: ModelResponse = serde_json::from_str(json).unwrap();
        assert!(resp.content.is_none());
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].id.0, "tc-1");
        assert_eq!(resp.tool_calls[0].name, "bash");
    }

    #[test]
    fn run_status_serde() {
        let status = RunStatus::Completed;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, "\"completed\"");
        let back: RunStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back, RunStatus::Completed);
    }

    #[test]
    fn step_kind_constants() {
        assert_eq!(StepKind::MODEL_CALL, "model_call");
        assert_eq!(StepKind::TOOL_CALL, "tool_call");
        assert_eq!(StepKind::CHILD_AGENT_CALL, "child_agent_call");
    }

    #[test]
    fn tool_output_construction() {
        let output = ToolOutput {
            content: "ok".into(),
            bytes: 2,
            duration_ms: 150,
            status: ToolOutputStatus::Success,
            attachments: None,
        };
        assert_eq!(output.content, "ok");
        assert_eq!(output.status, ToolOutputStatus::Success);
    }

    #[test]
    fn tool_output_attachments_additive_serde() {
        // 旧 payload（无 attachments 字段）→ None；None 序列化省略。
        let old: ToolOutput =
            serde_json::from_str(r#"{"content":"x","bytes":1,"duration_ms":0,"status":"success"}"#)
                .unwrap();
        assert!(old.attachments.is_none());
        let value = serde_json::to_value(&old).unwrap();
        assert!(value.get("attachments").is_none(), "{value}");
        // 带附件 → 引用形状持久化（data serde-skip，永不进 JSON）。
        let with_att = ToolOutput {
            attachments: Some(vec![Attachment {
                id: "tool-0".into(),
                mime: "image/png".into(),
                filename: None,
                data: Some(std::sync::Arc::from(&b"png"[..])),
            }]),
            ..old
        };
        let value = serde_json::to_value(&with_att).unwrap();
        assert_eq!(
            value["attachments"][0]["id"].as_str(),
            Some("tool-0"),
            "{value}"
        );
        assert!(value["attachments"][0].get("data").is_none(), "{value}");
    }

    #[test]
    fn message_role_serde() {
        assert_eq!(
            serde_json::to_string(&MessageRole::System).unwrap(),
            "\"system\""
        );
        assert_eq!(
            serde_json::to_string(&MessageRole::Tool).unwrap(),
            "\"tool\""
        );
    }
}
