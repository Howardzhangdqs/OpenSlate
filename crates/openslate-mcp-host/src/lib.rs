//! openslate-mcp-host — Termux 侧 MCP 聚合宿主（手机端 MCP 方案）。
//!
//! 架构（详见 docs 计划讨论）：
//!
//! ```text
//! OpenSlate App (rmcp client) ──HTTP+Bearer──▶ 127.0.0.1:8765/mcp
//!                                              ┌─ openslate-mcp-host ─┐
//!                                              │ 聚合 tools/list      │
//!                                              │ `alias__tool` 路由   │
//!                                              ├──────────────────────┤
//!                                              │ stdio 子进程 × N     │
//!                                              └──────────────────────┘
//! ```
//!
//! 设计要点：
//! - 上游 Streamable HTTP（stateless：`legacy_session_mode=false` +
//!   `json_response=true`，rmcp 服务端会在 handler 发出通知时自动 fallback
//!   SSE，progress 不丢）；
//! - 下游懒连接 + 失效重连（调用出错即弃置连接，下次调用重连）；
//! - 工具名 `alias__tool` 双下划线分隔，与 OpenSlate 侧 `{server}_{tool}`
//!   前缀在视觉上分层；
//! - 只绑 loopback；token 未配置时自动生成并写 `<config>.token` 旁车文件
//!   （App 侧可经 RUN_COMMAND `cat` 取回）。

pub mod downstream;
pub mod handler;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::http::{header::AUTHORIZATION, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::extract::{Request, State};
use axum::Router;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::{
    StreamableHttpServerConfig, StreamableHttpService,
};
use serde::Deserialize;

pub use downstream::{DownstreamHub, ServerSpec};
pub use handler::AggregateHandler;

/// 默认监听地址：只绑 loopback（Android 上 127.0.0.1 为全设备共享，
/// 必须配合 token 鉴权，绝不绑 0.0.0.0）。
pub const DEFAULT_BIND: &str = "127.0.0.1:8765";

/// MCP endpoint 路径（App 侧配置 `url = "http://127.0.0.1:8765/mcp"`）。
pub const MCP_PATH: &str = "/mcp";

/// 宿主清单（TOML）：
///
/// ```toml
/// bind = "127.0.0.1:8765"
/// token = "optional-managed-token"     # 缺省则自动生成写 .token 旁车文件
/// [servers.fs]
/// command = "npx"
/// args = ["-y", "@modelcontextprotocol/server-filesystem", "/sdcard"]
/// [servers.git]
/// command = "/data/data/com.termux/files/home/.npm-global/bin/mcp-server-git"
/// ```
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    /// 监听地址；缺省 [`DEFAULT_BIND`]。
    pub bind: Option<String>,
    /// 共享鉴权 token；`Some("")` 显式关闭鉴权（仅限本机调试）。
    #[serde(default)]
    pub token: Option<String>,
    /// 下游 stdio server 清单（alias → 启动规格）。
    #[serde(default)]
    pub servers: BTreeMap<String, ServerSpec>,
}

/// 解析清单文件。
pub fn load_config(path: &std::path::Path) -> Result<HostConfig> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading mcp-host config {}", path.display()))?;
    toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}

/// 生成随机 token：读 /dev/urandom 32 字节 hex（零额外依赖；Termux/桌面
/// Linux 均有 urandom）。
pub fn generate_token() -> std::io::Result<String> {
    use std::io::Read;
    let mut buf = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// 组装 axum Router：`/mcp` 挂 Streamable HTTP MCP 服务 + token 鉴权中间件。
///
/// 拆出此函数是为了集成测试能自行 bind 随机端口。
pub fn build_router(hub: Arc<DownstreamHub>, token: Option<String>) -> Router {
    let handler = AggregateHandler::new(hub);
    // README 提示：stateless 模式下 handler 工厂每请求执行一次，共享状态
    // 必须经由 Clone 句柄传递 —— AggregateHandler 内部是 Arc，clone 廉价。
    let service = StreamableHttpService::new(
        move || std::io::Result::Ok(handler.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            // stateless：无 Mcp-Session-Id、无独立 GET 流（2025-06-18+
            // 语义；Host 的下游连接常驻，无需上游 session 状态）。
            .with_legacy_session_mode(false)
            // 简单请求-响应走 application/json；handler 发出通知/请求时
            // 自动 fallback SSE（progress 不丢）。
            .with_json_response(true),
    );

    let mcp = Router::new().nest_service(MCP_PATH, service);
    match token {
        // Some("") = 显式关闭鉴权（调试）。
        Some(t) if t.is_empty() => mcp,
        Some(expected) => mcp.layer(middleware::from_fn_with_state(
            Arc::new(expected),
            auth_middleware,
        )),
        None => mcp,
    }
}

/// 解析绑定地址（清单未给时用 [`DEFAULT_BIND`]）。
pub fn resolve_bind(cfg: &HostConfig) -> Result<SocketAddr> {
    cfg.bind
        .as_deref()
        .unwrap_or(DEFAULT_BIND)
        .parse()
        .with_context(|| "invalid `bind` address in mcp-host config")
}

/// Token 鉴权：接受 `Authorization: Bearer <token>` 或 `X-API-Key: <token>`。
async fn auth_middleware(
    State(expected): State<Arc<String>>,
    req: Request,
    next: Next,
) -> Response {
    let bearer = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_owned);
    let provided = bearer.or_else(|| {
        req.headers()
            .get("x-api-key")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    });
    // 常数时间比较省略：token 是高熵随机值，非密码场景 timing 攻击无意义。
    match provided {
        Some(t) if t == *expected => next.run(req).await,
        _ => (
            StatusCode::UNAUTHORIZED,
            [(AUTHORIZATION, HeaderValue::from_static("Bearer"))],
        )
            .into_response(),
    }
}
