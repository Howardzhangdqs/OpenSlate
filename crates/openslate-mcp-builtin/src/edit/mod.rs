//! Edit MCP server: `edit_file` applies a context patch to an existing file.
//!
//! The matching engine lives in [`editor`] (pure, MCP-free); this module only
//! handles argument validation, workspace-relative path resolution and
//! mapping [`editor::EditError`] to LLM-facing messages.

pub mod editor;

use std::path::{Path, PathBuf};

use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::tool::ToolRouter,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock},
    schemars, tool, tool_handler, tool_router,
};

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct EditFileParams {
    #[schemars(description = "Workspace-relative path to an existing text file")]
    pub path: String,
    #[schemars(description = "Context patch containing only the required changes")]
    pub patch: String,
}

/// In-process MCP server exposing `edit_file` for files under a workspace
/// root. Unlike the fs server, paths must be workspace-relative (no absolute
/// paths) and must already exist.
#[derive(Clone)]
pub struct EditServer {
    root: PathBuf,
    tool_router: ToolRouter<Self>,
}

fn error_text(msg: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(msg.into())])
}

fn success_text(content: String) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(content)])
}

/// Render an [`editor::EditError`] as its LLM-facing business message.
fn error_message(err: editor::EditError) -> String {
    use editor::EditError;
    match err {
        EditError::NoMatch { hunk } => format!(
            "NO_MATCH: hunk {hunk}. Read the target area and retry with more accurate context."
        ),
        EditError::AnchorNotFound { hunk } => format!(
            "ANCHOR_NOT_FOUND: hunk {hunk}. The @@ anchor text does not appear in the file. The @@ line must be a short substring copied verbatim from the file (not a unified diff header)."
        ),
        EditError::AmbiguousMatch { hunk, matches } => format!(
            "AMBIGUOUS_MATCH: hunk {hunk} matched {matches} locations. Add an @@ anchor or more context."
        ),
        EditError::OverlappingHunks => {
            "OVERLAPPING_HUNKS: patch hunks modify overlapping regions.".to_owned()
        }
        EditError::InvalidPatch(msg) => format!("INVALID_PATCH: {msg}"),
        EditError::Io(err) => format!("IO_ERROR: {err}"),
    }
}

#[tool_router]
impl EditServer {
    /// Create a server confined to the given workspace root. The root is
    /// canonicalized once; on failure the raw path is kept so the error
    /// surfaces on first use instead of at construction.
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

    #[tool(
        description = "Edit an existing text file using a context patch. Format: each hunk starts with '@@ ' followed by a short substring copied verbatim from the file (never a unified-diff header like @@ -1,5 +1,5 @@); body lines are prefixed with ' ' (context, copied verbatim incl. indentation), '-' (remove) or '+' (add); separate multiple hunks with a blank line. Example (changes timeout: 30 to 60):\n@@ Config {\n-    timeout: 30,\n+    timeout: 60,\n     retries: 3,"
    )]
    async fn edit_file(
        &self,
        Parameters(args): Parameters<EditFileParams>,
    ) -> Result<CallToolResult, McpError> {
        if args.path.trim().is_empty() {
            return Err(McpError::invalid_params("'path' must not be empty", None));
        }
        if args.patch.trim().is_empty() {
            return Err(McpError::invalid_params("'patch' must not be empty", None));
        }

        // Edit is stricter than the fs tools: workspace-relative paths only.
        if Path::new(&args.path).is_absolute() {
            return Ok(error_text("PATH_ERROR: path must be workspace-relative"));
        }

        let full_path = self.root.join(&args.path);
        let canonical = match tokio::fs::canonicalize(&full_path).await {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(error_text(format!(
                    "PATH_ERROR: file not found: {}",
                    args.path
                )));
            }
            Err(e) => return Ok(error_text(format!("IO_ERROR: {e}"))),
        };
        if !canonical.starts_with(&self.root) {
            return Ok(error_text("PATH_ERROR: path escapes the workspace"));
        }
        match tokio::fs::metadata(&canonical).await {
            Ok(m) if !m.is_file() => {
                return Ok(error_text("PATH_ERROR: target is not a regular file"));
            }
            Ok(_) => {}
            Err(e) => return Ok(error_text(format!("IO_ERROR: {e}"))),
        }

        match editor::apply_context_patch(&canonical, &args.patch).await {
            Ok(result) => Ok(success_text(format!(
                "OK | {} hunks | +{} -{}",
                result.hunks, result.additions, result.deletions
            ))),
            Err(err) => Ok(error_text(error_message(err))),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for EditServer {}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{first_text, is_error, spawn_server};
    use rmcp::model::CallToolRequestParams;

    async fn client_in(
        dir: &tempfile::TempDir,
    ) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
        spawn_server(EditServer::new(dir.path())).await
    }

    async fn edit_file(
        client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
        path: &str,
        patch: &str,
    ) -> rmcp::model::CallToolResult {
        client
            .call_tool(
                CallToolRequestParams::new("edit_file").with_arguments(rmcp::object!({
                    "path": path,
                    "patch": patch
                })),
            )
            .await
            .expect("call_tool ok")
    }

    #[tokio::test]
    async fn successful_edit_reports_counts() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("code.txt"), "fn main() {\n    todo!()\n}\n").unwrap();
        let client = client_in(&dir).await;

        let result = edit_file(
            &client,
            "code.txt",
            " fn main() {\n-    todo!()\n+    println!(\"hi\");\n }\n",
        )
        .await;
        assert!(!is_error(&result), "{}", first_text(&result));
        assert_eq!(first_text(&result), "OK | 1 hunks | +1 -1");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("code.txt")).unwrap(),
            "fn main() {\n    println!(\"hi\");\n}\n"
        );
    }

    #[tokio::test]
    async fn subdirectory_relative_path_works() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("src/util")).unwrap();
        std::fs::write(dir.path().join("src/util/helper.txt"), "alpha\nbeta\n").unwrap();
        let client = client_in(&dir).await;

        let result = edit_file(&client, "src/util/helper.txt", " alpha\n-beta\n+BETA\n").await;
        assert!(!is_error(&result), "{}", first_text(&result));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src/util/helper.txt")).unwrap(),
            "alpha\nBETA\n"
        );
    }

    #[tokio::test]
    async fn no_match_message_is_exact() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "aaa\nbbb\n").unwrap();
        let client = client_in(&dir).await;

        let result = edit_file(&client, "f.txt", " zzz\n-zzz\n+x\n").await;
        assert!(is_error(&result));
        assert_eq!(
            first_text(&result),
            "NO_MATCH: hunk 1. Read the target area and retry with more accurate context."
        );
        // File untouched.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "aaa\nbbb\n"
        );
    }

    #[tokio::test]
    async fn ambiguous_match_message_is_exact() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "dup\nmid\ndup\n").unwrap();
        let client = client_in(&dir).await;

        let result = edit_file(&client, "f.txt", "-dup\n").await;
        assert!(is_error(&result));
        assert_eq!(
            first_text(&result),
            "AMBIGUOUS_MATCH: hunk 1 matched 2 locations. Add an @@ anchor or more context."
        );
    }

    #[tokio::test]
    async fn anchor_not_found_message_is_exact() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "aaa\nbbb\n").unwrap();
        let client = client_in(&dir).await;

        // Body is correct, but the @@ line is a malformed diff header
        // (missing the space between ranges, so it is not tolerated as a
        // separator) whose text appears nowhere in the file.
        let result = edit_file(&client, "f.txt", "@@ -1,5+1,5 @@\n-aaa\n+AAA\n").await;
        assert!(is_error(&result));
        assert_eq!(
            first_text(&result),
            "ANCHOR_NOT_FOUND: hunk 1. The @@ anchor text does not appear in the file. The @@ line must be a short substring copied verbatim from the file (not a unified diff header)."
        );
        // File untouched.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "aaa\nbbb\n"
        );
    }

    #[tokio::test]
    async fn unified_diff_header_patch_applies() {
        // End-to-end: the model wrote unified-diff headers instead of
        // anchors; with correct bodies the edit must still succeed.
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("cfg.rs"),
            "Config {\n    timeout: 30,\n    retries: 3,\n}\n",
        )
        .unwrap();
        let client = client_in(&dir).await;

        let result = edit_file(
            &client,
            "cfg.rs",
            "@@ -1,4 +1,4 @@\n-    timeout: 30,\n+    timeout: 60,\n     retries: 3,",
        )
        .await;
        assert!(!is_error(&result), "{}", first_text(&result));
        assert_eq!(first_text(&result), "OK | 1 hunks | +1 -1");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("cfg.rs")).unwrap(),
            "Config {\n    timeout: 60,\n    retries: 3,\n}\n"
        );
    }

    #[tokio::test]
    async fn overlapping_hunks_message_is_exact() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\nb\nc\n").unwrap();
        let client = client_in(&dir).await;

        let result = edit_file(&client, "f.txt", " a\n-b\n+B\n\n b\n-c\n+C\n").await;
        assert!(is_error(&result));
        assert_eq!(
            first_text(&result),
            "OVERLAPPING_HUNKS: patch hunks modify overlapping regions."
        );
    }

    #[tokio::test]
    async fn invalid_patch_message_is_prefixed() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\n").unwrap();
        let client = client_in(&dir).await;

        let result = edit_file(&client, "f.txt", "garbage-no-prefix\n").await;
        assert!(is_error(&result));
        assert!(
            first_text(&result).starts_with("INVALID_PATCH: "),
            "{}",
            first_text(&result)
        );
    }

    #[tokio::test]
    async fn absolute_path_rejected() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\n").unwrap();
        let client = client_in(&dir).await;

        let abs = dir.path().join("f.txt");
        let result = edit_file(&client, abs.to_str().unwrap(), " a\n-a\n+b\n").await;
        assert!(is_error(&result));
        assert_eq!(
            first_text(&result),
            "PATH_ERROR: path must be workspace-relative"
        );
    }

    #[tokio::test]
    async fn escaping_relative_path_rejected() {
        // Workspace and a sibling directory sharing a parent.
        let parent = tempfile::TempDir::new().unwrap();
        let workspace = parent.path().join("workspace");
        let sibling = parent.path().join("sibling");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(sibling.join("secret.txt"), "s\n").unwrap();

        let client = spawn_server(EditServer::new(&workspace)).await;
        let result = edit_file(&client, "../sibling/secret.txt", " s\n-s\n+x\n").await;
        assert!(is_error(&result));
        assert_eq!(
            first_text(&result),
            "PATH_ERROR: path escapes the workspace"
        );
        // The outside file must be untouched.
        assert_eq!(
            std::fs::read_to_string(sibling.join("secret.txt")).unwrap(),
            "s\n"
        );
    }

    #[tokio::test]
    async fn missing_file_rejected() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        let result = edit_file(&client, "nope.txt", " a\n-a\n+b\n").await;
        assert!(is_error(&result));
        assert_eq!(first_text(&result), "PATH_ERROR: file not found: nope.txt");
    }

    #[tokio::test]
    async fn directory_target_rejected() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("subdir")).unwrap();
        let client = client_in(&dir).await;

        let result = edit_file(&client, "subdir", " a\n-a\n+b\n").await;
        assert!(is_error(&result));
        assert_eq!(
            first_text(&result),
            "PATH_ERROR: target is not a regular file"
        );
    }

    #[tokio::test]
    async fn empty_path_is_invalid_params() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        let err = client
            .call_tool(
                CallToolRequestParams::new("edit_file").with_arguments(rmcp::object!({
                    "path": "",
                    "patch": " a\n-a\n+b\n"
                })),
            )
            .await
            .expect_err("empty path must be invalid params");
        assert!(format!("{err}").contains("path"));
    }

    #[tokio::test]
    async fn empty_patch_is_invalid_params() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\n").unwrap();
        let client = client_in(&dir).await;

        let err = client
            .call_tool(
                CallToolRequestParams::new("edit_file").with_arguments(rmcp::object!({
                    "path": "f.txt",
                    "patch": ""
                })),
            )
            .await
            .expect_err("empty patch must be invalid params");
        assert!(format!("{err}").contains("patch"));
    }

    #[tokio::test]
    async fn whitespace_only_patch_is_invalid_params() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\n").unwrap();
        let client = client_in(&dir).await;

        let err = client
            .call_tool(
                CallToolRequestParams::new("edit_file").with_arguments(rmcp::object!({
                    "path": "f.txt",
                    "patch": "   "
                })),
            )
            .await
            .expect_err("whitespace-only patch must be invalid params");
        assert!(format!("{err}").contains("patch"));
        // File untouched.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "a\n"
        );
    }

    #[tokio::test]
    async fn list_tools_exposes_edit_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        let tools = client.peer().list_tools(None).await.unwrap();
        let names: Vec<String> = tools.tools.iter().map(|t| t.name.to_string()).collect();
        assert_eq!(names, vec!["edit_file".to_string()]);
    }
}
