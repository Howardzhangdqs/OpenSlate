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

/// 一条在途 host call 的应答通道。
type Pending = tokio::sync::oneshot::Sender<Result<Value, String>>;

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

    /// 发起一次 host call：发出 `host_call_requested` 信封并等待应答。
    pub async fn call(&self, tool: &str, args: &Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = tokio::sync::oneshot::channel::<Result<Value, String>>();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);

        let envelope = serde_json::json!({
            "type": "host_call_requested",
            "id": id,
            "tool": tool,
            "args": args,
        });
        self.sink.emit_raw(envelope.to_string());

        let result = match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(res)) => res,
            // 发送端 drop（fail_all / runtime 关停）。
            Ok(Err(_)) => Err("host call aborted (runtime shutting down)".to_owned()),
            Err(_) => {
                self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
                return Err(format!(
                    "host call timeout after {}ms (host did not resolve)",
                    self.timeout.as_millis()
                ));
            }
        };
        result
    }

    /// 宿主应答入口。`ok=true` 时 `payload` 是结果 JSON 文本；
    /// `ok=false` 时 `payload` 是错误说明。返回 false = 无此在途调用。
    pub fn resolve(&self, id: u64, ok: bool, payload: String) -> bool {
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
        match pending {
            Some(tx) => {
                let result = if ok {
                    serde_json::from_str(&payload).map_err(|e| {
                        format!("host returned invalid JSON payload: {e}")
                    })
                } else {
                    Err(payload)
                };
                tx.send(result).is_ok()
            }
            None => false,
        }
    }

    /// 关停兜底：让所有在途调用立刻失败（引擎解栈）。
    pub fn fail_all(&self, reason: &str) {
        let drained: Vec<Pending> = {
            let mut guard = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            guard.drain().map(|(_, v)| v).collect()
        };
        for tx in drained {
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
        }
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
        let duration_ms = started.elapsed().as_millis() as u64;
        match self.router.call(&self.name, args).await {
            Ok(value) => {
                let content = serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_owned());
                Ok(ToolOutput {
                    bytes: content.len(),
                    content,
                    duration_ms,
                    status: ToolOutputStatus::Success,
                })
            }
            Err(message) => Ok(ToolOutput {
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
        rx.recv_timeout(Duration::from_secs(2)).expect("event within 2s")
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
        let task = tokio::spawn(async move {
            router_clone.call("boom", &serde_json::json!({})).await
        });
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
        let task = tokio::spawn(async move {
            router_clone.call("stuck", &serde_json::json!({})).await
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        router.fail_all("shutdown");
        let err = task.await.unwrap().expect_err("fail_all unblocks");
        assert!(err.contains("shutdown"));
    }
}
