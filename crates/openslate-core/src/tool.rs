//! Tool trait and registry for OpenSlate.
//!
//! Tools are the actions an agent can take. Each tool implements the `Tool` trait.
//! The `ToolRegistry` maps tool names to tool implementations.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::error::ToolError;
use crate::provider::ToolDefinition;
use crate::types::{ToolOutput, ToolOutputStatus};

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

/// Truncate tool output content if it exceeds the byte limit.
///
/// If the content is within limits, returns the output unchanged.
/// If truncated, sets status to `Truncated` and appends a truncation notice.
pub fn limit_tool_output(output: ToolOutput, max_bytes: usize) -> ToolOutput {
    if output.bytes <= max_bytes {
        return output;
    }

    let truncated_content: String = output.content.chars().take(max_bytes / 4).collect();
    let notice = format!(
        "\n\n[TRUNCATED: original {} bytes, showing {} bytes]",
        output.bytes,
        truncated_content.len()
    );

    ToolOutput {
        content: format!("{}{}", truncated_content, notice),
        bytes: truncated_content.len() + notice.len(),
        duration_ms: output.duration_ms,
        status: ToolOutputStatus::Truncated,
    }
}

/// Audit record for a tool execution.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolAuditRecord {
    pub tool_name: String,
    pub arguments_json: String,
    pub output_bytes: usize,
    pub output_status: String,
    pub duration_ms: u64,
    pub truncated: bool,
}

/// Create an audit record from tool execution details.
#[allow(dead_code)]
pub fn create_audit_record(
    tool_name: &str,
    arguments: &serde_json::Value,
    output: &ToolOutput,
) -> ToolAuditRecord {
    ToolAuditRecord {
        tool_name: tool_name.to_owned(),
        arguments_json: serde_json::to_string(arguments).unwrap_or_default(),
        output_bytes: output.bytes,
        output_status: match output.status {
            ToolOutputStatus::Success => "success".to_owned(),
            ToolOutputStatus::Error => "error".to_owned(),
            ToolOutputStatus::Truncated => "truncated".to_owned(),
        },
        duration_ms: output.duration_ms,
        truncated: output.status == ToolOutputStatus::Truncated,
    }
}

#[cfg(test)]
mod limit_tests {
    use super::*;

    #[test]
    fn test_limit_output_under_limit() {
        let output = ToolOutput {
            content: "hello".to_string(),
            bytes: 5,
            duration_ms: 10,
            status: ToolOutputStatus::Success,
        };
        let limited = limit_tool_output(output.clone(), 100);
        assert_eq!(limited.content, output.content);
        assert_eq!(limited.bytes, output.bytes);
        assert_eq!(limited.status, ToolOutputStatus::Success);
    }

    #[test]
    fn test_limit_output_over_limit() {
        let output = ToolOutput {
            content: "a".repeat(1000),
            bytes: 1000,
            duration_ms: 50,
            status: ToolOutputStatus::Success,
        };
        let limited = limit_tool_output(output, 100);
        assert!(limited.bytes < 1000);
        assert!(limited.content.contains("[TRUNCATED"));
        assert_eq!(limited.status, ToolOutputStatus::Truncated);
    }

    #[test]
    fn test_limit_output_exact_limit() {
        let content = "hello";
        let output = ToolOutput {
            content: content.to_string(),
            bytes: 5,
            duration_ms: 10,
            status: ToolOutputStatus::Success,
        };
        let limited = limit_tool_output(output.clone(), 5);
        assert_eq!(limited.content, content);
        assert_eq!(limited.bytes, 5);
        assert_eq!(limited.status, ToolOutputStatus::Success);
    }

    #[test]
    fn test_limit_output_zero_max() {
        let output = ToolOutput {
            content: "hello".to_string(),
            bytes: 5,
            duration_ms: 10,
            status: ToolOutputStatus::Success,
        };
        let limited = limit_tool_output(output.clone(), 0);
        assert!(limited.content.contains("[TRUNCATED"));
        assert_eq!(limited.status, ToolOutputStatus::Truncated);
    }

    #[test]
    fn test_create_audit_record_success() {
        let output = ToolOutput {
            content: "result".to_string(),
            bytes: 6,
            duration_ms: 100,
            status: ToolOutputStatus::Success,
        };
        let args = serde_json::json!({"cmd": "ls"});
        let record = create_audit_record("bash", &args, &output);

        assert_eq!(record.tool_name, "bash");
        assert!(record.arguments_json.contains("ls"));
        assert_eq!(record.output_bytes, 6);
        assert_eq!(record.output_status, "success");
        assert_eq!(record.duration_ms, 100);
        assert!(!record.truncated);
    }

    #[test]
    fn test_create_audit_record_truncated() {
        let output = ToolOutput {
            content: "truncated content".to_string(),
            bytes: 100,
            duration_ms: 50,
            status: ToolOutputStatus::Truncated,
        };
        let args = serde_json::json!({});
        let record = create_audit_record("echo", &args, &output);

        assert_eq!(record.output_status, "truncated");
        assert!(record.truncated);
    }

    #[test]
    fn test_create_audit_record_serialization() {
        let output = ToolOutput {
            content: "test".to_string(),
            bytes: 4,
            duration_ms: 25,
            status: ToolOutputStatus::Success,
        };
        let args = serde_json::json!({"x": 1});
        let record = create_audit_record("test_tool", &args, &output);

        let json = serde_json::to_string(&record).unwrap();
        let back: ToolAuditRecord = serde_json::from_str(&json).unwrap();

        assert_eq!(back.tool_name, record.tool_name);
        assert_eq!(back.output_bytes, record.output_bytes);
        assert_eq!(back.output_status, record.output_status);
        assert_eq!(back.duration_ms, record.duration_ms);
        assert_eq!(back.truncated, record.truncated);
    }
}
