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
//! - **会话作用域双集合（tool-approval P1b）**：`approve_all`（(a)
//!   本会话允许该工具）写 `allowed_tools`；`session_mcp`（(c) 本会话
//!   允许该 MCP）写 `allowed_servers`（键 = 审批作用域 slug，与规则
//!   引擎 `servers` 域同口径）；`new_session` 时两者清空（「当前会话」
//!   语义）。
//! - **rules 共享 cell**：[`SessionApprovalBridge::rules_handle`] 是给
//!   装配层注入 `ApprovalManager::with_rules` 的正道（P0-5：重建
//!   manager 拿游离 cell 会让 always_* / set_approval_config 热更永远
//!   到不了引擎且无编译错误）。装配层随后 `set_rules` 写初始值（mobile
//!   的 Auto opt-in 在装配层做，core 的 rules_from_config 段缺省 =
//!   纯 Allow 基线）。
//! - **无人可问即拒绝**：请求到达时一个客户端都没有 → 直接 deny
//!   （TUI 对应「事件通道关闭 → 拒绝」的分支）。
//! - **关停 deny_all**：优雅停机时先放行队列里所有阻塞请求为 deny，
//!   引擎才能解栈。

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use openslate_core::approval::{
    ApprovalCallback, ApprovalDecision, ApprovalRequest, ApprovalRules,
};
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
    /// (a) 本会话允许的工具名（approve_all 写入）。
    allowed_tools: HashSet<String>,
    /// (c) 本会话允许的 MCP server 作用域 slug（session_mcp 写入；
    /// 键与规则引擎 `ApprovalRules.servers` 同口径）。
    allowed_servers: HashSet<String>,
}

pub struct SessionApprovalBridge {
    sink: Arc<dyn MsgSink>,
    next_id: AtomicU64,
    inner: Mutex<BridgeInner>,
    /// 规则引擎共享 cell（毒化容忍，与全库锁口径一致）。初始 =
    /// `ApprovalRules::default()`（= Allow 从不问，P0-4）；装配层随后
    /// `set_rules` 写入 `rules_from_config` 结果（mobile 另做 Auto
    /// opt-in）。`rules_handle()` 交 `ApprovalManager::with_rules` 注入。
    rules: Arc<RwLock<ApprovalRules>>,
}

impl SessionApprovalBridge {
    pub fn new(sink: Arc<dyn MsgSink>) -> Self {
        Self {
            sink,
            next_id: AtomicU64::new(1),
            inner: Mutex::new(BridgeInner::default()),
            rules: Arc::new(RwLock::new(ApprovalRules::default())),
        }
    }

    /// 共享规则 cell 克隆（`ApprovalManager::with_rules` 注入点，P0-5
    /// 正道；本桥的 `set_rules`/`rules_snapshot` 与它同一 cell）。
    pub fn rules_handle(&self) -> Arc<RwLock<ApprovalRules>> {
        Arc::clone(&self.rules)
    }

    /// 热更新规则（与 `rules_handle()` 外部改写等价的便捷入口）。
    pub fn set_rules(&self, rules: ApprovalRules) {
        *self
            .rules
            .write()
            .unwrap_or_else(|e| e.into_inner()) = rules;
    }

    /// 当前规则快照（clone）。
    pub fn rules_snapshot(&self) -> ApprovalRules {
        self.rules
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 会话作用域放行判定（P0-3）：(a)/(c) 批准过的工具 / server 后续
    /// 调用直接放行。AI 审查装饰器**第一步**先查它——否则每次仍要付
    /// 一次 LLM 审查费。短临界区（inner 自锁，叶子锁）。
    pub fn session_allows(&self, req: &ApprovalRequest) -> bool {
        let inner = Self::lock(&self.inner);
        inner.allowed_tools.contains(&req.tool_name)
            || req
                .server
                .as_deref()
                .is_some_and(|server| inner.allowed_servers.contains(server))
    }

    /// 清空会话作用域集合（`new_session` 调用；「当前会话」语义。
    /// P1a 摸底确认旧实现不清 allowlist 是漏洞，本方法补上）。bridge
    /// 自锁，调用方须在 core 锁外调用（锁序红线：先 bridge 后 core）。
    pub fn clear_session_scopes(&self) {
        let mut inner = Self::lock(&self.inner);
        inner.allowed_tools.clear();
        inner.allowed_servers.clear();
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
                // (a) 当前会话允许该工具：写会话级工具集合。
                inner.allowed_tools.insert(req.tool_name.clone());
                ApprovalDecision::Approved
            }
            ApprovalAnswerChoice::Deny => {
                ApprovalDecision::Denied("用户在审批横幅上选择了拒绝".to_owned())
            }
            ApprovalAnswerChoice::SessionMcp => {
                // (c) 当前会话允许该 MCP：写会话级 server 作用域集合。
                // server 缺失（非 MCP 工具发来了 session_mcp）退化为
                // 工具级放行——防御分支，UI 只对 server≠null 的请求出
                // 该选项，正常不可达。
                match req.server.clone() {
                    Some(server) => {
                        inner.allowed_servers.insert(server);
                    }
                    None => {
                        tracing::warn!(
                            target: "openslate_approval",
                            "session_mcp answer for non-MCP tool '{}' — falling back to \
                             per-tool session allow",
                            req.tool_name
                        );
                        inner.allowed_tools.insert(req.tool_name.clone());
                    }
                }
                ApprovalDecision::Approved
            }
            // (b)/(d) 一直允许：本次放行但不写会话集合——持久化与规则
            // 热更由 dispatch 层负责（config_ops::apply_approval_rules
            // 共享更新链），bridge 只管会话作用域内存态。
            ApprovalAnswerChoice::AlwaysTool | ApprovalAnswerChoice::AlwaysMcp => {
                ApprovalDecision::Approved
            }
        }
    }

    fn lock(inner: &Mutex<BridgeInner>) -> std::sync::MutexGuard<'_, BridgeInner> {
        inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl ApprovalCallback for SessionApprovalBridge {
    fn decide(&self, req: &ApprovalRequest) -> ApprovalDecision {
        // 会话作用域快路径（approve_all / session_mcp 写入；P0-3）。
        if self.session_allows(req) {
            tracing::debug!(
                target: "openslate_approval",
                "session scope approved tool '{}' (server {:?})",
                req.tool_name,
                req.server
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

#[cfg(test)]
mod tests {
    //! 桥的会话作用域 / rules cell 语义单元测试（六路径引擎侧已在 core
    //! approval.rs 覆盖；此处只测 session 层新增行为）。

    use super::*;
    use openslate_core::approval::{ApprovalTrigger, RiskLevel};
    use std::sync::mpsc as std_mpsc;

    /// 记录型 sink（不真正连客户端；count 可注入）。
    struct RecordingSink {
        count: std::sync::atomic::AtomicUsize,
        broadcasted: Mutex<Vec<ServerMsg>>,
    }

    impl MsgSink for RecordingSink {
        fn broadcast(&self, msg: ServerMsg) {
            self.broadcasted
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(msg);
        }
        fn send_to(&self, _conn: u64, _msg: ServerMsg) -> bool {
            true
        }
        fn conn_ids(&self) -> Vec<u64> {
            vec![1]
        }
        fn count(&self) -> usize {
            self.count.load(std::sync::atomic::Ordering::SeqCst)
        }
        fn close_all(&self) {}
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    fn sink_with_clients(n: usize) -> Arc<RecordingSink> {
        Arc::new(RecordingSink {
            count: std::sync::atomic::AtomicUsize::new(n),
            broadcasted: Mutex::new(Vec::new()),
        })
    }

    fn req_for(tool: &str, server: Option<&str>) -> ApprovalRequest {
        ApprovalRequest {
            tool_name: tool.to_owned(),
            arguments: serde_json::json!({}),
            agent_id: "root".to_owned(),
            risk_level: RiskLevel::Medium,
            server: server.map(str::to_owned),
            trigger: ApprovalTrigger::AutoRisk,
            note: None,
        }
    }

    /// decide 阻塞在等应答——测试里起线程 decide、主线程 respond。
    fn decide_async(
        bridge: Arc<SessionApprovalBridge>,
        req: ApprovalRequest,
    ) -> std_mpsc::Receiver<ApprovalDecision> {
        let (tx, rx) = std_mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(ApprovalCallback::decide(&*bridge, &req));
        });
        rx
    }

    /// 等待 approval_requested 广播并返回其 id。
    fn wait_request_id(sink: &RecordingSink) -> u64 {
        for _ in 0..200 {
            {
                let guard = sink.broadcasted.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(ServerMsg::ApprovalRequested { id, .. }) = guard.last() {
                    return *id;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("approval_requested 未广播");
    }

    #[test]
    fn approve_all_scopes_tool_session_wide() {
        let sink = sink_with_clients(1);
        let bridge = Arc::new(SessionApprovalBridge::new(sink.clone()));
        let rx = decide_async(bridge.clone(), req_for("bash", None));
        let id = wait_request_id(&sink);
        assert!(bridge.respond(id, ApprovalAnswerChoice::ApproveAll).is_some());
        assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(5)), Ok(ApprovalDecision::Approved));
        // 后续同名调用直接放行（不弹横幅）。
        assert!(bridge.session_allows(&req_for("bash", None)));
        assert!(!bridge.session_allows(&req_for("write_file", None)));
    }

    #[test]
    fn session_mcp_scopes_server_session_wide() {
        let sink = sink_with_clients(1);
        let bridge = Arc::new(SessionApprovalBridge::new(sink.clone()));
        let rx = decide_async(
            bridge.clone(),
            req_for("termux_github__create_issue", Some("github")),
        );
        let id = wait_request_id(&sink);
        assert!(bridge.respond(id, ApprovalAnswerChoice::SessionMcp).is_some());
        assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(5)), Ok(ApprovalDecision::Approved));
        // 同 server 的其它工具直通；别的 server / 裸工具不放行。
        assert!(bridge.session_allows(&req_for("termux_github__delete_repo", Some("github"))));
        assert!(!bridge.session_allows(&req_for("termux_gitlab__delete_repo", Some("gitlab"))));
        assert!(!bridge.session_allows(&req_for("bash", None)));
    }

    #[test]
    fn session_mcp_without_server_falls_back_to_tool_scope() {
        // 防御分支：非 MCP 请求收到 session_mcp → 退化为工具级会话放行。
        let sink = sink_with_clients(1);
        let bridge = Arc::new(SessionApprovalBridge::new(sink.clone()));
        let rx = decide_async(bridge.clone(), req_for("bash", None));
        let id = wait_request_id(&sink);
        assert!(bridge.respond(id, ApprovalAnswerChoice::SessionMcp).is_some());
        assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(5)), Ok(ApprovalDecision::Approved));
        assert!(bridge.session_allows(&req_for("bash", None)));
    }

    #[test]
    fn always_choices_do_not_write_session_scopes() {
        // (b)/(d) 只放行本次：不写会话集合（持久化在 dispatch 层）。
        for choice in [ApprovalAnswerChoice::AlwaysTool, ApprovalAnswerChoice::AlwaysMcp] {
            let sink = sink_with_clients(1);
            let bridge = Arc::new(SessionApprovalBridge::new(sink.clone()));
            let rx = decide_async(
                bridge.clone(),
                req_for("termux_github__create_issue", Some("github")),
            );
            let id = wait_request_id(&sink);
            assert!(bridge.respond(id, choice).is_some());
            assert_eq!(
                rx.recv_timeout(std::time::Duration::from_secs(5)),
                Ok(ApprovalDecision::Approved)
            );
            assert!(!bridge.session_allows(&req_for("termux_github__create_issue", Some("github"))));
        }
    }

    #[test]
    fn new_session_clears_scopes() {
        let sink = sink_with_clients(1);
        let bridge = Arc::new(SessionApprovalBridge::new(sink.clone()));
        let rx = decide_async(bridge.clone(), req_for("bash", None));
        let id = wait_request_id(&sink);
        assert!(bridge.respond(id, ApprovalAnswerChoice::ApproveAll).is_some());
        let _ = rx.recv_timeout(std::time::Duration::from_secs(5));
        assert!(bridge.session_allows(&req_for("bash", None)));
        bridge.clear_session_scopes();
        assert!(!bridge.session_allows(&req_for("bash", None)));
    }

    #[test]
    fn rules_cell_shared_and_hot_swappable() {
        // P0-5：rules_handle 与桥自身 set_rules 是同一 cell。
        let sink = sink_with_clients(0);
        let bridge = SessionApprovalBridge::new(sink);
        let handle = bridge.rules_handle();
        let rules = ApprovalRules {
            default_level: openslate_core::approval::ToolApprovalLevel::Manual,
            ..ApprovalRules::default()
        };
        bridge.set_rules(rules.clone());
        let read = handle
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        assert_eq!(read, rules);
        // 初始值 = default()（= Allow 从不问，P0-4）。
        let fresh = SessionApprovalBridge::new(sink_with_clients(0));
        assert_eq!(fresh.rules_snapshot(), ApprovalRules::default());
    }
}
