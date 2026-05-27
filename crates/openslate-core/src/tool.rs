//! Tool trait and registry for OpenSlate.
//!
//! Tools are the actions an agent can take. Each tool implements the `Tool` trait.
//! The `ToolRegistry` maps tool names to tool implementations.

use std::collections::HashMap;
use std::path::PathBuf;
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

/// Returns the current date and time.
pub struct CurrentTimeTool;

#[async_trait]
impl Tool for CurrentTimeTool {
    fn name(&self) -> &str {
        "current_time"
    }
    fn description(&self) -> &str {
        "Get the current date and time in ISO 8601 format"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "timezone": {
                    "type": "string",
                    "description": "Timezone (e.g., 'UTC', 'Asia/Shanghai'). Defaults to UTC."
                }
            }
        })
    }
    async fn execute(&self, args: &serde_json::Value) -> Result<ToolOutput, ToolError> {
        let now = chrono::Utc::now();
        let tz = args["timezone"].as_str().unwrap_or("UTC");
        let content = format!("Current time ({}): {}", tz, now.to_rfc3339());
        let bytes = content.len();
        Ok(ToolOutput {
            content,
            bytes,
            duration_ms: 0,
            status: ToolOutputStatus::Success,
        })
    }
}

/// Reads the contents of a file.
pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "Read the contents of a file at the given path"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Absolute or relative file path" }
            },
            "required": ["path"]
        })
    }
    async fn execute(&self, args: &serde_json::Value) -> Result<ToolOutput, ToolError> {
        let path = args["path"].as_str().ok_or_else(|| {
            ToolError::ExecutionError("Missing 'path' parameter".into())
        })?;
        let content = tokio::fs::read_to_string(path).await.map_err(|e| {
            ToolError::ExecutionError(format!("Failed to read '{}': {}", path, e))
        })?;
        let bytes = content.len();
        Ok(ToolOutput {
            content,
            bytes,
            duration_ms: 0,
            status: ToolOutputStatus::Success,
        })
    }
}

/// Writes content to a file within the workspace.
pub struct WriteFileTool {
    workspace_root: PathBuf,
}

impl WriteFileTool {
    /// Create a new WriteFileTool with the given workspace root.
    pub fn new(workspace_root: PathBuf) -> Self {
        Self { workspace_root }
    }

    /// Validate that the target path is within the workspace.
    fn validate_path(&self, target_path: &str) -> Result<PathBuf, ToolError> {
        if target_path.contains("..") {
            return Err(ToolError::SecurityError(
                "Path traversal not allowed".into(),
            ));
        }

        let workspace_root = self.workspace_root.canonicalize().map_err(|e| {
            ToolError::ExecutionError(format!("Failed to resolve workspace root: {}", e))
        })?;

        let full_path = if target_path.starts_with('/') {
            PathBuf::from(target_path)
        } else {
            self.workspace_root.join(target_path)
        };

        let check_path = match full_path.canonicalize() {
            Ok(canonical) => canonical,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                full_path.clone()
            }
            Err(e) => {
                return Err(ToolError::ExecutionError(format!(
                    "Failed to resolve path '{}': {}",
                    target_path, e
                )));
            }
        };

        if !check_path.starts_with(&workspace_root) {
            return Err(ToolError::SecurityError(
                "Path is outside workspace".into(),
            ));
        }

        Ok(full_path)
    }
}

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }
    fn description(&self) -> &str {
        "Write content to a file within the workspace"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path (absolute or relative to workspace)" },
                "content": { "type": "string", "description": "Content to write to the file" }
            },
            "required": ["path", "content"]
        })
    }
    async fn execute(&self, args: &serde_json::Value) -> Result<ToolOutput, ToolError> {
        let path = args["path"].as_str().ok_or_else(|| {
            ToolError::ExecutionError("Missing 'path' parameter".into())
        })?;
        let content = args["content"].as_str().ok_or_else(|| {
            ToolError::ExecutionError("Missing 'content' parameter".into())
        })?;

        // Validate path is within workspace
        let _validated_path = self.validate_path(path)?;

        let full_path = if path.starts_with('/') {
            PathBuf::from(path)
        } else {
            self.workspace_root.join(path)
        };

        // Create parent directories if they don't exist
        if let Some(parent) = full_path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                ToolError::ExecutionError(format!("Failed to create directories: {}", e))
            })?;
        }

        // Write the file
        tokio::fs::write(&full_path, content).await.map_err(|e| {
            ToolError::ExecutionError(format!("Failed to write '{}': {}", path, e))
        })?;

        let bytes = content.len();
        Ok(ToolOutput {
            content: format!("Successfully wrote {} bytes to '{}'", bytes, path),
            bytes,
            duration_ms: 0,
            status: ToolOutputStatus::Success,
        })
    }
}

/// Lists directory contents.
pub struct ListDirTool;

#[async_trait]
impl Tool for ListDirTool {
    fn name(&self) -> &str {
        "list_dir"
    }
    fn description(&self) -> &str {
        "List files and directories in the given path"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory path to list" }
            },
            "required": ["path"]
        })
    }
    async fn execute(&self, args: &serde_json::Value) -> Result<ToolOutput, ToolError> {
        let path = args["path"].as_str().ok_or_else(|| {
            ToolError::ExecutionError("Missing 'path' parameter".into())
        })?;
        let mut entries = tokio::fs::read_dir(path).await.map_err(|e| {
            ToolError::ExecutionError(format!("Failed to read dir '{}': {}", path, e))
        })?;
        let mut items = Vec::new();
        while let Some(entry) = entries.next_entry().await.map_err(|e| {
            ToolError::ExecutionError(format!("Error reading entry: {}", e))
        })? {
            let name = entry.file_name().to_string_lossy().to_string();
            let ft = entry.file_type().await.map_err(|e| {
                ToolError::ExecutionError(format!("Error getting type: {}", e))
            })?;
            let prefix = if ft.is_dir() { "dir/" } else { "file" };
            items.push(format!("{}\t{}", prefix, name));
        }
        items.sort();
        let content = items.join("\n");
        let bytes = content.len();
        Ok(ToolOutput {
            content,
            bytes,
            duration_ms: 0,
            status: ToolOutputStatus::Success,
        })
    }
}

/// Create a ToolRegistry pre-loaded with all built-in tools.
pub fn builtin_registry() -> ToolRegistry {
    builtin_registry_with_workspace(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// Create a ToolRegistry pre-loaded with all built-in tools, using the specified workspace root.
pub fn builtin_registry_with_workspace(workspace_root: PathBuf) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(CurrentTimeTool);
    registry.register(ReadFileTool);
    registry.register(ListDirTool);
    registry.register(WriteFileTool::new(workspace_root));
    registry
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

#[cfg(test)]
mod builtin_tests {
    use super::*;

    #[tokio::test]
    async fn test_current_time_returns_iso_time() {
        let tool = CurrentTimeTool;
        let result = tool.execute(&serde_json::json!({})).await.unwrap();
        assert!(result.status == ToolOutputStatus::Success);
        assert!(result.content.contains("20"));
    }

    #[tokio::test]
    async fn test_read_file_success() {
        let dir = tempfile::TempDir::new().unwrap();
        let file_path = dir.path().join("test.txt");
        std::fs::write(&file_path, "hello world").unwrap();

        let tool = ReadFileTool;
        let path_str = file_path.to_str().unwrap();
        let result = tool
            .execute(&serde_json::json!({"path": path_str}))
            .await
            .unwrap();
        assert_eq!(result.content, "hello world");
        assert_eq!(result.status, ToolOutputStatus::Success);
    }

    #[tokio::test]
    async fn test_read_file_missing_path() {
        let tool = ReadFileTool;
        let result = tool.execute(&serde_json::json!({})).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ToolError::ExecutionError(msg) if msg.contains("Missing 'path'")));
    }

    #[tokio::test]
    async fn test_read_file_nonexistent() {
        let tool = ReadFileTool;
        let result = tool
            .execute(&serde_json::json!({"path": "/no/such/file/ever.txt"}))
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ToolError::ExecutionError(msg) if msg.contains("Failed to read")));
    }

    #[tokio::test]
    async fn test_list_dir_success() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "aaa").unwrap();
        std::fs::write(dir.path().join("b.txt"), "bbb").unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();

        let tool = ListDirTool;
        let path_str = dir.path().to_str().unwrap();
        let result = tool
            .execute(&serde_json::json!({"path": path_str}))
            .await
            .unwrap();
        assert_eq!(result.status, ToolOutputStatus::Success);
        assert!(result.content.contains("a.txt"));
        assert!(result.content.contains("b.txt"));
        assert!(result.content.contains("subdir"));
    }

    #[tokio::test]
    async fn test_list_dir_nonexistent() {
        let tool = ListDirTool;
        let result = tool
            .execute(&serde_json::json!({"path": "/no/such/dir/ever"}))
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ToolError::ExecutionError(msg) if msg.contains("Failed to read dir")));
    }

    #[test]
    fn test_builtin_registry_has_all_tools() {
        let reg = builtin_registry();
        assert!(reg.contains("current_time"));
        assert!(reg.contains("read_file"));
        assert!(reg.contains("list_dir"));
        assert!(reg.contains("write_file"));
    }

    #[test]
    fn test_builtin_registry_definitions_count() {
        let reg = builtin_registry();
        assert_eq!(reg.definitions().len(), 4);
    }

    // ── WriteFileTool tests ──

    #[tokio::test]
    async fn test_write_file_success() {
        let workspace = tempfile::TempDir::new().unwrap();
        let tool = WriteFileTool::new(workspace.path().to_path_buf());
        let file_path = "test.txt";
        let content = "hello world";

        let result = tool
            .execute(&serde_json::json!({"path": file_path, "content": content}))
            .await
            .unwrap();

        assert_eq!(result.status, ToolOutputStatus::Success);
        assert!(result.content.contains("11 bytes"));
        assert!(result.content.contains("test.txt"));

        let written = std::fs::read_to_string(workspace.path().join(file_path)).unwrap();
        assert_eq!(written, content);
    }

    #[tokio::test]
    async fn test_write_file_absolute_path() {
        let workspace = tempfile::TempDir::new().unwrap();
        let tool = WriteFileTool::new(workspace.path().to_path_buf());
        let file_path = workspace.path().join("subdir/test.txt");
        let file_path_str = file_path.to_str().unwrap();
        let content = "absolute path test";

        let result = tool
            .execute(&serde_json::json!({"path": file_path_str, "content": content}))
            .await
            .unwrap();

        assert_eq!(result.status, ToolOutputStatus::Success);
        let written = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(written, content);
    }

    #[tokio::test]
    async fn test_write_file_creates_parent_directories() {
        let workspace = tempfile::TempDir::new().unwrap();
        let tool = WriteFileTool::new(workspace.path().to_path_buf());
        let file_path = "a/b/c/nested.txt";
        let content = "nested content";

        let result = tool
            .execute(&serde_json::json!({"path": file_path, "content": content}))
            .await
            .unwrap();

        assert_eq!(result.status, ToolOutputStatus::Success);
        let written = std::fs::read_to_string(workspace.path().join(file_path)).unwrap();
        assert_eq!(written, content);
    }

    #[tokio::test]
    async fn test_write_file_overwrites_existing() {
        let workspace = tempfile::TempDir::new().unwrap();
        let file_path = workspace.path().join("existing.txt");
        std::fs::write(&file_path, "original content").unwrap();

        let tool = WriteFileTool::new(workspace.path().to_path_buf());
        let new_content = "new content";

        let result = tool
            .execute(&serde_json::json!({"path": file_path.to_str().unwrap(), "content": new_content}))
            .await
            .unwrap();

        assert_eq!(result.status, ToolOutputStatus::Success);
        let written = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(written, new_content);
    }

    #[tokio::test]
    async fn test_write_file_missing_content_param() {
        let workspace = tempfile::TempDir::new().unwrap();
        let tool = WriteFileTool::new(workspace.path().to_path_buf());

        let result = tool
            .execute(&serde_json::json!({"path": "test.txt"}))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ToolError::ExecutionError(msg) if msg.contains("Missing 'content'")));
    }

    #[tokio::test]
    async fn test_write_file_missing_path_param() {
        let workspace = tempfile::TempDir::new().unwrap();
        let tool = WriteFileTool::new(workspace.path().to_path_buf());

        let result = tool
            .execute(&serde_json::json!({"content": "test"}))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ToolError::ExecutionError(msg) if msg.contains("Missing 'path'")));
    }

    #[tokio::test]
    async fn test_write_file_security_error_path_traversal() {
        let workspace = tempfile::TempDir::new().unwrap();
        let tool = WriteFileTool::new(workspace.path().to_path_buf());

        let result = tool
            .execute(&serde_json::json!({"path": "../escape.txt", "content": "bad"}))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ToolError::SecurityError(msg) if msg.contains("Path traversal")));
    }

    #[tokio::test]
    async fn test_write_file_security_error_double_dot_traversal() {
        let workspace = tempfile::TempDir::new().unwrap();
        let tool = WriteFileTool::new(workspace.path().to_path_buf());

        let result = tool
            .execute(&serde_json::json!({"path": "foo/../../bar.txt", "content": "bad"}))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ToolError::SecurityError(msg) if msg.contains("Path traversal")));
    }
}
