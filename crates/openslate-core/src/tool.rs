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

/// Async executor for tools — the abstraction consumed by the runtime loop.
///
/// `ToolRegistry` implements this trait, but tests and custom callers can
/// provide their own implementation to mock or intercept tool execution.
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    /// Execute the named tool with the given JSON arguments.
    ///
    /// Implementations are expected to convert internal errors into a
    /// `ToolOutput` with `ToolOutputStatus::Error` rather than returning
    /// `Err`, so that the runtime loop can continue gracefully.
    async fn execute(&self, name: &str, args: &serde_json::Value) -> ToolOutput;
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
    ///
    /// Silently overwrites any existing tool with the same name. Prefer
    /// [`try_register`](Self::try_register) when collisions must be detected —
    /// e.g. when mixing builtin tools with MCP-provided tools.
    pub fn register(&mut self, tool: impl Tool + 'static) {
        self.tools.insert(tool.name().to_owned(), Arc::new(tool));
    }

    /// Register a tool, returning an error if the name is already taken.
    ///
    /// Unlike [`register`](Self::register), this surfaces collisions — covering
    /// both MCP↔builtin and MCP↔MCP — so an ambiguous tool identity fails loudly
    /// at startup rather than silently shadowing a tool and confusing the LLM.
    /// Callers that intentionally namespace tools should apply a prefix before
    /// calling this.
    pub fn try_register(
        &mut self,
        tool: impl Tool + 'static,
    ) -> Result<(), ToolNameConflict> {
        let name = tool.name().to_owned();
        if self.tools.contains_key(&name) {
            return Err(ToolNameConflict(name));
        }
        self.tools.insert(name, Arc::new(tool));
        Ok(())
    }

    /// Get a tool by name.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// Get tool definitions for all registered tools (to send to the model).
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|t| t.to_definition()).collect()
    }

    /// Get definitions for tools whose name matches any of the given patterns.
    ///
    /// Each pattern matches literally unless it contains `*` or `?`, in which
    /// case it is treated as a glob (`*` = any run of chars, `?` = one char).
    /// Examples: `"read_file"` (exact), `"filesystem_*"` (prefix), `"*_echo"`,
    /// `"*"` (all). Returned order follows the registry's internal order, not
    /// the pattern order.
    pub fn definitions_for(&self, patterns: &[String]) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .filter(|(name, _)| patterns.iter().any(|p| tool_name_matches(name, p)))
            .map(|(_, t)| t.to_definition())
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

/// Error returned by [`ToolRegistry::try_register`] when a tool name is already
/// registered. The wrapped [`String`] is the conflicting tool name.
#[derive(Debug, Clone, thiserror::Error)]
#[error("tool name already registered: '{0}'")]
pub struct ToolNameConflict(pub String);

/// Match a tool name against a `tools:`-list pattern. Plain patterns (no glob
/// metacharacters) require an exact match; patterns containing `*` or `?` are
/// treated as globs ([`wildcard_match`]).
fn tool_name_matches(name: &str, pattern: &str) -> bool {
    if !pattern.contains('*') && !pattern.contains('?') {
        return name == pattern;
    }
    wildcard_match(name, pattern)
}

/// Glob match: `*` matches any (possibly empty) run of characters, `?` matches
/// exactly one. All other characters match literally.
fn wildcard_match(text: &str, pattern: &str) -> bool {
    let text: Vec<char> = text.chars().collect();
    let pattern: Vec<char> = pattern.chars().collect();
    let mut ti = 0usize;
    let mut pi = 0usize;
    let mut star: Option<usize> = None;
    let mut stash = 0usize;
    while ti < text.len() {
        if pi < pattern.len() && (pattern[pi] == '?' || pattern[pi] == text[ti]) {
            ti += 1;
            pi += 1;
        } else if pi < pattern.len() && pattern[pi] == '*' {
            star = Some(pi);
            stash = ti;
            pi += 1;
        } else if let Some(s) = star {
            // backtrack: let the last `*` consume one more char
            pi = s + 1;
            stash += 1;
            ti = stash;
        } else {
            return false;
        }
    }
    while pi < pattern.len() && pattern[pi] == '*' {
        pi += 1;
    }
    pi == pattern.len()
}

#[async_trait]
impl ToolExecutor for ToolRegistry {
    async fn execute(&self, name: &str, args: &serde_json::Value) -> ToolOutput {
        match ToolRegistry::execute(self, name, args).await {
            Ok(output) => output,
            Err(e) => ToolOutput {
                content: format!("Error: {}", e),
                bytes: 0,
                duration_ms: 0,
                status: ToolOutputStatus::Error,
            },
        }
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

/// Validate that a target path is within the workspace root and return the resolved full path.
///
/// Rejects path traversal (`..`) and paths that canonicalize outside the workspace.
/// For non-existent files the check falls back to the lexical path.
fn resolve_workspace_path(
    workspace_root: &std::path::Path,
    target_path: &str,
) -> Result<PathBuf, ToolError> {
    if target_path.contains("..") {
        return Err(ToolError::SecurityError(
            "Path traversal not allowed".into(),
        ));
    }

    let canonical_root = workspace_root.canonicalize().map_err(|e| {
        ToolError::ExecutionError(format!("Failed to resolve workspace root: {}", e))
    })?;

    let full_path = if target_path.starts_with('/') {
        PathBuf::from(target_path)
    } else {
        canonical_root.join(target_path)
    };

    let check_path = match full_path.canonicalize() {
        Ok(canonical) => canonical,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => full_path.clone(),
        Err(e) => {
            return Err(ToolError::ExecutionError(format!(
                "Failed to resolve path '{}': {}",
                target_path, e
            )));
        }
    };

    if !check_path.starts_with(&canonical_root) {
        return Err(ToolError::SecurityError(
            "Path is outside workspace".into(),
        ));
    }

    Ok(full_path)
}

/// Reads the contents of a file within the workspace.
pub struct ReadFileTool {
    workspace_root: PathBuf,
}

impl ReadFileTool {
    /// Create a new ReadFileTool confined to the given workspace root.
    pub fn new(workspace_root: PathBuf) -> Self {
        Self { workspace_root }
    }
}

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "Read the contents of a file at the given path (within workspace)"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path (absolute or relative to workspace)" }
            },
            "required": ["path"]
        })
    }
    async fn execute(&self, args: &serde_json::Value) -> Result<ToolOutput, ToolError> {
        let path = args["path"].as_str().ok_or_else(|| {
            ToolError::ExecutionError("Missing 'path' parameter".into())
        })?;

        let full_path = resolve_workspace_path(&self.workspace_root, path)?;

        let content = tokio::fs::read_to_string(&full_path)
            .await
            .map_err(|e| {
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

        // Validate and resolve path — single source of truth.
        let full_path = resolve_workspace_path(&self.workspace_root, path)?;

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

/// Lists directory contents within the workspace.
pub struct ListDirTool {
    workspace_root: PathBuf,
}

impl ListDirTool {
    /// Create a new ListDirTool confined to the given workspace root.
    pub fn new(workspace_root: PathBuf) -> Self {
        Self { workspace_root }
    }
}

#[async_trait]
impl Tool for ListDirTool {
    fn name(&self) -> &str {
        "list_dir"
    }
    fn description(&self) -> &str {
        "List files and directories at the given path (within workspace)"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory path (absolute or relative to workspace)" }
            },
            "required": ["path"]
        })
    }
    async fn execute(&self, args: &serde_json::Value) -> Result<ToolOutput, ToolError> {
        let path = args["path"].as_str().ok_or_else(|| {
            ToolError::ExecutionError("Missing 'path' parameter".into())
        })?;

        let full_path = resolve_workspace_path(&self.workspace_root, path)?;

        let mut entries = tokio::fs::read_dir(&full_path)
            .await
            .map_err(|e| {
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
    registry.register(ReadFileTool::new(workspace_root.clone()));
    registry.register(ListDirTool::new(workspace_root.clone()));
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

        let tool = ReadFileTool::new(dir.path().to_path_buf());
        let result = tool
            .execute(&serde_json::json!({"path": "test.txt"}))
            .await
            .unwrap();
        assert_eq!(result.content, "hello world");
        assert_eq!(result.status, ToolOutputStatus::Success);
    }

    #[tokio::test]
    async fn test_read_file_absolute_path_in_workspace() {
        let dir = tempfile::TempDir::new().unwrap();
        let file_path = dir.path().join("abs.txt");
        std::fs::write(&file_path, "abs content").unwrap();

        let tool = ReadFileTool::new(dir.path().to_path_buf());
        let path_str = file_path.to_str().unwrap();
        let result = tool
            .execute(&serde_json::json!({"path": path_str}))
            .await
            .unwrap();
        assert_eq!(result.content, "abs content");
    }

    #[tokio::test]
    async fn test_read_file_missing_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let tool = ReadFileTool::new(dir.path().to_path_buf());
        let result = tool.execute(&serde_json::json!({})).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ToolError::ExecutionError(msg) if msg.contains("Missing 'path'")));
    }

    #[tokio::test]
    async fn test_read_file_nonexistent() {
        let dir = tempfile::TempDir::new().unwrap();
        let tool = ReadFileTool::new(dir.path().to_path_buf());
        let result = tool
            .execute(&serde_json::json!({"path": "nonexistent_file.txt"}))
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ToolError::ExecutionError(msg) if msg.contains("Failed to read")));
    }

    #[tokio::test]
    async fn test_read_file_rejects_path_traversal() {
        let dir = tempfile::TempDir::new().unwrap();
        let tool = ReadFileTool::new(dir.path().to_path_buf());
        let result = tool
            .execute(&serde_json::json!({"path": "../../../etc/passwd"}))
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ToolError::SecurityError(msg) if msg.contains("Path traversal")));
    }

    #[tokio::test]
    async fn test_read_file_rejects_outside_workspace() {
        let dir = tempfile::TempDir::new().unwrap();
        let tool = ReadFileTool::new(dir.path().to_path_buf());

        // Absolute path outside workspace
        let result = tool
            .execute(&serde_json::json!({"path": "/etc/hostname"}))
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, ToolError::SecurityError(_)),
            "expected SecurityError, got {err:?}"
        );
    }

    #[tokio::test]
    async fn test_read_file_nested_subdir() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        std::fs::write(dir.path().join("a/b/c.txt"), "deep").unwrap();

        let tool = ReadFileTool::new(dir.path().to_path_buf());
        let result = tool
            .execute(&serde_json::json!({"path": "a/b/c.txt"}))
            .await
            .unwrap();
        assert_eq!(result.content, "deep");
    }

    #[tokio::test]
    async fn test_list_dir_success() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "aaa").unwrap();
        std::fs::write(dir.path().join("b.txt"), "bbb").unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();

        let tool = ListDirTool::new(dir.path().to_path_buf());
        let result = tool
            .execute(&serde_json::json!({"path": "."}))
            .await
            .unwrap();
        assert_eq!(result.status, ToolOutputStatus::Success);
        assert!(result.content.contains("a.txt"));
        assert!(result.content.contains("b.txt"));
        assert!(result.content.contains("subdir"));
    }

    #[tokio::test]
    async fn test_list_dir_absolute_path() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/x.txt"), "x").unwrap();

        let tool = ListDirTool::new(dir.path().to_path_buf());
        let path_str = dir.path().join("sub").to_str().unwrap().to_string();
        let result = tool
            .execute(&serde_json::json!({"path": path_str}))
            .await
            .unwrap();
        assert!(result.content.contains("x.txt"));
    }

    #[tokio::test]
    async fn test_list_dir_missing_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let tool = ListDirTool::new(dir.path().to_path_buf());
        let result = tool.execute(&serde_json::json!({})).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ToolError::ExecutionError(msg) if msg.contains("Missing 'path'")));
    }

    #[tokio::test]
    async fn test_list_dir_nonexistent() {
        let dir = tempfile::TempDir::new().unwrap();
        let tool = ListDirTool::new(dir.path().to_path_buf());
        let result = tool
            .execute(&serde_json::json!({"path": "no_such_subdir"}))
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ToolError::ExecutionError(msg) if msg.contains("Failed to read dir")));
    }

    #[tokio::test]
    async fn test_list_dir_rejects_path_traversal() {
        let dir = tempfile::TempDir::new().unwrap();
        let tool = ListDirTool::new(dir.path().to_path_buf());
        let result = tool
            .execute(&serde_json::json!({"path": "../../../etc"}))
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ToolError::SecurityError(msg) if msg.contains("Path traversal")));
    }

    #[tokio::test]
    async fn test_list_dir_rejects_outside_workspace() {
        let dir = tempfile::TempDir::new().unwrap();
        let tool = ListDirTool::new(dir.path().to_path_buf());

        let result = tool
            .execute(&serde_json::json!({"path": "/etc"}))
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, ToolError::SecurityError(_)),
            "expected SecurityError, got {err:?}"
        );
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

#[cfg(test)]
mod registry_tests {
    use super::*;

    /// Minimal `Tool` impl for testing registry mechanics (name is configurable).
    struct DummyTool(&'static str);

    #[async_trait]
    impl Tool for DummyTool {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "dummy"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn execute(&self, _args: &serde_json::Value) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput {
                content: String::new(),
                bytes: 0,
                duration_ms: 0,
                status: ToolOutputStatus::Success,
            })
        }
    }

    #[test]
    fn try_register_accepts_a_new_name() {
        let mut reg = ToolRegistry::new();
        assert!(reg.try_register(DummyTool("foo")).is_ok());
        assert!(reg.contains("foo"));
    }

    #[test]
    fn try_register_rejects_a_duplicate() {
        let mut reg = ToolRegistry::new();
        reg.register(DummyTool("dup"));
        let err = reg
            .try_register(DummyTool("dup"))
            .expect_err("collision must error");
        assert_eq!(err.0, "dup");
    }

    #[test]
    fn register_overwrites_silently() {
        let mut reg = ToolRegistry::new();
        reg.register(DummyTool("x"));
        reg.register(DummyTool("x")); // no error — silent overwrite by design
        assert!(reg.contains("x"));
    }

    // ── definitions_for glob matching ───────────────────────────────────

    #[test]
    fn definitions_for_exact_name() {
        let mut reg = ToolRegistry::new();
        reg.register(DummyTool("alpha"));
        reg.register(DummyTool("beta"));
        let defs = reg.definitions_for(&["alpha".to_string()]);
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["alpha"]);
    }

    #[test]
    fn definitions_for_glob_prefix() {
        let mut reg = ToolRegistry::new();
        reg.register(DummyTool("fs_read"));
        reg.register(DummyTool("fs_write"));
        reg.register(DummyTool("other"));
        let defs = reg.definitions_for(&["fs_*".to_string()]);
        let mut names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["fs_read", "fs_write"]);
    }

    #[test]
    fn definitions_for_glob_suffix_and_question() {
        let mut reg = ToolRegistry::new();
        reg.register(DummyTool("get_x"));
        reg.register(DummyTool("get_y"));
        reg.register(DummyTool("set_x"));
        // "*_x" → get_x + set_x
        let defs_a = reg.definitions_for(&["*_x".to_string()]);
        let mut a: Vec<&str> = defs_a.iter().map(|d| d.name.as_str()).collect();
        a.sort();
        assert_eq!(a, vec!["get_x", "set_x"]);
        // "?et_x" → get_x + set_x (? = g or s)
        let defs_b = reg.definitions_for(&["?et_x".to_string()]);
        let mut b: Vec<&str> = defs_b.iter().map(|d| d.name.as_str()).collect();
        b.sort();
        assert_eq!(b, vec!["get_x", "set_x"]);
    }

    #[test]
    fn definitions_for_multiple_patterns_union() {
        let mut reg = ToolRegistry::new();
        reg.register(DummyTool("a1"));
        reg.register(DummyTool("a2"));
        reg.register(DummyTool("b1"));
        let defs = reg.definitions_for(&["a*".to_string(), "b1".to_string()]);
        assert_eq!(defs.len(), 3, "union of patterns");
    }

    #[test]
    fn definitions_for_star_matches_all() {
        let mut reg = ToolRegistry::new();
        reg.register(DummyTool("a"));
        reg.register(DummyTool("b"));
        assert_eq!(reg.definitions_for(&["*".to_string()]).len(), 2);
    }

    #[test]
    fn definitions_for_pattern_matching_nothing_is_empty() {
        let mut reg = ToolRegistry::new();
        reg.register(DummyTool("alpha"));
        assert!(reg.definitions_for(&["zeta_*".to_string()]).is_empty());
    }
}
