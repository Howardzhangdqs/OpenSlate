//! Query operations for [`SqliteStore`](crate::SqliteStore).
//!
//! Provides typed record structs and read-only query methods for all
//! OpenSlate SQLite tables.

use openslate_core::error::StoreError;
use serde::{Deserialize, Serialize};
use sqlx::{query_as, query_scalar};

use crate::store::SqliteStore;

// ---------------------------------------------------------------------------
// Record structs
// ---------------------------------------------------------------------------

/// A run record from the database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub id: String,
    pub title: Option<String>,
    pub root_agent_id: String,
    pub status: String,
    pub input_json: String,
    pub output_json: Option<String>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    /// Accumulated model spend in USD (P2-3); 0.0 when unpriced.
    pub cost_usd: f64,
}

/// An execution node record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionNodeRecord {
    pub id: String,
    pub run_id: String,
    pub agent_id: String,
    pub parent_execution_id: Option<String>,
    pub parent_call_id: Option<String>,
    pub status: String,
    pub input_json: String,
    pub output_json: Option<String>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
}

/// A step record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepRecord {
    pub id: String,
    pub run_id: String,
    pub execution_node_id: String,
    pub agent_id: String,
    pub kind: String,
    pub data_json: String,
    /// Per-run monotonic sequence number (writer-assigned). Conversation /
    /// step order is load-bearing; `started_at` alone cannot order
    /// same-batch inserts.
    pub seq: i64,
    pub started_at: i64,
    pub finished_at: Option<i64>,
}

/// A message record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageRecord {
    pub id: String,
    pub run_id: String,
    pub execution_node_id: String,
    pub agent_id: Option<String>,
    pub role: String,
    pub content_json: String,
    /// Per-run monotonic sequence number (writer-assigned). Readers order by
    /// it — `created_at` alone cannot order same-batch inserts, and a
    /// provider rejects shuffled `tool_calls`/tool-result pairs.
    pub seq: i64,
    pub created_at: i64,
}

/// A prompt snapshot record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptSnapshotRecord {
    pub id: String,
    pub run_id: String,
    pub execution_node_id: String,
    pub agent_id: String,
    pub profile_name: String,
    pub source_kind: String,
    pub source_path: Option<String>,
    pub content_hash: String,
    pub rendered_prompt: String,
    pub created_at: i64,
}

/// An audit log entry record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditLogRecord {
    pub id: String,
    pub run_id: Option<String>,
    pub agent_id: Option<String>,
    pub event_type: String,
    pub event_json: String,
    pub created_at: i64,
}

/// A trace event record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceEventRecord {
    pub id: String,
    pub run_id: String,
    pub execution_node_id: Option<String>,
    pub step_id: Option<String>,
    pub agent_id: Option<String>,
    pub event_name: String,
    pub event_kind: String,
    pub ts_ns: i64,
    pub dur_ns: Option<i64>,
    pub track: String,
    pub args_json: Option<String>,
}

// ---------------------------------------------------------------------------
// Row tuple types (column order must match SELECT *)
// ---------------------------------------------------------------------------

type RunRow = (
    String,         // id
    Option<String>, // title
    String,         // root_agent_id
    String,         // status
    String,         // input_json
    Option<String>, // output_json
    i64,            // started_at
    Option<i64>,    // finished_at
    f64,            // cost_usd
);

type ExecutionNodeRow = (
    String,         // id
    String,         // run_id
    String,         // agent_id
    Option<String>, // parent_execution_id
    Option<String>, // parent_call_id
    String,         // status
    String,         // input_json
    Option<String>, // output_json
    i64,            // started_at
    Option<i64>,    // finished_at
);

type StepRow = (
    String,      // id
    String,      // run_id
    String,      // execution_node_id
    String,      // agent_id
    String,      // kind
    String,      // data_json
    i64,         // seq
    i64,         // started_at
    Option<i64>, // finished_at
);

type MessageRow = (
    String,         // id
    String,         // run_id
    String,         // execution_node_id
    Option<String>, // agent_id
    String,         // role
    String,         // content_json
    i64,            // seq
    i64,            // created_at
);

type PromptSnapshotRow = (
    String,         // id
    String,         // run_id
    String,         // execution_node_id
    String,         // agent_id
    String,         // profile_name
    String,         // source_kind
    Option<String>, // source_path
    String,         // content_hash
    String,         // rendered_prompt
    i64,            // created_at
);

type AuditLogRow = (
    String,         // id
    Option<String>, // run_id
    Option<String>, // agent_id
    String,         // event_type
    String,         // event_json
    i64,            // created_at
);

type TraceEventRow = (
    String,         // id
    String,         // run_id
    Option<String>, // execution_node_id
    Option<String>, // step_id
    Option<String>, // agent_id
    String,         // event_name
    String,         // event_kind
    i64,            // ts_ns
    Option<i64>,    // dur_ns
    String,         // track
    Option<String>, // args_json
);

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

fn row_to_run(row: RunRow) -> RunRecord {
    RunRecord {
        id: row.0,
        title: row.1,
        root_agent_id: row.2,
        status: row.3,
        input_json: row.4,
        output_json: row.5,
        started_at: row.6,
        finished_at: row.7,
        cost_usd: row.8,
    }
}

fn row_to_execution_node(row: ExecutionNodeRow) -> ExecutionNodeRecord {
    ExecutionNodeRecord {
        id: row.0,
        run_id: row.1,
        agent_id: row.2,
        parent_execution_id: row.3,
        parent_call_id: row.4,
        status: row.5,
        input_json: row.6,
        output_json: row.7,
        started_at: row.8,
        finished_at: row.9,
    }
}

fn row_to_step(row: StepRow) -> StepRecord {
    StepRecord {
        id: row.0,
        run_id: row.1,
        execution_node_id: row.2,
        agent_id: row.3,
        kind: row.4,
        data_json: row.5,
        seq: row.6,
        started_at: row.7,
        finished_at: row.8,
    }
}

fn row_to_message(row: MessageRow) -> MessageRecord {
    MessageRecord {
        id: row.0,
        run_id: row.1,
        execution_node_id: row.2,
        agent_id: row.3,
        role: row.4,
        content_json: row.5,
        seq: row.6,
        created_at: row.7,
    }
}

fn row_to_prompt_snapshot(row: PromptSnapshotRow) -> PromptSnapshotRecord {
    PromptSnapshotRecord {
        id: row.0,
        run_id: row.1,
        execution_node_id: row.2,
        agent_id: row.3,
        profile_name: row.4,
        source_kind: row.5,
        source_path: row.6,
        content_hash: row.7,
        rendered_prompt: row.8,
        created_at: row.9,
    }
}

fn row_to_audit_log(row: AuditLogRow) -> AuditLogRecord {
    AuditLogRecord {
        id: row.0,
        run_id: row.1,
        agent_id: row.2,
        event_type: row.3,
        event_json: row.4,
        created_at: row.5,
    }
}

fn row_to_trace_event(row: TraceEventRow) -> TraceEventRecord {
    TraceEventRecord {
        id: row.0,
        run_id: row.1,
        execution_node_id: row.2,
        step_id: row.3,
        agent_id: row.4,
        event_name: row.5,
        event_kind: row.6,
        ts_ns: row.7,
        dur_ns: row.8,
        track: row.9,
        args_json: row.10,
    }
}

// ---------------------------------------------------------------------------
// Helper: map sqlx errors to StoreError
// ---------------------------------------------------------------------------

fn qerr(e: sqlx::Error) -> StoreError {
    StoreError::QueryError(e.to_string())
}

// ---------------------------------------------------------------------------
// Query implementations
// ---------------------------------------------------------------------------

impl SqliteStore {
    /// Fetch a single run by ID.
    pub async fn get_run(&self, id: &str) -> Result<Option<RunRecord>, StoreError> {
        let pool = self.pool();
        let row: Option<RunRow> = query_as(
            "SELECT id, title, root_agent_id, status, input_json, output_json, started_at, finished_at, cost_usd \
             FROM runs WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(qerr)?;

        Ok(row.map(row_to_run))
    }

    /// List runs ordered by most recently started, with pagination.
    ///
    /// 排序带 `id DESC` tiebreaker：`started_at` 是毫秒精度，同毫秒的
    /// run 若无 tiebreaker，`LIMIT`/`OFFSET` 翻页时可能串页（同一行在
    /// 相邻两页重复出现或被跳过）。
    pub async fn list_runs(&self, limit: i64, offset: i64) -> Result<Vec<RunRecord>, StoreError> {
        let pool = self.pool();
        let rows: Vec<RunRow> = query_as(
            "SELECT id, title, root_agent_id, status, input_json, output_json, started_at, finished_at, cost_usd \
             FROM runs ORDER BY started_at DESC, id DESC LIMIT ? OFFSET ?",
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await
        .map_err(qerr)?;

        Ok(rows.into_iter().map(row_to_run).collect())
    }

    /// Fetch a single execution node by ID.
    pub async fn get_execution_node(
        &self,
        id: &str,
    ) -> Result<Option<ExecutionNodeRecord>, StoreError> {
        let pool = self.pool();
        let row: Option<ExecutionNodeRow> = query_as(
            "SELECT id, run_id, agent_id, parent_execution_id, parent_call_id, \
                    status, input_json, output_json, started_at, finished_at \
             FROM execution_nodes WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(qerr)?;

        Ok(row.map(row_to_execution_node))
    }

    /// List all steps for a given run, ordered by `seq` (the writer-assigned
    /// per-run sequence; `started_at` alone cannot order same-batch inserts).
    pub async fn list_steps(&self, run_id: &str) -> Result<Vec<StepRecord>, StoreError> {
        let pool = self.pool();
        let rows: Vec<StepRow> = query_as(
            "SELECT id, run_id, execution_node_id, agent_id, kind, data_json, seq, started_at, finished_at \
             FROM steps WHERE run_id = ? ORDER BY seq",
        )
        .bind(run_id)
        .fetch_all(pool)
        .await
        .map_err(qerr)?;

        Ok(rows.into_iter().map(row_to_step).collect())
    }

    /// List all messages for a given execution node, ordered by `seq`.
    pub async fn list_messages(
        &self,
        execution_node_id: &str,
    ) -> Result<Vec<MessageRecord>, StoreError> {
        let pool = self.pool();
        let rows: Vec<MessageRow> = query_as(
            "SELECT id, run_id, execution_node_id, agent_id, role, content_json, seq, created_at \
             FROM messages WHERE execution_node_id = ? ORDER BY seq",
        )
        .bind(execution_node_id)
        .fetch_all(pool)
        .await
        .map_err(qerr)?;

        Ok(rows.into_iter().map(row_to_message).collect())
    }

    /// List the full conversation of a run (all execution nodes merged),
    /// ordered by `seq`. This is the resume path: the root conversation is
    /// rebuilt from it.
    ///
    /// **注意**：这是无上限的全量读取，仅 resume 场景（必须重建完整
    /// 历史）使用；UI / 列表场景请用 [`Self::list_messages_by_run_after_seq`]
    /// 做 keyset 分页。
    pub async fn list_messages_by_run(
        &self,
        run_id: &str,
    ) -> Result<Vec<MessageRecord>, StoreError> {
        let pool = self.pool();
        let rows: Vec<MessageRow> = query_as(
            "SELECT id, run_id, execution_node_id, agent_id, role, content_json, seq, created_at \
             FROM messages WHERE run_id = ? ORDER BY seq",
        )
        .bind(run_id)
        .fetch_all(pool)
        .await
        .map_err(qerr)?;

        Ok(rows.into_iter().map(row_to_message).collect())
    }

    /// Keyset 分页读取一个 run 的会话：返回 `seq > after_seq` 的最早
    /// `limit` 条（`ORDER BY seq ASC`）。
    ///
    /// UI / 列表场景应使用本方法而不是 [`Self::list_messages_by_run`]：
    /// 相比 OFFSET 分页，keyset 在数据增长时不会跳行/串页；`after_seq`
    /// 传 `0` 即从头发 `limit` 条，之后每页传上一页最后一条的 `seq`。
    pub async fn list_messages_by_run_after_seq(
        &self,
        run_id: &str,
        after_seq: i64,
        limit: i64,
    ) -> Result<Vec<MessageRecord>, StoreError> {
        let pool = self.pool();
        let rows: Vec<MessageRow> = query_as(
            "SELECT id, run_id, execution_node_id, agent_id, role, content_json, seq, created_at \
             FROM messages WHERE run_id = ? AND seq > ? ORDER BY seq ASC LIMIT ?",
        )
        .bind(run_id)
        .bind(after_seq)
        .bind(limit)
        .fetch_all(pool)
        .await
        .map_err(qerr)?;

        Ok(rows.into_iter().map(row_to_message).collect())
    }

    /// 一个 run 的消息总数（分页 UI 显示总量 / 测试用）。
    pub async fn count_messages_by_run(&self, run_id: &str) -> Result<i64, StoreError> {
        let pool = self.pool();
        let count: i64 = query_scalar("SELECT COUNT(*) FROM messages WHERE run_id = ?")
            .bind(run_id)
            .fetch_one(pool)
            .await
            .map_err(qerr)?;
        Ok(count)
    }

    /// Highest `seq` used by a run's messages (`0` when the run has none).
    /// Callers continuing a run start assigning at `max + 1`.
    pub async fn max_message_seq(&self, run_id: &str) -> Result<i64, StoreError> {
        let pool = self.pool();
        let max: i64 = query_scalar("SELECT COALESCE(MAX(seq), 0) FROM messages WHERE run_id = ?")
            .bind(run_id)
            .fetch_one(pool)
            .await
            .map_err(qerr)?;
        Ok(max)
    }

    /// Get the most recent prompt snapshot for a given run + agent.
    pub async fn get_prompt_snapshot(
        &self,
        run_id: &str,
        agent_id: &str,
    ) -> Result<Option<PromptSnapshotRecord>, StoreError> {
        let pool = self.pool();
        let row: Option<PromptSnapshotRow> = query_as(
            "SELECT id, run_id, execution_node_id, agent_id, profile_name, \
                    source_kind, source_path, content_hash, rendered_prompt, created_at \
             FROM prompt_snapshots \
             WHERE run_id = ? AND agent_id = ? \
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(run_id)
        .bind(agent_id)
        .fetch_optional(pool)
        .await
        .map_err(qerr)?;

        Ok(row.map(row_to_prompt_snapshot))
    }

    /// List all audit log entries for a given run, ordered by `created_at`.
    pub async fn list_audit_logs(&self, run_id: &str) -> Result<Vec<AuditLogRecord>, StoreError> {
        let pool = self.pool();
        let rows: Vec<AuditLogRow> = query_as(
            "SELECT id, run_id, agent_id, event_type, event_json, created_at \
             FROM audit_log WHERE run_id = ? ORDER BY created_at",
        )
        .bind(run_id)
        .fetch_all(pool)
        .await
        .map_err(qerr)?;

        Ok(rows.into_iter().map(row_to_audit_log).collect())
    }

    /// List all trace events for a given run, ordered by `ts_ns`.
    pub async fn list_trace_events(
        &self,
        run_id: &str,
    ) -> Result<Vec<TraceEventRecord>, StoreError> {
        let pool = self.pool();
        let rows: Vec<TraceEventRow> = query_as(
            "SELECT id, run_id, execution_node_id, step_id, agent_id, \
                    event_name, event_kind, ts_ns, dur_ns, track, args_json \
             FROM trace_events WHERE run_id = ? ORDER BY ts_ns",
        )
        .bind(run_id)
        .fetch_all(pool)
        .await
        .map_err(qerr)?;

        Ok(rows.into_iter().map(row_to_trace_event).collect())
    }

    /// Get the most recently started interrupted run
    /// （`id DESC` tiebreaker，与 [`Self::list_runs`] 的全序一致）。
    pub async fn get_last_interrupted_run(&self) -> Result<Option<RunRecord>, StoreError> {
        let pool = self.pool();
        let row: Option<RunRow> = query_as(
            "SELECT id, title, root_agent_id, status, input_json, output_json, started_at, finished_at, cost_usd \
             FROM runs WHERE status = 'interrupted' \
             ORDER BY started_at DESC, id DESC LIMIT 1",
        )
        .fetch_optional(pool)
        .await
        .map_err(qerr)?;

        Ok(row.map(row_to_run))
    }

    /// Get the most recently started resumable run: any run whose status is
    /// not `failed` (running = crashed/mid-session, interrupted, cancelled,
    /// completed REPL sessions). Failed runs are excluded — their transcript
    /// typically ends mid-error, and the operator asked to fail.
    /// （`id DESC` tiebreaker，与 [`Self::list_runs`] 的全序一致。）
    pub async fn get_last_resumable_run(&self) -> Result<Option<RunRecord>, StoreError> {
        let pool = self.pool();
        let row: Option<RunRow> = query_as(
            "SELECT id, title, root_agent_id, status, input_json, output_json, started_at, finished_at, cost_usd \
             FROM runs WHERE status != 'failed' \
             ORDER BY started_at DESC, id DESC LIMIT 1",
        )
        .fetch_optional(pool)
        .await
        .map_err(qerr)?;

        Ok(row.map(row_to_run))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::SqlitePool;

    async fn setup_store() -> SqliteStore {
        let store = SqliteStore::new_in_memory().await.expect("store created");
        store.run_migrations().await.expect("migrations run");
        store
    }

    async fn insert_run(
        pool: &SqlitePool,
        id: &str,
        title: Option<&str>,
        root_agent_id: &str,
        status: &str,
        input_json: &str,
        started_at: i64,
    ) {
        sqlx::query(
            "INSERT INTO runs (id, title, root_agent_id, status, input_json, started_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(title)
        .bind(root_agent_id)
        .bind(status)
        .bind(input_json)
        .bind(started_at)
        .execute(pool)
        .await
        .expect("insert run");
    }

    #[allow(clippy::too_many_arguments)] // test scaffold: row fields map 1:1
    async fn insert_execution_node(
        pool: &SqlitePool,
        id: &str,
        run_id: &str,
        agent_id: &str,
        parent_execution_id: Option<&str>,
        parent_call_id: Option<&str>,
        status: &str,
        input_json: &str,
        started_at: i64,
    ) {
        sqlx::query(
            "INSERT INTO execution_nodes \
             (id, run_id, agent_id, parent_execution_id, parent_call_id, status, input_json, started_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(run_id)
        .bind(agent_id)
        .bind(parent_execution_id)
        .bind(parent_call_id)
        .bind(status)
        .bind(input_json)
        .bind(started_at)
        .execute(pool)
        .await
        .expect("insert execution node");
    }

    #[tokio::test]
    async fn test_get_run_existing() {
        let store = setup_store().await;
        insert_run(
            store.pool(),
            "run-1",
            Some("Test Run"),
            "root-agent",
            "running",
            "{}",
            1000,
        )
        .await;

        let record = store.get_run("run-1").await.expect("query");
        assert!(record.is_some(), "should find the run");
        let r = record.unwrap();
        assert_eq!(r.id, "run-1");
        assert_eq!(r.title.as_deref(), Some("Test Run"));
        assert_eq!(r.root_agent_id, "root-agent");
        assert_eq!(r.status, "running");
        assert_eq!(r.input_json, "{}");
        assert!(r.output_json.is_none());
        assert_eq!(r.started_at, 1000);
        assert!(r.finished_at.is_none());
    }

    #[tokio::test]
    async fn test_get_run_nonexistent() {
        let store = setup_store().await;
        let record = store.get_run("no-such-run").await.expect("query");
        assert!(record.is_none(), "should return None for nonexistent run");
    }

    #[tokio::test]
    async fn test_list_runs_pagination() {
        let store = setup_store().await;
        let pool = store.pool();

        insert_run(pool, "run-a", None, "agent", "completed", "{}", 1000).await;
        insert_run(pool, "run-b", None, "agent", "completed", "{}", 2000).await;
        insert_run(pool, "run-c", None, "agent", "completed", "{}", 3000).await;

        // First page: 2 most recent
        let page1 = store.list_runs(2, 0).await.expect("page 1");
        assert_eq!(page1.len(), 2);
        // Ordered by started_at DESC
        assert_eq!(page1[0].id, "run-c");
        assert_eq!(page1[1].id, "run-b");

        // Second page: remaining 1
        let page2 = store.list_runs(2, 2).await.expect("page 2");
        assert_eq!(page2.len(), 1);
        assert_eq!(page2[0].id, "run-a");
    }

    #[tokio::test]
    async fn test_get_execution_node() {
        let store = setup_store().await;
        let pool = store.pool();

        insert_run(pool, "run-1", None, "root", "running", "{}", 1000).await;
        insert_execution_node(
            pool,
            "en-1",
            "run-1",
            "root",
            None,
            None,
            "running",
            r#"{"task":"do stuff"}"#,
            1100,
        )
        .await;

        let record = store.get_execution_node("en-1").await.expect("query");
        assert!(record.is_some());
        let en = record.unwrap();
        assert_eq!(en.id, "en-1");
        assert_eq!(en.run_id, "run-1");
        assert_eq!(en.agent_id, "root");
        assert!(en.parent_execution_id.is_none());
        assert!(en.parent_call_id.is_none());
        assert_eq!(en.status, "running");
        assert_eq!(en.input_json, r#"{"task":"do stuff"}"#);

        // Non-existent
        let missing = store.get_execution_node("en-999").await.expect("query");
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn test_list_steps() {
        let store = setup_store().await;
        let pool = store.pool();

        insert_run(pool, "run-1", None, "root", "running", "{}", 1000).await;
        insert_execution_node(
            pool, "en-1", "run-1", "root", None, None, "running", "{}", 1100,
        )
        .await;

        // Insert 2 steps (same started_at; seq is the order tiebreaker)
        sqlx::query(
            "INSERT INTO steps (id, run_id, execution_node_id, agent_id, kind, data_json, seq, started_at) \
             VALUES ('step-1', 'run-1', 'en-1', 'root', 'model_call', '{\"model\":\"gpt-4\"}', 1, 1200)",
        )
        .execute(pool)
        .await
        .expect("insert step 1");

        sqlx::query(
            "INSERT INTO steps (id, run_id, execution_node_id, agent_id, kind, data_json, seq, started_at) \
             VALUES ('step-2', 'run-1', 'en-1', 'root', 'tool_call', '{\"tool\":\"bash\"}', 2, 1200)",
        )
        .execute(pool)
        .await
        .expect("insert step 2");

        let steps = store.list_steps("run-1").await.expect("list_steps");
        assert_eq!(steps.len(), 2);
        // Ordered by seq (started_at is identical here)
        assert_eq!(steps[0].id, "step-1");
        assert_eq!(steps[0].kind, "model_call");
        assert_eq!(steps[0].seq, 1);
        assert_eq!(steps[1].id, "step-2");
        assert_eq!(steps[1].kind, "tool_call");
        assert_eq!(steps[1].seq, 2);
    }

    #[tokio::test]
    async fn test_list_messages() {
        let store = setup_store().await;
        let pool = store.pool();

        insert_run(pool, "run-1", None, "root", "running", "{}", 1000).await;
        insert_execution_node(
            pool, "en-1", "run-1", "root", None, None, "running", "{}", 1100,
        )
        .await;

        // Same created_at on purpose: seq must be the order tiebreaker.
        sqlx::query(
            "INSERT INTO messages (id, run_id, execution_node_id, agent_id, role, content_json, seq, created_at) \
             VALUES ('msg-1', 'run-1', 'en-1', 'root', 'user', '\"hello\"', 1, 1200)",
        )
        .execute(pool)
        .await
        .expect("insert msg 1");

        sqlx::query(
            "INSERT INTO messages (id, run_id, execution_node_id, agent_id, role, content_json, seq, created_at) \
             VALUES ('msg-2', 'run-1', 'en-1', 'root', 'assistant', '\"world\"', 2, 1200)",
        )
        .execute(pool)
        .await
        .expect("insert msg 2");

        let msgs = store.list_messages("en-1").await.expect("list_messages");
        assert_eq!(msgs.len(), 2);
        // Ordered by seq (created_at is identical here)
        assert_eq!(msgs[0].id, "msg-1");
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[0].seq, 1);
        assert_eq!(msgs[1].id, "msg-2");
        assert_eq!(msgs[1].role, "assistant");
        assert_eq!(msgs[1].seq, 2);
    }

    #[tokio::test]
    async fn test_list_messages_by_run_orders_across_nodes_by_seq() {
        let store = setup_store().await;
        let pool = store.pool();

        insert_run(pool, "run-1", None, "root", "running", "{}", 1000).await;
        insert_execution_node(
            pool, "en-1", "run-1", "root", None, None, "running", "{}", 1100,
        )
        .await;
        insert_execution_node(
            pool,
            "en-2",
            "run-1",
            "child",
            Some("en-1"),
            None,
            "running",
            "{}",
            1150,
        )
        .await;

        // Interleaved across nodes, all sharing one timestamp; only seq
        // restores the true conversation order (a shuffled tool_call/tool
        // pair would be rejected by providers on resume).
        sqlx::query(
            "INSERT INTO messages (id, run_id, execution_node_id, agent_id, role, content_json, seq, created_at) \
             VALUES ('m-1', 'run-1', 'en-1', 'root', 'user', '\"q\"', 1, 1200)",
        )
        .execute(pool)
        .await
        .expect("insert m-1");
        sqlx::query(
            "INSERT INTO messages (id, run_id, execution_node_id, agent_id, role, content_json, seq, created_at) \
             VALUES ('m-2', 'run-1', 'en-2', 'child', 'assistant', '\"a\"', 2, 1200)",
        )
        .execute(pool)
        .await
        .expect("insert m-2");
        sqlx::query(
            "INSERT INTO messages (id, run_id, execution_node_id, agent_id, role, content_json, seq, created_at) \
             VALUES ('m-3', 'run-1', 'en-1', 'root', 'tool', '\"t\"', 3, 1200)",
        )
        .execute(pool)
        .await
        .expect("insert m-3");

        let msgs = store
            .list_messages_by_run("run-1")
            .await
            .expect("list_messages_by_run");
        let ids: Vec<&str> = msgs.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["m-1", "m-2", "m-3"]);

        // Other runs' messages are excluded.
        insert_run(pool, "run-2", None, "root", "running", "{}", 2000).await;
        insert_execution_node(
            pool, "en-3", "run-2", "root", None, None, "running", "{}", 2100,
        )
        .await;
        sqlx::query(
            "INSERT INTO messages (id, run_id, execution_node_id, agent_id, role, content_json, seq, created_at) \
             VALUES ('m-9', 'run-2', 'en-3', 'root', 'user', '\"other\"', 1, 2200)",
        )
        .execute(pool)
        .await
        .expect("insert m-9");
        let msgs = store
            .list_messages_by_run("run-1")
            .await
            .expect("list again");
        assert_eq!(msgs.len(), 3, "run-2 messages must not leak in");
    }

    #[tokio::test]
    async fn test_max_message_seq() {
        let store = setup_store().await;
        let pool = store.pool();

        insert_run(pool, "run-1", None, "root", "running", "{}", 1000).await;
        insert_execution_node(
            pool, "en-1", "run-1", "root", None, None, "running", "{}", 1100,
        )
        .await;

        assert_eq!(
            store.max_message_seq("run-1").await.expect("max seq"),
            0,
            "no messages yet → 0"
        );

        for (i, id) in ["m-1", "m-2", "m-3"].into_iter().enumerate() {
            sqlx::query(
                "INSERT INTO messages (id, run_id, execution_node_id, agent_id, role, content_json, seq, created_at) \
                 VALUES (?, 'run-1', 'en-1', 'root', 'user', '\"x\"', ?, 1200)",
            )
            .bind(id)
            .bind((i + 1) as i64)
            .execute(pool)
            .await
            .expect("insert");
        }

        assert_eq!(
            store.max_message_seq("run-1").await.expect("max seq"),
            3,
            "continuing the run must start assigning at 4"
        );
    }

    #[tokio::test]
    async fn test_get_prompt_snapshot() {
        let store = setup_store().await;
        let pool = store.pool();

        insert_run(pool, "run-1", None, "root", "running", "{}", 1000).await;
        insert_execution_node(
            pool, "en-1", "run-1", "root", None, None, "running", "{}", 1100,
        )
        .await;

        sqlx::query(
            "INSERT INTO prompt_snapshots \
             (id, run_id, execution_node_id, agent_id, profile_name, source_kind, content_hash, rendered_prompt, created_at) \
             VALUES ('ps-1', 'run-1', 'en-1', 'root', 'default', 'file', 'abc123', 'Hello {{task}}', 1200)",
        )
        .execute(pool)
        .await
        .expect("insert prompt snapshot");

        let snap = store
            .get_prompt_snapshot("run-1", "root")
            .await
            .expect("get_prompt_snapshot");
        assert!(snap.is_some());
        let s = snap.unwrap();
        assert_eq!(s.id, "ps-1");
        assert_eq!(s.profile_name, "default");
        assert_eq!(s.source_kind, "file");
        assert!(s.source_path.is_none());
        assert_eq!(s.content_hash, "abc123");
        assert_eq!(s.rendered_prompt, "Hello {{task}}");

        // Different agent → None
        let missing = store
            .get_prompt_snapshot("run-1", "other-agent")
            .await
            .expect("query");
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn test_list_trace_events() {
        let store = setup_store().await;
        let pool = store.pool();

        insert_run(pool, "run-1", None, "root", "running", "{}", 1000).await;

        for (i, ts) in [5000u64, 6000, 7000].into_iter().enumerate() {
            sqlx::query(
                "INSERT INTO trace_events \
                 (id, run_id, event_name, event_kind, ts_ns, track) \
                 VALUES (?, 'run-1', ?, 'span', ?, 'main')",
            )
            .bind(format!("te-{}", i + 1))
            .bind(format!("event-{i}"))
            .bind(ts as i64)
            .execute(pool)
            .await
            .expect("insert trace event");
        }

        let events = store
            .list_trace_events("run-1")
            .await
            .expect("list_trace_events");
        assert_eq!(events.len(), 3);
        // Ordered by ts_ns
        assert_eq!(events[0].id, "te-1");
        assert_eq!(events[0].ts_ns, 5000);
        assert_eq!(events[1].id, "te-2");
        assert_eq!(events[1].ts_ns, 6000);
        assert_eq!(events[2].id, "te-3");
        assert_eq!(events[2].ts_ns, 7000);
    }

    #[tokio::test]
    async fn test_get_last_interrupted_run() {
        let store = setup_store().await;
        let pool = store.pool();

        insert_run(pool, "run-ok", None, "root", "completed", "{}", 1000).await;
        insert_run(pool, "run-int1", None, "root", "interrupted", "{}", 2000).await;
        insert_run(pool, "run-int2", None, "root", "interrupted", "{}", 3000).await;

        let result = store.get_last_interrupted_run().await.expect("query");
        assert!(result.is_some());
        let r = result.unwrap();
        // Should return the most recently started interrupted run
        assert_eq!(r.id, "run-int2");
        assert_eq!(r.status, "interrupted");
    }

    #[tokio::test]
    async fn test_get_last_resumable_run() {
        let store = setup_store().await;
        let pool = store.pool();

        // Failed runs are never resumable; everything else is.
        insert_run(pool, "run-failed", None, "root", "failed", "{}", 1000).await;
        insert_run(pool, "run-done", None, "root", "completed", "{}", 2000).await;
        insert_run(pool, "run-crashed", None, "root", "running", "{}", 3000).await;

        let result = store.get_last_resumable_run().await.expect("query");
        assert!(result.is_some());
        assert_eq!(result.unwrap().id, "run-crashed");
    }

    #[tokio::test]
    async fn test_get_last_resumable_run_skips_failed() {
        let store = setup_store().await;
        let pool = store.pool();

        insert_run(pool, "run-int", None, "root", "interrupted", "{}", 1000).await;
        insert_run(pool, "run-failed", None, "root", "failed", "{}", 2000).await;

        let result = store.get_last_resumable_run().await.expect("query");
        assert!(result.is_some());
        assert_eq!(
            result.unwrap().id,
            "run-int",
            "a newer failed run must not shadow the older resumable one"
        );
    }

    #[tokio::test]
    async fn test_get_last_resumable_run_empty() {
        let store = setup_store().await;
        let result = store.get_last_resumable_run().await.expect("query");
        assert!(result.is_none());
    }

    /// 同毫秒 started_at：list_runs 必须以 id DESC 决定顺序，翻页不串页。
    #[tokio::test]
    async fn test_list_runs_tiebreaks_on_id_when_same_started_at() {
        let store = setup_store().await;
        let pool = store.pool();

        insert_run(pool, "run-a", None, "root", "completed", "{}", 5000).await;
        insert_run(pool, "run-b", None, "root", "completed", "{}", 5000).await;
        insert_run(pool, "run-c", None, "root", "completed", "{}", 5000).await;

        let page1 = store.list_runs(2, 0).await.expect("page 1");
        assert_eq!(
            page1.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["run-c", "run-b"],
            "同毫秒时按 id DESC 排序"
        );

        let page2 = store.list_runs(2, 2).await.expect("page 2");
        assert_eq!(
            page2.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["run-a"],
            "tiebreaker 保证第二页不与第一页重复/漏行"
        );
    }

    /// keyset 分页：after_seq 正确切页，且与其他 run 的消息隔离。
    #[tokio::test]
    async fn test_list_messages_by_run_after_seq_keyset_pagination() {
        let store = setup_store().await;
        let pool = store.pool();

        insert_run(pool, "run-1", None, "root", "running", "{}", 1000).await;
        insert_execution_node(
            pool, "en-1", "run-1", "root", None, None, "running", "{}", 1100,
        )
        .await;
        // 另一个 run 的消息（不能泄漏进 run-1 的分页结果）
        insert_run(pool, "run-2", None, "root", "running", "{}", 2000).await;
        insert_execution_node(
            pool, "en-2", "run-2", "root", None, None, "running", "{}", 2100,
        )
        .await;

        for (i, seq) in (1..=5).enumerate() {
            sqlx::query(
                "INSERT INTO messages (id, run_id, execution_node_id, agent_id, role, content_json, seq, created_at) \
                 VALUES (?, 'run-1', 'en-1', 'root', 'user', '\"x\"', ?, 1200)",
            )
            .bind(format!("m-{i}"))
            .bind(seq)
            .execute(pool)
            .await
            .expect("insert run-1 message");
        }
        sqlx::query(
            "INSERT INTO messages (id, run_id, execution_node_id, agent_id, role, content_json, seq, created_at) \
             VALUES ('other-1', 'run-2', 'en-2', 'root', 'user', '\"y\"', 1, 2200)",
        )
        .execute(pool)
        .await
        .expect("insert run-2 message");

        // 第一页：after_seq=0, limit=2 → seq 1,2
        let page1 = store
            .list_messages_by_run_after_seq("run-1", 0, 2)
            .await
            .expect("page 1");
        assert_eq!(
            page1.iter().map(|m| m.seq).collect::<Vec<_>>(),
            vec![1, 2]
        );

        // 第二页：after_seq=2, limit=2 → seq 3,4
        let page2 = store
            .list_messages_by_run_after_seq("run-1", 2, 2)
            .await
            .expect("page 2");
        assert_eq!(
            page2.iter().map(|m| m.seq).collect::<Vec<_>>(),
            vec![3, 4]
        );

        // 第三页：after_seq=4, limit=2 → 仅 seq 5
        let page3 = store
            .list_messages_by_run_after_seq("run-1", 4, 2)
            .await
            .expect("page 3");
        assert_eq!(page3.iter().map(|m| m.seq).collect::<Vec<_>>(), vec![5]);

        // 尾后为空
        let empty = store
            .list_messages_by_run_after_seq("run-1", 5, 2)
            .await
            .expect("after last");
        assert!(empty.is_empty());

        // 按 id 升序稳定（seq 升序）
        assert_eq!(page1[0].id, "m-0");
        assert_eq!(page1[1].id, "m-1");
    }

    #[tokio::test]
    async fn test_count_messages_by_run() {
        let store = setup_store().await;
        let pool = store.pool();

        insert_run(pool, "run-1", None, "root", "running", "{}", 1000).await;
        insert_execution_node(
            pool, "en-1", "run-1", "root", None, None, "running", "{}", 1100,
        )
        .await;

        assert_eq!(
            store.count_messages_by_run("run-1").await.expect("count"),
            0,
            "无消息时为 0"
        );

        for i in 0..3 {
            sqlx::query(
                "INSERT INTO messages (id, run_id, execution_node_id, agent_id, role, content_json, seq, created_at) \
                 VALUES (?, 'run-1', 'en-1', 'root', 'user', '\"x\"', ?, 1200)",
            )
            .bind(format!("cm-{i}"))
            .bind((i + 1) as i64)
            .execute(pool)
            .await
            .expect("insert");
        }

        assert_eq!(
            store.count_messages_by_run("run-1").await.expect("count"),
            3
        );
        assert_eq!(
            store.count_messages_by_run("no-such-run").await.expect("count"),
            0
        );
    }

    #[tokio::test]
    async fn test_list_audit_logs() {
        let store = setup_store().await;

        store
            .insert_audit_event(
                "audit-1",
                Some("run-1"),
                Some("agent-a"),
                "tool_approved",
                r#"{"tool":"bash","details":{"approved":true}}"#,
                1000,
            )
            .await
            .expect("insert audit 1");
        store
            .insert_audit_event(
                "audit-2",
                Some("run-1"),
                Some("agent-a"),
                "tool_denied",
                r#"{"tool":"rm","details":{"reason":"dangerous"}}"#,
                2000,
            )
            .await
            .expect("insert audit 2");
        store
            .insert_audit_event(
                "audit-3",
                Some("run-2"),
                None,
                "tool_executed",
                r#"{"tool":"ls"}"#,
                3000,
            )
            .await
            .expect("insert audit 3");

        let logs_run1 = store
            .list_audit_logs("run-1")
            .await
            .expect("list_audit_logs run-1");
        assert_eq!(logs_run1.len(), 2);
        assert_eq!(logs_run1[0].event_type, "tool_approved");
        assert_eq!(logs_run1[1].event_type, "tool_denied");

        let logs_run2 = store
            .list_audit_logs("run-2")
            .await
            .expect("list_audit_logs run-2");
        assert_eq!(logs_run2.len(), 1);
        assert_eq!(logs_run2[0].event_type, "tool_executed");

        let logs_run3 = store
            .list_audit_logs("run-3")
            .await
            .expect("list_audit_logs run-3");
        assert_eq!(logs_run3.len(), 0);
    }
}
