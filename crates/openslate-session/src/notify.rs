//! notify 通知应答的会话桥接（模式照抄 [`crate::question`] 的
//! `QuestionHandler` 接法，notify 设计 S1b）。
//!
//! 与问卷桥同款关键差异：通知的等待（host call）、持久化与迟到应答
//! **续跑**由宿主（mobile）侧完整实现——session 侧不持有通知状态
//!（pending 清单在宿主 `pending_notifications` 持久层；通知本体由 OS
//! 通知栏持有，快照不回放），这里只定义转发接缝 trait 与默认 no-op
//! 装配。
//!
//! 注入点在 [`crate::state::SessionCore::set_notify_handler`] 而非
//! `AppState` 字段：与 `set_question_handler` 同款防破坏论证（加私有
//! 字段零破坏）。宿主实现持 `Weak<AppState>` 即可避免引用环。

use openslate_protocol::NotificationOutcome;

/// notify 通知应答处理器（宿主注入；未注入 = 通知应答通道关闭）。
pub trait NotifyHandler: Send + Sync {
    /// 一条通知应答（即时或迟到）。返回 `true` = `request_id` 有效且被
    /// 本次应答消费（幂等首答者赢——语义同 [`crate::question::QuestionHandler::on_reply`]）；
    /// `false` = 未知 / 已关闭 / outcome 非法（调用方对发起连接定向
    /// notice）。
    ///
    /// 宿主实现职责（mobile lane）：在途 host call 先 `resolve` 唤醒
    /// 工具（即时路径）；否则原子关卷 `pending_notifications` 行（首答
    /// 者赢）→ 以与即时路径完全一致的结果内容补全原 tool call → 经
    /// `RunManager::resume_from_tool_result` 续跑 agent 循环。
    /// `outcome` 的上行白名单在协议枚举处机制化（timeout / expired
    /// 仅 Rust 侧合成，wire 不可表达），本 trait 收到的恒为合法值。
    #[allow(clippy::too_many_arguments)]
    fn on_reply(
        &self,
        request_id: &str,
        run_id: &str,
        outcome: NotificationOutcome,
        label: Option<String>,
        value: Option<String>,
        text: Option<String>,
    ) -> bool;

    /// 用户显式取消回合：悬挂的 wait 通知行按 superseded 关卷（引擎
    /// select! 会为整批悬挂 tool call 合成 cancelled 结果，历史侧自洽；
    /// 这里只收持久层行——语义对齐 `QuestionHandler::on_cancel_all`）。
    /// 默认 no-op。
    fn on_cancel_all(&self) {}
}
