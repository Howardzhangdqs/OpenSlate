//! Termux 专用 bash 执行 server：`bash` 工具。
//!
//! 为什么单独一个 server 而不是「让 ShellServer 换用 bash」：
//! [`crate::shell::ShellServer`] 走平台 shell（Unix `sh -c`，Termux 上是
//! toybox），而 Termux 的完整 bash 在 `$PREFIX/bin/bash`——路径解析是
//! 宿主环境知识，mcp-builtin 是零环境探测的通用叶子 crate（不读
//! `$PREFIX`，不依赖 openslate-core），所以 bash 可执行路径由调用方
//! （openslate-mcp-host）显式传入。
//!
//! 执行语义（默认 30s / 上限 120s 超时、64KB 头尾字符边界安全截断、
//! 敏感环境变量脱敏、exit_code + stdout/stderr 分节输出）全部复用
//! [`crate::shell::run_shell_command`] 共享核心——不复制实现，防漂移。

use std::path::{Path, PathBuf};

use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::tool::ToolRouter,
    handler::server::wrapper::Parameters,
    model::CallToolResult,
    schemars, tool, tool_handler, tool_router,
};

use crate::shell::run_shell_command;

/// bash 的命令标志（`bash -c <command>`），跨平台一致。
const BASH_FLAG: &str = "-c";

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct BashParams {
    #[schemars(description = "Bash command to run")]
    pub command: String,
    #[schemars(description = "Optional timeout in milliseconds (default 30000, max 120000)")]
    pub timeout_ms: Option<u64>,
}

/// In-process MCP server exposing the `bash` tool: runs commands with an
/// explicit bash executable in the workspace root. Consumed by
/// openslate-mcp-host (Termux), which resolves the bash path from `$PREFIX`.
#[derive(Clone)]
pub struct BashServer {
    root: PathBuf,
    bash_path: PathBuf,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl BashServer {
    /// Create a server that runs commands via `bash_path` with `root` as
    /// working directory. The root is canonicalized once; on failure the
    /// raw path is kept and the error surfaces at spawn time instead.
    pub fn new(root: impl AsRef<Path>, bash_path: impl Into<PathBuf>) -> Self {
        let root = root
            .as_ref()
            .canonicalize()
            .unwrap_or_else(|_| root.as_ref().to_path_buf());
        Self {
            root,
            bash_path: bash_path.into(),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Run a bash command in the workspace root and return exit code, stdout and stderr"
    )]
    async fn bash(
        &self,
        Parameters(args): Parameters<BashParams>,
    ) -> Result<CallToolResult, McpError> {
        run_shell_command(
            &self.bash_path,
            BASH_FLAG,
            &self.root,
            &args.command,
            args.timeout_ms,
        )
        .await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for BashServer {}

// ── Tests ───────────────────────────────────────────────────────────────────
//
// 与 shell.rs 的命令级测试同模式：经 tuple transport 直连 server，覆盖
// 共享执行语义在 bash 解释器下同样成立。PATH 上的 `bash` 在 Unix 开发
// 环境普遍存在，测试与 shell.rs 一样用 `#[cfg(unix)]` 门控。

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{first_text, is_error, spawn_server};
    use rmcp::model::CallToolRequestParams;

    async fn client_in(
        dir: &tempfile::TempDir,
    ) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
        spawn_server(BashServer::new(dir.path(), "bash")).await
    }

    async fn bash(
        client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
        command: &str,
        timeout_ms: Option<u64>,
    ) -> rmcp::model::CallToolResult {
        let mut args = rmcp::object!({"command": command});
        if let Some(ms) = timeout_ms {
            args.insert("timeout_ms".into(), ms.into());
        }
        client
            .call_tool(CallToolRequestParams::new("bash").with_arguments(args))
            .await
            .expect("call_tool ok")
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn echo_succeeds() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        let result = bash(&client, "echo hello-bash", None).await;
        assert!(!is_error(&result), "{}", first_text(&result));
        let text = first_text(&result);
        assert!(text.contains("exit_code: 0"), "{text}");
        assert!(text.contains("hello-bash"), "{text}");
        assert!(text.contains("--- stdout ---"), "{text}");
        assert!(text.contains("--- stderr ---"), "{text}");
    }

    // bash 语义回归：`[[ ]]` 是 bashism，系统 sh 不支持——证明走的
    // 确实是 bash 而非平台 shell。
    #[cfg(unix)]
    #[tokio::test]
    async fn runs_bash_not_platform_sh() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        let result = bash(&client, "[[ bash-ism-works ]] && echo yes", None).await;
        assert!(!is_error(&result), "{}", first_text(&result));
        assert!(first_text(&result).contains("yes"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn nonzero_exit_is_error_but_output_visible() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        let result = bash(&client, "echo boom 1>&2; exit 3", None).await;
        assert!(is_error(&result));
        let text = first_text(&result);
        assert!(text.contains("exit_code: 3"), "{text}");
        assert!(text.contains("boom"), "{text}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_command() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        let started = std::time::Instant::now();
        let result = bash(&client, "sleep 30", Some(500)).await;
        assert!(is_error(&result));
        assert!(first_text(&result).contains("TIMEOUT"));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn oversized_output_is_truncated() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        // ~109KB：头尾保留 + 截断标记（共享截断逻辑的 bash 侧回归）。
        let result = bash(&client, "seq 1 20000", Some(5000)).await;
        let text = first_text(&result);
        assert!(!is_error(&result), "{text}");
        assert!(text.contains("exit_code: 0"), "{text}");
        assert!(text.contains("[truncated"), "{text}");
        assert!(text.contains("\n1\n2\n3\n"), "{text}");
        assert!(text.contains("20000"), "{text}");
    }

    #[tokio::test]
    async fn empty_command_is_invalid_params() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        let err = client
            .call_tool(
                CallToolRequestParams::new("bash")
                    .with_arguments(rmcp::object!({"command": "  "})),
            )
            .await
            .expect_err("empty command must be invalid params");
        assert!(format!("{err}").contains("command"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runs_in_workspace_root() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        let result = bash(&client, "pwd", None).await;
        let text = first_text(&result);
        let root = dir.path().canonicalize().unwrap();
        assert!(text.contains(root.to_str().unwrap()), "{text}");
    }
}
