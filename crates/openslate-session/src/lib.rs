//! openslate-session：传输无关的共享单会话核心。
//!
//! 从 openslate-server（web-1）抽取的会话状态机：`SessionCore`（引擎 +
//! 转写镜像 + 配置镜像）、引擎回合任务（engine）、审批桥（approval）、
//! 配置 CRUD 写回链（config_ops）与 `ClientMsg` 派发（session）。所有
//! 出站事件经 [`state::MsgSink`] 送出——WS server 用 ConnectionHub 扇出
//! 到多条连接，mobile 用 EventSink 序列化为 JSON 回调宿主，二者共享同
//! 一份事件序与转写镜像语义（同一连接内事件全序）。
//!
//! 本 crate 不依赖任何传输（axum/WS/FFI 均不在依赖树中）。
//!
//! ## 锁与顺序（不变量，与原 server 相同，改动前必读）
//!
//! - `SessionCore::inner` 是 **std Mutex**：引擎的 ProgressCallback 是
//!   同步 trait，无法 await tokio 锁；所有回调持锁「先改镜像、再广播」，
//!   保证同一连接上的事件序 == 转写追加序。
//! - 锁方向唯一：`core → sink` 与 `core → approval bridge`（持 core 锁
//!   期间允许 broadcast / 读审批队首，二者内部都是非阻塞短临界区）；
//!   **严禁** `bridge → core`（审批应答路径必须先 `respond()` 释放
//!   bridge 锁、再取 core 锁推转写，反向会与 snapshot 构成环）。
//! - `RunManager` 沿用所有权回传模式：submit 时 take，回合结束由引擎
//!   任务放回（manager 不是 Sync 共享，靠 `Option` 槽位表达「在家/
//!   引擎持有」两态）。

pub mod approval;
pub mod config_ops;
pub mod engine;
pub mod session;
pub mod state;

/// 运行时单行事件 target（原 server 的 `serve_stdout` 语义承接者）：
/// `openslate serve` 的 stdout 事件层与 mobile 侧日志都会放行它。
pub const EVENT_TARGET: &str = "openslate_session";

/// 单行运行时事件（英文文案、grep 友好）。**token / api key 值绝不进这里。**
#[macro_export]
macro_rules! session_event {
    ($($arg:tt)*) => {
        tracing::info!(target: crate::EVENT_TARGET, $($arg)*)
    };
}
