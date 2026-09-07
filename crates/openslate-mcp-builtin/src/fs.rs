//! Filesystem MCP server: `read_file` / `write_file` sandboxed to a workspace
//! root.
//!
//! Faithful port of the legacy `ReadFileTool` / `WriteFileTool` from
//! `openslate-core::tool` (parameters, output formats and error messages are
//! preserved verbatim). Differences inherent to MCP:
//!
//! - Business failures (missing file, sandbox rejection, I/O errors) return
//!   `Ok(CallToolResult::error(...))` so the error text is fed back to the
//!   LLM instead of surfacing as a protocol error.
//! - Structurally invalid arguments (e.g. an empty `path`) return
//!   `Err(McpError::invalid_params(...))`.

use std::path::{Path, PathBuf};

use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::tool::ToolRouter,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock},
    schemars, tool, tool_handler, tool_router,
};

// ── sandbox path resolution (ported from openslate-core tool.rs) ────────────

/// Sandbox violation or execution failure while resolving a target path.
/// Mirrors `ToolError::SecurityError` / `ToolError::ExecutionError` from the
/// legacy tools; both map to a tool-level error result.
#[derive(Debug)]
enum PathError {
    /// Sandbox violation (path traversal, outside workspace).
    Security(&'static str),
    /// I/O or resolution failure.
    Execution(String),
}

impl PathError {
    fn message(&self) -> String {
        match self {
            PathError::Security(msg) => (*msg).to_owned(),
            PathError::Execution(msg) => msg.clone(),
        }
    }
}

/// Validate that a target path is within the workspace root and return the
/// resolved full path.
///
/// Rules:
/// 1. Any `..` component → rejected ("Path traversal not allowed").
/// 2. Unresolvable workspace root → execution error.
/// 3. Absolute targets are used as-is; relative targets are joined onto the
///    canonical root (absolute paths inside the workspace are allowed).
/// 4. If the target does not exist yet, the longest *existing* ancestor is
///    canonicalized and the remaining components appended (this is what lets
///    `write_file` create new files — while still resolving symlinks in the
///    existing part of the path, so a symlinked directory inside the
///    workspace cannot smuggle a write past the containment check).
/// 5. Targets not under the canonical root → "Path is outside workspace".
fn resolve_workspace_path(workspace_root: &Path, target_path: &str) -> Result<PathBuf, PathError> {
    if target_path.contains("..") {
        return Err(PathError::Security("Path traversal not allowed"));
    }

    let canonical_root = workspace_root
        .canonicalize()
        .map_err(|e| PathError::Execution(format!("Failed to resolve workspace root: {}", e)))?;

    let full_path = if target_path.starts_with('/') {
        PathBuf::from(target_path)
    } else {
        canonical_root.join(target_path)
    };

    let resolved = match full_path.canonicalize() {
        Ok(canonical) => canonical,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => canonicalize_via_ancestor(&full_path)
            .map_err(|e| {
                PathError::Execution(format!("Failed to resolve path '{}': {}", target_path, e))
            })?,
        Err(e) => {
            return Err(PathError::Execution(format!(
                "Failed to resolve path '{}': {}",
                target_path, e
            )));
        }
    };

    if !resolved.starts_with(&canonical_root) {
        return Err(PathError::Security("Path is outside workspace"));
    }

    Ok(resolved)
}

/// Canonicalize the longest existing ancestor of `path` and append the
/// remaining components.
///
/// Unlike a plain lexical fallback, this resolves symlinks in the part of
/// the path that already exists, so containment checks see where the path
/// *actually* leads. Example: with `ws/link -> /tmp/evil` (a symlinked
/// directory), `ws/link/new.txt` resolves to `/tmp/evil/new.txt` and is
/// rejected instead of passing the check lexically.
fn canonicalize_via_ancestor(path: &Path) -> std::io::Result<PathBuf> {
    let mut existing = path.to_path_buf();
    let mut remainder: Vec<std::ffi::OsString> = Vec::new();
    loop {
        match existing.canonicalize() {
            Ok(canonical) => {
                let mut resolved = canonical;
                for component in remainder.iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Climb one component up; the filesystem root always exists,
                // so the loop terminates.
                let Some(name) = existing.file_name().map(|n| n.to_os_string()) else {
                    return Err(e);
                };
                remainder.push(name);
                if !existing.pop() {
                    return Err(e);
                }
            }
            Err(e) => return Err(e),
        }
    }
}

// ── MCP server ──────────────────────────────────────────────────────────────

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ReadFileParams {
    #[schemars(description = "File path (absolute or relative to workspace)")]
    pub path: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct WriteFileParams {
    #[schemars(description = "File path (absolute or relative to workspace)")]
    pub path: String,
    #[schemars(description = "Content to write to the file")]
    pub content: String,
}

/// In-process MCP server exposing `read_file` / `write_file` inside a
/// workspace sandbox.
#[derive(Clone)]
pub struct FsServer {
    root: PathBuf,
    tool_router: ToolRouter<Self>,
}

fn error_text(msg: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(msg.into())])
}

fn success_text(content: String) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(content)])
}

#[tool_router]
impl FsServer {
    /// Create a server confined to the given workspace root. The root is
    /// canonicalized once; if that fails (root missing) the raw path is kept
    /// so the failure surfaces on first use, like the legacy tools.
    pub fn new(root: impl AsRef<Path>) -> Self {
        let root = root
            .as_ref()
            .canonicalize()
            .unwrap_or_else(|_| root.as_ref().to_path_buf());
        Self {
            root,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "Read the contents of a file at the given path (within workspace)")]
    async fn read_file(
        &self,
        Parameters(args): Parameters<ReadFileParams>,
    ) -> Result<CallToolResult, McpError> {
        if args.path.trim().is_empty() {
            return Err(McpError::invalid_params("'path' must not be empty", None));
        }

        let full_path = match resolve_workspace_path(&self.root, &args.path) {
            Ok(p) => p,
            Err(e) => return Ok(error_text(e.message())),
        };

        let content = match tokio::fs::read_to_string(&full_path).await {
            Ok(c) => c,
            Err(e) => {
                return Ok(error_text(format!("Failed to read '{}': {}", args.path, e)));
            }
        };
        Ok(success_text(content))
    }

    #[tool(description = "Write content to a file within the workspace")]
    async fn write_file(
        &self,
        Parameters(args): Parameters<WriteFileParams>,
    ) -> Result<CallToolResult, McpError> {
        if args.path.trim().is_empty() {
            return Err(McpError::invalid_params("'path' must not be empty", None));
        }

        let full_path = match resolve_workspace_path(&self.root, &args.path) {
            Ok(p) => p,
            Err(e) => return Ok(error_text(e.message())),
        };

        // Create parent directories if they don't exist (legacy behavior).
        if let Some(parent) = full_path.parent()
            && let Err(e) = tokio::fs::create_dir_all(parent).await
        {
            return Ok(error_text(format!("Failed to create directories: {}", e)));
        }

        if let Err(e) = tokio::fs::write(&full_path, &args.content).await {
            return Ok(error_text(format!(
                "Failed to write '{}': {}",
                args.path, e
            )));
        }

        Ok(success_text(format!(
            "Successfully wrote {} bytes to '{}'",
            args.content.len(),
            args.path
        )))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for FsServer {}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{first_text, is_error, spawn_server};
    use rmcp::model::CallToolRequestParams;

    async fn client_in(
        dir: &tempfile::TempDir,
    ) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
        spawn_server(FsServer::new(dir.path())).await
    }

    async fn call(
        client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
        tool: &str,
        args: rmcp::model::JsonObject,
    ) -> rmcp::model::CallToolResult {
        client
            .call_tool(CallToolRequestParams::new(tool.to_owned()).with_arguments(args))
            .await
            .expect("call_tool should not be a protocol error")
    }

    // ── read_file ──

    #[tokio::test]
    async fn read_file_success() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("test.txt"), "hello world").unwrap();

        let client = client_in(&dir).await;
        let result = call(&client, "read_file", rmcp::object!({"path": "test.txt"})).await;
        assert!(!is_error(&result));
        assert_eq!(first_text(&result), "hello world");
    }

    #[tokio::test]
    async fn read_file_absolute_path_in_workspace() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("abs.txt"), "abs content").unwrap();

        let client = client_in(&dir).await;
        let abs = dir.path().join("abs.txt");
        let result = call(
            &client,
            "read_file",
            rmcp::object!({"path": abs.to_str().unwrap()}),
        )
        .await;
        assert!(!is_error(&result));
        assert_eq!(first_text(&result), "abs content");
    }

    #[tokio::test]
    async fn read_file_nested_subdir() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        std::fs::write(dir.path().join("a/b/c.txt"), "deep").unwrap();

        let client = client_in(&dir).await;
        let result = call(&client, "read_file", rmcp::object!({"path": "a/b/c.txt"})).await;
        assert!(!is_error(&result));
        assert_eq!(first_text(&result), "deep");
    }

    #[tokio::test]
    async fn read_file_nonexistent_is_business_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        let result = call(
            &client,
            "read_file",
            rmcp::object!({"path": "nonexistent_file.txt"}),
        )
        .await;
        assert!(is_error(&result));
        assert!(
            first_text(&result).contains("Failed to read"),
            "{}",
            first_text(&result)
        );
    }

    #[tokio::test]
    async fn read_file_rejects_path_traversal() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        let result = call(
            &client,
            "read_file",
            rmcp::object!({"path": "../../../etc/passwd"}),
        )
        .await;
        assert!(is_error(&result));
        assert_eq!(first_text(&result), "Path traversal not allowed");
    }

    #[tokio::test]
    async fn read_file_rejects_outside_workspace() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        let result = call(
            &client,
            "read_file",
            rmcp::object!({"path": "/etc/hostname"}),
        )
        .await;
        assert!(is_error(&result));
        assert_eq!(first_text(&result), "Path is outside workspace");
    }

    #[tokio::test]
    async fn read_file_missing_path_param_is_tool_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        // rmcp's parameter wrapper turns a missing required field into a
        // tool-level error result (message visible to the LLM), not a
        // protocol error.
        let result = call(&client, "read_file", rmcp::object!({})).await;
        assert!(is_error(&result));
        assert!(
            first_text(&result).contains("missing field `path`"),
            "{}",
            first_text(&result)
        );
    }

    #[tokio::test]
    async fn read_file_empty_path_is_invalid_params() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        let err = client
            .call_tool(
                CallToolRequestParams::new("read_file").with_arguments(rmcp::object!({"path": ""})),
            )
            .await
            .expect_err("empty path must be invalid params");
        assert!(format!("{err}").contains("path"));
    }

    // ── write_file ──

    #[tokio::test]
    async fn write_file_success() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        let result = call(
            &client,
            "write_file",
            rmcp::object!({"path": "test.txt", "content": "hello world"}),
        )
        .await;
        assert!(!is_error(&result));
        assert_eq!(
            first_text(&result),
            "Successfully wrote 11 bytes to 'test.txt'"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("test.txt")).unwrap(),
            "hello world"
        );
    }

    #[tokio::test]
    async fn write_file_absolute_path_in_workspace() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;
        let target = dir.path().join("subdir/test.txt");

        let result = call(
            &client,
            "write_file",
            rmcp::object!({"path": target.to_str().unwrap(), "content": "absolute"}),
        )
        .await;
        assert!(!is_error(&result));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "absolute");
    }

    #[tokio::test]
    async fn write_file_creates_parent_directories() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        let result = call(
            &client,
            "write_file",
            rmcp::object!({"path": "a/b/c/nested.txt", "content": "nested content"}),
        )
        .await;
        assert!(!is_error(&result));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a/b/c/nested.txt")).unwrap(),
            "nested content"
        );
    }

    #[tokio::test]
    async fn write_file_overwrites_existing() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("existing.txt"), "original").unwrap();
        let client = client_in(&dir).await;

        let result = call(
            &client,
            "write_file",
            rmcp::object!({"path": "existing.txt", "content": "new content"}),
        )
        .await;
        assert!(!is_error(&result));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("existing.txt")).unwrap(),
            "new content"
        );
    }

    #[tokio::test]
    async fn write_file_rejects_path_traversal() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        let result = call(
            &client,
            "write_file",
            rmcp::object!({"path": "../escape.txt", "content": "bad"}),
        )
        .await;
        assert!(is_error(&result));
        assert_eq!(first_text(&result), "Path traversal not allowed");
    }

    #[tokio::test]
    async fn write_file_rejects_double_dot_traversal() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        let result = call(
            &client,
            "write_file",
            rmcp::object!({"path": "foo/../../bar.txt", "content": "bad"}),
        )
        .await;
        assert!(is_error(&result));
        assert_eq!(first_text(&result), "Path traversal not allowed");
    }

    #[tokio::test]
    async fn write_file_rejects_outside_workspace() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        let result = call(
            &client,
            "write_file",
            rmcp::object!({"path": "/tmp/definitely-outside-openslate.txt", "content": "x"}),
        )
        .await;
        assert!(is_error(&result));
        assert_eq!(first_text(&result), "Path is outside workspace");
    }

    #[tokio::test]
    async fn write_file_missing_content_param_is_tool_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        let result = call(&client, "write_file", rmcp::object!({"path": "test.txt"})).await;
        assert!(is_error(&result));
        assert!(
            first_text(&result).contains("missing field `content`"),
            "{}",
            first_text(&result)
        );
    }

    // ── listing ──

    #[tokio::test]
    async fn list_tools_exposes_read_and_write() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        let tools = client.peer().list_tools(None).await.unwrap();
        let names: Vec<String> = tools.tools.iter().map(|t| t.name.to_string()).collect();
        assert!(names.contains(&"read_file".to_string()));
        assert!(names.contains(&"write_file".to_string()));
    }

    // ── sandbox helper unit tests (ports of the tool.rs semantics) ──

    #[test]
    fn resolve_rejects_traversal() {
        let dir = tempfile::TempDir::new().unwrap();
        let err = resolve_workspace_path(dir.path(), "../x").unwrap_err();
        assert!(matches!(
            err,
            PathError::Security("Path traversal not allowed")
        ));
    }

    #[test]
    fn resolve_rejects_outside_workspace() {
        let dir = tempfile::TempDir::new().unwrap();
        let err = resolve_workspace_path(dir.path(), "/etc/hosts").unwrap_err();
        assert!(matches!(
            err,
            PathError::Security("Path is outside workspace")
        ));
    }

    #[test]
    fn resolve_allows_absolute_inside_workspace() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "x").unwrap();
        let abs = dir.path().join("f.txt");
        let resolved = resolve_workspace_path(dir.path(), abs.to_str().unwrap()).unwrap();
        assert_eq!(resolved, abs);
    }

    #[test]
    fn resolve_nonexistent_target_falls_back_to_lexical() {
        let dir = tempfile::TempDir::new().unwrap();
        // Key path for write_file: a not-yet-existing file inside the root
        // must resolve fine (lexical fallback).
        let resolved = resolve_workspace_path(dir.path(), "new/deep/file.txt").unwrap();
        assert_eq!(
            resolved,
            dir.path().canonicalize().unwrap().join("new/deep/file.txt")
        );
    }

    #[test]
    fn resolve_fails_when_workspace_root_missing() {
        // Rule 2 of the sandbox: an unresolvable root is an execution error
        // (FsServer::new keeps the raw path, so this fires on first use).
        let missing = tempfile::TempDir::new().unwrap();
        let root = missing.path().join("does/not/exist");
        drop(missing);

        let err = resolve_workspace_path(&root, "a.txt").unwrap_err();
        assert!(
            matches!(err, PathError::Execution(ref msg) if msg.starts_with("Failed to resolve workspace root")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn nonexistent_root_reports_execution_error_on_first_use() {
        let missing = tempfile::TempDir::new().unwrap();
        let root = missing.path().join("gone");
        drop(missing);

        let client = spawn_server(FsServer::new(&root)).await;
        let result = call(&client, "read_file", rmcp::object!({"path": "a.txt"})).await;
        assert!(is_error(&result));
        assert!(
            first_text(&result).starts_with("Failed to resolve workspace root"),
            "{}",
            first_text(&result)
        );

        let result = call(
            &client,
            "write_file",
            rmcp::object!({"path": "a.txt", "content": "x"}),
        )
        .await;
        assert!(is_error(&result));
        assert!(
            first_text(&result).starts_with("Failed to resolve workspace root"),
            "{}",
            first_text(&result)
        );
    }

    // ── symlink sandbox tests (symlink creation is Unix-only in std) ──

    #[cfg(unix)]
    #[tokio::test]
    async fn read_file_rejects_symlink_to_outside_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let evil_dir = tempfile::TempDir::new().unwrap();
        std::fs::write(evil_dir.path().join("secret.txt"), "stolen").unwrap();
        std::os::unix::fs::symlink(
            evil_dir.path().join("secret.txt"),
            dir.path().join("alias.txt"),
        )
        .unwrap();

        let client = client_in(&dir).await;
        let result = call(&client, "read_file", rmcp::object!({"path": "alias.txt"})).await;
        assert!(is_error(&result));
        assert_eq!(first_text(&result), "Path is outside workspace");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_file_rejects_new_file_via_symlinked_dir() {
        // The write-side escape: `link -> /tmp/evil`, target `link/new.txt`
        // does not exist yet, so only the ancestor walk catches it.
        let dir = tempfile::TempDir::new().unwrap();
        let evil_dir = tempfile::TempDir::new().unwrap();
        std::os::unix::fs::symlink(evil_dir.path(), dir.path().join("link")).unwrap();

        let client = client_in(&dir).await;
        let result = call(
            &client,
            "write_file",
            rmcp::object!({"path": "link/new.txt", "content": "escaped"}),
        )
        .await;
        assert!(is_error(&result));
        assert_eq!(first_text(&result), "Path is outside workspace");
        // The outside target must not have been created.
        assert!(!evil_dir.path().join("new.txt").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn in_root_symlinked_file_read_and_write_still_work() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("real.txt"), "original").unwrap();
        std::os::unix::fs::symlink("real.txt", dir.path().join("alias.txt")).unwrap();

        let client = client_in(&dir).await;

        let result = call(&client, "read_file", rmcp::object!({"path": "alias.txt"})).await;
        assert!(!is_error(&result), "{}", first_text(&result));
        assert_eq!(first_text(&result), "original");

        let result = call(
            &client,
            "write_file",
            rmcp::object!({"path": "alias.txt", "content": "updated"}),
        )
        .await;
        assert!(!is_error(&result), "{}", first_text(&result));
        // The write lands on the real file (symlink dereferenced).
        assert_eq!(
            std::fs::read_to_string(dir.path().join("real.txt")).unwrap(),
            "updated"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_new_file_via_in_root_symlinked_dir_works() {
        // Legit use of the fallback path: symlinked directory that stays
        // inside the workspace — new files through it are allowed.
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("realdir")).unwrap();
        std::os::unix::fs::symlink("realdir", dir.path().join("dirlink")).unwrap();

        let client = client_in(&dir).await;
        let result = call(
            &client,
            "write_file",
            rmcp::object!({"path": "dirlink/new.txt", "content": "inside"}),
        )
        .await;
        assert!(!is_error(&result), "{}", first_text(&result));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("realdir/new.txt")).unwrap(),
            "inside"
        );
    }
}
