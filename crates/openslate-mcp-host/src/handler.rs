//! 聚合 [`ServerHandler`]：对上游 App 表现为一台 MCP server。
//!
//! 只实现 OpenSlate 消费的最小子集（initialize / tools/list / tools/call），
//! capabilities 有意只声明 `tools` —— resources / prompts / logging 不在
//! v1 计划内（主动裁剪，非协议损失；后续 OpenSlate 需要时再扩展声明）。

use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};

use crate::downstream::{CallFailure, DownstreamHub, PREFIX_SEP};

/// 聚合处理器。`Clone` 廉价（内部 `Arc`），可安全被 stateless 的
/// Streamable HTTP 工厂每请求克隆。
#[derive(Clone)]
pub struct AggregateHandler {
    hub: Arc<DownstreamHub>,
}

fn error_text(msg: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(msg.into())])
}

impl AggregateHandler {
    pub fn new(hub: Arc<DownstreamHub>) -> Self {
        Self { hub }
    }
}

impl ServerHandler for AggregateHandler {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "openslate-mcp-host",
                env!("CARGO_PKG_VERSION"),
            ))
    }

    /// 聚合所有下游工具清单（`alias__tool` 前缀）。单个下游故障被跳过
    /// （其工具暂不可见），与 OpenSlate 侧"失败即跳过"一致。
    ///
    /// v1 不分页：忽略 `next_cursor`，一次返回全部（本地聚合器工具量级
    /// 远低于分页阈值 8192 tokens）。
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let tools = self.hub.collect_tools().await;
        Ok(ListToolsResult {
            tools,
            next_cursor: None,
            ..Default::default()
        })
    }

    /// 按前缀路由：`alias__tool` → 下游 `tool`。结果原样透传（含
    /// content blocks 与 `is_error` —— 工具自身的失败语义不被吞掉）。
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let name = request.name.as_ref();
        let Some((alias, tool)) = name.split_once(PREFIX_SEP) else {
            return Ok(error_text(format!(
                "tool name '{name}' is missing the server prefix (expected '<server>{PREFIX_SEP}<tool>')"
            ))
            .into());
        };
        match self.hub.call(alias, tool, request.arguments).await {
            Ok(result) => Ok(result.into()),
            Err(CallFailure::UnknownServer) => Ok(error_text(format!(
                "unknown server '{alias}' (not in host manifest)"
            ))
            .into()),
            Err(CallFailure::Transport(msg)) => {
                Ok(error_text(format!("server '{alias}' failed: {msg}")).into())
            }
        }
    }
}
