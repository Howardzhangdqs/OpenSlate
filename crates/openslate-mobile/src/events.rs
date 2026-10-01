//! 事件出口：`EventSink`（[`MsgSink`] 实现，单队列 + 泵线程 → 宿主回调）。
//!
//! 与 WS server 的 ConnectionHub 同语义不同拓扑：mobile 只有一个嵌入式
//! 客户端（宿主），所以单条 std mpsc 队列 + 一个专用泵线程逐条回调，
//! 天然保序（「同一连接内事件全序」），且 `broadcast` 永不阻塞引擎线程。
//!
//! 回调运行在泵线程上——宿主实现（UniFFI foreign callback）必须自行
//! 保证线程安全并尽快返回；重活应转投宿主自己的队列。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use openslate_protocol::ServerMsg;
use openslate_session::state::MsgSink;

/// mobile 嵌入式客户端的固定连接 id（单客户端；notice 定向用它）。
pub const MOBILE_CONN_ID: u64 = 1;

/// 宿主事件回调：每条事件 = 一个 JSON 对象（`ServerMsg` 序列化，或
/// `host_call_requested` / `host_call_result` 信封，见 hostcall.rs）。
pub trait EventCallback: Send + Sync {
    fn on_event(&self, event_json: String);
}

/// 事件出口（`Arc` 共享给 AppState / HostCallRouter / MobileRuntime）。
pub struct EventSink {
    /// `Some` = 泵线程存活；`None` = 已 close（close_all 后不再投递）。
    tx: Mutex<Option<std::sync::mpsc::Sender<String>>>,
    alive: Arc<AtomicBool>,
    /// 保活泵线程句柄（回调 drop 后线程自然退出）。
    _pump: std::thread::JoinHandle<()>,
}

impl EventSink {
    /// 创建 sink 并启动泵线程（线程名便于 adb logcat 排查）。
    pub fn new(callback: Arc<dyn EventCallback>) -> Arc<Self> {
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let alive = Arc::new(AtomicBool::new(true));
        let alive_flag = alive.clone();
        let pump = std::thread::Builder::new()
            .name("openslate-event-pump".to_owned())
            .spawn(move || {
                for event in rx {
                    callback.on_event(event);
                }
                alive_flag.store(false, Ordering::SeqCst);
            })
            .expect("spawn event pump thread");
        Arc::new(Self {
            tx: Mutex::new(Some(tx)),
            alive,
            _pump: pump,
        })
    }

    /// 非 JSON 的协议消息直接入队（host call 信封走这里）。
    /// 编码失败兜底为协议 error 信封（与 server 写任务同款语义）。
    pub fn emit_raw(&self, json: String) {
        let guard = self.tx.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(tx) = guard.as_ref() {
            let _ = tx.send(json);
        }
    }

    fn emit_msg(&self, msg: ServerMsg) {
        let json = serde_json::to_string(&msg).unwrap_or_else(|_| {
            serde_json::to_string(&ServerMsg::Error {
                code: "encode".into(),
                message: "消息编码失败".into(),
            })
            .unwrap_or_default()
        });
        self.emit_raw(json);
    }
}

impl MsgSink for EventSink {
    fn broadcast(&self, msg: ServerMsg) {
        self.emit_msg(msg);
    }

    fn send_to(&self, conn_id: u64, msg: ServerMsg) -> bool {
        if conn_id != MOBILE_CONN_ID {
            return false;
        }
        self.emit_msg(msg);
        true
    }

    fn conn_ids(&self) -> Vec<u64> {
        if self.count() > 0 {
            vec![MOBILE_CONN_ID]
        } else {
            Vec::new()
        }
    }

    fn count(&self) -> usize {
        usize::from(self.alive.load(Ordering::SeqCst))
    }

    fn close_all(&self) {
        // 丢掉 sender → 泵线程 drain 完余量后退出 → alive=false。
        // 已入队的事件仍会送达（优雅语义，与 WS hub 丢 sender 一致）。
        *self.tx.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
