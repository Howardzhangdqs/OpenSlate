//! MCP host 死亡自愈：装饰器 + 单飞恢复 + 无感重试。
//!
//! Termux 里的 MCP 聚合 host（HTTP 127.0.0.1:8765）被杀后，原行为是
//! `McpTool::execute` 向模型返回「无法连接」死端错误（`Err(
//! ExecutionError("MCP tool '{name}' call failed: Transport …"))`），回合
//! 只能靠模型/用户自纠。本模块在外部 MCP 工具外面再包一层
//! [`RecoveringMcpTool`] 装饰器，把这条死端错误变成一次自愈循环：
//!
//! ```text
//! RecoveringMcpTool::execute
//!   → inner.execute(args)                 [旧 McpTool，连接已死]
//!   → Err 且命中连接失败签名（见 CONNECT_FAILURE_SIGNATURES）
//!   → RecoveryCtx::run_recovery            [单飞闩 + 冷却]
//!       └ HostCallRouter::call_with_id(id, "mcp.recover_host", …, 900s)
//!           → Kotlin 恢复阶梯（面板提示 → 拉起 Termux/host → reload）
//!   → 恢复 Ok → registry.execute(同名同参)   [reload 后拿到新 McpTool]
//!   → 结果无感回填：tool_call↔result 配对不破、审计一次执行、模型零感知
//! ```
//!
//! ## 状态机（并行批 / 冷却 / 预算）
//!
//! - **单飞恢复闩**（`latch: tokio::sync::Mutex`）：同一时刻至多一轮恢复
//!   在途；并行批里多个 MCP 工具同时失败时，后到者在闩上排队，拿到闩时
//!   若完成代数（`generation`，仅成功 +1）已前进即**共享**该轮（不发自己
//!   的 host call），直接进入重试。
//! - **冷却窗口**（`cooldown`，默认 10s）：一轮恢复完成（无论成败）后，
//!   窗口内的后续失败不再发起 host call（`Suppressed`）——但注册表可能
//!   已被上一轮成功恢复刷新，仍给一次注册表重试（预算不变）。
//! - **重试预算 = 1**：每次用户可见调用至多经注册表重发一次。重发期间以
//!   task-local 标记防递归（reload 未换新装饰器时，registry 重发回到
//!   装饰器的重入直接上抛原始错误，不再走恢复/重试）。
//! - 失败兜底：恢复 host call 被 resolve false / 超时 / `fail_all` → 返回
//!   **原始错误**并附一行恢复动作说明（模型可据此换路径，而非死端）。
//!
//! ## 包装点（两处，与 SharedTool/回滚语义正交）
//!
//! - 装配期：`create_with` 在 `register_bash_tools` 后对名册整体包一层
//!   （`wrap_mcp_tools`，register 覆盖式原子换入，对在途调用零影响）；
//! - reload 期：`reload_mcp` 第 4 步 scratch 迁移时逐工具包
//!   （`SharedTool(Arc<RecoveringMcpTool>)`）。回滚路径装回的旧 Arc 本就
//!   是已包装的装饰器，不会双层包装。
//!
//! ## 边界
//!
//! - **签名取舍**：只认 `call failed: Transport send error` /
//!   `call failed: Transport closed`（连接层失败 = host 进程死的确定性
//!   信号）。`timed out after 300s` **不纳入**——挂死型超时与合法慢调用
//!   无法区分，误重启会杀掉在途的合法工作；此类仍走原超时错误。
//! - **MCP 业务失败不经此路径**：`Ok(status=Error)`（is_error 回传）不
//!   是基础设施失败，装饰器原样透传。
//! - **跨进程重启不持久化**：App 进程被杀则挂起的恢复/重试随 future
//!   消失（host call 通道已不存在）；重启后由装配期重连自愈。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use openslate_core::error::ToolError;
use openslate_core::tool::{Tool, ToolRegistry};
use openslate_core::types::ToolOutput;
use serde_json::Value;

use crate::hostcall::HostCallRouter;

/// host call 工具名（Kotlin `RuntimeBridge.handleHostCall` 分发键）。
pub const MCP_RECOVER_HOST_TOOL: &str = "mcp.recover_host";

/// 恢复 host call 等待上限：恢复阶梯含拉起 Termux（前台冷启动）+
/// restartHost（内部 ~8s 端口轮询）+ reloadMcpServers（逐 server 连接），
/// 要给足用户可见恢复的余量；照 exec.rs `TERMUX_HOST_CALL_TIMEOUT` 与
/// ask_user 的 900s 先例。turn 取消 / runtime 关停另有 fail_all 兜底。
pub const MCP_RECOVER_HOST_CALL_TIMEOUT: Duration = Duration::from_secs(900);

/// 恢复冷却窗口：一轮恢复完成后，窗口内的后续连接失败不再发起 host call
/// （防恢复未奏效时的请求风暴；注册表重试照给——上一轮成功刷新过的
/// 注册表值得再试一次）。
pub const MCP_RECOVERY_COOLDOWN: Duration = Duration::from_secs(10);

/// 连接失败签名（`McpTool::execute` 的 `Err(ExecutionError)` 文案子串）：
/// host 死亡 = rmcp 传输层的 send 失败 / 传输关闭。超时**有意不在此列**
/// （见模块文档「签名取舍」）。
const CONNECT_FAILURE_SIGNATURES: &[&str] = &[
    "call failed: Transport send error",
    "call failed: Transport closed",
];

/// 错误是否为 MCP 连接失败签名（自愈路径的唯一入口判定）。
pub fn is_mcp_connection_failure(err: &ToolError) -> bool {
    match err {
        ToolError::ExecutionError(msg) => {
            CONNECT_FAILURE_SIGNATURES.iter().any(|sig| msg.contains(sig))
        }
        _ => false,
    }
}

/// 一轮恢复请求的结局（单飞语义下的三种出口）。
#[derive(Debug, Clone)]
pub enum RecoveryOutcome {
    /// 恢复已发生（本轮发起者拿到宿主回执）或共享了别轮刚完成的**成功**
    /// 恢复（等待者拿到闩时代数已前进）——注册表大概率已被 reload 刷新，
    /// 值得重试一次。
    Recovered(Value),
    /// 冷却窗口内的快速失败：未发 host call。注册表仍可能带着上一轮成功
    /// 恢复刷新过的工具，同样给一次重试（重试预算 1 不变）。
    Suppressed,
    /// 恢复发起但失败（宿主 resolve false / 超时 / runtime 关停）。
    Failed(String),
}

/// 恢复共享上下文（装配层一份，所有 [`RecoveringMcpTool`] 装饰器共享）。
pub struct RecoveryCtx {
    router: Arc<HostCallRouter>,
    /// 弱引用：装饰器经注册表重发需要它，但不阻止装配层释放（runtime
    /// 关停时 registry 先于装饰器消失是正常序）。
    registry: Weak<ToolRegistry>,
    /// 单飞恢复闩：同一时刻至多一轮恢复在途；并行批的后到者在此排队。
    latch: tokio::sync::Mutex<()>,
    /// 已完成**成功**恢复的代数（每轮成功 +1，在闩内置位）：等待者以进入
    /// 时快照对比，拿到闩时代数已前进 = 共享了该轮，不发自己的 host call。
    generation: AtomicU64,
    /// 最近一轮恢复完成时刻（**无论成败**——失败的冷却同样生效，防
    /// 失败风暴；std Mutex，锁内无 await）。
    last_finished: std::sync::Mutex<Option<Instant>>,
    cooldown: Duration,
    call_timeout: Duration,
}

impl RecoveryCtx {
    /// 生产构造（默认冷却 10s / host call 超时 900s）。
    pub fn new(router: Arc<HostCallRouter>, registry: &Arc<ToolRegistry>) -> Arc<Self> {
        Self::with_tunables(
            router,
            registry,
            MCP_RECOVERY_COOLDOWN,
            MCP_RECOVER_HOST_CALL_TIMEOUT,
        )
    }

    /// 测试注入口：冷却窗口与 host call 超时可调。
    pub fn with_tunables(
        router: Arc<HostCallRouter>,
        registry: &Arc<ToolRegistry>,
        cooldown: Duration,
        call_timeout: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            router,
            registry: Arc::downgrade(registry),
            latch: tokio::sync::Mutex::new(()),
            generation: AtomicU64::new(0),
            last_finished: std::sync::Mutex::new(None),
            cooldown,
            call_timeout,
        })
    }

    fn registry(&self) -> Option<Arc<ToolRegistry>> {
        self.registry.upgrade()
    }

    /// 单飞恢复入口。调用方 = 装饰器的 execute（连接失败签名命中后）。
    pub async fn run_recovery(&self, failing_tool: &str, reason: &str) -> RecoveryOutcome {
        let entry_gen = self.generation.load(Ordering::Acquire);
        // 单飞闩：在途恢复期间后到者在此排队；闩内三判（共享 / 冷却 / 发起）。
        let _guard = self.latch.lock().await;

        // (a) 共享：排队期间已有一轮**成功**恢复完成（代数前进）——不发
        //     自己的 host call，按「已恢复」放行重试。
        if self.generation.load(Ordering::Acquire) > entry_gen {
            crate::alog!("mcp_recovery: {failing_tool:?} 共享了刚完成的恢复轮");
            return RecoveryOutcome::Recovered(serde_json::json!({"shared": true}));
        }

        // (b) 冷却：上一轮完成未满窗口 → 快速失败（不发 host call；调用
        //     方仍重试一次——注册表可能已被上一轮 reload 刷新）。
        {
            let last = *self
                .last_finished
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(finished) = last {
                if finished.elapsed() < self.cooldown {
                    crate::alog!(
                        "mcp_recovery: 冷却中（上轮 {:?} 前 < {:?}），跳过恢复",
                        finished.elapsed(),
                        self.cooldown
                    );
                    return RecoveryOutcome::Suppressed;
                }
            }
        }

        // (c) 发起：本轮唯一一次恢复 host call（900s 上限，Kotlin 侧恢复
        //     阶梯：面板提示 → 拉起 Termux/host → reload）。
        crate::alog!(
            "mcp_recovery: 发起 host 恢复（tool={failing_tool:?} reason={reason:?}）"
        );
        let id = self.router.allocate_id();
        let args = serde_json::json!({
            "failing_tool": failing_tool,
            "reason": reason,
        });
        let result = self
            .router
            .call_with_id(id, MCP_RECOVER_HOST_TOOL, &args, self.call_timeout)
            .await;

        // 记账在闩内、放闩前完成：等待者拿到闩后才能观察到新代数/新时刻。
        // 代数只在成功时 +1（失败轮的等待者走冷却快速失败，而非误共享）。
        *self.last_finished.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        match result {
            Ok(v) => {
                self.generation.fetch_add(1, Ordering::Release);
                RecoveryOutcome::Recovered(v)
            }
            Err(e) => RecoveryOutcome::Failed(e),
        }
    }
}

// 重试重入标记（task-local）：registry.execute 同名重发期间置位。reload
// 未换新装饰器时，重发会回到（可能就是本实例的）装饰器 execute——标记
// 命中即预算已用，原始错误直接上抛，不再递归恢复/重试。并发的新调用
// （并行批里同名工具的另一次调用）在**另一个 task** 上，看不到标记，
// 照常走单飞恢复（闩保证它们共享同一轮）。
// （// 而非 ///：task_local! 宏产物不吃 doc comment。）
tokio::task_local! {
    static RETRY_INVOCATION: ();
}

/// MCP 工具的恢复装饰器：连接失败签名 → 单飞恢复 host call → 注册表
/// 同名同参重发一次。name/description/schema/namespace 全部透传 inner
/// （namespace 是 PTC 沙箱暴露的 `tools.<ns>.<method>` 组成部分，必须
/// 与被包装的 McpTool 一致）。
pub struct RecoveringMcpTool {
    inner: Arc<dyn Tool>,
    shared: Arc<RecoveryCtx>,
}

impl RecoveringMcpTool {
    pub fn new(inner: Arc<dyn Tool>, shared: Arc<RecoveryCtx>) -> Self {
        Self { inner, shared }
    }

    fn in_retry_invocation() -> bool {
        RETRY_INVOCATION.try_with(|_| true).unwrap_or(false)
    }

    async fn retry_via_registry(
        &self,
        name: &str,
        args: &Value,
    ) -> Result<ToolOutput, ToolError> {
        match self.shared.registry() {
            Some(registry) => registry.execute(name, args).await,
            None => Err(ToolError::ExecutionError(
                "MCP 恢复重试失败：注册表已释放（runtime 关停中）".to_owned(),
            )),
        }
    }
}

#[async_trait]
impl Tool for RecoveringMcpTool {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn namespace(&self) -> Option<String> {
        self.inner.namespace()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn parameters_schema(&self) -> Value {
        self.inner.parameters_schema()
    }

    async fn execute(&self, args: &Value) -> Result<ToolOutput, ToolError> {
        let started = Instant::now();
        match self.inner.execute(args).await {
            Ok(out) => Ok(out),
            // 连接失败签名 → 自愈路径。重试重入（task-local 标记）= 预算
            // 已用：原始错误直接上抛（外层重试路径会附指引）。
            Err(e) if is_mcp_connection_failure(&e) && !Self::in_retry_invocation() => {
                self.recover_and_retry(args, e, started).await
            }
            // 其余错误（含 300s 超时、MCP 业务失败走 Ok 不进这里）原样透传。
            Err(e) => Err(e),
        }
    }
}

impl RecoveringMcpTool {
    async fn recover_and_retry(
        &self,
        args: &Value,
        original: ToolError,
        started: Instant,
    ) -> Result<ToolOutput, ToolError> {
        let name = self.inner.name().to_owned();
        let reason = original.to_string();
        let outcome = self.shared.run_recovery(&name, &reason).await;
        match outcome {
            RecoveryOutcome::Failed(why) => Err(with_recovery_note(original, &why)),
            RecoveryOutcome::Recovered(_) | RecoveryOutcome::Suppressed => {
                // 重试预算 = 1：同名同参经注册表重发（reload 后拿到新
                // McpTool/新装饰器；未换新时的重入被 task-local 标记挡回）。
                let retry = RETRY_INVOCATION
                    .scope((), self.retry_via_registry(&name, args))
                    .await;
                match retry {
                    Ok(mut out) => {
                        // 恢复等待计入耗时（审计口径 = 一次用户可见执行）。
                        out.duration_ms = started.elapsed().as_millis() as u64;
                        crate::alog!(
                            "mcp_recovery: {name:?} 恢复后重试成功（含恢复等待 {:?}）",
                            started.elapsed()
                        );
                        Ok(out)
                    }
                    Err(retry_err) => Err(with_retry_note(retry_err)),
                }
            }
        }
    }
}

/// 恢复失败回执：保留原始错误 + 一行恢复动作说明（模型可据此换路径或
/// 引导用户，而不是看到死端错误）。仅 ExecutionError 可附加文案。
fn with_recovery_note(original: ToolError, why: &str) -> ToolError {
    match original {
        ToolError::ExecutionError(msg) => ToolError::ExecutionError(format!(
            "{msg}\n[MCP host recovery attempted but failed: {why}; \
             修复入口：设置页「MCP」重载/一键安装，或打开 Termux 让 host 常驻]"
        )),
        other => other,
    }
}

/// 重试失败回执：重发后的原始错误 + 预算耗尽说明。
fn with_retry_note(retry_err: ToolError) -> ToolError {
    match retry_err {
        ToolError::ExecutionError(msg) => ToolError::ExecutionError(format!(
            "{msg}\n[MCP host recovery attempted (host restarted + tools reloaded) \
             but this call still failed; retry budget exhausted]"
        )),
        other => other,
    }
}

/// 把注册表里的外部 MCP 工具（按名册）包上恢复装饰器——**装配期包装点**。
/// `register` 覆盖式换入（单写锁内原子替换，对在途调用零影响：get 先
/// clone Arc，旧工具随旧 Arc 存活）。名册 = `connect_external_mcp` 注册的
/// 外部 MCP 工具名：bash 域被 MCP 接管的名字也在此列（名册里的都是
/// McpTool 本体，让位判定按名不按身份）；HostTool 壳 / native bash 不在
/// 名册内，不会被包。
pub fn wrap_mcp_tools(registry: &ToolRegistry, roster: &[String], shared: &Arc<RecoveryCtx>) {
    for name in roster {
        let Some(tool) = registry.get(name) else {
            continue;
        };
        registry.register_boxed(Box::new(RecoveringMcpTool::new(tool, shared.clone())));
    }
}

// ── 测试 ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventCallback;
    use crate::events::EventSink;
    use openslate_core::types::ToolOutputStatus;
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc as std_mpsc;
    use std::sync::Mutex;

    // ── 假工具 ───────────────────────────────────────────────────────────

    /// 可编程假 MCP 工具：按调用序弹出预排结果（Err=连接签名/其它错误，
    /// Ok=成功输出）；脚本耗尽后按 `exhausted_conn_failure` 兜底（true =
    /// 持续连接失败，可反复触发自愈；false = 健康）。
    struct ScriptedTool {
        name: String,
        script: Mutex<VecDeque<Result<ToolOutput, ToolError>>>,
        exhausted_conn_failure: bool,
    }

    impl ScriptedTool {
        fn new(
            name: &str,
            script: Vec<Result<ToolOutput, ToolError>>,
            exhausted_conn_failure: bool,
        ) -> Self {
            Self {
                name: name.to_owned(),
                script: Mutex::new(script.into_iter().collect()),
                exhausted_conn_failure,
            }
        }
    }

    #[async_trait]
    impl Tool for ScriptedTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn namespace(&self) -> Option<String> {
            Some("srv".to_owned())
        }
        fn description(&self) -> &str {
            "scripted mcp stand-in"
        }
        fn parameters_schema(&self) -> Value {
            serde_json::json!({})
        }
        async fn execute(&self, _args: &Value) -> Result<ToolOutput, ToolError> {
            let mut script = self.script.lock().unwrap_or_else(|e| e.into_inner());
            script.pop_front().unwrap_or_else(|| {
                if self.exhausted_conn_failure {
                    Err(conn_err(&self.name))
                } else {
                    Ok(ok_out("healthy"))
                }
            })
        }
    }

    fn conn_err(name: &str) -> ToolError {
        ToolError::ExecutionError(format!(
            "MCP tool '{name}' call failed: Transport send error: os error 111 (connection refused)"
        ))
    }

    fn ok_out(content: &str) -> ToolOutput {
        ToolOutput {
            bytes: content.len(),
            content: content.to_owned(),
            duration_ms: 1,
            status: ToolOutputStatus::Success,
        }
    }

    // ── 测试台（sink + router + registry + 假宿主线程）───────────────────

    struct TestRig {
        registry: Arc<ToolRegistry>,
        recover_calls: Arc<AtomicUsize>,
        _host_thread: std::thread::JoinHandle<()>,
    }

    /// 换新工具动作（模拟 Kotlin 恢复阶梯的 reload）：注册一个包装了恢复
    /// 装饰器的 ScriptedTool（`exhausted_conn_failure` = 换上的是否仍是
    /// 坏工具——重启未奏效的场景）。
    fn swap_tool(
        name: &str,
        still_broken: bool,
        ctx: &Arc<RecoveryCtx>,
    ) -> Box<dyn Fn(&Arc<ToolRegistry>) + Send> {
        let name = name.to_owned();
        let ctx = ctx.clone();
        Box::new(move |registry: &Arc<ToolRegistry>| {
            let fresh: Arc<dyn Tool> = Arc::new(ScriptedTool::new(&name, vec![], still_broken));
            registry.register_boxed(Box::new(RecoveringMcpTool::new(fresh, ctx.clone())));
        })
    }

    /// 组装测试台并注册一个**已包装**的初始工具（脚本 exhausted 后持续
    /// 连接失败，可反复触发自愈路径）。
    fn rig(
        tool: &str,
        initial_script: Vec<Result<ToolOutput, ToolError>>,
        resolve_ok: bool,
        on_recover: Option<Box<dyn Fn(&Arc<ToolRegistry>) + Send>>,
        delay_ms: u64,
        cooldown: Duration,
    ) -> TestRig {
        let (sink, rx) = test_sink();
        let router = HostCallRouter::new(sink, Duration::from_secs(5));
        let registry = Arc::new(ToolRegistry::new());
        let recover_calls = Arc::new(AtomicUsize::new(0));
        let ctx = RecoveryCtx::with_tunables(
            router.clone(),
            &registry,
            cooldown,
            Duration::from_secs(5),
        );
        registry.register(ScriptedTool::new(tool, initial_script, true));
        let inner = registry.get(tool).expect("initial tool registered");
        registry.register_boxed(Box::new(RecoveringMcpTool::new(inner, ctx.clone())));
        let host_thread = spawn_fake_host(
            rx,
            router,
            registry.clone(),
            FakeHost {
                recover_calls: recover_calls.clone(),
                resolve_ok,
                on_recover,
                delay_ms,
            },
        );
        TestRig {
            registry,
            recover_calls,
            _host_thread: host_thread,
        }
    }

    struct FakeHost {
        /// 收到 mcp.recover_host 的次数。
        recover_calls: Arc<AtomicUsize>,
        /// resolve ok?
        resolve_ok: bool,
        /// resolve 前动作（模拟 Kotlin 的 reload 换新工具；可选延迟模拟
        /// 慢恢复，让并行批的第二个失败者排上单飞闩）。
        on_recover: Option<Box<dyn Fn(&Arc<ToolRegistry>) + Send>>,
        delay_ms: u64,
    }

    fn spawn_fake_host(
        rx: std_mpsc::Receiver<String>,
        router: Arc<HostCallRouter>,
        registry: Arc<ToolRegistry>,
        host: FakeHost,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            while let Ok(event) = rx.recv_timeout(Duration::from_secs(30)) {
                let Ok(v) = serde_json::from_str::<Value>(&event) else {
                    continue;
                };
                if v["type"] != "host_call_requested" {
                    continue;
                }
                let id = v["id"].as_u64().unwrap_or(0);
                if v["tool"].as_str() == Some(MCP_RECOVER_HOST_TOOL) {
                    host.recover_calls.fetch_add(1, Ordering::SeqCst);
                    if host.delay_ms > 0 {
                        std::thread::sleep(Duration::from_millis(host.delay_ms));
                    }
                    if let Some(action) = &host.on_recover {
                        action(&registry);
                    }
                    let _ = router.resolve(id, host.resolve_ok, r#"{"reloaded":true}"#.to_owned());
                } else {
                    let _ = router.resolve(id, false, "unsupported".to_owned());
                }
            }
        })
    }

    fn test_sink() -> (Arc<EventSink>, std_mpsc::Receiver<String>) {
        struct Cb(std_mpsc::Sender<String>);
        impl EventCallback for Cb {
            fn on_event(&self, event_json: String) {
                let _ = self.0.send(event_json);
            }
        }
        let (tx, rx) = std_mpsc::channel::<String>();
        (EventSink::new(Arc::new(Cb(tx))), rx)
    }

    // ── 签名分类 ─────────────────────────────────────────────────────────

    #[test]
    fn connection_signature_classification() {
        let e = |msg: &str| ToolError::ExecutionError(msg.to_owned());
        assert!(is_mcp_connection_failure(&e(
            "MCP tool 't__x' call failed: Transport send error: os error 111"
        )));
        assert!(is_mcp_connection_failure(&e(
            "MCP tool 't__x' call failed: Transport closed"
        )));
        // 挂死型超时不纳入（与合法慢调用无法区分，误杀代价高）。
        assert!(!is_mcp_connection_failure(&e(
            "MCP tool 't__x' timed out after 300s"
        )));
        assert!(!is_mcp_connection_failure(&e(
            "MCP tool 't__x' call failed: something else"
        )));
        assert!(!is_mcp_connection_failure(&ToolError::NotFound("t__x".into())));
    }

    // ── 透传 ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn success_and_non_connection_errors_passthrough() {
        let rig = rig(
            "t__x",
            vec![
                Ok(ok_out("fine")),
                Err(ToolError::ExecutionError(
                    "MCP tool 't__x' call failed: boom".into(),
                )),
            ],
            true,
            None,
            0,
            Duration::from_secs(10),
        );
        let out = rig
            .registry
            .execute("t__x", &serde_json::json!({}))
            .await
            .expect("成功透传");
        assert_eq!(out.content, "fine");
        let err = rig
            .registry
            .execute("t__x", &serde_json::json!({}))
            .await
            .expect_err("非连接失败错误透传");
        assert!(err.to_string().contains("boom"), "{err}");
        // 无恢复 host call。
        assert_eq!(rig.recover_calls.load(Ordering::SeqCst), 0);
    }

    // ── 自愈主路径 ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn recover_swaps_and_retries_transparently() {
        // 初始工具持续连接失败；假宿主 resolve 前把注册表换成健康工具
        // （模拟 Kotlin reload）→ 模型视角：一次调用、一次成功。
        let (sink, rx) = test_sink();
        let router = HostCallRouter::new(sink, Duration::from_secs(5));
        let registry = Arc::new(ToolRegistry::new());
        let recover_calls = Arc::new(AtomicUsize::new(0));
        let ctx = RecoveryCtx::with_tunables(
            router.clone(),
            &registry,
            Duration::from_secs(10),
            Duration::from_secs(5),
        );
        registry.register(ScriptedTool::new("t__x", vec![], true));
        let inner = registry.get("t__x").expect("initial tool registered");
        registry.register_boxed(Box::new(RecoveringMcpTool::new(inner, ctx.clone())));
        // 假宿主延迟 30ms 再 resolve：保证恢复等待可被观测（duration 覆盖
        // 断言），也给并行场景留排队窗口。
        let _host = spawn_fake_host(
            rx,
            router,
            registry.clone(),
            FakeHost {
                recover_calls: recover_calls.clone(),
                resolve_ok: true,
                on_recover: Some(swap_tool("t__x", false, &ctx)),
                delay_ms: 30,
            },
        );

        let out = registry
            .execute("t__x", &serde_json::json!({"a": 1}))
            .await
            .expect("恢复后重试成功");
        assert_eq!(out.content, "healthy");
        assert_eq!(out.status, ToolOutputStatus::Success);
        // 恢复等待计入耗时（脚本 duration_ms=1，重试成功路径覆盖为全程，
        // 假宿主 30ms 延迟确保 >= 30）。
        assert!(out.duration_ms >= 30, "duration={}", out.duration_ms);
        // 恰一次恢复 host call。
        assert_eq!(recover_calls.load(Ordering::SeqCst), 1);
    }

    // ── 并行批单飞 ───────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn parallel_failures_share_single_recovery() {
        let (sink, rx) = test_sink();
        let router = HostCallRouter::new(sink, Duration::from_secs(5));
        let registry = Arc::new(ToolRegistry::new());
        let recover_calls = Arc::new(AtomicUsize::new(0));
        let ctx = RecoveryCtx::with_tunables(
            router.clone(),
            &registry,
            Duration::from_secs(10),
            Duration::from_secs(5),
        );
        for name in ["t__a", "t__b"] {
            registry.register(ScriptedTool::new(name, vec![], true));
            let inner = registry.get(name).expect("registered");
            registry.register_boxed(Box::new(RecoveringMcpTool::new(inner, ctx.clone())));
        }
        // 假宿主延迟 300ms 再 resolve（让第二个失败者排上单飞闩），resolve
        // 前把两个名字都换成健康工具。
        let swap_broken = false;
        let on_recover = {
            let ctx = ctx.clone();
            Box::new(move |reg: &Arc<ToolRegistry>| {
                for name in ["t__a", "t__b"] {
                    let fresh: Arc<dyn Tool> =
                        Arc::new(ScriptedTool::new(name, vec![], swap_broken));
                    reg.register_boxed(Box::new(RecoveringMcpTool::new(fresh, ctx.clone())));
                }
            }) as Box<dyn Fn(&Arc<ToolRegistry>) + Send>
        };
        let _host = spawn_fake_host(
            rx,
            router,
            registry.clone(),
            FakeHost {
                recover_calls: recover_calls.clone(),
                resolve_ok: true,
                on_recover: Some(on_recover),
                delay_ms: 300,
            },
        );

        let reg_a = registry.clone();
        let reg_b = registry.clone();
        let (a, b) = tokio::join!(
            async move { reg_a.execute("t__a", &serde_json::json!({})).await },
            async move { reg_b.execute("t__b", &serde_json::json!({})).await },
        );
        let a = a.expect("A 恢复后成功");
        let b = b.expect("B 共享同一轮恢复后成功");
        assert_eq!(a.content, "healthy");
        assert_eq!(b.content, "healthy");
        // 并行批共享：两个失败只发一次恢复 host call。
        assert_eq!(recover_calls.load(Ordering::SeqCst), 1);
    }

    // ── 预算 = 1 ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn retry_budget_is_one() {
        // 恢复「成功」（resolve ok）但换上的仍是连接失败工具（host 重启
        // 未奏效）→ 重试一次后上抛，且不发起第二次 host call。
        let (sink, rx) = test_sink();
        let router = HostCallRouter::new(sink, Duration::from_secs(5));
        let registry = Arc::new(ToolRegistry::new());
        let recover_calls = Arc::new(AtomicUsize::new(0));
        let ctx = RecoveryCtx::with_tunables(
            router.clone(),
            &registry,
            Duration::from_secs(10),
            Duration::from_secs(5),
        );
        registry.register(ScriptedTool::new("t__x", vec![], true));
        let inner = registry.get("t__x").expect("initial tool registered");
        registry.register_boxed(Box::new(RecoveringMcpTool::new(inner, ctx.clone())));
        // 换上的新工具**仍是坏的**，且同样包装饰器（task-local 重入防护
        // 的试金石：registry 重发回到新装饰器，标记命中直接上抛）。
        let _host = spawn_fake_host(
            rx,
            router,
            registry.clone(),
            FakeHost {
                recover_calls: recover_calls.clone(),
                resolve_ok: true,
                on_recover: Some(swap_tool("t__x", true, &ctx)),
                delay_ms: 0,
            },
        );

        let err = registry
            .execute("t__x", &serde_json::json!({}))
            .await
            .expect_err("重试后仍失败");
        let msg = err.to_string();
        assert!(msg.contains("Transport send error"), "保留原始错误: {msg}");
        assert!(msg.contains("recovery attempted"), "附预算说明: {msg}");
        // 一次 host call（重试的重入被 task-local 标记挡住，未再发起）。
        assert_eq!(recover_calls.load(Ordering::SeqCst), 1);
    }

    // ── 恢复失败 → 原始错误 ──────────────────────────────────────────────

    #[tokio::test]
    async fn failed_recovery_returns_original_error() {
        let rig = rig("t__x", vec![], false, None, 0, Duration::from_secs(10));
        let err = rig
            .registry
            .execute("t__x", &serde_json::json!({}))
            .await
            .expect_err("恢复失败");
        let msg = err.to_string();
        assert!(
            msg.contains("Transport send error"),
            "原始错误保留: {msg}"
        );
        assert!(
            msg.contains("recovery attempted but failed"),
            "附恢复失败说明: {msg}"
        );
        assert_eq!(rig.recover_calls.load(Ordering::SeqCst), 1);
    }

    // ── 冷却：窗口内快速失败，过期重武装 ─────────────────────────────────

    #[tokio::test]
    async fn cooldown_suppresses_then_rearms() {
        // 冷却 500ms（防 CI 抖动误判），过期阈值用 700ms 等待越过。
        // 假宿主的换工具动作带状态：首轮换坏工具（重启未奏效）、次轮换
        // 健康工具——分别覆盖「冷却内快速失败」与「过期重武装后成功」。
        let (sink, rx) = test_sink();
        let router = HostCallRouter::new(sink, Duration::from_secs(5));
        let registry = Arc::new(ToolRegistry::new());
        let recover_calls = Arc::new(AtomicUsize::new(0));
        let ctx = RecoveryCtx::with_tunables(
            router.clone(),
            &registry,
            Duration::from_millis(500),
            Duration::from_secs(5),
        );
        registry.register(ScriptedTool::new("t__x", vec![], true));
        let inner = registry.get("t__x").expect("initial tool registered");
        registry.register_boxed(Box::new(RecoveringMcpTool::new(inner, ctx.clone())));
        let swap_count = Arc::new(AtomicUsize::new(0));
        let on_recover = {
            let ctx = ctx.clone();
            let swap_count = swap_count.clone();
            Box::new(move |reg: &Arc<ToolRegistry>| {
                // 首次恢复换上的仍是坏工具；第二次换健康工具。
                let still_broken = swap_count.fetch_add(1, Ordering::SeqCst) == 0;
                let fresh: Arc<dyn Tool> = Arc::new(ScriptedTool::new("t__x", vec![], still_broken));
                reg.register_boxed(Box::new(RecoveringMcpTool::new(fresh, ctx.clone())));
            }) as Box<dyn Fn(&Arc<ToolRegistry>) + Send>
        };
        let _host = spawn_fake_host(
            rx,
            router,
            registry.clone(),
            FakeHost {
                recover_calls: recover_calls.clone(),
                resolve_ok: true,
                on_recover: Some(on_recover),
                delay_ms: 0,
            },
        );

        // 第 1 次：发起恢复（host call 1），换上的仍是坏工具 → 重试失败。
        assert!(registry
            .execute("t__x", &serde_json::json!({}))
            .await
            .is_err());
        assert_eq!(recover_calls.load(Ordering::SeqCst), 1);

        // 第 2 次（冷却内）：Suppressed——不发 host call，仅一次注册表重试
        // （重入被 task-local 标记挡回）。
        assert!(registry
            .execute("t__x", &serde_json::json!({}))
            .await
            .is_err());
        assert_eq!(
            recover_calls.load(Ordering::SeqCst),
            1,
            "冷却内的后续失败不再发起恢复"
        );

        // 冷却过期 → 第 3 次失败重新发起恢复（host call 2），这次换上的
        // 是健康工具 → 重试成功。
        tokio::time::sleep(Duration::from_millis(700)).await;
        let out = registry
            .execute("t__x", &serde_json::json!({}))
            .await
            .expect("冷却过期后恢复成功");
        assert_eq!(out.content, "healthy");
        assert_eq!(recover_calls.load(Ordering::SeqCst), 2);
    }

    // ── 装配期包装点 ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn wrap_only_roster_names() {
        let (sink, rx) = test_sink();
        let router = HostCallRouter::new(sink, Duration::from_secs(5));
        let registry = Arc::new(ToolRegistry::new());
        let recover_calls = Arc::new(AtomicUsize::new(0));
        let ctx = RecoveryCtx::with_tunables(
            router.clone(),
            &registry,
            Duration::from_secs(10),
            Duration::from_secs(5),
        );
        let _host = spawn_fake_host(
            rx,
            router,
            registry.clone(),
            FakeHost {
                recover_calls: recover_calls.clone(),
                resolve_ok: true,
                on_recover: None,
                delay_ms: 0,
            },
        );

        registry.register(ScriptedTool::new(
            "mcp__x",
            vec![Ok(ok_out("ok"))],
            false,
        ));
        struct PlainBash;
        #[async_trait]
        impl Tool for PlainBash {
            fn name(&self) -> &str {
                "bash"
            }
            fn description(&self) -> &str {
                "host fallback shell"
            }
            fn parameters_schema(&self) -> Value {
                serde_json::json!({})
            }
            async fn execute(&self, _args: &Value) -> Result<ToolOutput, ToolError> {
                Ok(ok_out("native"))
            }
        }
        registry.register(PlainBash);
        let bash_before = registry.get("bash").expect("bash registered");

        wrap_mcp_tools(&registry, &["mcp__x".to_owned()], &ctx);

        // 名册内的被换为装饰器：identity 变化 + 元数据透传。
        let mcp_after = registry.get("mcp__x").expect("still registered");
        assert_eq!(mcp_after.name(), "mcp__x");
        assert_eq!(mcp_after.namespace().as_deref(), Some("srv"));
        // bash 原样（HostTool 壳 / native 兜底不在名册，不包装）。
        let bash_after = registry.get("bash").expect("bash registered");
        assert!(
            Arc::ptr_eq(&bash_before, &bash_after),
            "非名册工具不被包装"
        );
        // 包装后调用照常成功（脚本 Ok）。
        let out = registry
            .execute("mcp__x", &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(out.content, "ok");
        assert_eq!(recover_calls.load(Ordering::SeqCst), 0);
    }
}
