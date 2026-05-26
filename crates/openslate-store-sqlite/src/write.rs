//! Write operations for the SQLite store.
//!
//! Provides insert/update methods on [`SqliteStore`] for all table types:
//! runs, execution_nodes, steps, messages, prompt_snapshots, audit_log,
//! and trace_events.

use openslate_core::error::StoreError;
use sqlx::query;

use crate::store::SqliteStore;

#[allow(clippy::too_many_arguments)]
impl SqliteStore {
    /// Insert a new run record.
    pub async fn insert_run(
        &self,
        id: &str,
        title: Option<&str>,
        root_agent_id: &str,
        status: &str,
        input_json: &str,
        started_at: i64,
    ) -> Result<(), StoreError> {
        query(
            "INSERT INTO runs (id, title, root_agent_id, status, input_json, started_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(title)
        .bind(root_agent_id)
        .bind(status)
        .bind(input_json)
        .bind(started_at)
        .execute(self.pool())
        .await
        .map_err(|e| StoreError::WriteError(e.to_string()))?;
        Ok(())
    }

    /// Update run status and optionally set output_json / finished_at.
    pub async fn update_run_status(
        &self,
        id: &str,
        status: &str,
        output_json: Option<&str>,
        finished_at: Option<i64>,
    ) -> Result<(), StoreError> {
        query(
            "UPDATE runs SET status = ?, output_json = ?, finished_at = ? WHERE id = ?",
        )
        .bind(status)
        .bind(output_json)
        .bind(finished_at)
        .bind(id)
        .execute(self.pool())
        .await
        .map_err(|e| StoreError::WriteError(e.to_string()))?;
        Ok(())
    }

    /// Insert an execution node.
    pub async fn insert_execution_node(
        &self,
        id: &str,
        run_id: &str,
        agent_id: &str,
        parent_execution_id: Option<&str>,
        parent_call_id: Option<&str>,
        status: &str,
        input_json: &str,
        started_at: i64,
    ) -> Result<(), StoreError> {
        query(
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
        .execute(self.pool())
        .await
        .map_err(|e| StoreError::WriteError(e.to_string()))?;
        Ok(())
    }

    /// Insert a step.
    pub async fn insert_step(
        &self,
        id: &str,
        run_id: &str,
        execution_node_id: &str,
        agent_id: &str,
        kind: &str,
        data_json: &str,
        started_at: i64,
    ) -> Result<(), StoreError> {
        query(
            "INSERT INTO steps \
             (id, run_id, execution_node_id, agent_id, kind, data_json, started_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(run_id)
        .bind(execution_node_id)
        .bind(agent_id)
        .bind(kind)
        .bind(data_json)
        .bind(started_at)
        .execute(self.pool())
        .await
        .map_err(|e| StoreError::WriteError(e.to_string()))?;
        Ok(())
    }

    /// Insert a message.
    pub async fn insert_message(
        &self,
        id: &str,
        run_id: &str,
        execution_node_id: &str,
        agent_id: Option<&str>,
        role: &str,
        content_json: &str,
        created_at: i64,
    ) -> Result<(), StoreError> {
        query(
            "INSERT INTO messages \
             (id, run_id, execution_node_id, agent_id, role, content_json, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(run_id)
        .bind(execution_node_id)
        .bind(agent_id)
        .bind(role)
        .bind(content_json)
        .bind(created_at)
        .execute(self.pool())
        .await
        .map_err(|e| StoreError::WriteError(e.to_string()))?;
        Ok(())
    }

    /// Insert a prompt snapshot.
    pub async fn insert_prompt_snapshot(
        &self,
        id: &str,
        run_id: &str,
        execution_node_id: &str,
        agent_id: &str,
        profile_name: &str,
        source_kind: &str,
        source_path: Option<&str>,
        content_hash: &str,
        rendered_prompt: &str,
        created_at: i64,
    ) -> Result<(), StoreError> {
        query(
            "INSERT INTO prompt_snapshots \
             (id, run_id, execution_node_id, agent_id, profile_name, source_kind, source_path, \
              content_hash, rendered_prompt, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(run_id)
        .bind(execution_node_id)
        .bind(agent_id)
        .bind(profile_name)
        .bind(source_kind)
        .bind(source_path)
        .bind(content_hash)
        .bind(rendered_prompt)
        .bind(created_at)
        .execute(self.pool())
        .await
        .map_err(|e| StoreError::WriteError(e.to_string()))?;
        Ok(())
    }

    /// Insert an audit event.
    pub async fn insert_audit_event(
        &self,
        id: &str,
        run_id: Option<&str>,
        agent_id: Option<&str>,
        event_type: &str,
        event_json: &str,
        created_at: i64,
    ) -> Result<(), StoreError> {
        query(
            "INSERT INTO audit_log (id, run_id, agent_id, event_type, event_json, created_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(run_id)
        .bind(agent_id)
        .bind(event_type)
        .bind(event_json)
        .bind(created_at)
        .execute(self.pool())
        .await
        .map_err(|e| StoreError::WriteError(e.to_string()))?;
        Ok(())
    }

    /// Insert a trace event.
    pub async fn insert_trace_event(
        &self,
        id: &str,
        run_id: &str,
        execution_node_id: Option<&str>,
        step_id: Option<&str>,
        agent_id: Option<&str>,
        event_name: &str,
        event_kind: &str,
        ts_ns: i64,
        dur_ns: Option<i64>,
        track: &str,
        args_json: Option<&str>,
    ) -> Result<(), StoreError> {
        query(
            "INSERT INTO trace_events \
             (id, run_id, execution_node_id, step_id, agent_id, event_name, event_kind, \
              ts_ns, dur_ns, track, args_json) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(run_id)
        .bind(execution_node_id)
        .bind(step_id)
        .bind(agent_id)
        .bind(event_name)
        .bind(event_kind)
        .bind(ts_ns)
        .bind(dur_ns)
        .bind(track)
        .bind(args_json)
        .execute(self.pool())
        .await
        .map_err(|e| StoreError::WriteError(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::query_scalar;

    async fn setup_store() -> SqliteStore {
        let store = SqliteStore::new_in_memory().await.expect("store created");
        store.run_migrations().await.expect("migrations run");
        store
    }

    async fn seed_run(store: &SqliteStore) {
        store
            .insert_run("run-1", Some("test run"), "agent-root", "running", "{}", 1000)
            .await
            .expect("seed run");
    }

    async fn seed_execution_node(store: &SqliteStore) {
        seed_run(store).await;
        store
            .insert_execution_node(
                "enode-1",
                "run-1",
                "agent-root",
                None,
                None,
                "running",
                r#"{"prompt":"hello"}"#,
                1100,
            )
            .await
            .expect("seed execution node");
    }

    #[tokio::test]
    async fn test_insert_run_and_verify() {
        let store = setup_store().await;

        store
            .insert_run("run-1", Some("my run"), "agent-a", "running", r#"{"q":"hi"}"#, 1000)
            .await
            .expect("insert run");

        let status: String =
            query_scalar::<_, String>("SELECT status FROM runs WHERE id = 'run-1'")
                .fetch_one(store.pool())
                .await
                .expect("query status");

        assert_eq!(status, "running");

        let title: Option<String> =
            query_scalar::<_, Option<String>>("SELECT title FROM runs WHERE id = 'run-1'")
                .fetch_one(store.pool())
                .await
                .expect("query title");

        assert_eq!(title, Some("my run".to_string()));
    }

    #[tokio::test]
    async fn test_update_run_status() {
        let store = setup_store().await;
        seed_run(&store).await;

        store
            .update_run_status("run-1", "completed", Some(r#"{"a":1}"#), Some(2000))
            .await
            .expect("update status");

        let (status, output, finished): (String, Option<String>, Option<i64>) =
            sqlx::query_as::<_, (String, Option<String>, Option<i64>)>(
                "SELECT status, output_json, finished_at FROM runs WHERE id = 'run-1'",
            )
            .fetch_one(store.pool())
            .await
            .expect("query run");

        assert_eq!(status, "completed");
        assert_eq!(output, Some(r#"{"a":1}"#.to_string()));
        assert_eq!(finished, Some(2000));
    }

    #[tokio::test]
    async fn test_insert_execution_node() {
        let store = setup_store().await;
        seed_run(&store).await;

        store
            .insert_execution_node(
                "enode-1",
                "run-1",
                "agent-root",
                None,
                None,
                "running",
                r#"{"prompt":"test"}"#,
                1100,
            )
            .await
            .expect("insert execution node");

        let count: i64 =
            query_scalar::<_, i64>("SELECT COUNT(*) FROM execution_nodes WHERE id = 'enode-1'")
                .fetch_one(store.pool())
                .await
                .expect("count");

        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn test_insert_step() {
        let store = setup_store().await;
        seed_execution_node(&store).await;

        store
            .insert_step(
                "step-1",
                "run-1",
                "enode-1",
                "agent-root",
                "model_call",
                r#"{"model":"gpt-4"}"#,
                1200,
            )
            .await
            .expect("insert step");

        let kind: String =
            query_scalar::<_, String>("SELECT kind FROM steps WHERE id = 'step-1'")
                .fetch_one(store.pool())
                .await
                .expect("query kind");

        assert_eq!(kind, "model_call");
    }

    #[tokio::test]
    async fn test_insert_message() {
        let store = setup_store().await;
        seed_execution_node(&store).await;

        store
            .insert_message(
                "msg-1",
                "run-1",
                "enode-1",
                Some("agent-root"),
                "user",
                r#"{"text":"hello"}"#,
                1300,
            )
            .await
            .expect("insert message");

        let (role, content): (String, String) =
            sqlx::query_as::<_, (String, String)>(
                "SELECT role, content_json FROM messages WHERE id = 'msg-1'",
            )
            .fetch_one(store.pool())
            .await
            .expect("query message");

        assert_eq!(role, "user");
        assert_eq!(content, r#"{"text":"hello"}"#);
    }

    #[tokio::test]
    async fn test_insert_prompt_snapshot() {
        let store = setup_store().await;
        seed_execution_node(&store).await;

        store
            .insert_prompt_snapshot(
                "ps-1",
                "run-1",
                "enode-1",
                "agent-root",
                "default",
                "file",
                Some("/path/to/prompt.md"),
                "abc123hash",
                "You are a helpful assistant.",
                1400,
            )
            .await
            .expect("insert prompt snapshot");

        let profile: String =
            query_scalar::<_, String>("SELECT profile_name FROM prompt_snapshots WHERE id = 'ps-1'")
                .fetch_one(store.pool())
                .await
                .expect("query profile");

        assert_eq!(profile, "default");

        let hash: String =
            query_scalar::<_, String>("SELECT content_hash FROM prompt_snapshots WHERE id = 'ps-1'")
                .fetch_one(store.pool())
                .await
                .expect("query hash");

        assert_eq!(hash, "abc123hash");
    }

    #[tokio::test]
    async fn test_insert_audit_event() {
        let store = setup_store().await;

        store
            .insert_audit_event(
                "audit-1",
                None,
                None,
                "system_start",
                r#"{"version":"0.1"}"#,
                1500,
            )
            .await
            .expect("insert audit event");

        let (run_id, event_type): (Option<String>, String) =
            sqlx::query_as::<_, (Option<String>, String)>(
                "SELECT run_id, event_type FROM audit_log WHERE id = 'audit-1'",
            )
            .fetch_one(store.pool())
            .await
            .expect("query audit");

        assert_eq!(run_id, None);
        assert_eq!(event_type, "system_start");
    }

    #[tokio::test]
    async fn test_insert_trace_event() {
        let store = setup_store().await;
        seed_run(&store).await;

        store
            .insert_trace_event(
                "trace-1",
                "run-1",
                Some("enode-1"),
                Some("step-1"),
                Some("agent-root"),
                "llm_call",
                "span",
                1_000_000_000,
                Some(500_000),
                "main",
                Some(r#"{"model":"gpt-4"}"#),
            )
            .await
            .expect("insert trace event");

        let (event_name, ts_ns): (String, i64) =
            sqlx::query_as::<_, (String, i64)>(
                "SELECT event_name, ts_ns FROM trace_events WHERE id = 'trace-1'",
            )
            .fetch_one(store.pool())
            .await
            .expect("query trace");

        assert_eq!(event_name, "llm_call");
        assert_eq!(ts_ns, 1_000_000_000);
    }

    #[tokio::test]
    async fn test_large_content_insert() {
        let store = setup_store().await;
        seed_execution_node(&store).await;

        let large_content = "x".repeat(1_048_576);

        store
            .insert_message(
                "msg-big",
                "run-1",
                "enode-1",
                None,
                "assistant",
                &large_content,
                1600,
            )
            .await
            .expect("large insert should succeed");

        let len: i64 =
            query_scalar::<_, i64>("SELECT LENGTH(content_json) FROM messages WHERE id = 'msg-big'")
                .fetch_one(store.pool())
                .await
                .expect("query length");

        assert_eq!(len, 1_048_576);
    }
}
