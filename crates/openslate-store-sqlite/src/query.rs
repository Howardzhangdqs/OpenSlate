//! Query operations for [`SqliteStore`](crate::SqliteStore).
//!
//! Provides typed record structs and read-only query methods for all
//! OpenSlate SQLite tables.

use openslate_core::error::StoreError;
use serde::{Deserialize, Serialize};
use sqlx::query_as;

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
    String,             // id
    Option<String>,     // title
    String,             // root_agent_id
    String,             // status
    String,             // input_json
    Option<String>,     // output_json
    i64,                // started_at
    Option<i64>,        // finished_at
);

type ExecutionNodeRow = (
    String,             // id
    String,             // run_id
    String,             // agent_id
    Option<String>,     // parent_execution_id
    Option<String>,     // parent_call_id
    String,             // status
    String,             // input_json
    Option<String>,     // output_json
    i64,                // started_at
    Option<i64>,        // finished_at
);

type StepRow = (
    String,             // id
    String,             // run_id
    String,             // execution_node_id
    String,             // agent_id
    String,             // kind
    String,             // data_json
    i64,                // started_at
    Option<i64>,        // finished_at
);

type MessageRow = (
    String,             // id
    String,             // run_id
    String,             // execution_node_id
    Option<String>,     // agent_id
    String,             // role
    String,             // content_json
    i64,                // created_at
);

type PromptSnapshotRow = (
    String,             // id
    String,             // run_id
    String,             // execution_node_id
    String,             // agent_id
    String,             // profile_name
    String,             // source_kind
    Option<String>,     // source_path
    String,             // content_hash
    String,             // rendered_prompt
    i64,                // created_at
);

type TraceEventRow = (
    String,             // id
    String,             // run_id
    Option<String>,     // execution_node_id
    Option<String>,     // step_id
    Option<String>,     // agent_id
    String,             // event_name
    String,             // event_kind
    i64,                // ts_ns
    Option<i64>,        // dur_ns
    String,             // track
    Option<String>,     // args_json
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
        started_at: row.6,
        finished_at: row.7,
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
        created_at: row.6,
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
            "SELECT id, title, root_agent_id, status, input_json, output_json, started_at, finished_at \
             FROM runs WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(qerr)?;

        Ok(row.map(row_to_run))
    }

    /// List runs ordered by most recently started, with pagination.
    pub async fn list_runs(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<RunRecord>, StoreError> {
        let pool = self.pool();
        let rows: Vec<RunRow> = query_as(
            "SELECT id, title, root_agent_id, status, input_json, output_json, started_at, finished_at \
             FROM runs ORDER BY started_at DESC LIMIT ? OFFSET ?",
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

    /// List all steps for a given run, ordered by `started_at`.
    pub async fn list_steps(&self, run_id: &str) -> Result<Vec<StepRecord>, StoreError> {
        let pool = self.pool();
        let rows: Vec<StepRow> = query_as(
            "SELECT id, run_id, execution_node_id, agent_id, kind, data_json, started_at, finished_at \
             FROM steps WHERE run_id = ? ORDER BY started_at",
        )
        .bind(run_id)
        .fetch_all(pool)
        .await
        .map_err(qerr)?;

        Ok(rows.into_iter().map(row_to_step).collect())
    }

    /// List all messages for a given execution node, ordered by `created_at`.
    pub async fn list_messages(
        &self,
        execution_node_id: &str,
    ) -> Result<Vec<MessageRecord>, StoreError> {
        let pool = self.pool();
        let rows: Vec<MessageRow> = query_as(
            "SELECT id, run_id, execution_node_id, agent_id, role, content_json, created_at \
             FROM messages WHERE execution_node_id = ? ORDER BY created_at",
        )
        .bind(execution_node_id)
        .fetch_all(pool)
        .await
        .map_err(qerr)?;

        Ok(rows.into_iter().map(row_to_message).collect())
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

    /// Get the most recently started interrupted run.
    pub async fn get_last_interrupted_run(&self) -> Result<Option<RunRecord>, StoreError> {
        let pool = self.pool();
        let row: Option<RunRow> = query_as(
            "SELECT id, title, root_agent_id, status, input_json, output_json, started_at, finished_at \
             FROM runs WHERE status = 'interrupted' \
             ORDER BY started_at DESC LIMIT 1",
        )
        .fetch_optional(pool)
        .await
        .map_err(qerr)?;

        Ok(row.map(row_to_run))
    }

    /// List runs whose `input_json` contains the given CWD path.
    pub async fn list_runs_by_cwd(
        &self,
        cwd: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<RunRecord>, StoreError> {
        let pool = self.pool();
        let pattern = format!("%{cwd}%");
        let rows: Vec<RunRow> = query_as(
            "SELECT id, title, root_agent_id, status, input_json, output_json, started_at, finished_at \
             FROM runs WHERE input_json LIKE ? \
             ORDER BY started_at DESC LIMIT ? OFFSET ?",
        )
        .bind(pattern)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await
        .map_err(qerr)?;

        Ok(rows.into_iter().map(row_to_run).collect())
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
        insert_execution_node(pool, "en-1", "run-1", "root", None, None, "running", "{}", 1100)
            .await;

        // Insert 2 steps
        sqlx::query(
            "INSERT INTO steps (id, run_id, execution_node_id, agent_id, kind, data_json, started_at) \
             VALUES ('step-1', 'run-1', 'en-1', 'root', 'model_call', '{\"model\":\"gpt-4\"}', 1200)",
        )
        .execute(pool)
        .await
        .expect("insert step 1");

        sqlx::query(
            "INSERT INTO steps (id, run_id, execution_node_id, agent_id, kind, data_json, started_at) \
             VALUES ('step-2', 'run-1', 'en-1', 'root', 'tool_call', '{\"tool\":\"bash\"}', 1300)",
        )
        .execute(pool)
        .await
        .expect("insert step 2");

        let steps = store.list_steps("run-1").await.expect("list_steps");
        assert_eq!(steps.len(), 2);
        // Ordered by started_at
        assert_eq!(steps[0].id, "step-1");
        assert_eq!(steps[0].kind, "model_call");
        assert_eq!(steps[1].id, "step-2");
        assert_eq!(steps[1].kind, "tool_call");
    }

    #[tokio::test]
    async fn test_list_messages() {
        let store = setup_store().await;
        let pool = store.pool();

        insert_run(pool, "run-1", None, "root", "running", "{}", 1000).await;
        insert_execution_node(pool, "en-1", "run-1", "root", None, None, "running", "{}", 1100)
            .await;

        sqlx::query(
            "INSERT INTO messages (id, run_id, execution_node_id, agent_id, role, content_json, created_at) \
             VALUES ('msg-1', 'run-1', 'en-1', 'root', 'user', '\"hello\"', 1200)",
        )
        .execute(pool)
        .await
        .expect("insert msg 1");

        sqlx::query(
            "INSERT INTO messages (id, run_id, execution_node_id, agent_id, role, content_json, created_at) \
             VALUES ('msg-2', 'run-1', 'en-1', 'root', 'assistant', '\"world\"', 1300)",
        )
        .execute(pool)
        .await
        .expect("insert msg 2");

        let msgs = store.list_messages("en-1").await.expect("list_messages");
        assert_eq!(msgs.len(), 2);
        // Ordered by created_at
        assert_eq!(msgs[0].id, "msg-1");
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[1].id, "msg-2");
        assert_eq!(msgs[1].role, "assistant");
    }

    #[tokio::test]
    async fn test_get_prompt_snapshot() {
        let store = setup_store().await;
        let pool = store.pool();

        insert_run(pool, "run-1", None, "root", "running", "{}", 1000).await;
        insert_execution_node(pool, "en-1", "run-1", "root", None, None, "running", "{}", 1100)
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

        let events = store.list_trace_events("run-1").await.expect("list_trace_events");
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
    async fn test_list_runs_by_cwd() {
        let store = setup_store().await;
        let pool = store.pool();

        insert_run(
            pool,
            "run-1",
            None,
            "root",
            "completed",
            r#"{"cwd":"/home/user/project-a"}"#,
            1000,
        )
        .await;
        insert_run(
            pool,
            "run-2",
            None,
            "root",
            "completed",
            r#"{"cwd":"/home/user/project-b"}"#,
            2000,
        )
        .await;
        insert_run(
            pool,
            "run-3",
            None,
            "root",
            "completed",
            r#"{"cwd":"/home/user/project-a/sub"}"#,
            3000,
        )
        .await;

        let results = store
            .list_runs_by_cwd("/home/user/project-a", 10, 0)
            .await
            .expect("list_runs_by_cwd");

        // Both run-1 and run-3 contain "/home/user/project-a"
        assert_eq!(results.len(), 2);
        // Ordered by started_at DESC
        assert_eq!(results[0].id, "run-3");
        assert_eq!(results[1].id, "run-1");
    }
}
