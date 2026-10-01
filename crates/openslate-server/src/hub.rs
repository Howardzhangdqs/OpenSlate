//! WS 连接 fan-out 集线器（[`MsgSink`] 的 WS 传输实现）。
//!
//! 每连接一个 unbounded 队列 + 一个写任务，队列天然给出「同一连接内
//! 事件全序」。会话核心只见 [`MsgSink`]（openslate-session），本模块
//! 是 server 侧的传输实现。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use openslate_session::state::MsgSink;

/// WS 连接 fan-out 集线器：每连接一个 unbounded 队列 + 一个写任务，
/// 队列天然给出「同一连接内事件全序」（spec §3 末尾）。
pub struct ConnectionHub {
    next_id: AtomicU64,
    conns:
        Mutex<HashMap<u64, tokio::sync::mpsc::UnboundedSender<Arc<openslate_protocol::ServerMsg>>>>,
}

impl Default for ConnectionHub {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectionHub {
    pub fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            conns: Mutex::new(HashMap::new()),
        }
    }

    /// 注册一条连接，返回连接 id。调用方必须在 `SessionCore` 锁内完成
    /// 注册 + 首条 snapshot 入队（见 ws.rs 的连接握手）。
    pub fn register(
        &self,
        tx: tokio::sync::mpsc::UnboundedSender<Arc<openslate_protocol::ServerMsg>>,
    ) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.conns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);
        id
    }

    pub fn unregister(&self, id: u64) {
        self.conns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
    }
}

impl MsgSink for ConnectionHub {
    /// 广播：遍历发送，失败（写任务已退出）即剔除。非阻塞。
    fn broadcast(&self, msg: openslate_protocol::ServerMsg) {
        let shared = Arc::new(msg);
        let mut conns = self.conns.lock().unwrap_or_else(|e| e.into_inner());
        conns.retain(|_, tx| tx.send(Arc::clone(&shared)).is_ok());
    }

    /// 定向发送（notice / snapshot）。返回 false = 连接已不在。
    fn send_to(&self, id: u64, msg: openslate_protocol::ServerMsg) -> bool {
        let conns = self.conns.lock().unwrap_or_else(|e| e.into_inner());
        match conns.get(&id) {
            Some(tx) => tx.send(Arc::new(msg)).is_ok(),
            None => false,
        }
    }

    /// 当前连接 id 快照（/new 后逐连接补发 snapshot 用）。
    fn conn_ids(&self) -> Vec<u64> {
        self.conns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .copied()
            .collect()
    }

    fn count(&self) -> usize {
        self.conns.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// 关闭所有连接（优雅停机：丢弃 sender → 写任务发 Close 帧退出）。
    fn close_all(&self) {
        self.conns.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// 传输侧下沉（连接注册/退订是 WS 特有操作）。
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
