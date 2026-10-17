//! 聚合 [`ServerHandler`]：对上游 App 表现为一台 MCP server。
//!
//! 只实现 OpenSlate 消费的最小子集（initialize / tools/list / tools/call），
//! capabilities 有意只声明 `tools` —— resources / prompts / logging 不在
//! v1 计划内（主动裁剪，非协议损失；后续 OpenSlate 需要时再扩展声明）。
//!
//! 工具名两分：裸名（`bash` / `read_file` / …）路由到进程内 builtin
//! server 群；含 `__` 的 `alias__tool` 路由到下游 stdio server。

use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};

use crate::builtin::{BuiltinFailure, BuiltinHub};
use crate::downstream::{CallFailure, DownstreamHub, PREFIX_SEP};

/// 聚合处理器。`Clone` 廉价（内部 `Arc`），可安全被 stateless 的
/// Streamable HTTP 工厂每请求克隆。
#[derive(Clone)]
pub struct AggregateHandler {
    hub: Arc<DownstreamHub>,
    builtin: Arc<BuiltinHub>,
}

fn error_text(msg: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(msg.into())])
}

impl AggregateHandler {
    pub fn new(hub: Arc<DownstreamHub>, builtin: Arc<BuiltinHub>) -> Self {
        Self { hub, builtin }
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

    /// 聚合清单 = builtin 裸名工具（bash/read_file/write_file/edit_file）
    /// + 所有下游工具（`alias__tool` 前缀）。单个下游故障被跳过
    /// （其工具暂不可见），与 OpenSlate 侧"失败即跳过"一致。
    ///
    /// v1 不分页：忽略 `next_cursor`，一次返回全部（本地聚合器工具量级
    /// 远低于分页阈值 8192 tokens）。
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let mut tools = self.builtin.tools();
        tools.extend(self.hub.collect_tools().await);
        Ok(ListToolsResult {
            tools,
            next_cursor: None,
            ..Default::default()
        })
    }

    /// 路由两分：名字含 `__` → 下游 `alias__tool`（结果原样透传，含
    /// content blocks 与 `is_error`）；裸名 → builtin server 群。裸名
    /// **不再要求 server 前缀**——builtin 工具是 host 自供的，无需
    /// alias；两类失败都返回 `Ok(CallToolResult::error(...))`（协议层
    /// 不报错，错误文本给 LLM）。
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let name = request.name.as_ref();
        if let Some((alias, tool)) = name.split_once(PREFIX_SEP) {
            return match self.hub.call(alias, tool, request.arguments).await {
                Ok(result) => Ok(result.into()),
                Err(CallFailure::UnknownServer) => Ok(error_text(format!(
                    "unknown server '{alias}' (not in host manifest)"
                ))
                .into()),
                Err(CallFailure::Transport(msg)) => {
                    Ok(error_text(format!("server '{alias}' failed: {msg}")).into())
                }
            };
        }
        match self.builtin.call(name, request.arguments).await {
            Ok(result) => Ok(result.into()),
            Err(BuiltinFailure::UnknownTool) => {
                // 语义提示而非单纯"unknown tool"：同时给出已知裸名与
                // 下游命名形状，LLM 一次自纠到位（builtin 关闭时明示）。
                let names = self.builtin.tool_names().join(", ");
                let known = if names.is_empty() {
                    "none — builtin tools are disabled ([builtin] enabled = false)".to_string()
                } else {
                    names
                };
                Ok(error_text(format!(
                    "unknown builtin tool '{name}' (known builtin tools: {known}; \
                     downstream tools are named '<server>{PREFIX_SEP}<tool>')"
                ))
                .into())
            }
            Err(BuiltinFailure::Call(msg)) => Ok(error_text(format!(
                "builtin tool '{name}' failed: {msg}"
            ))
            .into()),
        }
    }
}
