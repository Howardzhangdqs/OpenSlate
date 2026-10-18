//! ask_user 问卷应答的会话桥接（模式照抄 [`crate::approval`] 的
//! `SessionApprovalBridge` 接法，ask-user 设计 r4/r5）。
//!
//! 与审批桥的关键差异：提问的等待（host call）、持久化与迟到答案
//! **续跑**由宿主（mobile）侧完整实现——session 侧不持有问卷状态
//! （pending 清单在宿主持久层，快照 `pending_questions` 也由宿主装配
//! 层回填），这里只定义转发接缝 trait 与默认 no-op 装配。
//!
//! 注入点在 [`crate::state::SessionCore::set_question_handler`] 而非
//! `AppState` 字段：`AppState` / `CoreInner` 由 server 与 mobile 侧
//! 结构体字面量装配，加必填字段会破坏现有装配；`SessionCore` 仅经
//! `new` 构造，加私有字段零破坏。宿主实现持 `Weak<AppState>` 即可
//! 避免 AppState → SessionCore → handler → AppState 引用环。

use openslate_protocol::QuestionReplyAction;

/// ask_user 应答处理器（宿主注入；未注入 = 提问通道关闭）。
pub trait QuestionHandler: Send + Sync {
    /// 一份问卷应答（即时或迟到，含 decline）。返回 `true` =
    /// `request_id` 有效且被本次应答消费（幂等首答者赢——语义同审批桥
    /// `respond`）；`false` = 未知/已关闭（调用方对发起连接定向 notice）。
    ///
    /// 宿主实现职责（mobile lane）：标记问卷状态 → 以答案补全原
    /// tool call 的 tool result（`openslate_core::ask_user`）→ 经
    /// `RunManager::resume_from_tool_result` 续跑 agent 循环。
    fn on_reply(
        &self,
        request_id: &str,
        action: QuestionReplyAction,
        answers: serde_json::Value,
    ) -> bool;

    /// 用户显式取消回合：所有悬挂问题按 cancelled 关卷（标记状态、
    /// best-effort 解析 host call——引擎 select! 对已取消 token 有
    /// 偏置，真实结果不保证落地；兜底是下一 turn 前的 superseded
    /// 修复，见 `session::handle_submit`）。默认 no-op。
    fn on_cancel_all(&self) {}
}
