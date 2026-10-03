//! SQLite 存储的写操作。
//!
//! 提供 [`SqliteStore`] 上全部表类型的插入/更新/删除方法：
//! runs、execution_nodes、steps、messages、prompt_snapshots、audit_log、
//! trace_events，以及删除/保留（delete_run、prune_runs_keep_last）等
//! 生命周期 API。

use openslate_core::error::StoreError;
use sqlx::query;

use crate::store::SqliteStore;

fn werr(e: sqlx::Error) -> StoreError {
    StoreError::WriteError(e.to_string())
}

#[allow(clippy::too_many_arguments)]
impl SqliteStore {
    /// 插入一条新的 run 记录。
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
        .map_err(werr)?;
        Ok(())
    }

    /// 在单事务内插入 run 行与 recorder 的持久化执行节点
    /// （`RunRecorder::begin` 使用）。
    ///
    /// 合并为单事务的原因：两条独立语句之间存在崩溃窗口，会留下
    /// "有 node 无 run"（FK 失效）或"有 run 无 node"的半状态。
    ///
    /// 节点固定为 `status = "running"`、`input_json = "{}"`（仅用于满足
    /// messages 的 FK，见 recorder 模块文档）。
    pub(crate) async fn insert_run_with_persist_node(
        &self,
        run_id: &str,
        title: Option<&str>,
        root_agent_id: &str,
        status: &str,
        input_json: &str,
        started_at: i64,
        node_id: &str,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool().begin().await.map_err(werr)?;

        query(
            "INSERT INTO runs (id, title, root_agent_id, status, input_json, started_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(run_id)
        .bind(title)
        .bind(root_agent_id)
        .bind(status)
        .bind(input_json)
        .bind(started_at)
        .execute(&mut *tx)
        .await
        .map_err(werr)?;

        query(
            "INSERT INTO execution_nodes \
             (id, run_id, agent_id, parent_execution_id, parent_call_id, status, input_json, started_at) \
             VALUES (?, ?, ?, NULL, NULL, 'running', '{}', ?)",
        )
        .bind(node_id)
        .bind(run_id)
        .bind(root_agent_id)
        .bind(started_at)
        .execute(&mut *tx)
        .await
        .map_err(werr)?;

        tx.commit().await.map_err(werr)?;
        Ok(())
    }

    /// 更新 run 的标题（会话标题在首条回复后确定等场景）。
    pub async fn update_run_title(&self, id: &str, title: &str) -> Result<(), StoreError> {
        query("UPDATE runs SET title = ? WHERE id = ?")
            .bind(title)
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(werr)?;
        Ok(())
    }

    /// 把 run 置回 `running`（resume 过的 recorder 首次写入消息时调用，
    /// 见 `RunRecorder::write_message`）。不存在的 run 静默无操作。
    pub(crate) async fn activate_run(&self, id: &str) -> Result<(), StoreError> {
        query("UPDATE runs SET status = 'running' WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(werr)?;
        Ok(())
    }

    /// 更新 run 状态；`output_json` / `finished_at` 为 `Some` 时才更新，
    /// `None` **保持原值**（动态 SET 构造，不整体覆盖）。
    ///
    /// 语义变更说明：旧实现无条件 `SET status=?, output_json=?, finished_at=?`，
    /// 传 `None` 会把已有值清空。
    pub async fn update_run_status(
        &self,
        id: &str,
        status: &str,
        output_json: Option<&str>,
        finished_at: Option<i64>,
    ) -> Result<(), StoreError> {
        // 动态 SET：只更新传入 Some 的列，避免 NULL 覆盖已有值。
        let sql = match (output_json, finished_at) {
            (Some(_), Some(_)) => {
                "UPDATE runs SET status = ?, output_json = ?, finished_at = ? WHERE id = ?"
            }
            (Some(_), None) => "UPDATE runs SET status = ?, output_json = ? WHERE id = ?",
            (None, Some(_)) => "UPDATE runs SET status = ?, finished_at = ? WHERE id = ?",
            (None, None) => "UPDATE runs SET status = ? WHERE id = ?",
        };

        let mut q = query(sql);
        q = q.bind(status);
        if output_json.is_some() {
            q = q.bind(output_json);
        }
        if finished_at.is_some() {
            q = q.bind(finished_at);
        }
        q.bind(id).execute(self.pool()).await.map_err(werr)?;
        Ok(())
    }

    /// 单独记录 run 的累计成本（美元，P2-3）。未配置定价时规范值为
    /// `0.0`（"未配置→成本记 0"）。
    pub async fn update_run_cost(&self, id: &str, cost_usd: f64) -> Result<(), StoreError> {
        query("UPDATE runs SET cost_usd = ? WHERE id = ?")
            .bind(cost_usd)
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(werr)?;
        Ok(())
    }

    /// 在单事务内写入 run 的终态：status、可选 output、finished_at 与
    /// 累计成本（`RunRecorder::finish` 使用）。
    ///
    /// 合并为单事务的原因：status 与 cost 分两条语句提交时，中间崩溃
    /// 会留下"已 completed 但成本缺失"的半状态。`output_json` 为 `None`
    /// 时保持原值（与 [`Self::update_run_status`] 语义一致）。
    pub(crate) async fn finish_run(
        &self,
        id: &str,
        status: &str,
        output_json: Option<&str>,
        finished_at: i64,
        cost_usd: f64,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool().begin().await.map_err(werr)?;

        if output_json.is_some() {
            query("UPDATE runs SET status = ?, output_json = ?, finished_at = ? WHERE id = ?")
                .bind(status)
                .bind(output_json)
                .bind(finished_at)
                .bind(id)
                .execute(&mut *tx)
                .await
                .map_err(werr)?;
        } else {
            query("UPDATE runs SET status = ?, finished_at = ? WHERE id = ?")
                .bind(status)
                .bind(finished_at)
                .bind(id)
                .execute(&mut *tx)
                .await
                .map_err(werr)?;
        }

        query("UPDATE runs SET cost_usd = ? WHERE id = ?")
            .bind(cost_usd)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(werr)?;

        tx.commit().await.map_err(werr)?;
        Ok(())
    }

    /// 删除一个 run 及其全部子表数据，单事务内按依赖顺序显式删除。
    ///
    /// 删除顺序：messages → steps → prompt_snapshots → trace_events →
    /// audit_log → execution_nodes → runs（先删引用方再删被引用方）。
    /// 不依赖 `ON DELETE CASCADE` —— 存量库的外键定义无法追加删除动作。
    ///
    /// 返回 runs 表的删除行数（`0` 表示 run 不存在；子表此时也不会有
    /// 残留——即使有，也会一并清掉）。
    pub async fn delete_run(&self, run_id: &str) -> Result<u64, StoreError> {
        // 字面量语句数组（而非 format! 拼表名）：可静态审计，天然满足
        // sqlx 的 SqlSafeStr 约束。
        const CHILD_DELETES: &[&str] = &[
            "DELETE FROM messages WHERE run_id = ?",
            "DELETE FROM steps WHERE run_id = ?",
            "DELETE FROM prompt_snapshots WHERE run_id = ?",
            "DELETE FROM trace_events WHERE run_id = ?",
            "DELETE FROM audit_log WHERE run_id = ?",
            "DELETE FROM execution_nodes WHERE run_id = ?",
        ];

        let mut tx = self.pool().begin().await.map_err(werr)?;
        for &stmt in CHILD_DELETES {
            query(stmt).bind(run_id).execute(&mut *tx).await.map_err(werr)?;
        }
        let result = query("DELETE FROM runs WHERE id = ?")
            .bind(run_id)
            .execute(&mut *tx)
            .await
            .map_err(werr)?;
        tx.commit().await.map_err(werr)?;
        Ok(result.rows_affected())
    }

    /// 保留最近 `keep_n` 个 run（按 `started_at DESC, id DESC` 排序），
    /// 删除其余，返回被删除的 run_id 列表（按最旧优先）。
    ///
    /// 供保留策略（retention）使用。`keep_n` 为 `0` 时删除全部；
    /// 负值按 `0` 处理。
    pub async fn prune_runs_keep_last(&self, keep_n: i64) -> Result<Vec<String>, StoreError> {
        let keep_n = keep_n.max(0);

        let victim_ids: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM runs \
             WHERE id NOT IN (SELECT id FROM runs ORDER BY started_at DESC, id DESC LIMIT ?) \
             ORDER BY started_at ASC, id ASC",
        )
        .bind(keep_n)
        .fetch_all(self.pool())
        .await
        .map_err(|e| StoreError::QueryError(e.to_string()))?;

        let mut deleted = Vec::with_capacity(victim_ids.len());
        for id in &victim_ids {
            self.delete_run(id).await?;
            deleted.push(id.clone());
        }
        Ok(deleted)
    }

    /// 启动时清扫僵尸记录：把所有 `status = 'running'` 的 run 置为
    /// `interrupted`（上次进程未正常收尾），`finished_at` 缺失时补上
    /// `now_ms`（已有值则保留）。返回受影响行数。
    pub async fn mark_running_runs_interrupted(&self, now_ms: i64) -> Result<u64, StoreError> {
        let result =
            query("UPDATE runs SET status = 'interrupted', finished_at = COALESCE(finished_at, ?) \
                   WHERE status = 'running'")
                .bind(now_ms)
                .execute(self.pool())
                .await
                .map_err(werr)?;
        Ok(result.rows_affected())
    }

    /// 执行 `PRAGMA wal_checkpoint(TRUNCATE)`：把 WAL 落盘并把 WAL 文件
    /// 截断为 0。删除大量数据 / 退出前调用，可避免 WAL 文件无限增长。
    pub async fn wal_checkpoint(&self) -> Result<(), StoreError> {
        query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(self.pool())
            .await
            .map_err(werr)?;
        Ok(())
    }

    /// run 总数（保留策略 / 测试用）。
    pub async fn count_runs(&self) -> Result<i64, StoreError> {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM runs")
            .fetch_one(self.pool())
            .await
            .map_err(|e| StoreError::QueryError(e.to_string()))
    }

    /// 插入一个执行节点。
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
        .map_err(werr)?;
        Ok(())
    }

    /// 插入一个 step。
    ///
    /// `seq` 是 per-run 单调递增序号（由调用方分配）；查询按其排序，
    /// 同时间戳的批量插入保持稳定顺序。
    pub async fn insert_step(
        &self,
        id: &str,
        run_id: &str,
        execution_node_id: &str,
        agent_id: &str,
        kind: &str,
        data_json: &str,
        seq: i64,
        started_at: i64,
    ) -> Result<(), StoreError> {
        query(
            "INSERT INTO steps \
             (id, run_id, execution_node_id, agent_id, kind, data_json, seq, started_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(run_id)
        .bind(execution_node_id)
        .bind(agent_id)
        .bind(kind)
        .bind(data_json)
        .bind(seq)
        .bind(started_at)
        .execute(self.pool())
        .await
        .map_err(werr)?;
        Ok(())
    }

    /// 插入一条消息。
    ///
    /// `seq` 是 per-run 单调递增序号（由调用方分配）。会话顺序对
    /// resume 是关键语义 —— provider 会拒绝 `tool_calls` 与 tool 结果
    /// 分离的乱序历史 —— 因此读取端一律按 `seq` 排序，绝不单独按
    /// `created_at`。同一 run 内 `seq` 由唯一索引
    /// `idx_messages_run_seq_uq` 保证不重复（见 schema v2）。
    pub async fn insert_message(
        &self,
        id: &str,
        run_id: &str,
        execution_node_id: &str,
        agent_id: Option<&str>,
        role: &str,
        content_json: &str,
        seq: i64,
        created_at: i64,
    ) -> Result<(), StoreError> {
        query(
            "INSERT INTO messages \
             (id, run_id, execution_node_id, agent_id, role, content_json, seq, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(run_id)
        .bind(execution_node_id)
        .bind(agent_id)
        .bind(role)
        .bind(content_json)
        .bind(seq)
        .bind(created_at)
        .execute(self.pool())
        .await
        .map_err(werr)?;
        Ok(())
    }

    /// 插入一个 prompt 快照。
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
        .map_err(werr)?;
        Ok(())
    }

    /// 插入一条审计事件。
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
        .map_err(werr)?;
        Ok(())
    }

    /// 插入一条 trace 事件。
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
        .map_err(werr)?;
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
    async fn test_update_run_title() {
        let store = setup_store().await;
        seed_run(&store).await;

        store
            .update_run_title("run-1", "新标题")
            .await
            .expect("update title");

        let title: Option<String> =
            query_scalar::<_, Option<String>>("SELECT title FROM runs WHERE id = 'run-1'")
                .fetch_one(store.pool())
                .await
                .expect("query title");
        assert_eq!(title.as_deref(), Some("新标题"));
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

    /// 新语义回归测试：`None` 不清空已有值（旧实现会整体覆盖为 NULL）。
    #[tokio::test]
    async fn test_update_run_status_none_preserves_existing_values() {
        let store = setup_store().await;
        seed_run(&store).await;

        // 先落一个终态：output + finished_at 都有值
        store
            .update_run_status("run-1", "completed", Some(r#"{"a":1}"#), Some(2000))
            .await
            .expect("first update");

        // 再传 None：只改 status，output/finished_at 必须保持
        store
            .update_run_status("run-1", "interrupted", None, None)
            .await
            .expect("second update");

        let (status, output, finished): (String, Option<String>, Option<i64>) =
            sqlx::query_as::<_, (String, Option<String>, Option<i64>)>(
                "SELECT status, output_json, finished_at FROM runs WHERE id = 'run-1'",
            )
            .fetch_one(store.pool())
            .await
            .expect("query run");

        assert_eq!(status, "interrupted");
        assert_eq!(
            output,
            Some(r#"{"a":1}"#.to_string()),
            "output_json 不应被 None 清空"
        );
        assert_eq!(finished, Some(2000), "finished_at 不应被 None 清空");

        // 混合场景：只带 output
        store
            .update_run_status("run-1", "completed", Some(r#"{"b":2}"#), None)
            .await
            .expect("third update");
        let (output, finished): (Option<String>, Option<i64>) =
            sqlx::query_as::<_, (Option<String>, Option<i64>)>(
                "SELECT output_json, finished_at FROM runs WHERE id = 'run-1'",
            )
            .fetch_one(store.pool())
            .await
            .expect("query run again");
        assert_eq!(output.as_deref(), Some(r#"{"b":2}"#));
        assert_eq!(finished, Some(2000), "finished_at 仍应保持");
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
                1,
                1200,
            )
            .await
            .expect("insert step");

        let (kind, seq): (String, i64) =
            sqlx::query_as::<_, (String, i64)>("SELECT kind, seq FROM steps WHERE id = 'step-1'")
                .fetch_one(store.pool())
                .await
                .expect("query kind");

        assert_eq!(kind, "model_call");
        assert_eq!(seq, 1);
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
                1,
                1300,
            )
            .await
            .expect("insert message");

        let (role, content, seq): (String, String, i64) =
            sqlx::query_as::<_, (String, String, i64)>(
                "SELECT role, content_json, seq FROM messages WHERE id = 'msg-1'",
            )
            .fetch_one(store.pool())
            .await
            .expect("query message");

        assert_eq!(role, "user");
        assert_eq!(content, r#"{"text":"hello"}"#);
        assert_eq!(seq, 1);
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

        let profile: String = query_scalar::<_, String>(
            "SELECT profile_name FROM prompt_snapshots WHERE id = 'ps-1'",
        )
        .fetch_one(store.pool())
        .await
        .expect("query profile");

        assert_eq!(profile, "default");

        let hash: String = query_scalar::<_, String>(
            "SELECT content_hash FROM prompt_snapshots WHERE id = 'ps-1'",
        )
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

        let (event_name, ts_ns): (String, i64) = sqlx::query_as::<_, (String, i64)>(
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
                1,
                1600,
            )
            .await
            .expect("large insert should succeed");

        let len: i64 = query_scalar::<_, i64>(
            "SELECT LENGTH(content_json) FROM messages WHERE id = 'msg-big'",
        )
        .fetch_one(store.pool())
        .await
        .expect("query length");

        assert_eq!(len, 1_048_576);
    }

    // -----------------------------------------------------------------------
    // 删除 / 保留 API
    // -----------------------------------------------------------------------

    /// 造一个带全部子表数据的 run。
    async fn seed_full_run(store: &SqliteStore, run_id: &str, started_at: i64) {
        store
            .insert_run(run_id, Some(run_id), "root", "completed", "{}", started_at)
            .await
            .expect("insert run");
        store
            .insert_execution_node(
                &format!("{run_id}-enode"),
                run_id,
                "root",
                None,
                None,
                "completed",
                "{}",
                started_at + 1,
            )
            .await
            .expect("insert node");
        store
            .insert_step(
                &format!("{run_id}-step"),
                run_id,
                &format!("{run_id}-enode"),
                "root",
                "model_call",
                "{}",
                1,
                started_at + 2,
            )
            .await
            .expect("insert step");
        store
            .insert_message(
                &format!("{run_id}-msg"),
                run_id,
                &format!("{run_id}-enode"),
                Some("root"),
                "user",
                "\"hi\"",
                1,
                started_at + 3,
            )
            .await
            .expect("insert message");
        store
            .insert_prompt_snapshot(
                &format!("{run_id}-ps"),
                run_id,
                &format!("{run_id}-enode"),
                "root",
                "default",
                "file",
                None,
                "hash",
                "prompt",
                started_at + 4,
            )
            .await
            .expect("insert prompt snapshot");
        store
            .insert_trace_event(
                &format!("{run_id}-trace"),
                run_id,
                Some(&format!("{run_id}-enode")),
                Some(&format!("{run_id}-step")),
                Some("root"),
                "llm_call",
                "span",
                started_at * 1_000_000,
                None,
                "main",
                None,
            )
            .await
            .expect("insert trace");
        store
            .insert_audit_event(
                &format!("{run_id}-audit"),
                Some(run_id),
                Some("root"),
                "tool_approved",
                "{}",
                started_at + 5,
            )
            .await
            .expect("insert audit");
    }

    #[tokio::test]
    async fn test_delete_run_cascades_all_child_tables() {
        let store = setup_store().await;
        seed_full_run(&store, "victim", 1000).await;
        seed_full_run(&store, "survivor", 2000).await;

        let deleted = store.delete_run("victim").await.expect("delete");
        assert_eq!(deleted, 1, "runs 表删除 1 行");

        // 全部子表都不应残留 victim 的行
        for table in [
            "messages",
            "steps",
            "prompt_snapshots",
            "trace_events",
            "audit_log",
            "execution_nodes",
        ] {
            let count: i64 = query_scalar::<_, i64>(sqlx::AssertSqlSafe(format!(
                "SELECT COUNT(*) FROM {table} WHERE run_id = 'victim'"
            )))
                    .fetch_one(store.pool())
                    .await
                    .unwrap_or_else(|_| panic!("count {table}"));
            assert_eq!(count, 0, "{table} 不应残留 victim 数据");
        }
        let runs_left: i64 =
            query_scalar::<_, i64>("SELECT COUNT(*) FROM runs WHERE id = 'victim'")
                .fetch_one(store.pool())
                .await
                .expect("count runs");
        assert_eq!(runs_left, 0);

        // survivor 完好
        assert_eq!(store.count_runs().await.expect("count"), 1);
        let msgs = store.list_messages_by_run("survivor").await.expect("msgs");
        assert_eq!(msgs.len(), 1);
    }

    #[tokio::test]
    async fn test_delete_run_nonexistent_returns_zero() {
        let store = setup_store().await;
        let deleted = store.delete_run("nope").await.expect("delete");
        assert_eq!(deleted, 0);
    }

    #[tokio::test]
    async fn test_prune_runs_keep_last() {
        let store = setup_store().await;
        seed_full_run(&store, "old-1", 1000).await;
        seed_full_run(&store, "old-2", 2000).await;
        seed_full_run(&store, "new-1", 3000).await;
        seed_full_run(&store, "new-2", 4000).await;

        let deleted = store.prune_runs_keep_last(2).await.expect("prune");
        assert_eq!(deleted, vec!["old-1".to_string(), "old-2".to_string()]);

        assert_eq!(store.count_runs().await.expect("count"), 2);
        let remaining = store.list_runs(10, 0).await.expect("list");
        let ids: Vec<&str> = remaining.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["new-2", "new-1"]);

        // 子表数据一并清理
        let steps: i64 = query_scalar::<_, i64>("SELECT COUNT(*) FROM steps")
            .fetch_one(store.pool())
            .await
            .expect("count steps");
        assert_eq!(steps, 2, "被删 run 的 steps 应一并删除");
    }

    #[tokio::test]
    async fn test_prune_runs_keep_last_zero_and_negative() {
        let store = setup_store().await;
        seed_full_run(&store, "a", 1000).await;
        seed_full_run(&store, "b", 2000).await;

        let deleted = store.prune_runs_keep_last(0).await.expect("prune 0");
        assert_eq!(deleted.len(), 2);
        assert_eq!(store.count_runs().await.expect("count"), 0);

        // 负值按 0 处理（LIMIT 负数在 SQLite 里意味着"无限制"，必须防护）
        seed_full_run(&store, "c", 3000).await;
        let deleted = store.prune_runs_keep_last(-5).await.expect("prune -5");
        assert_eq!(deleted.len(), 1, "负 keep_n 应按 0 处理");
        assert_eq!(store.count_runs().await.expect("count"), 0);
    }

    /// 同毫秒 started_at：tiebreaker（id DESC）必须决定谁被保留。
    #[tokio::test]
    async fn test_prune_tiebreaks_on_id_when_same_started_at() {
        let store = setup_store().await;
        seed_full_run(&store, "run-a", 5000).await;
        seed_full_run(&store, "run-b", 5000).await;

        let deleted = store.prune_runs_keep_last(1).await.expect("prune");
        // started_at 相同 → id DESC：保留 run-b，删除 run-a
        assert_eq!(deleted, vec!["run-a".to_string()]);
        assert_eq!(store.count_runs().await.expect("count"), 1);
    }

    #[tokio::test]
    async fn test_mark_running_runs_interrupted() {
        let store = setup_store().await;
        store
            .insert_run("zombie-1", None, "root", "running", "{}", 1000)
            .await
            .expect("insert zombie");
        store
            .insert_run("zombie-2", None, "root", "running", "{}", 2000)
            .await
            .expect("insert zombie");
        store
            .insert_run("done", None, "root", "completed", "{}", 3000)
            .await
            .expect("insert done");

        let affected = store
            .mark_running_runs_interrupted(9999)
            .await
            .expect("sweep");
        assert_eq!(affected, 2);

        let zombie = store.get_run("zombie-1").await.expect("get").expect("run");
        assert_eq!(zombie.status, "interrupted");
        assert_eq!(zombie.finished_at, Some(9999), "缺失的 finished_at 应补上");

        let done = store.get_run("done").await.expect("get").expect("run");
        assert_eq!(done.status, "completed");
        assert_eq!(done.finished_at, None, "非 running 不应被动");
    }

    /// finished_at 已有值的 running run：COALESCE 保留原值。
    #[tokio::test]
    async fn test_mark_running_preserves_existing_finished_at() {
        let store = setup_store().await;
        store
            .insert_run("odd", None, "root", "running", "{}", 1000)
            .await
            .expect("insert");
        // 直接 SQL 造一个"running 但已有 finished_at"的奇怪状态
        sqlx::query("UPDATE runs SET finished_at = 1234 WHERE id = 'odd'")
            .execute(store.pool())
            .await
            .expect("set finished_at");

        store.mark_running_runs_interrupted(9999).await.expect("sweep");
        let run = store.get_run("odd").await.expect("get").expect("run");
        assert_eq!(run.finished_at, Some(1234), "已有 finished_at 不应被覆盖");
    }

    #[tokio::test]
    async fn test_count_runs() {
        let store = setup_store().await;
        assert_eq!(store.count_runs().await.expect("count"), 0);
        seed_run(&store).await;
        assert_eq!(store.count_runs().await.expect("count"), 1);
    }

    #[tokio::test]
    async fn test_wal_checkpoint_on_file_db() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("wal_ckpt.db");
        let store = SqliteStore::new(path.to_str().unwrap())
            .await
            .expect("store");
        store.run_migrations().await.expect("migrate");
        seed_run(&store).await;

        store.wal_checkpoint().await.expect("checkpoint");

        let run = store.get_run("run-1").await.expect("get").expect("run");
        assert_eq!(run.status, "running");
    }
}
