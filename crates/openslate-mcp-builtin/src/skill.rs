//! Skill MCP server: `read_skill` loads the full instructions of an agent
//! skill on demand.
//!
//! Each skill is a directory with a `SKILL.md` (YAML frontmatter
//! `name`+`description`, markdown body). At agent startup only the name and
//! description go into the system prompt; the body is loaded lazily through
//! this tool. Unlike the fs server, the catalog is an in-process snapshot
//! ([`SkillInfo`]) taken at connect time, so tool output is stable within a
//! run and user-level skill directories (outside any workspace sandbox) stay
//! readable.
//!
//! Error convention (same as [`crate::fs`]): business failures (unknown skill
//! name) return `Ok(CallToolResult::error(...))` so the message is fed back
//! to the LLM; structurally invalid arguments (empty `name`) return
//! `Err(McpError::invalid_params(...))`.

use std::path::Path;

use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::tool::ToolRouter,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock},
    schemars, tool, tool_handler, tool_router,
};

/// Maximum number of resource entries listed before truncation.
const MAX_RESOURCE_ENTRIES: usize = 50;

/// One discoverable skill, converted from core's catalog at connect time.
#[derive(Debug, Clone)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    /// Markdown body (frontmatter already stripped).
    pub body: String,
    /// Directory containing the SKILL.md (for resolving relative resources).
    pub dir: std::path::PathBuf,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ReadSkillParams {
    #[schemars(description = "Name of the skill, as listed in the system prompt's Skills section")]
    pub name: String,
}

/// In-process MCP server exposing `read_skill` over a fixed skill catalog.
#[derive(Clone)]
pub struct SkillServer {
    skills: Vec<SkillInfo>,
    tool_router: ToolRouter<Self>,
}

fn error_text(msg: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(msg.into())])
}

fn success_text(content: String) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(content)])
}

#[tool_router]
impl SkillServer {
    /// Create a server serving the given skills. The catalog is an owned
    /// snapshot: name, description and body all come from memory, never from
    /// disk (only the resource listing touches the filesystem).
    pub fn new(skills: Vec<SkillInfo>) -> Self {
        Self {
            skills,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Load the full instructions of a skill by name. Use when a task matches a skill listed in the system prompt's Skills section."
    )]
    async fn read_skill(
        &self,
        Parameters(args): Parameters<ReadSkillParams>,
    ) -> Result<CallToolResult, McpError> {
        if args.name.trim().is_empty() {
            return Err(McpError::invalid_params("'name' must not be empty", None));
        }

        let Some(skill) = self.skills.iter().find(|s| s.name == args.name) else {
            let mut names: Vec<&str> = self.skills.iter().map(|s| s.name.as_str()).collect();
            names.sort_unstable();
            let msg = if names.is_empty() {
                format!("unknown skill '{}'. no skills available", args.name)
            } else {
                format!(
                    "unknown skill '{}'. Available skills: {}",
                    args.name,
                    names.join(", ")
                )
            };
            return Ok(error_text(msg));
        };

        // Resource listing is best-effort: if the skill directory vanished
        // between discovery and this call, list nothing — the call still
        // succeeds and the in-memory body is returned.
        let mut files = Vec::new();
        if collect_resource_files(&skill.dir, Path::new(""), &mut files)
            .await
            .is_err()
        {
            files.clear();
        }
        files.sort();

        let mut resources = String::new();
        for file in files.iter().take(MAX_RESOURCE_ENTRIES) {
            resources.push_str("<file>");
            resources.push_str(file);
            resources.push_str("</file>\n");
        }
        if files.len() > MAX_RESOURCE_ENTRIES {
            resources.push_str(&format!(
                "<file>(listing truncated, {} more files)</file>\n",
                files.len() - MAX_RESOURCE_ENTRIES
            ));
        }

        let mut content = format!(
            "<skill name=\"{}\">\n{}\n\n{}\n\nSkill directory: {}\nRelative paths in this skill resolve against the skill directory.\n",
            skill.name,
            skill.description,
            skill.body,
            skill.dir.display()
        );
        content.push_str("<skill_resources>\n");
        content.push_str(&resources);
        content.push_str("</skill_resources>\n</skill>");

        Ok(success_text(content))
    }
}

/// Recursively collect files under `dir` as posix-style paths relative to the
/// skill root, skipping `SKILL.md` itself and anything dot-prefixed
/// (`.git`, `.DS_Store`, ...). Read failures propagate so the caller can
/// fall back to an empty listing.
async fn collect_resource_files(
    dir: &Path,
    rel: &Path,
    out: &mut Vec<String>,
) -> std::io::Result<()> {
    let mut entries = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if file_name.starts_with('.') {
            continue;
        }
        let rel_child = rel.join(&*file_name);
        if entry.file_type().await?.is_dir() {
            Box::pin(collect_resource_files(&entry.path(), &rel_child, out)).await?;
        } else if rel_child.as_path() != Path::new("SKILL.md") {
            // Normalize to posix separators regardless of platform.
            out.push(rel_child.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for SkillServer {}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{first_text, is_error, spawn_server};
    use rmcp::model::CallToolRequestParams;

    fn skill_info(name: &str, description: &str, body: &str, dir: &Path) -> SkillInfo {
        SkillInfo {
            name: name.to_owned(),
            description: description.to_owned(),
            body: body.to_owned(),
            dir: dir.to_path_buf(),
        }
    }

    async fn call(
        client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
        name: &str,
    ) -> rmcp::model::CallToolResult {
        client
            .call_tool(
                CallToolRequestParams::new("read_skill").with_arguments(rmcp::object!({
                    "name": name
                })),
            )
            .await
            .expect("call_tool should not be a protocol error")
    }

    // ── known skill ──

    #[tokio::test]
    async fn read_skill_renders_known_skill() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = spawn_server(SkillServer::new(vec![skill_info(
            "demo",
            "Demo skill",
            "Do the thing.",
            dir.path(),
        )]))
        .await;

        let result = call(&client, "demo").await;
        assert!(!is_error(&result), "{}", first_text(&result));
        let text = first_text(&result);
        assert!(text.contains("<skill name=\"demo\">"), "{}", text);
        assert!(text.contains("Demo skill"), "{}", text);
        assert!(text.contains("Do the thing."), "{}", text);
        assert!(
            text.contains(&format!("Skill directory: {}", dir.path().display())),
            "{}",
            text
        );
        assert!(text.ends_with("</skill>"), "{}", text);
    }

    #[tokio::test]
    async fn read_skill_output_format_is_exact() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = spawn_server(SkillServer::new(vec![skill_info(
            "demo",
            "Demo skill",
            "Do the thing.",
            dir.path(),
        )]))
        .await;

        let result = call(&client, "demo").await;
        assert_eq!(
            first_text(&result),
            format!(
                "<skill name=\"demo\">\nDemo skill\n\nDo the thing.\n\nSkill directory: {}\nRelative paths in this skill resolve against the skill directory.\n<skill_resources>\n</skill_resources>\n</skill>",
                dir.path().display()
            )
        );
    }

    #[tokio::test]
    async fn read_skill_serves_in_memory_snapshot_not_disk() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("SKILL.md"),
            "---\nname: demo\n---\nstale on-disk body",
        )
        .unwrap();
        let client = spawn_server(SkillServer::new(vec![skill_info(
            "demo",
            "in-memory description",
            "in-memory body",
            dir.path(),
        )]))
        .await;

        let text = first_text(&call(&client, "demo").await);
        assert!(text.contains("in-memory description"), "{}", text);
        assert!(text.contains("in-memory body"), "{}", text);
        assert!(!text.contains("stale on-disk body"), "{}", text);
    }

    // ── unknown skill ──

    #[tokio::test]
    async fn read_skill_unknown_name_lists_available_sorted() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = spawn_server(SkillServer::new(vec![
            skill_info("zeta", "z", "z body", dir.path()),
            skill_info("alpha", "a", "a body", dir.path()),
        ]))
        .await;

        let result = call(&client, "nope").await;
        assert!(is_error(&result));
        assert_eq!(
            first_text(&result),
            "unknown skill 'nope'. Available skills: alpha, zeta"
        );
    }

    #[tokio::test]
    async fn read_skill_unknown_name_when_catalog_empty() {
        let client = spawn_server(SkillServer::new(vec![])).await;

        let result = call(&client, "anything").await;
        assert!(is_error(&result));
        assert_eq!(
            first_text(&result),
            "unknown skill 'anything'. no skills available"
        );
    }

    #[tokio::test]
    async fn read_skill_empty_name_is_invalid_params() {
        let client = spawn_server(SkillServer::new(vec![])).await;

        let err = client
            .call_tool(
                CallToolRequestParams::new("read_skill")
                    .with_arguments(rmcp::object!({"name": ""})),
            )
            .await
            .expect_err("empty name must be invalid params");
        assert!(format!("{err}").contains("name"));
    }

    // ── resource listing ──

    #[tokio::test]
    async fn read_skill_lists_resources_sorted_posix_paths() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("SKILL.md"), "skill").unwrap();
        std::fs::create_dir_all(dir.path().join("scripts")).unwrap();
        std::fs::write(dir.path().join("scripts/extract.py"), "#!").unwrap();
        std::fs::create_dir_all(dir.path().join("references")).unwrap();
        std::fs::write(dir.path().join("references/REFERENCE.md"), "ref").unwrap();
        std::fs::write(dir.path().join("top.md"), "top").unwrap();
        // Dotfiles and dot-dirs are excluded, at any depth.
        std::fs::write(dir.path().join(".hidden"), "h").unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/config"), "git").unwrap();
        std::fs::create_dir_all(dir.path().join("scripts/.venv")).unwrap();
        std::fs::write(dir.path().join("scripts/.venv/py"), "v").unwrap();
        let client = spawn_server(SkillServer::new(vec![skill_info(
            "demo",
            "d",
            "b",
            dir.path(),
        )]))
        .await;

        let text = first_text(&call(&client, "demo").await);
        assert!(
            text.contains(
                "<skill_resources>\n<file>references/REFERENCE.md</file>\n<file>scripts/extract.py</file>\n<file>top.md</file>\n</skill_resources>"
            ),
            "{}",
            text
        );
        assert!(!text.contains("SKILL.md"), "{}", text);
        assert!(!text.contains(".git"), "{}", text);
        assert!(!text.contains(".hidden"), "{}", text);
        assert!(!text.contains(".venv"), "{}", text);
    }

    #[tokio::test]
    async fn read_skill_truncates_resource_listing_at_50() {
        let dir = tempfile::TempDir::new().unwrap();
        for i in 0..55 {
            std::fs::write(dir.path().join(format!("f{i:02}.txt")), "x").unwrap();
        }
        let client = spawn_server(SkillServer::new(vec![skill_info(
            "demo",
            "d",
            "b",
            dir.path(),
        )]))
        .await;

        let text = first_text(&call(&client, "demo").await);
        assert!(text.contains("<file>f00.txt</file>"), "{}", text);
        assert!(text.contains("<file>f49.txt</file>"), "{}", text);
        assert!(!text.contains("<file>f50.txt</file>"), "{}", text);
        assert!(
            text.contains("<file>(listing truncated, 5 more files)</file>"),
            "{}",
            text
        );
        // 50 entries + the truncation marker.
        assert_eq!(text.matches("<file>").count(), 51);
    }

    #[tokio::test]
    async fn read_skill_empty_dir_lists_no_files() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("SKILL.md"), "only the manifest").unwrap();
        let client = spawn_server(SkillServer::new(vec![skill_info(
            "demo",
            "d",
            "b",
            dir.path(),
        )]))
        .await;

        let text = first_text(&call(&client, "demo").await);
        assert!(
            text.contains("<skill_resources>\n</skill_resources>"),
            "{}",
            text
        );
        assert!(!text.contains("<file>"), "{}", text);
    }

    #[tokio::test]
    async fn read_skill_missing_skill_dir_still_returns_body() {
        // Directory deleted between discovery and call: listing is empty but
        // the call succeeds with the in-memory body.
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("SKILL.md"), "x").unwrap();
        let client = spawn_server(SkillServer::new(vec![skill_info(
            "gone",
            "Gone skill",
            "still readable body",
            dir.path(),
        )]))
        .await;
        std::fs::remove_dir_all(dir.path()).unwrap();

        let result = call(&client, "gone").await;
        assert!(!is_error(&result), "{}", first_text(&result));
        let text = first_text(&result);
        assert!(text.contains("still readable body"), "{}", text);
        assert!(
            text.contains("<skill_resources>\n</skill_resources>"),
            "{}",
            text
        );
    }

    // ── listing ──

    #[tokio::test]
    async fn list_tools_exposes_read_skill() {
        let client = spawn_server(SkillServer::new(vec![])).await;

        let tools = client.peer().list_tools(None).await.unwrap();
        let names: Vec<String> = tools.tools.iter().map(|t| t.name.to_string()).collect();
        assert_eq!(names, vec!["read_skill".to_string()]);
    }
}
