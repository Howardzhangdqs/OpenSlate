//! Tool trait and registry for OpenSlate.
//!
//! Tools are the actions an agent can take. Each tool implements the `Tool` trait.
//! The `ToolRegistry` maps tool names to implementations.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::error::ToolError;
use crate::provider::ToolDefinition;
use crate::types::ToolOutput;

#[cfg(test)]
use crate::types::ToolOutputStatus;

/// A tool that an agent can invoke.
#[async_trait]
pub trait Tool: Send + Sync {
    /// The unique name of this tool (e.g., "bash", "read_file").
    fn name(&self) -> &str;

    /// Human-readable description of what this tool does.
    fn description(&self) -> &str;

    /// JSON Schema describing the tool's parameters.
    fn parameters_schema(&self) -> serde_json::Value;

    /// Execute the tool with the given arguments.
    async fn execute(&self, args: &serde_json::Value) -> Result<ToolOutput, ToolError>;

    /// Convert to a ToolDefinition for sending to the model.
    fn to_definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_owned(),
            description: self.description().to_owned(),
            parameters: self.parameters_schema(),
        }
    }
}

/// Registry mapping tool names to tool implementations.
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    /// Register a tool.
    pub fn register(&mut self, tool: impl Tool + 'static) {
        self.tools.insert(tool.name().to_owned(), Arc::new(tool));
    }

    /// Get a tool by name.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// Get tool definitions for all registered tools (to send to the model).
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|t| t.to_definition()).collect()
    }

    /// Get definitions for specific tool names only.
    pub fn definitions_for(&self, names: &[String]) -> Vec<ToolDefinition> {
        names
            .iter()
            .filter_map(|name| self.tools.get(name).map(|t| t.to_definition()))
            .collect()
    }

    /// List all registered tool names.
    pub fn tool_names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// Execute a tool by name.
    pub async fn execute(
        &self,
        name: &str,
        args: &serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| ToolError::NotFound(name.to_owned()))?;
        tool.execute(args).await
    }

    /// Check if a tool is registered.
    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Mock tools for testing ---

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Echoes back the input"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "message": {"type": "string", "description": "Message to echo"}
                },
                "required": ["message"]
            })
        }
        async fn execute(&self, args: &serde_json::Value) -> Result<ToolOutput, ToolError> {
            let msg = args["message"].as_str().unwrap_or("");
            Ok(ToolOutput {
                content: msg.to_owned(),
                bytes: msg.len(),
                duration_ms: 0,
                status: ToolOutputStatus::Success,
            })
        }
    }

    struct FailingTool;

    #[async_trait]
    impl Tool for FailingTool {
        fn name(&self) -> &str {
            "fail"
        }
        fn description(&self) -> &str {
            "Always fails"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        async fn execute(&self, _args: &serde_json::Value) -> Result<ToolOutput, ToolError> {
            Err(ToolError::ExecutionError("intentional failure".into()))
        }
    }

    struct ReverseTool;

    #[async_trait]
    impl Tool for ReverseTool {
        fn name(&self) -> &str {
            "reverse"
        }
        fn description(&self) -> &str {
            "Reverses the input string"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "text": {"type": "string"}
                },
                "required": ["text"]
            })
        }
        async fn execute(&self, args: &serde_json::Value) -> Result<ToolOutput, ToolError> {
            let text = args["text"].as_str().unwrap_or("");
            Ok(ToolOutput {
                content: text.chars().rev().collect(),
                bytes: text.len(),
                duration_ms: 0,
                status: ToolOutputStatus::Success,
            })
        }
    }

    // --- Tests ---

    #[test]
    fn test_register_and_get_tool() {
        let mut registry = ToolRegistry::new();
        registry.register(EchoTool);
        let tool = registry.get("echo").expect("tool should be registered");
        assert_eq!(tool.name(), "echo");
    }

    #[test]
    fn test_get_nonexistent_tool() {
        let registry = ToolRegistry::new();
        assert!(registry.get("nope").is_none());
    }

    #[test]
    fn test_definitions_returns_all() {
        let mut registry = ToolRegistry::new();
        registry.register(EchoTool);
        registry.register(FailingTool);
        let defs = registry.definitions();
        assert_eq!(defs.len(), 2);
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"echo"));
        assert!(names.contains(&"fail"));
    }

    #[test]
    fn test_definitions_for_filters() {
        let mut registry = ToolRegistry::new();
        registry.register(EchoTool);
        registry.register(FailingTool);
        registry.register(ReverseTool);
        let names = vec!["echo".to_string(), "reverse".to_string()];
        let defs = registry.definitions_for(&names);
        assert_eq!(defs.len(), 2);
        let def_names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(def_names.contains(&"echo"));
        assert!(def_names.contains(&"reverse"));
        assert!(!def_names.contains(&"fail"));
    }

    #[tokio::test]
    async fn test_execute_tool() {
        let mut registry = ToolRegistry::new();
        registry.register(EchoTool);
        let args = serde_json::json!({"message": "hello"});
        let output = registry.execute("echo", &args).await.unwrap();
        assert_eq!(output.content, "hello");
        assert_eq!(output.bytes, 5);
        assert_eq!(output.status, ToolOutputStatus::Success);
    }

    #[tokio::test]
    async fn test_execute_nonexistent_tool() {
        let registry = ToolRegistry::new();
        let result = registry.execute("ghost", &serde_json::json!({})).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ToolError::NotFound(name) if name == "ghost"));
    }

    #[tokio::test]
    async fn test_execute_failing_tool() {
        let mut registry = ToolRegistry::new();
        registry.register(FailingTool);
        let result = registry.execute("fail", &serde_json::json!({})).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, ToolError::ExecutionError(msg) if msg == "intentional failure")
        );
    }

    #[test]
    fn test_tool_names() {
        let mut registry = ToolRegistry::new();
        registry.register(EchoTool);
        registry.register(FailingTool);
        let mut names = registry.tool_names();
        names.sort();
        assert_eq!(names, vec!["echo", "fail"]);
    }

    #[test]
    fn test_contains() {
        let mut registry = ToolRegistry::new();
        registry.register(EchoTool);
        assert!(registry.contains("echo"));
        assert!(!registry.contains("bash"));
    }

    #[test]
    fn test_to_definition() {
        let echo = EchoTool;
        let def = echo.to_definition();
        assert_eq!(def.name, "echo");
        assert_eq!(def.description, "Echoes back the input");
        assert_eq!(def.parameters["type"], "object");
        assert!(def.parameters["properties"]["message"].is_object());
    }
}
