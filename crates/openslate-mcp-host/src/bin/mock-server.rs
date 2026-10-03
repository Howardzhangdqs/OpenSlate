//! 测试用 stdio MCP server（集成测试的下游挡板）。
//!
//! 暴露两个工具：`echo`（回显文本）与 `fail`（返回 is_error 结果），
//! 用于验证 Host 的工具清单聚合、前缀路由、结果/错误透传。

use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::tool::ToolRouter,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock},
    schemars, tool, tool_handler, tool_router, ServiceExt,
};
use serde::Deserialize;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EchoParams {
    /// 要回显的文本
    pub text: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FailParams {
    /// 失败信息
    pub message: String,
}

#[derive(Clone)]
struct MockServer {
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl MockServer {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "Echo the given text back, wrapped as [echo] <text>")]
    async fn echo(
        &self,
        Parameters(args): Parameters<EchoParams>,
    ) -> Result<CallToolResult, McpError> {
        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "[echo] {}",
            args.text
        ))]))
    }

    #[tool(description = "Always returns an is_error tool result carrying the message")]
    async fn fail(
        &self,
        Parameters(args): Parameters<FailParams>,
    ) -> Result<CallToolResult, McpError> {
        Ok(CallToolResult::error(vec![ContentBlock::text(format!(
            "[fail] {}",
            args.message
        ))]))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for MockServer {}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // (Stdin, Stdout) 元组实现 IntoTransport —— 无需额外包装。
    let service = MockServer::new()
        .serve(rmcp::transport::io::stdio())
        .await?;
    service.waiting().await?;
    Ok(())
}
