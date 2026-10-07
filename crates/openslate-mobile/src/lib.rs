//! openslate-mobile：Android 宿主的 Agent Runtime 组合层（PLAN 第一阶段）。
//!
//! 组合 [`openslate_session`] 的传输无关会话核心与 [`openslate_app`] 的
//! 装配链，对外暴露 FFI 友好的 [`runtime::MobileRuntime`]：
//!
//! ```text
//! Kotlin (UniFFI callback)◄──EventSink(单队列+泵线程)──┐
//!    │ ClientMsg JSON                                   │ ServerMsg JSON
//!    ▼                                                  ▼
//! MobileRuntime::send ──► session::dispatch ──► 引擎(RunManager) ──► LLM
//! ```
//!
//! 设计约束（PLAN §4/§6）：
//! - 协议复用：上行 `ClientMsg`、下行 `ServerMsg`，与 CLI/TUI/WS 同一
//!   协议语义；FFI 不发明第二套 onToken/onTool 接口。
//! - request/resolve：host tool 经 [`hostcall::HostCallRouter`] 发
//!   `host_call_requested` 信封给宿主，宿主 `resolve_host_call(id, …)`
//!   回填；Rust 侧 oneshot + timeout，永不让 FFI 阻塞主线程。
//! - secret：`set_api_key` 内存注入（Android Keystore 持久化由宿主负
//!   责），不落 .env / config（PLAN §18）。
//!
//! 本 crate 无任何 Android 依赖，可在 desktop 上完整测试。

pub mod alog;
pub mod bootstrap;
pub mod events;
pub mod exec;
pub mod hostcall;
pub mod provider;
pub mod registry;
pub mod runtime;

pub use bootstrap::MobilePaths;
pub use events::{EventCallback, EventSink, MOBILE_CONN_ID};
pub use exec::{ExecSelection, ExecSelectionCell, NativeShellTool};
pub use hostcall::{HostCallRouter, HostTool};
pub use runtime::{MobileRuntime, RuntimeOptions};

/// 注入 MCP host 鉴权 token（进程级 HTTP `Authorization: Bearer` 覆盖）。
///
/// 必须在 `MobileRuntime::create*` 之前调用——MCP 连接在装配期建立，
/// 之后注入不影响已建连接。见 `openslate_core::mcp::set_mcp_auth_override`。
pub fn set_mcp_host_token(token: &str) {
    openslate_core::mcp::set_mcp_auth_override(Some(token.to_owned()));
}
