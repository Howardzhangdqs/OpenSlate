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
//!   updates it via [`RunRecorder::finish`].
//! - [`RunRecorder::resume`] adopts an existing run (continuing `seq` from
//!   `max_message_seq`), used by `run --resume <id>` and REPL `/resume`.
//! - [`RunRecorder::load_messages`] reads a run's conversation back in `seq`
//!   order and decodes it into core [`Message`]s (full fidelity, including
//!   `tool_calls` pairing). A trailing assistant message with unanswered
//!   `tool_calls` (crash between the assistant turn and its tool results) is
//!   dropped — providers reject dangling `tool_calls`.

use std::sync::atomic::{AtomicI64, Ordering};

use openslate_core::error::StoreError;
use openslate_core::runtime::MessageSink;
use openslate_core::types::{Message, MessageRole, RunId};

use crate::store::SqliteStore;

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

    /// Begin recording a fresh run: insert the run row (`status = "running"`)
    /// and the recorder's execution node, then the caller drives the run.
    pub async fn begin(
        store: SqliteStore,
        run_id: RunId,
        root_agent_id: &str,
        title: Option<&str>,
        input_json: &str,
    ) -> Result<Self, StoreError> {
        let ts = now_ms();
        store
            .insert_run(&run_id.0, title, root_agent_id, "running", input_json, ts)
            .await?;
        let node_id = Self::persist_node_id(&run_id);
        store
            .insert_execution_node(
                &node_id,
                &run_id.0,
                root_agent_id,
                None,
                None,
                "running",
                "{}",
                ts,
            )
            .await?;
        Ok(Self {
            store,
            run_id,
            execution_node_id: node_id,
            agent_id: root_agent_id.to_owned(),
            last_seq: AtomicI64::new(0),
        })
    }

    /// Adopt an existing run for continuation: the run row must already
    /// exist; the recorder's execution node is created on demand (idempotent
    /// across repeated resumes), and `seq` continues from the highest value
    /// already stored.
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
        })
    }

    /// The run this recorder writes to.
    pub fn run_id(&self) -> &RunId {
        &self.run_id
    }

    /// Persist one message with the next `seq`. Fallible twin of the
    /// [`MessageSink::append`] impl — callers that want to surface errors
    /// (e.g. writing the initial user message) use this directly.
    pub async fn write_message(&self, message: &Message) -> Result<(), StoreError> {
        let seq = self.last_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let content_json = serde_json::to_string(message)
            .map_err(|e| StoreError::WriteError(format!("message encode failed: {e}")))?;
        let id = format!("msg-{}-{seq}", self.run_id.0);
        self.store
            .insert_message(
                &id,
                &self.run_id.0,
                &self.execution_node_id,
                Some(&self.agent_id),
                role_str(&message.role),
                &content_json,
                seq,
                now_ms(),
            )
            .await
    }

    /// Record the run's terminal state (status / optional output payload /
    /// finish timestamp / accumulated cost in USD). `status` uses the
    /// store's lowercase vocabulary ("completed", "interrupted",
    /// "cancelled", "failed"). `cost_usd` is the run's model spend (P2-3);
    /// pass `0.0` when no pricing is configured.
    pub async fn finish(
        &self,
        status: &str,
        output_json: Option<&str>,
        cost_usd: f64,
    ) -> Result<(), StoreError> {
        self.store
            .update_run_status(&self.run_id.0, status, output_json, Some(now_ms()))
            .await?;
        self.store.update_run_cost(&self.run_id.0, cost_usd).await
    }

    /// Load a run's persisted conversation in `seq` order, decoded back into
    /// core [`Message`]s. A trailing assistant message whose `tool_calls`
    /// have no tool results yet (crash mid-step) is dropped: providers
    /// reject dangling `tool_calls`, and the model simply re-decides.
    pub async fn load_messages(
        store: &SqliteStore,
        run_id: &str,
    ) -> Result<Vec<Message>, StoreError> {
        let records = store.list_messages_by_run(run_id).await?;
        let mut messages = Vec::with_capacity(records.len());
        for record in records {
            let message: Message = serde_json::from_str(&record.content_json).map_err(|e| {
                StoreError::QueryError(format!(
                    "run '{}' message '{}' failed to decode: {e}",
                    record.run_id, record.id
                ))
            })?;
            messages.push(message);
        }
        if matches!(
            messages.last(),
            Some(m) if m.role == MessageRole::Assistant
                && m.tool_calls.as_ref().is_some_and(|tcs| !tcs.is_empty())
        ) {
            messages.pop();
            tracing::info!(
                target: "openslate_store",
                "run {run_id}: dropped trailing assistant message with unanswered \
                 tool_calls (crash mid-step); the model will re-decide"
            );
        }
        Ok(messages)
    }
}

#[async_trait::async_trait]
impl MessageSink for RunRecorder {
    async fn append(&self, message: &Message) {
        // Persistence must never kill the run: warn and continue.
        if let Err(e) = self.write_message(message).await {
            tracing::warn!(
                target: "openslate_store",
                "failed to persist message for run {}: {}",
                self.run_id.0,
                e
            );
        }
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
            role: MessageRole::User,
            content: content.to_owned(),
            tool_call_id: None,
            name: None,
            tool_calls: None,
        }
    }

    fn assistant_with_tool_calls() -> Message {
        Message {
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
            role: MessageRole::Tool,
            content: "file_a\nfile_b".into(),
            tool_call_id: Some(ToolCallId("tc-1".into())),
            name: Some("shell".into()),
            tool_calls: None,
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
                role: MessageRole::Assistant,
                content: "done".into(),
                tool_call_id: None,
                name: None,
                tool_calls: None,
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

        // FK violation inside the sink → warn, not panic/abort.
        rec.append(&user_msg("ghost")).await;
    }
}
