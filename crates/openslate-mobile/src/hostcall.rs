//! Host tool 桥（PLAN §6 的 request/resolve 模型）。
//!
//! Rust 侧的 `Tool` 需要宿主（Kotlin）能力时，不直接 await 宿主异步，
//! 而是：
//!
//! ```text
//! Tool::execute ─► HostCallRouter::call(tool, args)
//!                     │ 注册 oneshot + 编号 id
//!                     │ emit {"type":"host_call_requested",...}（与 ServerMsg
//!                     │      同一条事件流，全局保序）
//!                     ▼
//!                Kotlin 异步执行 Android API
//!                     │
//!                     ▼
//!                resolve_host_call(id, ok, payload)
//!                     │ oneshot 唤醒 / 超时兜底
//!                     ▼
//!                Tool::execute 返回，Agent 继续
//! ```
//!
//! FFI 永不阻塞 Android Main Thread；Rust 侧对每个 host call 有超时
//! 兜底（宿主崩溃/忘记 resolve 不会挂死回合）。
//!
//! 信封格式（与 ServerMsg 的 `type` tag 命名空间不重叠，宿主按 `type`
//! 分发）：
//!
//! ```json
//! {"type":"host_call_requested","id":42,"tool":"android.app.launch","args":{…}}
//! ```
//!
//! 结果不广播（只回填等待方）；宿主可自行决定是否镜像 UI 状态。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use openslate_core::error::ToolError;
use openslate_core::tool::Tool;
use openslate_core::types::{ToolOutput, ToolOutputStatus};
use serde_json::Value;

use crate::events::EventSink;

/// host call 默认超时：宿主 UI 自动化/系统调用可能较慢，给足余量；
/// 超时视为工具失败（回合可继续），不是 runtime 失败。
pub const HOST_CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// 超时错误文案前缀（ask_user 等调用方靠它区分超时与基础设施错误，
/// 同 crate 内的稳定契约）。
pub const HOST_CALL_TIMEOUT_ERR_PREFIX: &str = "host call timeout";

/// `fail_all` 豁免工具名集合（拍板 D7：最小改法按名匹配，不引入按
/// 调用注册的机制）。ask_user = 问卷持久化恢复；notify = wait 通知行
/// 持久化恢复（非 wait 通知同被豁免属已知良性偏差，见 `fail_all`
/// 文档）。
const FAIL_ALL_EXEMPT_TOOLS: &[&str] = &[
    openslate_core::ask_user::ASK_USER_TOOL,
    crate::notify::NOTIFY_TOOL,
];

/// 一条在途 host call 的应答通道 + 发起工具名（`fail_all` 按名豁免）。
struct Pending {
    tx: tokio::sync::oneshot::Sender<Result<Value, String>>,
    tool: String,
}

pub struct HostCallRouter {
    sink: Arc<EventSink>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, Pending>>,
    timeout: Duration,
}

impl HostCallRouter {
    pub fn new(sink: Arc<EventSink>, timeout: Duration) -> Arc<Self> {
        Arc::new(Self {
            sink,
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            timeout,
        })
    }

    /// 预分配一个 host call id（ask_user 需在发出信封**之前**持久化
    /// 问卷行，幂等主键 = id；普通调用方走 [`Self::call`] 自取）。
    pub fn allocate_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }

    /// 发起一次 host call：发出 `host_call_requested` 信封并等待应答。
    pub async fn call(&self, tool: &str, args: &Value) -> Result<Value, String> {
        let id = self.allocate_id();
        self.call_with_id(id, tool, args, self.timeout).await
    }

    /// 带 id 与自定义超时的 host call（id 经 [`Self::allocate_id`] 预取；
    /// ask_user 的「等人」超时远超默认 120s，never 配置取近似无限的
    /// 极长时长）。
    pub async fn call_with_id(
        &self,
        id: u64,
        tool: &str,
        args: &Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let (tx, rx) = tokio::sync::oneshot::channel::<Result<Value, String>>();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                id,
                Pending {
                    tx,
                    tool: tool.to_owned(),
                },
            );

        let envelope = serde_json::json!({
            "type": "host_call_requested",
            "id": id,
            "tool": tool,
            "args": args,
        });
        self.sink.emit_raw(envelope.to_string());

        let result = match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(res)) => res,
            // 发送端 drop（fail_all / runtime 关停）。
            Ok(Err(_)) => Err("host call aborted (runtime shutting down)".to_owned()),
            Err(_) => {
                self.pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                return Err(format!(
                    "{HOST_CALL_TIMEOUT_ERR_PREFIX} after {}ms (host did not resolve)",
                    timeout.as_millis()
                ));
            }
        };
        result
    }

    /// 宿主应答入口。`ok=true` 时 `payload` 是结果 JSON 文本；
    /// `ok=false` 时 `payload` 是错误说明。返回 false = 无此在途调用
    /// （**对失效 id 无害 no-op**——取消后的 UI 补答安全）。
    pub fn resolve(&self, id: u64, ok: bool, payload: String) -> bool {
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
        match pending {
            Some(Pending { tx, .. }) => {
                let result = if ok {
                    serde_json::from_str(&payload)
                        .map_err(|e| format!("host returned invalid JSON payload: {e}"))
                } else {
                    Err(payload)
                };
                tx.send(result).is_ok()
            }
            None => false,
        }
    }

    /// 关停兜底：让在途调用立刻失败（引擎解栈）。
    ///
    /// **豁免工具集合**（ask_user：ask-user 设计 r5 中断豁免的对接点；
    /// notify：notify 设计 S1b 同款对接）：pending 的 ask_user / notify
    /// 不 fail——工具 future 随 runtime 收束被 drop，问卷 / 通知等待行
    /// 留在持久层 pending，重启后经快照回放 + 迟到答案补全恢复；若在
    /// 此处 fail，runner 会把错误结果并入历史、行被迫关卷，迟到答案
    /// 无处补全。其余工具照旧。
    ///
    /// 已知良性偏差（拍板 D7，勿「修」）：非 wait 的 notify 调用同样被
    /// 豁免——它没有落库行无需保护、宿主侧毫秒级完成即 resolve，豁免
    /// 与否无落地行为差异；ask_user 久有同款行为（按工具名粒度豁免，
    /// 不引入按调用注册的新机制是最小改法）。
    pub fn fail_all(&self, reason: &str) {
        let drained: Vec<(u64, Pending)> = {
            let mut guard = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            guard.drain().collect()
        };
        for (id, Pending { tx, tool }) in drained {
            if FAIL_ALL_EXEMPT_TOOLS.contains(&tool.as_str()) {
                // 放回待答（进程若继续存活，应答通路不变；随 router
                // drop 自然消亡）。
                self.pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(id, Pending { tx, tool });
                continue;
            }
            let _ = tx.send(Err(reason.to_owned()));
        }
    }
}

/// 由宿主执行的 `Tool` 壳：name/description/schema 进注册表，execute
/// 经 [`HostCallRouter`] 转发宿主。
pub struct HostTool {
    name: String,
    description: String,
    schema: Value,
    router: Arc<HostCallRouter>,
    /// host call 等待上限覆盖（None = router 默认 [`HostCallRouter::call`]）。
    /// termux 系壳需要远超默认 120s 的余量：命令停滞期间宿主进入「等待
    /// Termux 连接」模式（面板引导用户恢复、连接后原 id 重发续跑），见
    /// exec.rs 的 `TERMUX_HOST_CALL_TIMEOUT` 与 ask_user 的先例。
    timeout: Option<Duration>,
}

impl HostTool {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        schema: Value,
        router: Arc<HostCallRouter>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            schema,
            router,
            timeout: None,
        }
    }

    /// 覆盖 host call 等待上限（builder 风格；语义同 ask_user 对
    /// [`HostCallRouter::call_with_id`] 的自定义超时用法）。
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
}

#[async_trait]
impl Tool for HostTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> Value {
        self.schema.clone()
    }

    async fn execute(&self, args: &Value) -> Result<ToolOutput, ToolError> {
        let started = std::time::Instant::now();
        // 自定义超时走 call_with_id（照 ask_user 用法）；默认路径不变。
        let result = match self.timeout {
            Some(timeout) => {
                let id = self.router.allocate_id();
                self.router.call_with_id(id, &self.name, args, timeout).await
            }
            None => self.router.call(&self.name, args).await,
        };
        let duration_ms = started.elapsed().as_millis() as u64;
        match result {
            Ok(value) => {
                let content = serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_owned());
                Ok(ToolOutput {
                    attachments: None,
                    bytes: content.len(),
                    content,
                    duration_ms,
                    status: ToolOutputStatus::Success,
                })
            }
            Err(message) => Ok(ToolOutput {
                attachments: None,
                bytes: message.len(),
                content: message,
                duration_ms,
                status: ToolOutputStatus::Error,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{EventCallback, EventSink};
    use std::sync::mpsc as std_mpsc;

    fn wait_event(rx: &std_mpsc::Receiver<String>) -> String {
        rx.recv_timeout(Duration::from_secs(2))
            .expect("event within 2s")
    }

    #[tokio::test]
    async fn host_call_request_resolve_roundtrip() {
        let (tx, rx) = std_mpsc::channel::<String>();
        struct Cb(std_mpsc::Sender<String>);
        impl EventCallback for Cb {
            fn on_event(&self, event_json: String) {
                let _ = self.0.send(event_json);
            }
        }
        let sink = EventSink::new(Arc::new(Cb(tx)));
        let router = HostCallRouter::new(sink, Duration::from_secs(2));

        let router_clone = router.clone();
        let task = tokio::spawn(async move {
            router_clone
                .call("mobile.ping", &serde_json::json!({"x": 1}))
                .await
        });

        // current_thread runtime：先让 spawned future 跑到首个 await
        // （信封发出）再阻塞等宿主侧事件。
        tokio::time::sleep(Duration::from_millis(20)).await;

        // 宿主收到信封并应答。
        let event = wait_event(&rx);
        let v: Value = serde_json::from_str(&event).unwrap();
        assert_eq!(v["type"], "host_call_requested");
        assert_eq!(v["tool"], "mobile.ping");
        assert_eq!(v["args"]["x"], 1);
        let id = v["id"].as_u64().unwrap();
        assert!(router.resolve(id, true, r#"{"pong":true}"#.to_owned()));

        let result = task.await.unwrap().expect("resolved ok");
        assert_eq!(result["pong"], true);
    }

    #[tokio::test]
    async fn host_call_error_path() {
        let (tx, rx) = std_mpsc::channel::<String>();
        struct Cb(std_mpsc::Sender<String>);
        impl EventCallback for Cb {
            fn on_event(&self, event_json: String) {
                let _ = self.0.send(event_json);
            }
        }
        let sink = EventSink::new(Arc::new(Cb(tx)));
        let router = HostCallRouter::new(sink, Duration::from_secs(2));
        let router_clone = router.clone();
        let task =
            tokio::spawn(async move { router_clone.call("boom", &serde_json::json!({})).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let event = wait_event(&rx);
        let id = serde_json::from_str::<Value>(&event).unwrap()["id"]
            .as_u64()
            .unwrap();
        assert!(router.resolve(id, false, "android side exploded".to_owned()));
        let err = task.await.unwrap().expect_err("error propagated");
        assert!(err.contains("android side exploded"));
    }

    #[tokio::test]
    async fn host_call_timeout_path() {
        let (tx, _rx) = std_mpsc::channel::<String>();
        struct Cb(std_mpsc::Sender<String>);
        impl EventCallback for Cb {
            fn on_event(&self, event_json: String) {
                let _ = self.0.send(event_json);
            }
        }
        let sink = EventSink::new(Arc::new(Cb(tx)));
        let router = HostCallRouter::new(sink, Duration::from_millis(50));
        let err = router
            .call("slow.tool", &serde_json::json!({}))
            .await
            .expect_err("must time out");
        assert!(err.contains("timeout"));
        // 超时后 resolve 返回 false（已不在途）。
        assert!(!router.resolve(1, true, "{}".to_owned()));
    }

    #[tokio::test]
    async fn host_call_fail_all_unblocks() {
        let (tx, _rx) = std_mpsc::channel::<String>();
        struct Cb(std_mpsc::Sender<String>);
        impl EventCallback for Cb {
            fn on_event(&self, event_json: String) {
                let _ = self.0.send(event_json);
            }
        }
        let sink = EventSink::new(Arc::new(Cb(tx)));
        let router = HostCallRouter::new(sink, Duration::from_secs(60));
        let router_clone = router.clone();
        let task =
            tokio::spawn(async move { router_clone.call("stuck", &serde_json::json!({})).await });
        tokio::time::sleep(Duration::from_millis(30)).await;
        router.fail_all("shutdown");
        let err = task.await.unwrap().expect_err("fail_all unblocks");
        assert!(err.contains("shutdown"));
    }

    /// ask_user 豁免（ask-user r5 中断豁免对接点）：fail_all 不 fail 在途
    /// 的 ask_user（留给持久化恢复）；其余工具照旧。
    #[tokio::test]
    async fn host_call_fail_all_exempts_ask_user() {
        let (tx, _rx) = std_mpsc::channel::<String>();
        struct Cb(std_mpsc::Sender<String>);
        impl EventCallback for Cb {
            fn on_event(&self, event_json: String) {
                let _ = self.0.send(event_json);
            }
        }
        let sink = EventSink::new(Arc::new(Cb(tx)));
        let router = HostCallRouter::new(sink, Duration::from_secs(60));

        let ask_router = router.clone();
        let ask_task = tokio::spawn(async move {
            ask_router
                .call("ask_user", &serde_json::json!({"v": 1}))
                .await
        });
        let other_router = router.clone();
        let other_task = tokio::spawn(async move {
            other_router
                .call("stuck", &serde_json::json!({}))
                .await
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        router.fail_all("shutdown");

        // 普通工具被 fail 解栈。
        let err = other_task.await.unwrap().expect_err("普通工具被 fail");
        assert!(err.contains("shutdown"));
        // ask_user 未被 fail：调用仍在途（可继续 resolve）——问卷留给
        // 持久化恢复，迟到答案有处补全。
        let mut ask_task = ask_task;
        assert!(
            futures_poll_done(&mut ask_task).is_none(),
            "fail_all 后 ask_user future 不应立即完成"
        );
        // 在途证明：resolve 仍能唤醒它（真实场景由 runtime 收束 drop）。
        // 取信封 id（ask_user 是首个 allocate → id=1）。
        let resolved = router.resolve(1, true, r#"{"outcome":"declined"}"#.to_owned());
        assert!(resolved, "ask_user 仍可 resolve（未被 fail_all 杀掉）");
        let result = ask_task.await.unwrap().expect("resolve 后正常返回");
        assert_eq!(result["outcome"], "declined");
    }

    /// 辅助：future 是否已完成（不消费结果；None = 仍在等）。
    fn futures_poll_done(
        task: &mut tokio::task::JoinHandle<Result<Value, String>>,
    ) -> Option<Result<Result<Value, String>, tokio::task::JoinError>> {
        use std::future::Future;
        use std::pin::Pin;
        use std::task::{Context, Poll, Waker};
        struct noop_waker;
        impl std::task::Wake for noop_waker {
            fn wake(self: Arc<Self>) {}
        }
        let waker: Waker = Arc::new(noop_waker).into();
        let mut cx = Context::from_waker(&waker);
        // JoinHandle: Unpin —— 无需 unsafe 固定。
        match Pin::new(task).poll(&mut cx) {
            Poll::Ready(v) => Some(v),
            Poll::Pending => None,
        }
    }
}
