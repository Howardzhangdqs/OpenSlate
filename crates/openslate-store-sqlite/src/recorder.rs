//! RunRecorder — per-run incremental persistence adapter (Phase 3).
//!
//! Bridges the core runtime's [`MessageSink`] callback to the SQLite store:
//! every assistant message and tool result is written the moment it is
//! appended to the live conversation ("落盘后再 continue"), so a crash,
//! cancellation, or session exit always leaves a resumable partial
//! transcript behind.
//!
//! Lifecycle:
//! - [`RunRecorder::begin`] inserts the run row (`status = "running"`) and a
//!   root execution node upfront — the run exists in the store before any
//!   LLM call, and every terminal path (completed / interrupted / failed)
//!   updates it via [`RunRecorder::finish`] / [`RunRecorder::finish_with`].
//! - [`RunRecorder::resume`] adopts an existing run (continuing `seq` from
//!   `max_message_seq`), used by `run --resume <id>` and REPL `/resume`.
//! - [`RunRecorder::load_messages`] reads a run's conversation back in `seq`
//!   order and decodes it into core [`Message`]s (full fidelity, including
//!   `tool_calls` pairing). A trailing assistant message with unanswered
//!   `tool_calls` (crash between the assistant turn and its tool results) is
//!   dropped — providers reject dangling `tool_calls`.
//!
//! 存储可靠性（v5 设计 §5）：消息持久化失败不再"warn 后丢弃"——
//! [`Self::write_message_with_meta`] 先退避重试（100/250/500ms × 3），
//! 穷尽后推入**进程内挂起队列**并 `tracing::error`；每次 append / finish
//! 前先冲刷队列。边界：队列仅内存（进程崩溃仍丢——彻底解决需要 WAL 外
//! journal，超出本期），UI 侧经 [`Self::pending_persisted_writes`] 感知。
//!
//! per-message 元数据（v5 设计 §2）：assistant 消息的 usage /
//! finish_reason / latency / model_id 经 `meta_json` 旁路列落库（不进
//! `Message` 本体——provider 回放路径零泄漏）。core `MessageSink` trait
//! 的 `append_with_meta` 扩展与下方 [`RunRecorder`] 的 override 均已
//! 接线（L2）：runtime 捕获点构造 [`MessageMeta`] 随消息交付，本 impl
//! 转写入 `meta_json` 列。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Mutex;

use openslate_core::error::StoreError;
use openslate_core::runtime::MessageSink;
use openslate_core::types::{Message, MessageMeta, MessageRole, RunId};

use crate::store::SqliteStore;

/// append 失败的退避间隔（ms）：初始尝试 + 以下 3 次重试（设计 §5）。
const APPEND_RETRY_BACKOFF_MS: [u64; 3] = [100, 250, 500];

/// run 终态聚合（v5 设计 §2）。[`RunRecorder::finish_with`] 的入参形状；
/// `input_tokens` / `output_tokens` 为 run 级累加（无 usage 采集时传 0）。
#[derive(Debug, Clone)]
pub struct RunFinal {
    /// 小写词汇（"completed" / "interrupted" / "cancelled" / "failed"）。
    pub status: String,
    /// 可选 output 载荷；`None` 保持库中已有值。
    pub output_json: Option<String>,
    /// run 累计模型开销（USD）；未配置定价传 `0.0`。
    pub cost_usd: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Per-step persistence sink + run lifecycle recorder (see module docs).
pub struct RunRecorder {
    store: SqliteStore,
    run_id: RunId,
    /// Deterministic execution node the recorder's messages hang off. The
    /// runtime's own execution tree (with `en-{uuid}` ids) is persisted
    /// separately post-run; this node only needs to satisfy the messages FK.
    execution_node_id: String,
    agent_id: String,
    /// Last `seq` handed out; the next message gets `last_seq + 1`. Atomic so
    /// the sink (invoked from inside the runtime loop) and CLI-side writes
    /// (e.g. the turn's user message) share one monotonic counter.
    last_seq: AtomicI64,
    /// resume 过的 recorder 是否已把 run 置回 `running`。
    ///
    /// `resume()` 本身不改 status（保持 interrupted，避免会话再次中断时
    /// 产生新的僵尸 running 记录）；真正有新消息写入（run 确实被继续）
    /// 时才由首次 [`write_message`](Self::write_message) 激活为 running。
    /// `begin()` 创建即已 running，初始为 true。
    activated: AtomicBool,
    /// 挂起队列（设计 §5）：重试穷尽后的 `(Message, Option<MessageMeta>)`
    /// 兜底，下一次 append / finish 前冲刷。仅进程内——崩溃仍丢。
    pending: Mutex<VecDeque<(Message, Option<MessageMeta>)>>,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl std::fmt::Debug for RunRecorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunRecorder")
            .field("run_id", &self.run_id)
            .field("execution_node_id", &self.execution_node_id)
            .field("agent_id", &self.agent_id)
            .field("last_seq", &self.last_seq)
            .finish_non_exhaustive()
    }
}

fn role_str(role: &MessageRole) -> &'static str {
    match role {
        MessageRole::System => "system",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "tool",
    }
}

impl RunRecorder {
    /// Deterministic execution node id for a run's persisted messages.
    fn persist_node_id(run_id: &RunId) -> String {
        format!("{run_id}-persist")
    }

    /// Begin recording a fresh run: 在**单事务**内插入 run 行
    /// （`status = "running"`）与 recorder 的执行节点，然后由调用方驱动
    /// run。合并为单事务是为了消除两条独立语句之间的崩溃窗口。
    pub async fn begin(
        store: SqliteStore,
        run_id: RunId,
        root_agent_id: &str,
        title: Option<&str>,
        input_json: &str,
    ) -> Result<Self, StoreError> {
        let ts = now_ms();
        let node_id = Self::persist_node_id(&run_id);
        store
            .insert_run_with_persist_node(&run_id.0, title, root_agent_id, "running", input_json, ts, &node_id)
            .await?;
        Ok(Self {
            store,
            run_id,
            execution_node_id: node_id,
            agent_id: root_agent_id.to_owned(),
            last_seq: AtomicI64::new(0),
            activated: AtomicBool::new(true),
            pending: Mutex::new(VecDeque::new()),
        })
    }

    /// Adopt an existing run for continuation: the run row must already
    /// exist; the recorder's execution node is created on demand (idempotent
    /// across repeated resumes), and `seq` continues from the highest value
    /// already stored.
    ///
    /// **不修改 status**：run 保持原状态（通常为 interrupted）。避免
    /// resume 后尚未写入任何消息就再次中断时，留下新的僵尸 `running`
    /// 记录。真正继续会话时（首次写入消息），run 会被置回 running。
    pub async fn resume(
        store: SqliteStore,
        run_id: RunId,
        root_agent_id: &str,
    ) -> Result<Self, StoreError> {
        let run = store
            .get_run(&run_id.0)
            .await?
            .ok_or_else(|| StoreError::QueryError(format!("run '{}' not found", run_id.0)))?;

        let node_id = Self::persist_node_id(&run_id);
        if store.get_execution_node(&node_id).await?.is_none() {
            store
                .insert_execution_node(
                    &node_id,
                    &run_id.0,
                    root_agent_id,
                    None,
                    None,
                    "running",
                    "{}",
                    now_ms(),
                )
                .await?;
        }

        let last_seq = store.max_message_seq(&run_id.0).await?;
        Ok(Self {
            store,
            run_id,
            execution_node_id: node_id,
            // Prefer the run's recorded root agent; fall back to the caller's
            // (tree may have changed between sessions).
            agent_id: run.root_agent_id,
            last_seq: AtomicI64::new(last_seq),
            activated: AtomicBool::new(false),
            pending: Mutex::new(VecDeque::new()),
        })
    }

    /// The run this recorder writes to.
    pub fn run_id(&self) -> &RunId {
        &self.run_id
    }

    /// Persist one message (with optional per-message metadata) with the next
    /// `seq`, applying the v5 reliability policy (设计 §5):
    ///
    /// 1. 先冲刷挂起队列（前序失败的消息优先补写，保持顺序）；
    /// 2. 单次尝试失败 → 退避重试（100/250/500ms × 3）；
    /// 3. 穷尽 → `(Message, meta)` 推入进程内挂起队列 + `tracing::error`，
    ///    同时把最后的错误返回给调用方（sink 侧吞掉；fallible 调用方
    ///    可上浮——入队保证稍后仍会补写，不会丢）。
    ///
    /// meta 语义：`Some(&MessageMeta)` 仅 assistant 消息（usage /
    /// finish_reason / latency / model_id → `meta_json` 列）；ask_user /
    /// 初始 user 消息等无 meta 写入点传 `None`。
    ///
    /// resume 过的 recorder 在首次写入时把 run 置回 `running`
    /// （一次性，`AtomicBool` 去重）。
    pub async fn write_message_with_meta(
        &self,
        message: &Message,
        meta: Option<&MessageMeta>,
    ) -> Result<(), StoreError> {
        self.flush_pending().await;
        match self.persist_with_retry(message, meta).await {
            Ok(()) => Ok(()),
            Err(e) => {
                if let Ok(mut queue) = self.pending.lock() {
                    queue.push_back((message.clone(), meta.cloned()));
                }
                tracing::error!(
                    target: "openslate_store",
                    "run {}: message persist failed after {} retries, queued for later flush \
                     (pending={}): {}",
                    self.run_id.0,
                    APPEND_RETRY_BACKOFF_MS.len(),
                    self.pending_persisted_writes(),
                    e
                );
                Err(e)
            }
        }
    }

    /// Persist one message without metadata — delegates to
    /// [`Self::write_message_with_meta`] with `meta = None`（兼容全部既有
    /// 调用方：CLI 初始 user 消息、ask_user 补写等）。
    pub async fn write_message(&self, message: &Message) -> Result<(), StoreError> {
        self.write_message_with_meta(message, None).await
    }

    /// 挂起队列长度（UI 上报通道：>0 时 mobile 层发"存储写入失败，历史
    /// 可能不完整"横幅，设计 §5）。
    pub fn pending_persisted_writes(&self) -> usize {
        self.pending.lock().map(|q| q.len()).unwrap_or(0)
    }

    /// 单次持久化尝试：serde → zstd 压缩 → insert。seq 每次尝试重新分配
    /// （失败尝试烧掉的 seq 留下空洞，无害——单调唯一即可）。
    async fn persist_once(
        &self,
        message: &Message,
        meta: Option<&MessageMeta>,
    ) -> Result<(), StoreError> {
        if !self.activated.swap(true, Ordering::SeqCst) {
            self.store
                .activate_run(&self.run_id.0)
                .await?;
        }
        let seq = self.last_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let content_json = serde_json::to_string(message)
            .map_err(|e| StoreError::WriteError(format!("message encode failed: {e}")))?;
        let content_zst = crate::codec::encode_content(&content_json);
        let meta_json = match meta {
            Some(m) => Some(
                serde_json::to_string(m)
                    .map_err(|e| StoreError::WriteError(format!("message meta encode failed: {e}")))?,
            ),
            None => None,
        };
        let id = format!("msg-{}-{seq}", self.run_id.0);
        self.store
            .insert_message(
                &id,
                &self.run_id.0,
                &self.execution_node_id,
                Some(&self.agent_id),
                role_str(&message.role),
                &content_zst,
                meta_json.as_deref(),
                seq,
                now_ms(),
            )
            .await
    }

    /// 初始尝试 + 3 次退避重试；每次失败 warn（含下一次间隔）。
    async fn persist_with_retry(
        &self,
        message: &Message,
        meta: Option<&MessageMeta>,
    ) -> Result<(), StoreError> {
        let mut attempt = 0usize;
        loop {
            match self.persist_once(message, meta).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    let Some(&delay) = APPEND_RETRY_BACKOFF_MS.get(attempt) else {
                        return Err(e);
                    };
                    attempt += 1;
                    tracing::warn!(
                        target: "openslate_store",
                        "run {}: message persist attempt failed ({}), retrying in {delay}ms",
                        self.run_id.0,
                        e
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }
            }
        }
    }

    /// 冲刷挂起队列：逐条（队首优先，保持顺序）重试补写；某条再次穷尽
    /// 则保留队列原样并停止（当前消息走自己的重试路径）。锁只在取队首
    /// / 弹出时短暂持有，绝不跨 await。
    async fn flush_pending(&self) {
        loop {
            let front = match self.pending.lock() {
                Ok(queue) => match queue.front() {
                    Some((message, meta)) => Some((message.clone(), meta.clone())),
                    None => None,
                },
                Err(_) => return, // 锁中毒：放弃本次冲刷（下次再试）
            };
            let Some((message, meta)) = front else {
                return;
            };
            match self.persist_with_retry(&message, meta.as_ref()).await {
                Ok(()) => {
                    let _ = self
                        .pending
                        .lock()
                        .map(|mut queue| queue.pop_front());
                }
                Err(e) => {
                    tracing::error!(
                        target: "openslate_store",
                        "run {}: pending message flush failed again, kept queued: {}",
                        self.run_id.0,
                        e
                    );
                    return;
                }
            }
        }
    }

    /// Record the run's terminal state（v5 形态）：`RunFinal` 全字段
    /// （status / output / finished_at / cost / token 聚合）在**单事务**
    /// 内落库；写入前先冲刷挂起队列。见 [`RunFinal`]。
    pub async fn finish_with(&self, final_state: RunFinal) -> Result<(), StoreError> {
        self.flush_pending().await;
        self.store
            .finish_run(
                &self.run_id.0,
                &final_state.status,
                final_state.output_json.as_deref(),
                now_ms(),
                final_state.cost_usd,
                final_state.input_tokens,
                final_state.output_tokens,
            )
            .await
    }

    /// Record the run's terminal state（旧签名兼容包装）：委托
    /// [`Self::finish_with`]，token 聚合记 0（存量调用方——run_manager /
    /// session / mobile——编译不动；迁移到 `RunFinal` 归 L2/L4）。
    ///
    /// status / output / finished_at / cost 四项仍单事务落库；
    /// `output_json` 为 `None` 时保持原值。
    pub async fn finish(
        &self,
        status: &str,
        output_json: Option<&str>,
        cost_usd: f64,
    ) -> Result<(), StoreError> {
        self.finish_with(RunFinal {
            status: status.to_owned(),
            output_json: output_json.map(str::to_owned),
            cost_usd,
            input_tokens: 0,
            output_tokens: 0,
        })
        .await
    }

    /// Load a run's persisted conversation in `seq` order, decoded back into
    /// core [`Message`]s. A trailing assistant message whose `tool_calls`
    /// have no tool results yet (crash mid-step) is dropped: providers
    /// reject dangling `tool_calls`, and the model simply re-decides.
    ///
    /// 丢弃动作显性化（设计 §5）：`tracing::warn` 携带 run_id 与被丢弃
    /// 消息的 id（`msg-{run}-{seq}` 定位）。
    pub async fn load_messages(
        store: &SqliteStore,
        run_id: &str,
    ) -> Result<Vec<Message>, StoreError> {
        let records = store.list_messages_by_run(run_id).await?;
        let trailing_id = records.last().map(|r| r.id.clone());
        let mut messages = Self::decode_records(records)?;

        if matches!(
            messages.last(),
            Some(m) if m.role == MessageRole::Assistant
                && m.tool_calls.as_ref().is_some_and(|tcs| !tcs.is_empty())
        ) {
            messages.pop();
            tracing::warn!(
                target: "openslate_store",
                "run {run_id}: dropped trailing assistant message '{}' with unanswered \
                 tool_calls (crash mid-step); the model will re-decide",
                trailing_id.as_deref().unwrap_or("<unknown>")
            );
        }
        Ok(messages)
    }

    /// [`Self::load_messages`] 的保留悬挂变体：**不**丢弃尾部带未答
    /// `tool_calls` 的 assistant 消息。
    ///
    /// ask_user 重启恢复（ask-user 设计 r5 迟到等价不变量）依赖这条
    /// 通路：悬挂的 ask_user tool call 必须留在历史里，迟到答案才能经
    /// `append_late_tool_result` 补全该调用的 tool result 并续跑（丢掉
    /// 即无处补全）。调用方（mobile 装配层）负责在无需恢复时自行执行
    /// 与 `load_messages` 相同的丢弃，或悬挂修复（superseded/expired）。
    pub async fn load_messages_keep_dangling(
        store: &SqliteStore,
        run_id: &str,
    ) -> Result<Vec<Message>, StoreError> {
        let records = store.list_messages_by_run(run_id).await?;
        Self::decode_records(records)
    }

    /// 压缩记录 → core Message（解压在 query 侧完成，这里只做 JSON
    /// 反序列化；失败报带消息 id 的错误）。
    fn decode_records(
        records: Vec<crate::query::MessageRecord>,
    ) -> Result<Vec<Message>, StoreError> {
        records
            .into_iter()
            .map(|record| {
                serde_json::from_str(&record.content_json).map_err(|e| {
                    StoreError::QueryError(format!(
                        "run '{}' message '{}' failed to decode: {e}",
                        record.run_id, record.id
                    ))
                })
            })
            .collect()
    }
}

#[async_trait::async_trait]
impl MessageSink for RunRecorder {
    async fn append(&self, message: &Message) {
        // Persistence must never kill the run. write_message_with_meta 已做
        // 重试 + 入队 + error 记录；这里仅吞掉返回值。
        let _ = self.write_message_with_meta(message, None).await;
    }

    /// v5 §2 旁路通道接线（L2）：assistant 消息的采集元数据经
    /// `meta_json` 列落库。吞错风格与 [`Self::append`] 一致——
    /// 持久化失败绝不向 runtime 传播（重试/入队已在底层处理）。
    async fn append_with_meta(&self, message: &Message, meta: &MessageMeta) {
        let _ = self.write_message_with_meta(message, Some(meta)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openslate_core::types::{ToolCall, ToolCallId};

    async fn setup() -> SqliteStore {
        let store = SqliteStore::new_in_memory().await.expect("store");
        store.run_migrations().await.expect("migrations");
        store
    }

    fn user_msg(content: &str) -> Message {
        Message {
            attachments: None,
            role: MessageRole::User,
            content: content.to_owned(),
            reasoning_content: None,
            tool_call_id: None,
            name: None,
            tool_calls: None,
        }
    }

    fn assistant_with_tool_calls() -> Message {
        Message {
        attachments: None,
        reasoning_content: None,
            role: MessageRole::Assistant,
            content: String::new(),
            tool_call_id: None,
            name: None,
            tool_calls: Some(vec![ToolCall {
                id: ToolCallId("tc-1".into()),
                name: "shell".into(),
                arguments: serde_json::json!({"command": "ls"}),
            }]),
        }
    }

    fn tool_result() -> Message {
        Message {
            attachments: None,
            role: MessageRole::Tool,
            content: "file_a\nfile_b".into(),
            tool_call_id: Some(ToolCallId("tc-1".into())),
            name: Some("shell".into()),
            tool_calls: None,
            reasoning_content: None,
        }
    }

    #[tokio::test]
    async fn begin_inserts_running_run_and_node() {
        let store = setup().await;
        RunRecorder::begin(
            store.clone(),
            RunId("r1".into()),
            "root",
            Some("title"),
            r#"{"prompt":"hi"}"#,
        )
        .await
        .expect("begin");

        let run = store.get_run("r1").await.expect("get").expect("exists");
        assert_eq!(run.status, "running");
        assert_eq!(run.root_agent_id, "root");
        assert_eq!(run.title.as_deref(), Some("title"));
        assert!(run.finished_at.is_none());

        let node = store
            .get_execution_node("r1-persist")
            .await
            .expect("get")
            .expect("node exists");
        assert_eq!(node.run_id, "r1");
    }

    #[tokio::test]
    async fn finish_updates_status_and_output() {
        let store = setup().await;
        let rec = RunRecorder::begin(store.clone(), RunId("r1".into()), "root", None, "{}")
            .await
            .expect("begin");

        rec.finish("failed", Some(r#"{"error":"boom"}"#), 0.0)
            .await
            .expect("finish");

        let run = store.get_run("r1").await.expect("get").expect("exists");
        assert_eq!(run.status, "failed");
        assert_eq!(run.output_json.as_deref(), Some(r#"{"error":"boom"}"#));
        assert!(run.finished_at.is_some(), "failure path must be on disk");
    }

    #[tokio::test]
    async fn finish_persists_cost_usd() {
        let store = setup().await;
        let rec = RunRecorder::begin(store.clone(), RunId("r1".into()), "root", None, "{}")
            .await
            .expect("begin");

        // Fresh run rows default to 0.
        let run = store.get_run("r1").await.expect("get").expect("exists");
        assert_eq!(run.cost_usd, 0.0);

        rec.finish("completed", None, 0.0125).await.expect("finish");

        let run = store.get_run("r1").await.expect("get").expect("exists");
        assert_eq!(run.status, "completed");
        assert!(
            (run.cost_usd - 0.0125f64).abs() < 1e-12,
            "cost must survive the store round trip, got {}",
            run.cost_usd
        );
    }

    #[tokio::test]
    async fn round_trip_preserves_tool_call_pairing() {
        let store = setup().await;
        let rec = RunRecorder::begin(store.clone(), RunId("r1".into()), "root", None, "{}")
            .await
            .expect("begin");

        // Same millisecond in practice — seq must keep the order stable.
        for m in [
            user_msg("list the files"),
            assistant_with_tool_calls(),
            tool_result(),
            Message {
                attachments: None,
                role: MessageRole::Assistant,
                content: "done".into(),
                tool_call_id: None,
                name: None,
                tool_calls: None,
                reasoning_content: None,
            },
        ] {
            rec.write_message(&m).await.expect("write");
        }

        let loaded = RunRecorder::load_messages(&store, "r1")
            .await
            .expect("load");
        assert_eq!(loaded.len(), 4);
        assert_eq!(loaded[0].role, MessageRole::User);
        assert_eq!(loaded[0].content, "list the files");
        assert_eq!(loaded[1].role, MessageRole::Assistant);
        let tcs = loaded[1].tool_calls.as_ref().expect("tool_calls survive");
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0].id, ToolCallId("tc-1".into()));
        assert_eq!(tcs[0].name, "shell");
        assert_eq!(tcs[0].arguments, serde_json::json!({"command": "ls"}));
        assert_eq!(loaded[2].role, MessageRole::Tool);
        assert_eq!(
            loaded[2].tool_call_id,
            Some(ToolCallId("tc-1".into())),
            "tool result ↔ tool_call pairing must survive the round trip"
        );
        assert_eq!(loaded[2].name.as_deref(), Some("shell"));
        assert_eq!(loaded[3].content, "done");
    }

    #[tokio::test]
    async fn load_drops_trailing_dangling_tool_calls() {
        let store = setup().await;
        let rec = RunRecorder::begin(store.clone(), RunId("r1".into()), "root", None, "{}")
            .await
            .expect("begin");

        // Crash between the assistant's tool_calls and its tool result.
        rec.write_message(&user_msg("go"))
            .await
            .expect("write user");
        rec.write_message(&assistant_with_tool_calls())
            .await
            .expect("write dangling assistant");

        let loaded = RunRecorder::load_messages(&store, "r1")
            .await
            .expect("load");
        assert_eq!(
            loaded.len(),
            1,
            "dangling tool_calls assistant must be dropped"
        );
        assert_eq!(loaded[0].role, MessageRole::User);
    }

    #[tokio::test]
    async fn interrupted_run_resumes_with_partial_transcript() {
        let store = setup().await;
        let run_id = RunId("r1".into());
        let rec = RunRecorder::begin(store.clone(), run_id.clone(), "root", None, "{}")
            .await
            .expect("begin");

        // Partial transcript of an interrupted run: user + assistant(tool_calls)
        // + tool result, but no final assistant answer.
        rec.write_message(&user_msg("go")).await.expect("u");
        rec.write_message(&assistant_with_tool_calls())
            .await
            .expect("a");
        rec.write_message(&tool_result()).await.expect("t");
        rec.finish("interrupted", None, 0.0).await.expect("finish");

        // Resume path: the interrupted run is discoverable, adoptable, and
        // the partial tool transcript comes back in order.
        let resumable = store
            .get_last_resumable_run()
            .await
            .expect("query")
            .expect("interrupted run is resumable");
        assert_eq!(resumable.id, "r1");

        let rec2 = RunRecorder::resume(store.clone(), run_id, "root")
            .await
            .expect("resume");
        let loaded = RunRecorder::load_messages(&store, "r1")
            .await
            .expect("load");
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[2].role, MessageRole::Tool);

        // New messages continue the seq sequence (no clobbering).
        rec2.write_message(&user_msg("continue"))
            .await
            .expect("write after resume");
        let loaded = RunRecorder::load_messages(&store, "r1")
            .await
            .expect("load");
        assert_eq!(loaded.len(), 4);
        assert_eq!(loaded[3].content, "continue");
        assert_eq!(
            store.max_message_seq("r1").await.expect("max"),
            4,
            "seq must continue monotonically across resume"
        );
    }

    #[tokio::test]
    async fn resume_missing_run_fails() {
        let store = setup().await;
        let err = RunRecorder::resume(store, RunId("nope".into()), "root")
            .await
            .expect_err("must fail");
        assert!(err.to_string().contains("not found"), "got: {err}");
    }

    #[tokio::test]
    async fn resume_is_idempotent_across_repeated_adoptions() {
        let store = setup().await;
        let run_id = RunId("r1".into());
        RunRecorder::begin(store.clone(), run_id.clone(), "root", None, "{}")
            .await
            .expect("begin")
            .write_message(&user_msg("hi"))
            .await
            .expect("write");

        // Adopt twice in a row (crash → resume → crash → resume).
        for i in 0..2 {
            let rec = RunRecorder::resume(store.clone(), run_id.clone(), "root")
                .await
                .unwrap_or_else(|_| panic!("resume #{i}"));
            rec.write_message(&user_msg(&format!("again-{i}")))
                .await
                .expect("write");
        }

        let loaded = RunRecorder::load_messages(&store, "r1")
            .await
            .expect("load");
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[2].content, "again-1");
    }

    /// resume 不改 status（保持 interrupted），首次写入消息才置回
    /// running —— 避免产生新的僵尸 running 记录。
    #[tokio::test]
    async fn resume_keeps_interrupted_until_first_message() {
        let store = setup().await;
        let run_id = RunId("r1".into());
        let rec = RunRecorder::begin(store.clone(), run_id.clone(), "root", None, "{}")
            .await
            .expect("begin");
        rec.write_message(&user_msg("go")).await.expect("write");
        rec.finish("interrupted", None, 0.0).await.expect("finish");

        // resume 本身不动 status
        let rec2 = RunRecorder::resume(store.clone(), run_id.clone(), "root")
            .await
            .expect("resume");
        let run = store.get_run("r1").await.expect("get").expect("exists");
        assert_eq!(
            run.status, "interrupted",
            "resume 不应立即把 run 置回 running（否则中断会产生新僵尸记录）"
        );

        // 首条消息写入 → running
        rec2.write_message(&user_msg("continue"))
            .await
            .expect("write");
        let run = store.get_run("r1").await.expect("get").expect("exists");
        assert_eq!(run.status, "running", "首次写入消息后 run 应为 running");

        // begin 创建的 recorder 初始即已激活，写入不产生额外状态变化
        let rec3 = RunRecorder::begin(store.clone(), RunId("r2".into()), "root", None, "{}")
            .await
            .expect("begin r2");
        rec3.write_message(&user_msg("fresh")).await.expect("write");
        let run2 = store.get_run("r2").await.expect("get").expect("exists");
        assert_eq!(run2.status, "running");
    }

    /// finish 单事务：status / output / finished_at / cost 同时落库；
    /// output_json=None 时保留已有值。
    #[tokio::test]
    async fn finish_writes_status_output_finished_and_cost_atomically() {
        let store = setup().await;
        let rec = RunRecorder::begin(store.clone(), RunId("r1".into()), "root", None, "{}")
            .await
            .expect("begin");

        // 先落一个带 output 的终态
        rec.finish("completed", Some(r#"{"out":1}"#), 0.5)
            .await
            .expect("finish 1");
        let run = store.get_run("r1").await.expect("get").expect("exists");
        assert_eq!(run.status, "completed");
        assert_eq!(run.output_json.as_deref(), Some(r#"{"out":1}"#));
        assert!(run.finished_at.is_some());
        assert!((run.cost_usd - 0.5).abs() < 1e-12);

        // 再 finish（interrupted、无 output、不同 cost）：output 必须保留
        rec.finish("interrupted", None, 0.75)
            .await
            .expect("finish 2");
        let run = store.get_run("r1").await.expect("get").expect("exists");
        assert_eq!(run.status, "interrupted");
        assert_eq!(
            run.output_json.as_deref(),
            Some(r#"{"out":1}"#),
            "finish(None) 不应清空已有 output"
        );
        assert!(run.finished_at.is_some());
        assert!(
            (run.cost_usd - 0.75).abs() < 1e-12,
            "cost 应为最新值，got {}",
            run.cost_usd
        );
    }

    #[tokio::test]
    async fn sink_impl_swallows_errors_without_panicking() {
        // A sink whose run row was deleted underneath it must not propagate
        // the failure into the runtime loop.
        let store = setup().await;
        let rec = RunRecorder::begin(
            store.clone(),
            RunId("will-vanish".into()),
            "root",
            None,
            "{}",
        )
        .await
        .expect("begin");
        sqlx::query("DELETE FROM messages")
            .execute(store.pool())
            .await
            .expect("clear messages");
        sqlx::query("DELETE FROM execution_nodes")
            .execute(store.pool())
            .await
            .expect("clear nodes");
        sqlx::query("DELETE FROM runs")
            .execute(store.pool())
            .await
            .expect("clear runs");

        // FK violation inside the sink → 重试穷尽 → 入队 + error 日志；
        // sink 永不向 runtime 传播（不 panic / 不 abort）。
        rec.append(&user_msg("ghost")).await;
        assert_eq!(
            rec.pending_persisted_writes(),
            1,
            "重试穷尽后消息应进入挂起队列"
        );
    }

    /// v5 §5 全链路：FK 失败 → 重试穷尽入队；库恢复后下一次 append 先
    /// 冲刷队列（顺序保持），两条消息都落盘。
    #[tokio::test]
    async fn sink_failure_queues_then_flushes_on_next_append() {
        let store = setup().await;
        let rec = RunRecorder::begin(
            store.clone(),
            RunId("r-flush".into()),
            "root",
            None,
            "{}",
        )
        .await
        .expect("begin");

        // 掏空 run 行（FK 必然失败）
        sqlx::query("DELETE FROM messages")
            .execute(store.pool())
            .await
            .expect("clear messages");
        sqlx::query("DELETE FROM execution_nodes")
            .execute(store.pool())
            .await
            .expect("clear nodes");
        sqlx::query("DELETE FROM runs")
            .execute(store.pool())
            .await
            .expect("clear runs");

        rec.append(&user_msg("lost-then-queued")).await;
        assert_eq!(rec.pending_persisted_writes(), 1);

        // 库恢复（run + persist 节点重建）
        store
            .insert_run("r-flush", None, "root", "running", "{}", now_ms_like())
            .await
            .expect("recreate run");
        store
            .insert_execution_node(
                "r-flush-persist",
                "r-flush",
                "root",
                None,
                None,
                "running",
                "{}",
                now_ms_like(),
            )
            .await
            .expect("recreate node");

        // 下一次 append：先冲刷队列，再写当前消息
        rec.append(&user_msg("fresh")).await;
        assert_eq!(rec.pending_persisted_writes(), 0, "队列应被冲刷");

        let loaded = RunRecorder::load_messages_keep_dangling(&store, "r-flush")
            .await
            .expect("load");
        assert_eq!(loaded.len(), 2, "排队消息 + 新消息都落盘");
        assert_eq!(loaded[0].content, "lost-then-queued", "队列消息先落（顺序保持）");
        assert_eq!(loaded[1].content, "fresh");
    }

    fn now_ms_like() -> i64 {
        42
    }

    /// meta 通道写读往返：write_message_with_meta 的 meta 进 meta_json，
    /// Message 本体不混入 usage（provider 回放零泄漏）。
    #[tokio::test]
    async fn write_read_roundtrip_with_meta() {
        let store = setup().await;
        let rec = RunRecorder::begin(store.clone(), RunId("r-meta".into()), "root", None, "{}")
            .await
            .expect("begin");

        let meta = MessageMeta {
            model_id: Some("glm-4.7-flash".into()),
            usage: Some(openslate_core::types::Usage {
                input_tokens: 101,
                output_tokens: 202,
                cached_input_tokens: None,
                reasoning_tokens: None,
                image_tokens: None,
            }),
            finish_reason: Some("tool_calls".into()),
            latency_ms: Some(3_500),
        };
        rec.write_message(&user_msg("hi")).await.expect("no meta");
        rec.write_message_with_meta(&assistant_with_tool_calls(), Some(&meta))
            .await
            .expect("with meta");

        let records = store.list_messages_by_run("r-meta").await.expect("read");
        assert_eq!(records.len(), 2);
        assert!(records[0].meta_json.is_none(), "user 行无 meta");
        let stored: MessageMeta =
            serde_json::from_str(records[1].meta_json.as_deref().expect("assistant 行有 meta"))
                .expect("meta 反序列化");
        assert_eq!(stored.model_id.as_deref(), Some("glm-4.7-flash"));
        assert_eq!(stored.usage.expect("usage").input_tokens, 101);
        assert_eq!(stored.usage.expect("usage").output_tokens, 202);
        assert_eq!(stored.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(stored.latency_ms, Some(3_500));

        // Message 本体（content）不含 meta 字段——provider 回放路径零泄漏
        let messages = RunRecorder::load_messages_keep_dangling(&store, "r-meta")
            .await
            .expect("load");
        let raw = serde_json::to_value(&messages[1]).expect("serialize");
        assert!(
            raw.get("usage").is_none() && raw.get("finish_reason").is_none(),
            "Message wire 形状不携带 meta"
        );
    }

    /// RunFinal 两签名：finish_with 落 token 聚合；旧 finish 委托 tokens=0。
    #[tokio::test]
    async fn finish_with_runfinal_and_legacy_delegation() {
        let store = setup().await;
        let rec = RunRecorder::begin(store.clone(), RunId("r-final".into()), "root", None, "{}")
            .await
            .expect("begin");
        rec.finish_with(RunFinal {
            status: "completed".into(),
            output_json: Some(r#"{"out":1}"#.into()),
            cost_usd: 0.25,
            input_tokens: 1111,
            output_tokens: 2222,
        })
        .await
        .expect("finish_with");

        let run = store.get_run("r-final").await.expect("get").expect("row");
        assert_eq!(run.status, "completed");
        assert_eq!(run.input_tokens, 1111);
        assert_eq!(run.output_tokens, 2222);
        assert!((run.cost_usd - 0.25).abs() < 1e-12);

        // 旧签名委托：tokens 记 0（存量 run 语义）
        let rec2 = RunRecorder::begin(store.clone(), RunId("r-legacy".into()), "root", None, "{}")
            .await
            .expect("begin 2");
        rec2.finish("interrupted", None, 0.0).await.expect("legacy finish");
        let run2 = store.get_run("r-legacy").await.expect("get").expect("row");
        assert_eq!(run2.status, "interrupted");
        assert_eq!(run2.input_tokens, 0, "旧签名委托 tokens=0");
        assert_eq!(run2.output_tokens, 0);
    }
}
