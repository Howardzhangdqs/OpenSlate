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
id_type!(ExecutionNodeId, "Unique identifier for a node in the execution tree.");
id_type!(ToolCallId, "Unique identifier for a tool call within a step.");

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
}

/// The output produced by a tool execution.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolOutput {
    pub content: String,
    pub bytes: usize,
    pub duration_ms: u64,
    pub status: ToolOutputStatus,
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
            role: MessageRole::User,
            content: "hello".into(),
            tool_call_id: None,
            name: None,
        };
        assert_eq!(msg.role, MessageRole::User);
        assert_eq!(msg.content, "hello");
    }

    #[test]
    fn message_with_tool_fields() {
        let msg = Message {
            role: MessageRole::Tool,
            content: "result".into(),
            tool_call_id: Some(ToolCallId::from("tc-1")),
            name: Some("bash".into()),
        };
        assert!(msg.tool_call_id.is_some());
        assert_eq!(msg.name.as_deref(), Some("bash"));
    }

    #[test]
    fn agent_config_serde_roundtrip() {
        let config = AgentConfig {
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
        assert_eq!(resp.finish_reason.as_deref(), Some("stop"));
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
        };
        assert_eq!(output.content, "ok");
        assert_eq!(output.status, ToolOutputStatus::Success);
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
