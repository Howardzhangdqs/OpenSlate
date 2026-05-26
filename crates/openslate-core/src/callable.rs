//! Callable abstraction — base trait for tools and child agents.

use async_trait::async_trait;
use crate::error::ToolError;
use crate::provider::ToolDefinition;
use crate::tool::Tool;
use crate::types::{AgentId, ToolOutput, ToolOutputStatus};

/// Discriminator for the kind of callable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallableKind {
    Tool,
    ChildAgent,
}

/// Base trait shared by tools and child agents.
///
/// The actual async execution method will be added in a future task
/// (Task 15 — Tool trait + registry). For now this captures the
/// metadata every callable must expose.
pub trait Callable: Send + Sync {
    /// The name used to invoke this callable (e.g. `"bash"`, `"child-agent-1"`).
    fn name(&self) -> &str;

    /// Human-readable description of what this callable does.
    fn description(&self) -> &str;

    /// JSON Schema describing the parameters this callable accepts.
    fn parameters_schema(&self) -> serde_json::Value;

    /// Whether this callable is a tool or a child agent.
    fn kind(&self) -> CallableKind;
}

/// Schema for call_agent parameters.
pub const CALL_AGENT_SCHEMA: &str = r#"{
    "type": "object",
    "properties": {
        "agent_id": {"type": "string", "description": "The ID of the child agent to call"},
        "task": {"type": "string", "description": "The task description to send to the child agent"}
    },
    "required": ["agent_id", "task"]
}"#;

/// A tool that calls a child agent.
///
/// This is a placeholder/stub that represents a child agent callable.
/// The actual execution is handled by the runtime which intercepts
/// `call_agent` tool calls.
pub struct ChildAgentCallable {
    pub agent_id: AgentId,
    pub agent_name: String,
    /// Stored because `Tool::description` returns `&str`.
    description: String,
}

impl ChildAgentCallable {
    /// Create a new child agent callable for the given agent ID and name.
    pub fn new(agent_id: AgentId, agent_name: String) -> Self {
        let description = format!(
            "Call the child agent '{}' to delegate a sub-task",
            agent_name
        );
        Self {
            agent_id,
            agent_name,
            description,
        }
    }
}

#[async_trait]
impl Tool for ChildAgentCallable {
    fn name(&self) -> &str {
        "call_agent"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "agent_id": {
                    "type": "string",
                    "description": "The ID of the child agent to call",
                    "enum": [self.agent_id.0]
                },
                "task": {
                    "type": "string",
                    "description": "The task description to send to the child agent"
                }
            },
            "required": ["agent_id", "task"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolOutput, ToolError> {
        // This is a stub — the runtime intercepts call_agent calls
        // and handles them specially (creates new execution node, runs child agent).
        let task = args["task"].as_str().unwrap_or("");
        let content = format!(
            "Child agent '{}' would execute: {}",
            self.agent_id.0, task
        );
        let bytes = content.len();
        Ok(ToolOutput {
            content,
            bytes,
            duration_ms: 0,
            status: ToolOutputStatus::Success,
        })
    }
}

/// Build call_agent tool definitions for a list of child agent IDs.
///
/// Returns a vec of `ToolDefinition`s that the parent agent can use.
/// Only IDs that exist in the `agent_tree` will produce a definition.
pub fn child_agent_definitions(
    child_ids: &[AgentId],
    agent_tree: &crate::agent_tree::AgentTree,
) -> Vec<ToolDefinition> {
    child_ids
        .iter()
        .filter_map(|id| {
            agent_tree.get_agent(id).map(|node| {
                let callable = ChildAgentCallable::new(id.clone(), node.name.clone());
                callable.to_definition()
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callable_kind_equality() {
        assert_eq!(CallableKind::Tool, CallableKind::Tool);
        assert_ne!(CallableKind::Tool, CallableKind::ChildAgent);
    }

    #[test]
    fn callable_kind_copy() {
        let a = CallableKind::Tool;
        let b = a;
        assert_eq!(a, b);
    }

    #[test]
    fn callable_kind_debug() {
        assert_eq!(format!("{:?}", CallableKind::Tool), "Tool");
        assert_eq!(format!("{:?}", CallableKind::ChildAgent), "ChildAgent");
    }

    /// Minimal stub to verify the trait can be implemented.
    struct StubCallable {
        n: &'static str,
        desc: &'static str,
        k: CallableKind,
    }

    impl Callable for StubCallable {
        fn name(&self) -> &str {
            self.n
        }

        fn description(&self) -> &str {
            self.desc
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }

        fn kind(&self) -> CallableKind {
            self.k
        }
    }

    #[test]
    fn stub_callable_implements_trait() {
        let s = StubCallable {
            n: "stub",
            desc: "a stub",
            k: CallableKind::Tool,
        };
        assert_eq!(s.name(), "stub");
        assert_eq!(s.description(), "a stub");
        assert_eq!(s.kind(), CallableKind::Tool);
        // Verify the schema is valid JSON
        let schema = s.parameters_schema();
        assert_eq!(schema["type"], "object");
    }

    #[test]
    fn test_child_agent_callable_name() {
        let callable = ChildAgentCallable::new(AgentId("researcher".into()), "Researcher".into());
        assert_eq!(callable.name(), "call_agent");
    }

    #[test]
    fn test_child_agent_callable_schema() {
        let callable = ChildAgentCallable::new(AgentId("researcher".into()), "Researcher".into());
        let schema = callable.parameters_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["agent_id"].is_object());
        assert!(schema["properties"]["task"].is_object());
        let required = schema["required"].as_array().expect("required should be array");
        assert!(required.iter().any(|v| v == "agent_id"));
        assert!(required.iter().any(|v| v == "task"));
    }

    #[test]
    fn test_child_agent_callable_schema_enum() {
        let callable = ChildAgentCallable::new(AgentId("researcher".into()), "Researcher".into());
        let schema = callable.parameters_schema();
        let enum_vals = schema["properties"]["agent_id"]["enum"]
            .as_array()
            .expect("agent_id enum should be array");
        assert_eq!(enum_vals.len(), 1);
        assert_eq!(enum_vals[0].as_str(), Some("researcher"));
    }

    #[tokio::test]
    async fn test_child_agent_execute_stub() {
        let callable = ChildAgentCallable::new(AgentId("researcher".into()), "Researcher".into());
        let args = serde_json::json!({"agent_id": "researcher", "task": "analyze data"});
        let output = callable.execute(&args).await.expect("execute should succeed");
        assert_eq!(output.status, ToolOutputStatus::Success);
        assert!(output.content.contains("researcher"));
        assert!(output.content.contains("analyze data"));
    }

    #[test]
    fn test_child_agent_definitions() {
        use crate::agent_tree::AgentTree;
        use crate::types::AgentConfig;

        let configs = vec![
            AgentConfig {
                id: AgentId("root".into()),
                name: "Root".into(),
                model: "main".into(),
                children: vec![AgentId("child-a".into()), AgentId("child-b".into())],
                tools: vec![],
                default_prompt: "".into(),
            },
            AgentConfig {
                id: AgentId("child-a".into()),
                name: "Child A".into(),
                model: "fast".into(),
                children: vec![],
                tools: vec![],
                default_prompt: "".into(),
            },
            AgentConfig {
                id: AgentId("child-b".into()),
                name: "Child B".into(),
                model: "fast".into(),
                children: vec![],
                tools: vec![],
                default_prompt: "".into(),
            },
        ];
        let tree = AgentTree::from_configs(&configs).expect("tree should build");
        let child_ids = vec![AgentId("child-a".into()), AgentId("child-b".into())];
        let defs = child_agent_definitions(&child_ids, &tree);
        assert_eq!(defs.len(), 2);
        assert_eq!(defs[0].name, "call_agent");
        assert_eq!(defs[1].name, "call_agent");
    }

    #[tokio::test]
    async fn test_child_agent_callable_as_trait_object() {
        let callable: Box<dyn Tool> = Box::new(ChildAgentCallable::new(
            AgentId("writer".into()),
            "Writer".into(),
        ));
        assert_eq!(callable.name(), "call_agent");
        assert!(callable.description().contains("Writer"));
        let args = serde_json::json!({"agent_id": "writer", "task": "write report"});
        let output = callable.execute(&args).await.expect("execute should succeed");
        assert_eq!(output.status, ToolOutputStatus::Success);
        assert!(output.content.contains("writer"));
        assert!(output.content.contains("write report"));
    }
}
