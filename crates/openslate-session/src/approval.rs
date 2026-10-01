//! 会话审批桥（原 server 版审批桥，随 openslate-session 抽取对全体
//! 前端可用）。
//!
//! 与 TUI `event.rs` 的 `ApprovalBridge` 同机制不同语义：
//!
//! - **decide() 阻塞在 std mpsc recv**（与 TUI 相同）：`decide` 是同步
//!   trait，被引擎内联调用；阻塞发生在多线程 tokio runtime 的 worker
//!   线程上，一个线程阻塞不影响其他任务执行——这是 TUI 模块文档里
//!   论证过的安全模式，session 沿用。
//! - **任一客户端应答即 resolve**：`respond(id)` 由收到
//!   `approval_answer` 的任务调用；首答者赢得 `mpsc::Sender`，
//!   迟到者拿到 `false` → 定向 notice「已由其他客户端应答」。
//! - **session allowlist**：`approve_all` 把工具名写入本会话白名单，
//!   后续同名调用直接放行（REPL/TUI 同款语义）。
//! - **无人可问即拒绝**：请求到达时一个客户端都没有 → 直接 deny
//!   （TUI 对应「事件通道关闭 → 拒绝」的分支）。
//! - **关停 deny_all**：优雅停机时先放行队列里所有阻塞请求为 deny，
//!   引擎才能解栈。

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use openslate_core::approval::{ApprovalCallback, ApprovalDecision, ApprovalRequest};
use openslate_protocol::{ApprovalAnswerChoice, ApprovalSummaryDto, ServerMsg};

use crate::state::MsgSink;

/// 一条阻塞中的审批：id、横幅摘要、应答通道。
struct Pending {
    id: u64,
    summary: ApprovalSummaryDto,
    responder: std::sync::mpsc::Sender<ApprovalAnswerChoice>,
}

#[derive(Default)]
struct BridgeInner {
    pending: Vec<Pending>,
    allowlist: HashSet<String>,
}

pub struct SessionApprovalBridge {
    sink: Arc<dyn MsgSink>,
    next_id: AtomicU64,
    inner: Mutex<BridgeInner>,
}

impl SessionApprovalBridge {
    pub fn new(sink: Arc<dyn MsgSink>) -> Self {
        Self {
            sink,
            next_id: AtomicU64::new(1),
            inner: Mutex::new(BridgeInner::default()),
        }
    }

    /// 客户端应答入口。`Some(summary)` = 首答生效（返回横幅摘要供转写
    /// 条目用）；`None` = 无此待审（已被别人答掉/不存在）。
    pub fn respond(&self, id: u64, choice: ApprovalAnswerChoice) -> Option<ApprovalSummaryDto> {
        let mut inner = Self::lock(&self.inner);
        if let Some(pos) = inner.pending.iter().position(|p| p.id == id) {
            let pending = inner.pending.remove(pos);
            // 发送失败 = decide() 已放弃（recv 端 drop），应答自然丢失。
            pending.responder.send(choice).ok().map(|_| pending.summary)
        } else {
            None
        }
    }

    /// 优雅停机：deny 所有阻塞请求，引擎得以解栈。
    pub fn deny_all(&self) {
        let mut inner = Self::lock(&self.inner);
        for pending in inner.pending.drain(..) {
            let _ = pending.responder.send(ApprovalAnswerChoice::Deny);
        }
    }

    /// 队首待审（snapshot 的 pending_approval）。
    pub fn front_pending(&self) -> Option<(u64, ApprovalSummaryDto)> {
        let inner = Self::lock(&self.inner);
        inner.pending.first().map(|p| (p.id, p.summary.clone()))
    }

    fn choice_to_decision(
        inner: &mut BridgeInner,
        req: &ApprovalRequest,
        choice: ApprovalAnswerChoice,
    ) -> ApprovalDecision {
        match choice {
            ApprovalAnswerChoice::Approve => ApprovalDecision::Approved,
            ApprovalAnswerChoice::ApproveAll => {
                inner.allowlist.insert(req.tool_name.clone());
                ApprovalDecision::Approved
            }
            ApprovalAnswerChoice::Deny => {
                ApprovalDecision::Denied("用户在审批横幅上选择了拒绝".to_owned())
            }
        }
    }

    fn lock(inner: &Mutex<BridgeInner>) -> std::sync::MutexGuard<'_, BridgeInner> {
        inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl ApprovalCallback for SessionApprovalBridge {
    fn decide(&self, req: &ApprovalRequest) -> ApprovalDecision {
        // 会话白名单快路径（approve_all 写入）。
        if Self::lock(&self.inner).allowlist.contains(&req.tool_name) {
            tracing::debug!(
                target: "openslate_approval",
                "session allowlist approved tool '{}'",
                req.tool_name
            );
            return ApprovalDecision::Approved;
        }

        // 无人可问：拒绝而非永久阻塞（对齐 TUI「事件通道关闭→拒绝」）。
        if self.sink.count() == 0 {
            tracing::warn!(
                target: "openslate_approval",
                "approval needed for '{}' but no client connected — denied",
                req.tool_name
            );
            return ApprovalDecision::Denied("无客户端连接,默认拒绝".to_owned());
        }

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = std::sync::mpsc::channel();
        let summary = ApprovalSummaryDto::from(req);
        {
            let mut inner = Self::lock(&self.inner);
            inner.pending.push(Pending {
                id,
                summary: summary.clone(),
                responder: tx,
            });
        }

        // 广播横幅（不进转写镜像——Approval 条目在应答时产生）。
        self.sink.broadcast(ServerMsg::ApprovalRequested {
            id,
            summary: summary.clone(),
        });
        crate::session_event!(
            "approval: {} ({}) requested",
            summary.tool_name,
            summary.risk_level
        );

        let decision = match rx.recv() {
            Ok(choice) => {
                let mut inner = Self::lock(&self.inner);
                Self::choice_to_decision(&mut inner, req, choice)
            }
            Err(_) => ApprovalDecision::Denied("审批通道已关闭(关停),默认拒绝".to_owned()),
        };

        // decide() 放弃路径（respond 已移除）的兜底清理。
        let mut inner = Self::lock(&self.inner);
        inner.pending.retain(|p| p.id != id);
        decision
    }
}
