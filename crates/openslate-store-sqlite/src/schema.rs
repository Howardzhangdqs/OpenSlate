//! OpenSlate SQLite 存储的 schema 定义与版本化迁移。
//!
//! 全部 7 张表：`runs`、`execution_nodes`、`steps`、`messages`、
//! `prompt_snapshots`、`audit_log`、`trace_events`。
//!
//! 迁移机制（见 [`crate::store::SqliteStore::run_migrations`]）：
//! - 以 `PRAGMA user_version` 记录当前 schema 版本，目标版本为
//!   [`SCHEMA_VERSION`]；
//! - 每个版本步进封装为独立事务，`user_version` 与 DDL 同事务提交，
//!   崩溃后重跑不会出现半迁移状态；
//! - v0 兼容存量库（无版本号的旧库）与全新库：DDL 全部 `IF NOT EXISTS`，
//!   `ALTER TABLE` 前先经 `pragma_table_info` 检查列是否存在（不再依赖
//!   错误文案字符串匹配）。

use openslate_core::error::StoreError;
use sqlx::AssertSqlSafe;
use sqlx::{query, query_scalar, SqliteConnection};

/// 当前 schema 版本（`PRAGMA user_version` 的目标值）。
///
/// 版本历史：
/// - `1`：基线 —— 7 张表 + 索引（等价旧无版本机制的最终形态，
///   但基线 DDL 已含 CHECK 约束、不含 `runs.cwd`）；
/// - `2`：`runs` 增加 `title` 列；`messages(run_id, seq)` 唯一索引；
///   清理死列 `runs.cwd` 与死索引 `idx_runs_cwd`。
pub const SCHEMA_VERSION: i64 = 2;

/// 基线建表 DDL（v0 → v1 执行；全新库直接建成最终形态）。
///
/// 顺序有意义：被外键引用的表必须先建。
pub fn ddl_statements() -> Vec<&'static str> {
    vec![
        DDL_RUNS,
        DDL_EXECUTION_NODES,
        DDL_STEPS,
        DDL_MESSAGES,
        DDL_PROMPT_SNAPSHOTS,
        DDL_AUDIT_LOG,
        DDL_TRACE_EVENTS,
    ]
}

/// 基线索引 DDL（v0 → v1 执行），全部 `IF NOT EXISTS`，天然幂等。
///
/// 注意：不含 `idx_runs_cwd`（`cwd` 是死列，v2 中清理）；
/// `idx_messages_run_seq_uq` 属于 v2 步进。
pub fn index_statements() -> Vec<&'static str> {
    vec![
        IDX_RUNS_STATUS,
        IDX_RUNS_STARTED_AT,
        IDX_STEPS_RUN_ID,
        IDX_MESSAGES_EXECUTION_NODE_ID,
        IDX_MESSAGES_RUN_SEQ,
        IDX_TRACE_EVENTS_RUN_ID,
        IDX_AUDIT_LOG_RUN_ID,
    ]
}

// ---------------------------------------------------------------------------
// 表 DDL
// ---------------------------------------------------------------------------

// 合法 run 状态：全 workspace 实际写入的终态为 completed / interrupted /
// failed / cancelled；初始态 running；`error` 为预留值（CHECK 约束内联于
// DDL，测试 `status_and_role_checks_embedded_in_ddl` 防漂移）。
const DDL_RUNS: &str = r#"
CREATE TABLE IF NOT EXISTS runs (
    id TEXT PRIMARY KEY,
    title TEXT,
    root_agent_id TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('running','completed','interrupted','error','failed','cancelled')),
    input_json TEXT NOT NULL,
    output_json TEXT,
    started_at INTEGER NOT NULL,
    finished_at INTEGER,
    cost_usd REAL NOT NULL DEFAULT 0
);
"#;

const DDL_EXECUTION_NODES: &str = r#"
CREATE TABLE IF NOT EXISTS execution_nodes (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL,
    agent_id TEXT NOT NULL,
    parent_execution_id TEXT,
    parent_call_id TEXT,
    status TEXT NOT NULL,
    input_json TEXT NOT NULL,
    output_json TEXT,
    started_at INTEGER NOT NULL,
    finished_at INTEGER,
    FOREIGN KEY(run_id) REFERENCES runs(id)
);
"#;

const DDL_STEPS: &str = r#"
CREATE TABLE IF NOT EXISTS steps (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL,
    execution_node_id TEXT NOT NULL,
    agent_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    data_json TEXT NOT NULL,
    seq INTEGER NOT NULL DEFAULT 0,
    started_at INTEGER NOT NULL,
    finished_at INTEGER,
    FOREIGN KEY(run_id) REFERENCES runs(id),
    FOREIGN KEY(execution_node_id) REFERENCES execution_nodes(id)
);
"#;

const DDL_MESSAGES: &str = r#"
CREATE TABLE IF NOT EXISTS messages (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL,
    execution_node_id TEXT NOT NULL,
    agent_id TEXT,
    role TEXT NOT NULL CHECK (role IN ('system','user','assistant','tool')),
    content_json TEXT NOT NULL,
    seq INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    FOREIGN KEY(run_id) REFERENCES runs(id),
    FOREIGN KEY(execution_node_id) REFERENCES execution_nodes(id)
);
"#;

const DDL_PROMPT_SNAPSHOTS: &str = r#"
CREATE TABLE IF NOT EXISTS prompt_snapshots (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL,
    execution_node_id TEXT NOT NULL,
    agent_id TEXT NOT NULL,
    profile_name TEXT NOT NULL,
    source_kind TEXT NOT NULL,
    source_path TEXT,
    content_hash TEXT NOT NULL,
    rendered_prompt TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    FOREIGN KEY(run_id) REFERENCES runs(id),
    FOREIGN KEY(execution_node_id) REFERENCES execution_nodes(id)
);
"#;

const DDL_AUDIT_LOG: &str = r#"
CREATE TABLE IF NOT EXISTS audit_log (
    id TEXT PRIMARY KEY,
    run_id TEXT,
    agent_id TEXT,
    event_type TEXT NOT NULL,
    event_json TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
"#;

const DDL_TRACE_EVENTS: &str = r#"
CREATE TABLE IF NOT EXISTS trace_events (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL,
    execution_node_id TEXT,
    step_id TEXT,
    agent_id TEXT,
    event_name TEXT NOT NULL,
    event_kind TEXT NOT NULL,
    ts_ns INTEGER NOT NULL,
    dur_ns INTEGER,
    track TEXT NOT NULL,
    args_json TEXT,
    FOREIGN KEY(run_id) REFERENCES runs(id)
);
"#;

// ---------------------------------------------------------------------------
// v0 → v1：存量库补列用的 ALTER（先经 pragma_table_info 检查列存在）
// ---------------------------------------------------------------------------

/// v0 存量库（版本号机制引入前）的演进列。
///
/// 元组为 `(表名, 列名, ALTER 语句)`；列已存在则跳过。
/// 注意：`title` 属于 v2 步进，不在此列。
const BASELINE_ALTER_STEPS: &[(&str, &str, &str)] = &[
    (
        "runs",
        "cost_usd",
        "ALTER TABLE runs ADD COLUMN cost_usd REAL NOT NULL DEFAULT 0",
    ),
    (
        "messages",
        "seq",
        "ALTER TABLE messages ADD COLUMN seq INTEGER NOT NULL DEFAULT 0",
    ),
    (
        "steps",
        "seq",
        "ALTER TABLE steps ADD COLUMN seq INTEGER NOT NULL DEFAULT 0",
    ),
];

// ---------------------------------------------------------------------------
// 索引 DDL
// ---------------------------------------------------------------------------

/// 按状态过滤 runs（`get_last_interrupted_run` 等）。
const IDX_RUNS_STATUS: &str = "CREATE INDEX IF NOT EXISTS idx_runs_status ON runs(status)";

/// 按时间排序 runs（`list_runs`）。
const IDX_RUNS_STARTED_AT: &str =
    "CREATE INDEX IF NOT EXISTS idx_runs_started_at ON runs(started_at)";

/// 按运行列出 steps（`list_steps`）。
const IDX_STEPS_RUN_ID: &str = "CREATE INDEX IF NOT EXISTS idx_steps_run_id ON steps(run_id)";

/// 按执行节点列出 messages（`list_messages`）。
const IDX_MESSAGES_EXECUTION_NODE_ID: &str =
    "CREATE INDEX IF NOT EXISTS idx_messages_execution_node_id ON messages(execution_node_id)";

/// 按 seq 加载一个 run 的完整会话（`list_messages_by_run` —— resume 路径）。
const IDX_MESSAGES_RUN_SEQ: &str =
    "CREATE INDEX IF NOT EXISTS idx_messages_run_seq ON messages(run_id, seq)";

/// `messages(run_id, seq)` 唯一索引（v2 新增）。
///
/// 会话顺序是关键语义：同一 run 内 `seq` 必须唯一，否则 resume 加载的
/// 历史顺序不稳定（provider 会拒绝乱序的 `tool_calls`/tool 结果对）。
/// 存量数据若已存在重复 `seq` 导致创建失败，迁移仅告警跳过（best-effort，
/// 不阻断迁移）；重复数据本身依赖派生主键 `msg-{run_id}-{seq}` 的写入端
/// 约定兜底。
const IDX_MESSAGES_RUN_SEQ_UQ: &str =
    "CREATE UNIQUE INDEX IF NOT EXISTS idx_messages_run_seq_uq ON messages(run_id, seq)";

/// 按运行列出 trace events（`list_trace_events`）。
const IDX_TRACE_EVENTS_RUN_ID: &str =
    "CREATE INDEX IF NOT EXISTS idx_trace_events_run_id ON trace_events(run_id)";

/// 按运行列出审计日志（`list_audit_logs`）。
const IDX_AUDIT_LOG_RUN_ID: &str =
    "CREATE INDEX IF NOT EXISTS idx_audit_log_run_id ON audit_log(run_id)";

// ---------------------------------------------------------------------------
// 常量：测试校验用
// ---------------------------------------------------------------------------

/// 期望的表名（建表顺序）。
pub const TABLE_NAMES: &[&str] = &[
    "runs",
    "execution_nodes",
    "steps",
    "messages",
    "prompt_snapshots",
    "audit_log",
    "trace_events",
];

/// 期望的索引名（测试校验用）。含 v2 的唯一索引。
pub const INDEX_NAMES: &[&str] = &[
    "idx_runs_status",
    "idx_runs_started_at",
    "idx_steps_run_id",
    "idx_messages_execution_node_id",
    "idx_messages_run_seq",
    "idx_messages_run_seq_uq",
    "idx_trace_events_run_id",
    "idx_audit_log_run_id",
];

// ---------------------------------------------------------------------------
// 迁移实现（每个版本步进一个事务，由 run_migrations 驱动）
// ---------------------------------------------------------------------------

/// 检查表里是否已存在某列（基于 `pragma_table_info`，替代对
/// "duplicate column name" 错误文案的字符串匹配）。
///
/// `table` 只来自本文件内部常量，无注入风险（AssertSqlSafe 仅为通过
/// sqlx 的动态 SQL 审计）。
async fn column_exists(
    conn: &mut SqliteConnection,
    table: &str,
    column: &str,
) -> Result<bool, StoreError> {
    let exists: Option<i64> =
        query_scalar(AssertSqlSafe(format!(
            "SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?"
        )))
        .bind(column)
        .fetch_optional(&mut *conn)
        .await
        .map_err(|e| StoreError::MigrationError(e.to_string()))?;
    Ok(exists.is_some())
}

/// v0 → v1：基线。全新库直接建成最终形态；存量 v0 库靠 `IF NOT EXISTS`
/// 跳过建表、靠列存在检查跳过已应用的 ALTER。
pub(crate) async fn migrate_v0_to_v1(conn: &mut SqliteConnection) -> Result<(), StoreError> {
    let merr = |e: sqlx::Error| StoreError::MigrationError(e.to_string());

    for ddl in ddl_statements() {
        query(ddl).execute(&mut *conn).await.map_err(merr)?;
    }

    for &(table, column, alter) in BASELINE_ALTER_STEPS {
        if !column_exists(conn, table, column).await? {
            query(alter).execute(&mut *conn).await.map_err(merr)?;
        }
    }

    for idx in index_statements() {
        query(idx).execute(&mut *conn).await.map_err(merr)?;
    }
    Ok(())
}

/// v1 → v2：`runs.title` 列、`messages(run_id, seq)` 唯一索引、清理
/// `runs.cwd` 死列与 `idx_runs_cwd` 死索引。
///
/// 后两步均为 best-effort：存量数据存在重复 `seq`、或 `cwd` 因视图/触发器
/// 等原因无法删除时，仅告警跳过，不阻断迁移。
pub(crate) async fn migrate_v1_to_v2(conn: &mut SqliteConnection) -> Result<(), StoreError> {
    let merr = |e: sqlx::Error| StoreError::MigrationError(e.to_string());

    // 1. runs.title（列存在检查幂等）
    if !column_exists(conn, "runs", "title").await? {
        query("ALTER TABLE runs ADD COLUMN title TEXT")
            .execute(&mut *conn)
            .await
            .map_err(merr)?;
    }

    // 2. messages(run_id, seq) 唯一索引：存量重复数据时告警跳过
    if let Err(e) = query(IDX_MESSAGES_RUN_SEQ_UQ).execute(&mut *conn).await {
        tracing::warn!(
            target: "openslate_store",
            "创建 messages(run_id, seq) 唯一索引失败（存量数据存在重复 seq，本次跳过）: {e}"
        );
    }

    // 3. 清理 runs.cwd 死列与 idx_runs_cwd 死索引（写入端从未写过 cwd）
    if column_exists(conn, "runs", "cwd").await? {
        if let Err(e) = query("DROP INDEX IF EXISTS idx_runs_cwd").execute(&mut *conn).await {
            tracing::warn!(
                target: "openslate_store",
                "删除索引 idx_runs_cwd 失败（跳过，不影响功能）: {e}"
            );
        }
        if let Err(e) = query("ALTER TABLE runs DROP COLUMN cwd")
            .execute(&mut *conn)
            .await
        {
            tracing::warn!(
                target: "openslate_store",
                "删除 runs.cwd 死列失败（跳过，不影响功能）: {e}"
            );
        }
    } else if let Err(e) = query("DROP INDEX IF EXISTS idx_runs_cwd")
        .execute(&mut *conn)
        .await
    {
        tracing::warn!(
            target: "openslate_store",
            "清理残留索引 idx_runs_cwd 失败（跳过）: {e}"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CHECK 约束的期望文本（防止 DDL 内联文本与预期漂移）。
    /// role 与核心 `MessageRole` 一一对应。
    const RUN_STATUS_CHECK: &str =
        "CHECK (status IN ('running','completed','interrupted','error','failed','cancelled'))";
    const MESSAGE_ROLE_CHECK: &str = "CHECK (role IN ('system','user','assistant','tool'))";

    /// CHECK 约束常量与 DDL 内联文本保持一致（防止两处漂移）。
    #[test]
    fn status_and_role_checks_embedded_in_ddl() {
        assert!(DDL_RUNS.contains(RUN_STATUS_CHECK));
        assert!(DDL_MESSAGES.contains(MESSAGE_ROLE_CHECK));
        assert!(!DDL_RUNS.contains("cwd"), "基线 DDL 不应再包含 cwd 死列");
        assert!(!index_statements()
            .iter()
            .any(|idx| idx.contains("idx_runs_cwd")));
    }
}
