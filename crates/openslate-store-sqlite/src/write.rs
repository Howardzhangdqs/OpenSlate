//! SQLite 存储的写操作。
//!
//! 提供 [`SqliteStore`] 上全部表类型的插入/更新/删除方法：
//! runs、execution_nodes、steps、messages、prompt_snapshots、audit_log、
//! trace_events，以及删除/保留（delete_run、prune_runs_keep_last）等
//! 生命周期 API。

use openslate_core::error::StoreError;
use sqlx::{query, query_scalar};

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

    /// 在单事务内写入 run 的终态：status、可选 output、finished_at、
    /// 累计成本与 token 聚合（`RunRecorder::finish_with` 使用）。
    ///
    /// 合并为单事务的原因：status 与 cost 分两条语句提交时，中间崩溃
    /// 会留下"已 completed 但成本缺失"的半状态。`output_json` 为 `None`
    /// 时保持原值（与 [`Self::update_run_status`] 语义一致）。
    /// `input_tokens` / `output_tokens` 为 run 级聚合（v5；存量 run 无
    /// meta → 调用方传 0，不回填）。
    pub(crate) async fn finish_run(
        &self,
        id: &str,
        status: &str,
        output_json: Option<&str>,
        finished_at: i64,
        cost_usd: f64,
        input_tokens: u64,
        output_tokens: u64,
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

        query(
            "UPDATE runs SET cost_usd = ?, input_tokens = ?, output_tokens = ? WHERE id = ?",
        )
        .bind(cost_usd)
        .bind(input_tokens as i64)
        .bind(output_tokens as i64)
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
    /// audit_log → attachments → transcript_entries → tool_outputs →
    /// execution_nodes → runs（先删引用方再删被引用方）。不依赖
    /// `ON DELETE CASCADE` —— 存量库的外键定义无法追加删除动作。
    ///
    /// 事务提交后对受影响附件与长输出文件做**孤儿文件清扫**：逐 sha256
    /// 行级 `SELECT 1 FROM … WHERE sha256 = ?`，无行引用则删除对应文件
    /// （跨 run 复用的文件仅在最后一个引用消失时删除）。
    ///
    /// 返回 runs 表的删除行数（`0` 表示 run 不存在；子表此时也不会有
    /// 残留——即使有，也会一并清掉）。
    pub async fn delete_run(&self, run_id: &str) -> Result<u64, StoreError> {
        // GC 判定需要在删除前收集该 run 附件 / 长输出的 sha256（删后就
        // 无从得知）
        let affected_sha256s: Vec<String> =
            sqlx::query_scalar("SELECT DISTINCT sha256 FROM attachments WHERE run_id = ?")
                .bind(run_id)
                .fetch_all(self.pool())
                .await
                .map_err(|e| StoreError::QueryError(e.to_string()))?;
        let affected_output_sha256s: Vec<String> =
            sqlx::query_scalar("SELECT DISTINCT sha256 FROM tool_outputs WHERE run_id = ?")
                .bind(run_id)
                .fetch_all(self.pool())
                .await
                .map_err(|e| StoreError::QueryError(e.to_string()))?;

        // 字面量语句数组（而非 format! 拼表名）：可静态审计，天然满足
        // sqlx 的 SqlSafeStr 约束。
        const CHILD_DELETES: &[&str] = &[
            "DELETE FROM messages WHERE run_id = ?",
            "DELETE FROM steps WHERE run_id = ?",
            "DELETE FROM prompt_snapshots WHERE run_id = ?",
            "DELETE FROM trace_events WHERE run_id = ?",
            "DELETE FROM audit_log WHERE run_id = ?",
            "DELETE FROM attachments WHERE run_id = ?",
            "DELETE FROM transcript_entries WHERE run_id = ?",
            "DELETE FROM tool_outputs WHERE run_id = ?",
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

        // 事务外文件清扫（DB 行已是权威状态）
        self.sweep_orphan_files(&affected_sha256s).await?;
        self.sweep_orphan_output_files(&affected_output_sha256s)
            .await?;
        Ok(result.rows_affected())
    }

    /// 孤儿文件清扫：对给定 sha256 集合逐一判定是否仍被任何 attachments
    /// 行引用（行级 `SELECT 1`，非全表 diff）；无引用则删除
    /// `files/{sha256}.{ext}` 文件（ext 未知，按 `{sha256}.` 前缀匹配）。
    ///
    /// 内存库无 files_dir → 无文件可扫，直接 Ok。
    async fn sweep_orphan_files(&self, sha256s: &[String]) -> Result<(), StoreError> {
        if sha256s.is_empty() {
            return Ok(());
        }
        let Some(files_dir) = self.files_dir() else {
            return Ok(());
        };
        for sha in sha256s {
            let still_referenced: Option<i64> =
                sqlx::query_scalar("SELECT 1 FROM attachments WHERE sha256 = ? LIMIT 1")
                    .bind(sha)
                    .fetch_optional(self.pool())
                    .await
                    .map_err(|e| StoreError::QueryError(e.to_string()))?;
            if still_referenced.is_some() {
                continue;
            }
            let prefix = format!("{sha}.");
            let Ok(entries) = std::fs::read_dir(&files_dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                if name.to_string_lossy().starts_with(&prefix) {
                    if let Err(e) = std::fs::remove_file(entry.path()) {
                        tracing::warn!(
                            target: "openslate_store",
                            "failed to remove orphan attachment file {}: {e}",
                            entry.path().display()
                        );
                    }
                }
            }
        }
        Ok(())
    }

    /// 长输出孤儿文件清扫（与 [`Self::sweep_orphan_files`] 同构，目录不同）：
    /// 对给定 sha256 集合逐一判定是否仍被任何 `tool_outputs` 行引用；无
    /// 引用则删除 `files/outputs/{sha256}.zst`（跨 run 复用的文件仅在最后
    /// 一个引用消失时删除）。
    ///
    /// 内存库无 files_dir → 无文件可扫，直接 Ok。
    async fn sweep_orphan_output_files(&self, sha256s: &[String]) -> Result<(), StoreError> {
        if sha256s.is_empty() {
            return Ok(());
        }
        let Some(outputs_dir) = self.files_dir().map(|d| d.join("outputs")) else {
            return Ok(());
        };
        for sha in sha256s {
            let still_referenced: Option<i64> =
                sqlx::query_scalar("SELECT 1 FROM tool_outputs WHERE sha256 = ? LIMIT 1")
                    .bind(sha)
                    .fetch_optional(self.pool())
                    .await
                    .map_err(|e| StoreError::QueryError(e.to_string()))?;
            if still_referenced.is_some() {
                continue;
            }
            let path = outputs_dir.join(format!("{sha}.zst"));
            if path.exists() {
                if let Err(e) = std::fs::remove_file(&path) {
                    tracing::warn!(
                        target: "openslate_store",
                        "failed to remove orphan tool output file {}: {e}",
                        path.display()
                    );
                }
            }
        }
        Ok(())
    }

    /// 启动 GC：清扫 `files/outputs/` 下无任何 `tool_outputs` 行引用的
    /// 文件（收敛「文件写成功、行未落」的崩溃窗口——两步非事务，崩溃
    /// 后可能留下无行孤儿）。返回删除的文件数。
    ///
    /// 全量 read_dir ↔ `SELECT DISTINCT sha256` 比对（行集驱动，文件数
    /// 量级 = 保留 run 数 × 每 run 超限输出数，全量可接受）。内存库无
    /// files_dir / 目录不存在 → Ok(0)。
    pub async fn gc_orphan_output_files(&self) -> Result<u64, StoreError> {
        let Some(outputs_dir) = self.files_dir().map(|d| d.join("outputs")) else {
            return Ok(0);
        };
        let referenced: Vec<String> =
            sqlx::query_scalar("SELECT DISTINCT sha256 FROM tool_outputs")
                .fetch_all(self.pool())
                .await
                .map_err(|e| StoreError::QueryError(e.to_string()))?;
        let Ok(entries) = std::fs::read_dir(&outputs_dir) else {
            return Ok(0);
        };
        let mut removed = 0u64;
        for entry in entries.flatten() {
            // 只动本机制管辖的命名形状（{64hex}.zst），其余文件不碰。
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Some(stem) = name.strip_suffix(".zst") else {
                continue;
            };
            if !referenced.iter().any(|sha| sha == stem) {
                if let Err(e) = std::fs::remove_file(entry.path()) {
                    tracing::warn!(
                        target: "openslate_store",
                        "gc: failed to remove orphan tool output file {}: {e}",
                        entry.path().display()
                    );
                    continue;
                }
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// 启动暂存清扫（GC）：删除 `run_id IS NULL AND created_at < now-7d`
    /// 的 attachments 行（用户在 composer 选了图但从未发送的暂存），并对
    /// 其 sha256 做文件清扫。返回删除行数。
    ///
    /// 绑定行（已属于某个 run）不受影响；7 天内的暂存保留（用户可能
    /// 还在编辑草稿）。
    pub async fn gc_stale_unbound_attachments(&self, now_ms: i64) -> Result<u64, StoreError> {
        const SEVEN_DAYS_MS: i64 = 7 * 24 * 60 * 60 * 1000;
        let cutoff = now_ms - SEVEN_DAYS_MS;

        let affected_sha256s: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT sha256 FROM attachments WHERE run_id IS NULL AND created_at < ?",
        )
        .bind(cutoff)
        .fetch_all(self.pool())
        .await
        .map_err(|e| StoreError::QueryError(e.to_string()))?;

        let result = query("DELETE FROM attachments WHERE run_id IS NULL AND created_at < ?")
            .bind(cutoff)
            .execute(self.pool())
            .await
            .map_err(werr)?;

        self.sweep_orphan_files(&affected_sha256s).await?;
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

    /// 插入一条消息（v5：载荷为压缩字节 + 可选 meta 旁路 JSON）。
    ///
    /// - `content_zst`：`serde_json::to_string(Message)` 经
    ///   [`crate::codec::encode_content`] 压缩的字节（zstd-3）。
    /// - `meta_json`：`Option<MessageMeta>` 的 serde JSON（assistant 行的
    ///   usage/finish_reason/latency/model_id）；user/tool 行传 `None`。
    ///
    /// `seq` 是 per-run 单调递增序号（由调用方分配）。会话顺序对
    /// resume 是关键语义 —— provider 会拒绝 `tool_calls` 与 tool 结果
    /// 分离的乱序历史 —— 因此读取端一律按 `seq` 排序，绝不单独按
    /// `created_at`。同一 run 内 `seq` 由唯一索引
    /// `idx_messages_run_seq_uq` 保证不重复（v5 重建后必然存在）。
    pub async fn insert_message(
        &self,
        id: &str,
        run_id: &str,
        execution_node_id: &str,
        agent_id: Option<&str>,
        role: &str,
        content_zst: &[u8],
        meta_json: Option<&str>,
        seq: i64,
        created_at: i64,
    ) -> Result<(), StoreError> {
        query(
            "INSERT INTO messages \
             (id, run_id, execution_node_id, agent_id, role, content_zst, meta_json, seq, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(run_id)
        .bind(execution_node_id)
        .bind(agent_id)
        .bind(role)
        .bind(content_zst)
        .bind(meta_json)
        .bind(seq)
        .bind(created_at)
        .execute(self.pool())
        .await
        .map_err(werr)?;
        Ok(())
    }

    // ── 附件（v5；字节 content-addressed 落 files/，DB 存引用）────────

    /// mime → 文件扩展名映射（设计 §3.1 wire 白名单：图像 jpg/png、
    /// 视频 mp4/mkv/mov + ingest 可解的 webp/gif）。
    fn mime_extension(mime: &str) -> Option<&'static str> {
        match mime {
            "image/jpeg" => Some("jpg"),
            "image/png" => Some("png"),
            "image/webp" => Some("webp"),
            "image/gif" => Some("gif"),
            "video/mp4" => Some("mp4"),
            "video/x-matroska" => Some("mkv"),
            "video/quicktime" => Some("mov"),
            _ => None,
        }
    }

    /// 写入一个附件：字节落 `files/{sha256}.{ext}`（content-addressed，
    /// 幂等覆盖同名文件），行进 `attachments` 表。
    ///
    /// - `run_id` 可为 `None`（composer 暂存，Submit 时
    ///   [`Self::bind_attachments`] 绑定）；
    /// - `sha256` / `size_bytes` 由调用方（mobile ingest 层）计算并保证
    ///   与 `data` 一致，store 信任不复算；
    /// - 内存库（无 files_dir）或未映射 mime → 明确报错（fail fast，
    ///   不静默降级）。
    pub async fn insert_attachment(
        &self,
        id: &str,
        run_id: Option<&str>,
        sha256: &str,
        mime: &str,
        filename: Option<&str>,
        size_bytes: i64,
        data: &[u8],
        created_at: i64,
    ) -> Result<(), StoreError> {
        let files_dir = self.files_dir().ok_or_else(|| {
            StoreError::WriteError(
                "attachment writes require a file-backed store; in-memory store has no files_dir"
                    .to_owned(),
            )
        })?;
        let ext = Self::mime_extension(mime).ok_or_else(|| {
            StoreError::WriteError(format!(
                "unsupported attachment mime '{mime}' (no file extension mapping)"
            ))
        })?;

        std::fs::create_dir_all(&files_dir)
            .map_err(|e| StoreError::WriteError(format!("create files_dir failed: {e}")))?;
        let path = files_dir.join(format!("{sha256}.{ext}"));
        std::fs::write(&path, data).map_err(|e| {
            StoreError::WriteError(format!(
                "write attachment file {} failed: {e}",
                path.display()
            ))
        })?;

        query(
            "INSERT INTO attachments (id, run_id, sha256, mime, filename, size_bytes, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(run_id)
        .bind(sha256)
        .bind(mime)
        .bind(filename)
        .bind(size_bytes)
        .bind(created_at)
        .execute(self.pool())
        .await
        .map_err(werr)?;
        Ok(())
    }

    /// 批量把暂存附件（`run_id IS NULL`）绑定到 run（Submit 门控时
    /// mobile 层调用）。返回实际更新的行数（不存在的 id 静默跳过）。
    pub async fn bind_attachments(
        &self,
        ids: &[&str],
        run_id: &str,
    ) -> Result<u64, StoreError> {
        if ids.is_empty() {
            return Ok(0);
        }
        // 占位符来自 ids.len()（内部整数），无注入风险；AssertSqlSafe 仅为
        // 通过 sqlx 的动态 SQL 审计。
        let placeholders = std::iter::repeat("?")
            .take(ids.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("UPDATE attachments SET run_id = ? WHERE id IN ({placeholders})");
        let mut q = query(sqlx::AssertSqlSafe(sql)).bind(run_id);
        for id in ids {
            q = q.bind(*id);
        }
        let result = q.execute(self.pool()).await.map_err(werr)?;
        Ok(result.rows_affected())
    }

    // ── 工具长输出（v6；压缩字节 content-addressed 落 files/outputs/）──

    /// 写长输出文件：`files/outputs/{sha256}.zst`（幂等——文件已存在则
    /// 跳过，跨 run 共享同一份，见 `schema::DDL_TOOL_OUTPUTS`）。
    fn write_tool_output_file(
        outputs_dir: &std::path::Path,
        sha256: &str,
        data_zst: &[u8],
    ) -> Result<(), StoreError> {
        let path = outputs_dir.join(format!("{sha256}.zst"));
        if path.exists() {
            return Ok(());
        }
        std::fs::create_dir_all(outputs_dir)
            .map_err(|e| StoreError::WriteError(format!("create outputs dir failed: {e}")))?;
        std::fs::write(&path, data_zst).map_err(|e| {
            StoreError::WriteError(format!(
                "write tool output file {} failed: {e}",
                path.display()
            ))
        })
    }

    /// 写入一条工具长输出：压缩字节先落 `files/outputs/{sha256}.zst`
    /// （不存在才写），再插 `tool_outputs` 行。
    ///
    /// - `ref_id`：**会话内唯一**短 id（`out-N`，调用方 storer 按 run 内
    ///   自增序号分配；查询侧收 `out-` + 1..16 位数字形态，不做旧
    ///   16hex 兼容）；
    /// - `data_zst`：**调用方传入的已压缩字节**（sha256 是原文的 sha，
    ///     store 信任不复算、不感知压缩参数；v8 起调用方应传 seekable
    ///   分块格式，`frame_index` 与之配套——空 `frame_index` = legacy
    ///   单帧文件，读取端按整篇解压兜底）；
    /// - `frame_index`（v8）：行级帧索引 BLOB（布局见 `schema.rs`
    ///   `DDL_TOOL_OUTPUTS` kdoc；store 不解析，透传入库。同 sha ⇒ 同
    ///   内容 ⇒ 同索引，幂等冲突分支无需刷新）；
    /// - 同 run 同 sha 幂等：`ON CONFLICT(run_id, sha256) DO UPDATE` 仅
    ///   刷新 `created_at`（同 sha ⇒ 同内容，其余计量字段必然一致），
    ///   并**返回既有行的 ref**（`RETURNING`）——调用方拿真实 ref 构造
    ///   落盘回执，序号竞态窗口内也不会出现「回执 ref 与行不符」；
    /// - 同 run 同 ref 撞不同 sha：理论不可达（序号分配在 storer 的串行
    ///   路径上），防御处理——`ON CONFLICT(run_id, ref) DO NOTHING` +
    ///   `RETURNING` 空 → 报错（不覆盖既有行，调用方回落旧截断路径）；
    /// - 内存库（无 files_dir）明确报错（fail fast，对齐
    ///   [`Self::insert_attachment`]——测试库不应静默丢全文）。
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_tool_output(
        &self,
        id: &str,
        run_id: &str,
        ref_id: &str,
        sha256: &str,
        tool_name: &str,
        orig_bytes: i64,
        line_count: i64,
        frame_index: &[u8],
        data_zst: &[u8],
        created_at: i64,
    ) -> Result<String, StoreError> {
        let outputs_dir = self.files_dir().map(|d| d.join("outputs")).ok_or_else(|| {
            StoreError::WriteError(
                "tool output writes require a file-backed store; in-memory store has no files_dir"
                    .to_owned(),
            )
        })?;
        Self::write_tool_output_file(&outputs_dir, sha256, data_zst)?;

        // 双冲突子句：sha 命中 → 幂等刷新 created_at 并回既有行的 ref；
        // ref 命中（防御）→ DO NOTHING，RETURNING 空 → 报错。
        let effective_ref: Option<String> = query_scalar(
            "INSERT INTO tool_outputs \
             (id, run_id, ref, sha256, tool_name, orig_bytes, line_count, frame_index, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(run_id, sha256) DO UPDATE SET created_at = excluded.created_at \
             ON CONFLICT(run_id, ref) DO NOTHING \
             RETURNING ref",
        )
        .bind(id)
        .bind(run_id)
        .bind(ref_id)
        .bind(sha256)
        .bind(tool_name)
        .bind(orig_bytes)
        .bind(line_count)
        .bind(frame_index)
        .bind(created_at)
        .fetch_optional(self.pool())
        .await
        .map_err(werr)?;
        effective_ref.ok_or_else(|| {
            StoreError::WriteError(format!(
                "tool output ref '{ref_id}' already exists in run '{run_id}' with a different \
                 sha256 (ref allocation raced); no row was overwritten"
            ))
        })
    }

    // ── 转写条目（v5；entry_json 透传，store 不解析）──────────────────

    /// 插入一条转写条目。`seq` 由 mobile 层收口 helper 按 recorder 同款
    /// AtomicI64 模式分配（run 内唯一，`idx_transcript_run_seq` 兜底）。
    ///
    /// C1 起 `INSERT OR IGNORE`：同 run 同 seq 冲突静默让位首写行（同
    /// run 同 seq 的内容确定性地来自同一转写下标——自举读失败按 0 重插
    /// 的场景由「致命告警死循环」降级为幂等补写；调用方按 attempted 推
    /// 进游标是安全的）。
    pub async fn insert_transcript_entry(
        &self,
        id: &str,
        run_id: &str,
        seq: i64,
        entry_json: &str,
        created_at: i64,
    ) -> Result<(), StoreError> {
        query(
            "INSERT OR IGNORE INTO transcript_entries (id, run_id, seq, entry_json, created_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(run_id)
        .bind(seq)
        .bind(entry_json)
        .bind(created_at)
        .execute(self.pool())
        .await
        .map_err(werr)?;
        Ok(())
    }

    /// 删除该 run `seq >= from_seq` 的尾部消息行（悬挂尾巴 pop 回写，
    /// 历史转写丢失修复 C2）：open_session / 重启恢复 pop 悬挂 assistant
    /// 后同步删行，修「只改内存不回写 → 反复重开反复 pop」。返回删除
    /// 行数（0 = 无匹配 / run 无消息）。
    pub async fn delete_messages_from_seq(
        &self,
        run_id: &str,
        from_seq: i64,
    ) -> Result<u64, StoreError> {
        let result = query("DELETE FROM messages WHERE run_id = ? AND seq >= ?")
            .bind(run_id)
            .bind(from_seq)
            .execute(self.pool())
            .await
            .map_err(werr)?;
        Ok(result.rows_affected())
    }

    /// 删除该 run 的全部转写条目行（悬挂 pop 重建清场，C2）：pop 后转写
    /// 走消息投影重建，entries 全删 + 调用方失效 SyncState 游标 →
    /// sync_loop 从 0 全量重写（幂等；悬挂是罕见路径，一次全量重写可
    /// 接受）。返回删除行数（0 = 本就无行）。
    pub async fn delete_transcript_entries_for_run(&self, run_id: &str) -> Result<u64, StoreError> {
        let result = query("DELETE FROM transcript_entries WHERE run_id = ?")
            .bind(run_id)
            .execute(self.pool())
            .await
            .map_err(werr)?;
        Ok(result.rows_affected())
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

    // ── ask_user 问卷持久化（v3；ask-user 设计 r4/r5）──────────────────

    /// 插入一条 pending 问卷（request_id = host call id，幂等主键；
    /// 重复插入（同 request_id 重放）按主键冲突报错，由调用方决定语义）。
    pub async fn insert_pending_question(
        &self,
        request_id: &str,
        tool_call_id: &str,
        questions_json: &str,
        version: i64,
        created_at: i64,
    ) -> Result<(), StoreError> {
        query(
            "INSERT INTO pending_questions \
             (request_id, tool_call_id, questions_json, status, version, created_at) \
             VALUES (?, ?, ?, 'pending', ?, ?)",
        )
        .bind(request_id)
        .bind(tool_call_id)
        .bind(questions_json)
        .bind(version)
        .bind(created_at)
        .execute(self.pool())
        .await
        .map_err(werr)?;
        Ok(())
    }

    /// 原子状态迁移（幂等首答者赢）：仅当当前状态等于 `from` 时改为
    /// `to`。返回 `true` = 本次迁移生效；`false` = 状态不匹配（已被
    /// 首答者赢 / 不存在）。任何状态对都可表达，语义由调用方负责
    /// （如 `answered → pending` 用于续跑失败后的回滚重开）。
    pub async fn transition_pending_question(
        &self,
        request_id: &str,
        from: &str,
        to: &str,
    ) -> Result<bool, StoreError> {
        let result = query(
            "UPDATE pending_questions SET status = ? WHERE request_id = ? AND status = ?",
        )
        .bind(to)
        .bind(request_id)
        .bind(from)
        .execute(self.pool())
        .await
        .map_err(werr)?;
        Ok(result.rows_affected() == 1)
    }

    // ── notify 通知等待持久化（v9；notify 设计 S1b，拍板 D1-D3）──────

    /// 插入一条 pending 通知等待行（request_id = host call id，幂等主键；
    /// 工具在发出信封**之前**调用——崩溃窗口内通知已可见则行必在）。
    /// 重复插入（同 request_id 重放）按主键冲突报错，由调用方决定语义。
    /// `args_json` 为完整信封快照（含 `v` 与 `run_id`，D2/D9）。
    pub async fn insert_pending_notification(
        &self,
        request_id: &str,
        tool_call_id: &str,
        run_id: &str,
        args_json: &str,
        created_at: i64,
    ) -> Result<(), StoreError> {
        query(
            "INSERT INTO pending_notifications \
             (request_id, tool_call_id, run_id, args_json, status, created_at) \
             VALUES (?, ?, ?, ?, 'pending', ?)",
        )
        .bind(request_id)
        .bind(tool_call_id)
        .bind(run_id)
        .bind(args_json)
        .bind(created_at)
        .execute(self.pool())
        .await
        .map_err(werr)?;
        Ok(())
    }

    /// 原子状态迁移（幂等首答者赢，`transition_pending_question` 同款）：
    /// 单条 `UPDATE … SET status = ? WHERE request_id = ? AND status = ?`，
    /// `rows_affected == 1` 即本次迁移生效；`false` = 状态不匹配（已被
    /// 首答者赢 / 不存在——timeout 关卷后的迟到上行在此原子落空）。
    pub async fn transition_pending_notification(
        &self,
        request_id: &str,
        from: &str,
        to: &str,
    ) -> Result<bool, StoreError> {
        let result = query(
            "UPDATE pending_notifications SET status = ? WHERE request_id = ? AND status = ?",
        )
        .bind(to)
        .bind(request_id)
        .bind(from)
        .execute(self.pool())
        .await
        .map_err(werr)?;
        Ok(result.rows_affected() == 1)
    }

    // ── ask_user 持久凭据（v4）────────────────────────────────────────

    /// 写入/覆盖一条持久凭据（name 主键 upsert；`remember = true` 的
    /// secret 提交落这里）。`now` 为毫秒时间戳（created_at 首写固定，
    /// updated_at 每次 upsert 刷新）。明文 value 的威胁模型边界见
    /// `schema::DDL_CREDENTIALS` 注释。
    pub async fn upsert_credential(
        &self,
        name: &str,
        kind: &str,
        target: &str,
        value: &str,
        now_ms: i64,
    ) -> Result<(), StoreError> {
        query(
            "INSERT INTO credentials (name, kind, target, value, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?) \
             ON CONFLICT(name) DO UPDATE SET \
               kind = excluded.kind, \
               target = excluded.target, \
               value = excluded.value, \
               updated_at = excluded.updated_at",
        )
        .bind(name)
        .bind(kind)
        .bind(target)
        .bind(value)
        .bind(now_ms)
        .bind(now_ms)
        .execute(self.pool())
        .await
        .map_err(werr)?;
        Ok(())
    }

    /// 删除一条持久凭据（幂等：不存在返回 `false`）。只动本表行——
    /// 会话内存条目 / 已写出的 file 由 vault 侧联动清理。
    pub async fn delete_credential(&self, name: &str) -> Result<bool, StoreError> {
        let result = query("DELETE FROM credentials WHERE name = ?")
            .bind(name)
            .execute(self.pool())
            .await
            .map_err(werr)?;
        Ok(result.rows_affected() == 1)
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

    /// 文件库（附件/文件布局/GC 测试用）。
    async fn setup_file_store(dir: &tempfile::TempDir) -> SqliteStore {
        let path = dir.path().join("test.sqlite");
        let store = SqliteStore::new(path.to_str().expect("utf-8 path"))
            .await
            .expect("file store");
        store.run_migrations().await.expect("migrations");
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

        let meta = r#"{"model_id":"glm-4.7","usage":{"input_tokens":10,"output_tokens":20}}"#;
        store
            .insert_message(
                "msg-1",
                "run-1",
                "enode-1",
                Some("agent-root"),
                "user",
                &crate::codec::encode_content(r#"{"text":"hello"}"#),
                None,
                1,
                1300,
            )
            .await
            .expect("insert message (no meta)");
        store
            .insert_message(
                "msg-2",
                "run-1",
                "enode-1",
                Some("agent-root"),
                "assistant",
                &crate::codec::encode_content(r#"{"text":"world"}"#),
                Some(meta),
                2,
                1301,
            )
            .await
            .expect("insert message (with meta)");

        let msgs = store.list_messages_by_run("run-1").await.expect("read back");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[0].content_json, r#"{"text":"hello"}"#);
        assert_eq!(msgs[0].seq, 1);
        assert!(msgs[0].meta_json.is_none(), "无 meta 的行为 NULL");
        assert_eq!(msgs[1].role, "assistant");
        assert_eq!(msgs[1].content_json, r#"{"text":"world"}"#);
        assert_eq!(
            msgs[1].meta_json.as_deref(),
            Some(meta),
            "meta_json 透传零损"
        );
        assert_eq!(msgs[1].seq, 2);
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

    /// 大文本消息：压缩落盘 + 解压零损往返 + 高冗余文本实际变小。
    #[tokio::test]
    async fn test_large_content_compresses_and_roundtrips() {
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
                &crate::codec::encode_content(&large_content),
                None,
                1,
                1600,
            )
            .await
            .expect("large insert should succeed");

        let blob_len: i64 = query_scalar::<_, i64>(
            "SELECT LENGTH(content_zst) FROM messages WHERE id = 'msg-big'",
        )
        .fetch_one(store.pool())
        .await
        .expect("query blob length");
        assert!(
            blob_len < 200_000,
            "1MB 高冗余文本压缩后应远小于原大, got {blob_len}"
        );

        let loaded = store.list_messages_by_run("run-1").await.expect("read");
        assert_eq!(loaded[0].content_json.len(), 1_048_576, "解压零损");
        assert_eq!(loaded[0].content_json, large_content);
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
                &crate::codec::encode_content("\"hi\""),
                None,
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

        // v5 新子表也要随 run 删除（行级塞数据，不涉文件）
        sqlx::query(
            "INSERT INTO attachments (id, run_id, sha256, mime, filename, size_bytes, created_at) \
             VALUES ('victim-att', 'victim', 'sha-v', 'image/png', NULL, 1, 1500)",
        )
        .execute(store.pool())
        .await
        .expect("victim attachment");
        sqlx::query(
            "INSERT INTO transcript_entries (id, run_id, seq, entry_json, created_at) \
             VALUES ('victim-te', 'victim', 1, '{}', 1500)",
        )
        .execute(store.pool())
        .await
        .expect("victim transcript entry");
        // v6 子表同上（tool_outputs 行级塞数据）
        sqlx::query(
            "INSERT INTO tool_outputs (id, run_id, ref, sha256, tool_name, orig_bytes, line_count, created_at) \
             VALUES ('victim-to', 'victim', 'out-1', 'sha-v-out', 'bash', 100, 5, 1500)",
        )
        .execute(store.pool())
        .await
        .expect("victim tool output");

        let deleted = store.delete_run("victim").await.expect("delete");
        assert_eq!(deleted, 1, "runs 表删除 1 行");

        // 全部子表都不应残留 victim 的行
        for table in [
            "messages",
            "steps",
            "prompt_snapshots",
            "trace_events",
            "audit_log",
            "attachments",
            "transcript_entries",
            "tool_outputs",
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

    // -----------------------------------------------------------------------
    // ask_user 问卷持久化（v3）
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_pending_question_insert_and_first_wins_transition() {
        let store = setup_store().await;
        store
            .insert_pending_question("42", "call-1", r#"[{"id":"q1"}]"#, 1, 1000)
            .await
            .expect("insert");

        // pending → answered：首答者赢（首次成功）。
        assert!(
            store
                .transition_pending_question("42", "pending", "answered")
                .await
                .expect("transition 1")
        );
        // 同一 from 的第二次迁移必须失败（幂等首答者赢）。
        assert!(
            !store
                .transition_pending_question("42", "pending", "declined")
                .await
                .expect("transition 2")
        );
        // 不存在的 request_id 也返回 false。
        assert!(
            !store
                .transition_pending_question("ghost", "pending", "answered")
                .await
                .expect("transition 3")
        );
        // 回滚语义（answered → pending）同样可表达。
        assert!(
            store
                .transition_pending_question("42", "answered", "pending")
                .await
                .expect("rollback")
        );

        let row = store
            .get_pending_question("42")
            .await
            .expect("get")
            .expect("row exists");
        assert_eq!(row.status, "pending");
        assert_eq!(row.tool_call_id, "call-1");
        assert_eq!(row.questions_json, r#"[{"id":"q1"}]"#);
        assert_eq!(row.version, 1);
    }

    #[tokio::test]
    async fn test_list_pending_questions_orders_and_filters() {
        let store = setup_store().await;
        store
            .insert_pending_question("1", "call-a", "[]", 1, 3000)
            .await
            .expect("insert a");
        store
            .insert_pending_question("2", "call-b", "[]", 1, 1000)
            .await
            .expect("insert b");
        assert!(
            store
                .transition_pending_question("2", "pending", "superseded")
                .await
                .expect("close b")
        );

        let pending = store.list_pending_questions().await.expect("list");
        assert_eq!(pending.len(), 1, "只列 pending 状态");
        assert_eq!(pending[0].request_id, "1");
    }

    #[tokio::test]
    async fn test_pending_question_rejects_unknown_status() {
        let store = setup_store().await;
        // CHECK 约束：非法 status 直接被库拒绝。
        let err = store
            .insert_pending_question("x", "c", "[]", 1, 1)
            .await;
        assert!(err.is_ok(), "insert 本身写 pending（合法）");
        let result = sqlx::query(
            "UPDATE pending_questions SET status = 'weird' WHERE request_id = 'x'",
        )
        .execute(store.pool())
        .await;
        assert!(result.is_err(), "CHECK 约束应拒绝未知状态");
    }

    // -----------------------------------------------------------------------
    // ask_user 持久凭据（v4）
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_credential_upsert_overwrite_and_delete() {
        let store = setup_store().await;
        store
            .upsert_credential("GITHUB_TOKEN", "env", "GITHUB_TOKEN", "v1", 1000)
            .await
            .expect("insert");
        store
            .upsert_credential("file:/tmp/netrc", "file", "/tmp/netrc", "k1", 1000)
            .await
            .expect("insert file");

        // 同名 upsert = 覆盖（value/updated_at 刷新，created_at 保持）。
        store
            .upsert_credential("GITHUB_TOKEN", "env", "GITHUB_TOKEN", "v2", 2000)
            .await
            .expect("overwrite");

        assert_eq!(
            store.get_credential_value("GITHUB_TOKEN").await.expect("get"),
            Some("v2".to_owned()),
            "明文按需取值（最新覆盖值）"
        );
        let list = store.list_credentials().await.expect("list");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "GITHUB_TOKEN", "按 name 字典序");
        assert_eq!(list[0].kind, "env");
        assert_eq!(list[0].created_at, 1000, "created_at 首写固定");
        assert_eq!(list[0].updated_at, 2000, "updated_at 随 upsert 刷新");
        assert_eq!(list[1].name, "file:/tmp/netrc");
        assert_eq!(list[1].target, "/tmp/netrc");

        // 镜像加载（含明文）。
        let entries = store.load_credentials_with_values().await.expect("load");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "GITHUB_TOKEN");
        assert_eq!(entries[0].value, "v2");

        // 删除：幂等（首删 true，再删 false）。
        assert!(store.delete_credential("GITHUB_TOKEN").await.expect("del"));
        assert!(!store.delete_credential("GITHUB_TOKEN").await.expect("del again"));
        assert_eq!(
            store.list_credentials().await.expect("list").len(),
            1,
            "file 行保留"
        );
    }

    #[tokio::test]
    async fn test_credential_rejects_unknown_kind() {
        let store = setup_store().await;
        let result = store
            .upsert_credential("x", "weird", "x", "v", 1)
            .await;
        assert!(result.is_err(), "CHECK 约束应拒绝未知 kind");
    }

    // -----------------------------------------------------------------------
    // 附件 / 转写条目 / 文件 GC（v5）
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_attachment_crud_file_layout_and_bind() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = setup_file_store(&dir).await;
        store
            .insert_run("run-1", None, "root", "running", "{}", 1000)
            .await
            .expect("seed run");

        // 暂存（未绑定）附件：run_id NULL + 文件落 files/{sha}.png
        store
            .insert_attachment(
                "att-1",
                None,
                "sha-abc",
                "image/png",
                Some("photo.png"),
                4,
                &[1, 2, 3, 4],
                2000,
            )
            .await
            .expect("insert unbound attachment");
        let files_dir = store.files_dir().expect("files_dir");
        let file = files_dir.join("sha-abc.png");
        assert_eq!(
            std::fs::read(&file).expect("file written"),
            vec![1, 2, 3, 4],
            "字节按 sha256 寻址落盘"
        );

        // 绑定到 run（Submit 路径）
        let bound = store
            .bind_attachments(&["att-1", "ghost-id"], "run-1")
            .await
            .expect("bind");
        assert_eq!(bound, 1, "仅存在的 id 被更新");
        let att = store.get_attachment("att-1").await.expect("get").expect("row");
        assert_eq!(att.run_id.as_deref(), Some("run-1"));
        assert_eq!(att.mime, "image/png");
        assert_eq!(att.size_bytes, 4);
        assert_eq!(att.filename.as_deref(), Some("photo.png"));

        // run 视图只列已绑定的
        store
            .insert_attachment(
                "att-2",
                None,
                "sha-def",
                "video/mp4",
                None,
                2,
                &[9, 9],
                2100,
            )
            .await
            .expect("second attachment");
        let listed = store.list_run_attachments("run-1").await.expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "att-1");

        // 同 sha 重复绑定同 run → UNIQUE(run_id, sha256) 拒绝
        let dup = store
            .insert_attachment("att-3", Some("run-1"), "sha-abc", "image/png", None, 4, &[1, 2, 3, 4], 2200)
            .await;
        assert!(dup.is_err(), "UNIQUE(run_id, sha256) 应拒绝 run 内重复");
    }

    #[tokio::test]
    async fn test_attachment_in_memory_store_rejected() {
        let store = setup_store().await;
        let err = store
            .insert_attachment("att-x", None, "sha", "image/png", None, 1, &[1], 1)
            .await
            .expect_err("内存库必须明确拒绝");
        assert!(
            err.to_string().contains("files_dir"),
            "错误应说明 files_dir 缺失, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_attachment_unsupported_mime_rejected() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = setup_file_store(&dir).await;
        let err = store
            .insert_attachment("att-x", None, "sha", "application/pdf", None, 1, &[1], 1)
            .await
            .expect_err("未映射 mime 必须拒绝");
        assert!(
            err.to_string().contains("application/pdf"),
            "错误应指出 mime, got: {err}"
        );
    }

    /// 跨 run 共享同一 sha256 的文件：删一个 run 文件保留（另一 run 仍
    /// 引用），删最后一个引用 run 才删文件。
    #[tokio::test]
    async fn test_delete_run_gc_shared_sha_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = setup_file_store(&dir).await;
        store
            .insert_run("run-a", None, "root", "completed", "{}", 1000)
            .await
            .expect("run a");
        store
            .insert_run("run-b", None, "root", "completed", "{}", 2000)
            .await
            .expect("run b");

        let data = vec![7u8; 16];
        store
            .insert_attachment("att-a", Some("run-a"), "sha-shared", "image/jpeg", None, 16, &data, 1500)
            .await
            .expect("att a");
        store
            .insert_attachment("att-b", Some("run-b"), "sha-shared", "image/jpeg", None, 16, &data, 2500)
            .await
            .expect("att b");
        let file = store.files_dir().expect("dir").join("sha-shared.jpg");
        assert!(file.exists());

        store.delete_run("run-a").await.expect("delete run a");
        assert!(
            file.exists(),
            "run-b 仍引用 → 文件必须保留（跨 run 复用）"
        );

        store.delete_run("run-b").await.expect("delete run b");
        assert!(
            !file.exists(),
            "最后一个引用消失 → 孤儿文件被清扫"
        );
    }

    /// 启动暂存清扫：>7 天的未绑定附件行+文件删除；7 天内暂存与已绑定
    /// 行不受影响。
    #[tokio::test]
    async fn test_gc_stale_unbound_attachments() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = setup_file_store(&dir).await;
        store
            .insert_run("run-1", None, "root", "completed", "{}", 1000)
            .await
            .expect("run");

        let now = 100 * 24 * 60 * 60 * 1000i64; // 任意"现在"
        let eight_days_ago = now - 8 * 24 * 60 * 60 * 1000;
        let one_day_ago = now - 1 * 24 * 60 * 60 * 1000;

        // 旧暂存（应被清）
        store
            .insert_attachment("att-old", None, "sha-old", "image/png", None, 1, &[1], eight_days_ago)
            .await
            .expect("old stale");
        // 新暂存（应保留）
        store
            .insert_attachment("att-new", None, "sha-new", "image/png", None, 1, &[2], one_day_ago)
            .await
            .expect("recent stale");
        // 旧的已绑定行（应保留）
        store
            .insert_attachment("att-bound", Some("run-1"), "sha-bound", "image/png", None, 1, &[3], eight_days_ago)
            .await
            .expect("old bound");

        let deleted = store
            .gc_stale_unbound_attachments(now)
            .await
            .expect("gc");
        assert_eq!(deleted, 1, "只删 >7d 的未绑定行");

        let files = store.files_dir().expect("dir");
        assert!(!files.join("sha-old.png").exists(), "过期暂存文件被清扫");
        assert!(files.join("sha-new.png").exists(), "近期暂存保留");
        assert!(files.join("sha-bound.png").exists(), "已绑定文件保留");
        assert!(
            store.get_attachment("att-bound").await.expect("q").is_some(),
            "绑定行保留"
        );
        assert!(
            store.get_attachment("att-old").await.expect("q").is_none(),
            "过期暂存行已删"
        );
    }

    #[tokio::test]
    async fn test_transcript_entry_insert_and_unique_seq() {
        let store = setup_store().await;
        store
            .insert_run("run-1", None, "root", "running", "{}", 1000)
            .await
            .expect("run");

        store
            .insert_transcript_entry("te-1", "run-1", 1, r#"{"kind":"user"}"#, 2000)
            .await
            .expect("insert 1");
        store
            .insert_transcript_entry("te-2", "run-1", 2, r#"{"kind":"meta"}"#, 2100)
            .await
            .expect("insert 2");

        // C1 起 INSERT OR IGNORE：同 run 同 seq 冲突静默让位首写行（原
        // 「唯一索引报错」语义退役——游标按 0 自举的重插场景从致命告警
        // 死循环降级为幂等补写）。
        store
            .insert_transcript_entry("te-3", "run-1", 1, r#"{"kind":"other"}"#, 2200)
            .await
            .expect("duplicate seq ignored (OR IGNORE)");
        let rows = store.list_transcript_entries("run-1").await.expect("rows");
        assert_eq!(rows.len(), 2, "重复 seq 不新增行");
        assert_eq!(
            rows[0].entry_json, r#"{"kind":"user"}"#,
            "首写行内容胜出（IGNORE 让位）"
        );

        // 不同 run 同 seq 合法（唯一索引按 run 分域）
        store
            .insert_run("run-2", None, "root", "running", "{}", 3000)
            .await
            .expect("run 2");
        store
            .insert_transcript_entry("te-4", "run-2", 1, "{}", 3100)
            .await
            .expect("cross-run same seq ok");
    }

    // -----------------------------------------------------------------------
    // 工具长输出（v6；幂等插入 / 跨 run 共享 / delete_run 级联 / GC）
    // -----------------------------------------------------------------------

    /// 64 个 'a' 的 sha 形状（合法 hex，内容任意——store 信任不复算）。
    fn sha64(seed: char) -> String {
        std::iter::repeat(seed).take(64).collect()
    }

    #[tokio::test]
    async fn test_tool_output_insert_idempotent_same_run_same_sha() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = setup_file_store(&dir).await;
        store
            .insert_run("run-1", None, "root", "running", "{}", 1000)
            .await
            .expect("run");

        let sha = sha64('a');
        let frame_index: &[u8] = &[1, 2, 3, 4, 5];
        let first = store
            .insert_tool_output("out-1", "run-1", "out-1", &sha, "bash", 100, 5, frame_index, &[9, 9], 2000)
            .await
            .expect("first insert");
        assert_eq!(first, "out-1", "新行返回调用方传入的 ref");
        // 同 run 同 sha：幂等——不报错、不新增行、仅刷新 created_at，
        // 且返回既有行的 ref（调用方拿真实 ref 构造回执）。
        let second = store
            .insert_tool_output("out-2", "run-1", "out-2", &sha, "bash", 100, 5, &[7, 7], &[9, 9], 3000)
            .await
            .expect("upsert insert");
        assert_eq!(second, "out-1", "sha 命中 → 既有行的 ref 胜出");

        // frame_index 首写持久化（幂等分支不覆盖：首写的帧索引保留）
        let stored: Vec<u8> =
            query_scalar::<_, Vec<u8>>("SELECT frame_index FROM tool_outputs WHERE run_id = 'run-1'")
                .fetch_one(store.pool())
                .await
                .expect("frame_index");
        assert_eq!(stored, frame_index, "frame_index 列持久化往返");

        let (count, created_at): (i64, i64) =
            sqlx::query_as::<_, (i64, i64)>("SELECT COUNT(*), MAX(created_at) FROM tool_outputs")
                .fetch_one(store.pool())
                .await
                .expect("count");
        assert_eq!(count, 1, "同 run 同 sha 不新增行");
        assert_eq!(created_at, 3000, "created_at 被刷新为最新值");
    }

    /// 同 run 同 ref 撞不同 sha（防御路径）：DO NOTHING + 报错，既有行
    /// 不被覆盖。序号分配在 storer 串行路径上，正常不可达——本测试钉死
    /// 「真撞上也不静默损坏」。
    #[tokio::test]
    async fn test_tool_output_ref_conflict_errors_without_overwrite() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = setup_file_store(&dir).await;
        store
            .insert_run("run-1", None, "root", "running", "{}", 1000)
            .await
            .expect("run");

        store
            .insert_tool_output("out-1", "run-1", "out-1", &sha64('a'), "bash", 10, 1, &[], &[1], 1000)
            .await
            .expect("first");
        let err = store
            .insert_tool_output("out-2", "run-1", "out-1", &sha64('b'), "bash", 10, 1, &[], &[2], 2000)
            .await
            .expect_err("ref 撞不同 sha 必须报错");
        assert!(
            err.to_string().contains("ref 'out-1' already exists"),
            "got: {err}"
        );

        // 既有行原样保留
        let (sha,): (String,) =
            sqlx::query_as::<_, (String,)>("SELECT sha256 FROM tool_outputs WHERE run_id = 'run-1'")
                .fetch_one(store.pool())
                .await
                .expect("row");
        assert_eq!(sha, sha64('a'), "既有行未被覆盖");
    }

    #[tokio::test]
    async fn test_tool_output_cross_run_same_sha_single_file_two_rows() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = setup_file_store(&dir).await;
        store
            .insert_run("run-a", None, "root", "completed", "{}", 1000)
            .await
            .expect("run a");
        store
            .insert_run("run-b", None, "root", "completed", "{}", 2000)
            .await
            .expect("run b");

        let sha = sha64('b');
        store
            .insert_tool_output("out-a", "run-a", "out-1", &sha, "bash", 10, 2, &[9, 9], &[0; 4], 1500)
            .await
            .expect("run a output");
        // ref 序号按 run 独立：run-b 的第一条也从 out-1 起
        store
            .insert_tool_output("out-b", "run-b", "out-1", &sha, "bash", 10, 2, &[9, 9], &[0; 4], 2500)
            .await
            .expect("run b output");

        let count: i64 = query_scalar::<_, i64>("SELECT COUNT(*) FROM tool_outputs")
            .fetch_one(store.pool())
            .await
            .expect("count");
        assert_eq!(count, 2, "行按 run 独立：两行");
        let file = store
            .files_dir()
            .expect("dir")
            .join("outputs")
            .join(format!("{sha}.zst"));
        assert!(file.exists(), "文件按 sha 共享：单文件");
    }

    /// 复刻 `test_delete_run_gc_shared_sha_file` 模式：删 run 级联删行 +
    /// 末引用消失才删文件。
    #[tokio::test]
    async fn test_delete_run_gc_shared_output_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = setup_file_store(&dir).await;
        store
            .insert_run("run-a", None, "root", "completed", "{}", 1000)
            .await
            .expect("run a");
        store
            .insert_run("run-b", None, "root", "completed", "{}", 2000)
            .await
            .expect("run b");

        let sha = sha64('c');
        store
            .insert_tool_output("out-a", "run-a", "out-1", &sha, "bash", 16, 1, &[], &[7; 8], 1500)
            .await
            .expect("out a");
        store
            .insert_tool_output("out-b", "run-b", "out-1", &sha, "bash", 16, 1, &[], &[7; 8], 2500)
            .await
            .expect("out b");
        let file = store
            .files_dir()
            .expect("dir")
            .join("outputs")
            .join(format!("{sha}.zst"));
        assert!(file.exists());

        store.delete_run("run-a").await.expect("delete run a");
        let rows: i64 = query_scalar::<_, i64>("SELECT COUNT(*) FROM tool_outputs")
            .fetch_one(store.pool())
            .await
            .expect("count");
        assert_eq!(rows, 1, "run-a 行已级联删除，run-b 行保留");
        assert!(
            file.exists(),
            "run-b 仍引用 → 文件必须保留（跨 run 共享）"
        );

        store.delete_run("run-b").await.expect("delete run b");
        let rows: i64 = query_scalar::<_, i64>("SELECT COUNT(*) FROM tool_outputs")
            .fetch_one(store.pool())
            .await
            .expect("count");
        assert_eq!(rows, 0);
        assert!(
            !file.exists(),
            "最后一个引用消失 → 孤儿文件被清扫"
        );
    }

    #[tokio::test]
    async fn test_gc_orphan_output_files() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = setup_file_store(&dir).await;
        store
            .insert_run("run-1", None, "root", "completed", "{}", 1000)
            .await
            .expect("run");

        // 有行引用的文件（保留）与无行孤儿文件（清掉）
        let sha_referenced = sha64('d');
        store
            .insert_tool_output(
                "out-keep",
                "run-1",
                "out-1",
                &sha_referenced,
                "bash",
                4,
                1,
                &[],
                &[1, 2, 3, 4],
                2000,
            )
            .await
            .expect("referenced");
        let outputs_dir = store.files_dir().expect("dir").join("outputs");
        let sha_orphan = sha64('e');
        std::fs::write(outputs_dir.join(format!("{sha_orphan}.zst")), b"junk").expect("orphan file");

        let removed = store.gc_orphan_output_files().await.expect("gc");
        assert_eq!(removed, 1, "只清无行引用的文件");
        assert!(outputs_dir.join(format!("{sha_referenced}.zst")).exists());
        assert!(!outputs_dir.join(format!("{sha_orphan}.zst")).exists());

        // 幂等：再跑无东西可清
        assert_eq!(store.gc_orphan_output_files().await.expect("gc again"), 0);
    }

    /// 内存库（无 files_dir）insert 必须明确报错（对齐 insert_attachment
    /// 的 fail-fast）。
    #[tokio::test]
    async fn test_tool_output_in_memory_store_rejected() {
        let store = setup_store().await;
        store
            .insert_run("run-1", None, "root", "running", "{}", 1000)
            .await
            .expect("run");
        let err = store
            .insert_tool_output("out-x", "run-1", "out-1", &sha64('f'), "bash", 1, 1, &[], &[1], 1)
            .await
            .expect_err("内存库必须明确拒绝");
        assert!(
            err.to_string().contains("files_dir"),
            "错误应说明 files_dir 缺失, got: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // 悬挂 pop 回写删除 API（C2；空集 / 越界 / 返回计数）
    // -----------------------------------------------------------------------

    /// C2：`delete_messages_from_seq` 的边界语义。
    #[tokio::test]
    async fn test_delete_messages_from_seq_boundaries() {
        let store = setup_store().await;
        seed_execution_node(&store).await;
        for (id, seq) in [("m-1", 1), ("m-2", 2), ("m-3", 3)] {
            store
                .insert_message(
                    id,
                    "run-1",
                    "enode-1",
                    Some("agent-root"),
                    "user",
                    &crate::codec::encode_content(r#""x""#),
                    None,
                    seq,
                    1300 + seq,
                )
                .await
                .expect("seed message");
        }

        // 越界（高于最大 seq）→ 0 行。
        assert_eq!(
            store.delete_messages_from_seq("run-1", 99).await.expect("del"),
            0,
            "无匹配返回 0"
        );
        // 不存在的 run → 0 行（不报错）。
        assert_eq!(
            store
                .delete_messages_from_seq("no-run", 0)
                .await
                .expect("del"),
            0
        );
        // 尾部删除：seq >= 3 → 恰 1 行。
        assert_eq!(
            store.delete_messages_from_seq("run-1", 3).await.expect("del"),
            1
        );
        // 中点删除：seq >= 2 → 剩下的尾部 1 行。
        assert_eq!(
            store.delete_messages_from_seq("run-1", 2).await.expect("del"),
            1
        );
        // 前缀全删：seq >= 0 → 最后 1 行。
        assert_eq!(
            store.delete_messages_from_seq("run-1", 0).await.expect("del"),
            1
        );
        // 空集：再删 → 0。
        assert_eq!(
            store.delete_messages_from_seq("run-1", 0).await.expect("del"),
            0
        );
        assert_eq!(store.max_message_seq("run-1").await.expect("max"), 0);
    }

    /// C2：`delete_transcript_entries_for_run` 的边界语义（空 run / 不存在
    /// 的 run / 全删计数 / 不误伤其它 run）。
    #[tokio::test]
    async fn test_delete_transcript_entries_for_run_boundaries() {
        let store = setup_store().await;
        store
            .insert_run("run-1", None, "root", "running", "{}", 1000)
            .await
            .expect("run 1");
        store
            .insert_run("run-2", None, "root", "running", "{}", 2000)
            .await
            .expect("run 2");

        // 空集 / 不存在的 run → 0 行。
        assert_eq!(
            store
                .delete_transcript_entries_for_run("run-1")
                .await
                .expect("del"),
            0
        );
        assert_eq!(
            store
                .delete_transcript_entries_for_run("no-run")
                .await
                .expect("del"),
            0
        );

        for seq in 0..3 {
            store
                .insert_transcript_entry(
                    &format!("te-a{seq}"),
                    "run-1",
                    seq,
                    r#"{"kind":"user"}"#,
                    3000 + seq,
                )
                .await
                .expect("seed run-1");
        }
        store
            .insert_transcript_entry("te-b0", "run-2", 0, r#"{"kind":"meta"}"#, 4000)
            .await
            .expect("seed run-2");

        // 全删：返回该 run 全部行数，不误伤其它 run。
        assert_eq!(
            store
                .delete_transcript_entries_for_run("run-1")
                .await
                .expect("del"),
            3
        );
        assert_eq!(
            store.list_transcript_entries("run-1").await.expect("q").len(),
            0
        );
        assert_eq!(
            store.list_transcript_entries("run-2").await.expect("q").len(),
            1,
            "其它 run 不受影响"
        );
        // 幂等：再删 → 0。
        assert_eq!(
            store
                .delete_transcript_entries_for_run("run-1")
                .await
                .expect("del"),
            0
        );
    }
}
