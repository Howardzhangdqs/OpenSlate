//! OpenSlate SQLite 存储的 schema 定义与版本化迁移。
//!
//! 全部 11 张表：`runs`、`execution_nodes`、`steps`、`messages`、
//! `prompt_snapshots`、`audit_log`、`trace_events`、`pending_questions`
//! （v3）、`credentials`（v4）、`attachments`（v5）、`transcript_entries`
//! （v5）。
//!
//! 迁移机制（见 [`crate::store::SqliteStore::run_migrations`]）：
//! - 以 `PRAGMA user_version` 记录当前 schema 版本，目标版本为
//!   [`SCHEMA_VERSION`]；
//! - 每个版本步进封装为独立事务，`user_version` 与 DDL 同事务提交，
//!   崩溃后重跑不会出现半迁移状态；
//! - v0 兼容存量库（无版本号的旧库）与全新库：DDL 全部 `IF NOT EXISTS`，
//!   `ALTER TABLE` 前先经 `pragma_table_info` 检查列是否存在（不再依赖
//!   错误文案字符串匹配）；
//! - **v4 → v5 为破坏性迁移**（用户拍板 2026-10-21，软件未发布开发期）：
//!   `messages` 历史直接丢弃重建，不做数据保真复制；`runs` /
//!   `credentials` / `pending_questions` 原样保留（API key、会话列表不丢）。

use openslate_core::error::StoreError;
use sqlx::AssertSqlSafe;
use sqlx::{query, query_scalar, SqliteConnection};

/// 当前 schema 版本（`PRAGMA user_version` 的目标值）。
///
/// 版本历史：
/// - `1`：基线 —— 7 张表 + 索引（等价旧无版本机制的最终形态，
///   但基线 DDL 已含 CHECK 约束、不含 `runs.cwd`）；
/// - `2`：`runs` 增加 `title` 列；`messages(run_id, seq)` 唯一索引；
///   清理死列 `runs.cwd` 与死索引 `idx_runs_cwd`；
/// - `3`：新增 `pending_questions` 表（ask_user 问卷持久化，ask-user
///   设计 r4/r5：request_id 主键 + tool_call_id 关联悬挂调用 + TTL 状态机）。
/// - `4`：新增 `credentials` 表（ask_user secret 持久凭据库：name 主键 +
///   kind env|file + target + value；见 `DDL_CREDENTIALS` 的威胁模型注释）。
/// - `5`：存储 v5（设计 PLAN-storage-v5-multimodal）——① `messages` 整表
///   重建：`content_json TEXT` → `content_zst BLOB NOT NULL`（zstd-3，见
///   `crate::codec`）+ 新增 `meta_json TEXT`（per-message 元数据旁路通道）；
///   ② `runs` 加 `input_tokens`/`output_tokens` 聚合列；③ 新表
///   `attachments`（content-addressed 附件元数据 + `files/` 文件布局）与
///   `transcript_entries`（转写完整性行级持久化）；④ 删除非唯一索引
///   `idx_messages_run_seq`（`idx_messages_run_seq_uq` 最左前缀完全覆盖，
///   不重建）。**破坏性**（用户拍板）：`messages` 历史随 DROP 丢弃，
///   存量脏数据（重复 seq）一并消失，新表 `UNIQUE(run_id, seq)` 必然可建。
pub const SCHEMA_VERSION: i64 = 5;

/// 基线建表 DDL（v0 → v1 执行；全新库直接建成最终形态）。
///
/// 顺序有意义：被外键引用的表必须先建。v5 的 `attachments` /
/// `transcript_entries` 只引用 `runs`，追加在基线集合尾部；全新库随后
/// 仍会逐版本步进到 v5（步进 DDL 全部 `IF NOT EXISTS`，幂等）。
pub fn ddl_statements() -> Vec<&'static str> {
    vec![
        DDL_RUNS,
        DDL_EXECUTION_NODES,
        DDL_STEPS,
        DDL_MESSAGES,
        DDL_PROMPT_SNAPSHOTS,
        DDL_AUDIT_LOG,
        DDL_TRACE_EVENTS,
        DDL_ATTACHMENTS,
        DDL_TRANSCRIPT_ENTRIES,
    ]
}

/// 基线索引 DDL（v0 → v1 执行），全部 `IF NOT EXISTS`，天然幂等。
///
/// 注意：不含 `idx_runs_cwd`（`cwd` 是死列，v2 中清理）；
/// `idx_messages_run_seq_uq` 属于 v2 步进；v5 起不再有非唯一的
/// `idx_messages_run_seq`（uq 最左前缀覆盖，见 `IDX_MESSAGES_RUN_SEQ_UQ`）。
pub fn index_statements() -> Vec<&'static str> {
    vec![
        IDX_RUNS_STATUS,
        IDX_RUNS_STARTED_AT,
        IDX_STEPS_RUN_ID,
        IDX_MESSAGES_EXECUTION_NODE_ID,
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

/// v5 形态的 messages 表：压缩载荷 + per-message 元数据旁路列。
///
/// - `content_zst`：`serde_json::to_string(Message)` 的 zstd(level 3)
///   压缩字节（见 [`crate::codec`]）。v4 及以前是 `content_json TEXT`，
///   v4→v5 迁移**整表重建**（破坏性，历史丢弃），不存在双列并存形态。
/// - `meta_json`：`Option<MessageMeta>` 的 serde JSON（assistant 消息的
///   usage / finish_reason / latency / model id）。user/tool 行为 NULL，
///   单列 NULL 语义干净（元数据不进 content envelope，读端零翻倍）。
const DDL_MESSAGES: &str = r#"
CREATE TABLE IF NOT EXISTS messages (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL,
    execution_node_id TEXT NOT NULL,
    agent_id TEXT,
    role TEXT NOT NULL CHECK (role IN ('system','user','assistant','tool')),
    content_zst BLOB NOT NULL,
    meta_json TEXT,
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

/// ask_user 问卷持久化（v3；ask-user 设计 r4/r5）。
///
/// 无意 FK 到 `runs`：行按 `request_id`（host call id）幂等寻址，
/// `tool_call_id` 只在迟到答案补全时用于定位消息历史中的悬挂调用
/// （可能属于任意 run；行本身随 TTL 24h 过期清理，孤儿行无害）。
const DDL_PENDING_QUESTIONS: &str = r#"
CREATE TABLE IF NOT EXISTS pending_questions (
    request_id TEXT PRIMARY KEY,
    tool_call_id TEXT NOT NULL,
    questions_json TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('pending','answered','declined','superseded','expired')),
    version INTEGER NOT NULL DEFAULT 1,
    created_at INTEGER NOT NULL
);
"#;

/// ask_user 持久凭据库（v4；ask-user 设计 r3/r4「记住此凭据」）。
///
/// 行来源：用户在 ask_user secret 题型提交时勾选「记住此凭据」。
/// - `name`：凭据键（env 变量名，或 `file:<path>` 形态的文件键）——
///   vault 侧 lookup/注入的统一寻址，主键；
/// - `kind`：落地形态（`env` = 注入 shell/bash 执行环境，`file` = 写入
///   指定路径）；
/// - `target`：变量名 / 文件路径（与 `name` 的派生关系由 vault 侧维护）；
/// - `value`：明文凭据。
///
/// ## 加密边界（已拍板的工程取舍）
///
/// `value` 以**明文**存于 app 私有 sqlite。威胁模型：防意外泄漏（回显 /
/// 日志 / 转写 / 误分享数据库文件时的直接可读性），依赖 Android 沙箱
/// （app 私有目录，其他应用与普通 adb 不可读）。**不防** root、调试
/// 备份提取与对 app 本身的攻陷；也**不承诺**防模型诱导 shell 间接拼接
/// 输出（设计 §0 防护边界）。升级路径：Android Keystore 包裹对称密钥
/// 经 FFI 注入、value 列改密文（表结构已把 value 隔离为单列，替换点
/// 集中在 vault 读写两侧）。不引入 SQLCipher（不为本表新增依赖）。
///
/// 无 FK 到 `runs`：凭据跨会话存续，与单个 run 无从属关系。
const DDL_CREDENTIALS: &str = r#"
CREATE TABLE IF NOT EXISTS credentials (
    name TEXT PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('env','file')),
    target TEXT NOT NULL,
    value TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
"#;

/// 附件元数据（v5；设计 §3.1）。字节进文件系统（content-addressed：
/// `{db 父目录}/files/{sha256}.{ext}`，跨 run 按 sha 复用、天然去重），
/// DB 只存引用。
///
/// - `run_id` 可空：用户在 composer 选图时 run 可能尚未创建；Submit 时
///   由 mobile 层批量绑定（`bind_attachments`）。SQLite FK 对 NULL 父值
///   放行。
/// - `UNIQUE(run_id, sha256)`：run 内去重；跨 run 复用靠相同 sha256 指向
///   同一文件。
/// - ingest（mime 嗅探/归一化/写文件）在 openslate-mobile，store 只管
///   行与文件生命周期（`insert_attachment` / `delete_run` GC / 启动暂存
///   清扫）。
const DDL_ATTACHMENTS: &str = r#"
CREATE TABLE IF NOT EXISTS attachments (
    id TEXT PRIMARY KEY,
    run_id TEXT,
    sha256 TEXT NOT NULL,
    mime TEXT NOT NULL,
    filename TEXT,
    size_bytes INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE(run_id, sha256),
    FOREIGN KEY(run_id) REFERENCES runs(id)
);
"#;

/// 转写条目（v5；设计 §4）。一行一条 `EntryDto`（serde JSON 透传，store
/// 不解析）；`entry_json` 不压缩（单条几百字节，压缩负收益）。
///
/// 与 `steps` 的语义边界：steps 是执行步骤；Approval/Delegate/StepBreak/
/// Meta 不是执行步骤，复用即语义盗用。行级写入天然增量，"回合中途崩溃"
/// 也不丢整回合 UI 条目（Approval 恰好多发生在回合中）。重建逻辑
/// （mobile 层）优先读本表，空则回退 message 投影。
const DDL_TRANSCRIPT_ENTRIES: &str = r#"
CREATE TABLE IF NOT EXISTS transcript_entries (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    entry_json TEXT NOT NULL,
    created_at INTEGER NOT NULL,
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
///
/// v5 起此**非唯一**索引不再存在：`idx_messages_run_seq_uq` 最左前缀
/// `(run_id, seq)` 完全覆盖其查询计划，v4→v5 随 `DROP TABLE messages`
/// 一并消失且不重建。

/// `messages(run_id, seq)` 唯一索引（v2 新增）。
///
/// 会话顺序是关键语义：同一 run 内 `seq` 必须唯一，否则 resume 加载的
/// 历史顺序不稳定（provider 会拒绝乱序的 `tool_calls`/tool 结果对）。
/// 存量数据若已存在重复 `seq` 导致创建失败，迁移仅告警跳过（best-effort，
/// 不阻断迁移）；重复数据本身依赖派生主键 `msg-{run_id}-{seq}` 的写入端
/// 约定兜底。v5 的破坏性迁移 DROP 整表后重建，新表从零开始，唯一索引
/// 必然可建。
const IDX_MESSAGES_RUN_SEQ_UQ: &str =
    "CREATE UNIQUE INDEX IF NOT EXISTS idx_messages_run_seq_uq ON messages(run_id, seq)";

/// 按运行列出 trace events（`list_trace_events`）。
const IDX_TRACE_EVENTS_RUN_ID: &str =
    "CREATE INDEX IF NOT EXISTS idx_trace_events_run_id ON trace_events(run_id)";

/// 按运行列出审计日志（`list_audit_logs`）。
const IDX_AUDIT_LOG_RUN_ID: &str =
    "CREATE INDEX IF NOT EXISTS idx_audit_log_run_id ON audit_log(run_id)";

/// 问卷按状态过滤（快照回填 pending 清单 / TTL 清理扫描）。
/// v3 步进创建（同 `idx_messages_run_seq_uq` 先例：不进基线索引集，
/// 全新库也经 v2→v3 步进获得）。
const IDX_PENDING_QUESTIONS_STATUS: &str =
    "CREATE INDEX IF NOT EXISTS idx_pending_questions_status ON pending_questions(status)";

/// 附件按内容寻址定位（v5）：`delete_run` / 启动 GC 的孤儿文件判定做
/// 行级 `SELECT 1 FROM attachments WHERE sha256 = ?`，走本索引。
const IDX_ATTACHMENTS_SHA256: &str =
    "CREATE INDEX IF NOT EXISTS idx_attachments_sha256 ON attachments(sha256)";

/// 转写条目按 (run_id, seq) 唯一定序（v5）。UNIQUE：seq 由 mobile 层
/// 的收口 helper 按 recorder 同款 AtomicI64 模式分配，行级顺序即转写
/// 顺序。
const IDX_TRANSCRIPT_RUN_SEQ: &str =
    "CREATE UNIQUE INDEX IF NOT EXISTS idx_transcript_run_seq ON transcript_entries(run_id, seq)";

// ---------------------------------------------------------------------------
// 常量：测试校验用
// ---------------------------------------------------------------------------

/// 期望的表名（建表顺序）。含 v3 的 `pending_questions`、v4 的
/// `credentials`、v5 的 `attachments` 与 `transcript_entries`。
pub const TABLE_NAMES: &[&str] = &[
    "runs",
    "execution_nodes",
    "steps",
    "messages",
    "prompt_snapshots",
    "audit_log",
    "trace_events",
    "pending_questions",
    "credentials",
    "attachments",
    "transcript_entries",
];

/// 期望的索引名（测试校验用）。含 v2 的唯一索引、v3 的问卷状态索引、
/// v5 的两个新索引；**不含** `idx_messages_run_seq`（v5 起随表重建消亡，
/// uq 最左前缀覆盖）。
pub const INDEX_NAMES: &[&str] = &[
    "idx_runs_status",
    "idx_runs_started_at",
    "idx_steps_run_id",
    "idx_messages_execution_node_id",
    "idx_messages_run_seq_uq",
    "idx_trace_events_run_id",
    "idx_audit_log_run_id",
    "idx_pending_questions_status",
    "idx_attachments_sha256",
    "idx_transcript_run_seq",
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

/// v2 → v3：`pending_questions` 表 + 状态索引（全部 `IF NOT EXISTS`，
/// 全新库与存量库同一路径，天然幂等）。
pub(crate) async fn migrate_v2_to_v3(conn: &mut SqliteConnection) -> Result<(), StoreError> {
    let merr = |e: sqlx::Error| StoreError::MigrationError(e.to_string());
    query(DDL_PENDING_QUESTIONS)
        .execute(&mut *conn)
        .await
        .map_err(merr)?;
    query(IDX_PENDING_QUESTIONS_STATUS)
        .execute(&mut *conn)
        .await
        .map_err(merr)?;
    Ok(())
}

/// v3 → v4：`credentials` 表（ask_user 持久凭据库，全部 `IF NOT EXISTS`，
/// 全新库与存量库同一路径，天然幂等）。无附加索引（name 主键即全部
/// 访问路径；行数量级 = 用户手动记住的凭据数，极小）。
pub(crate) async fn migrate_v3_to_v4(conn: &mut SqliteConnection) -> Result<(), StoreError> {
    let merr = |e: sqlx::Error| StoreError::MigrationError(e.to_string());
    query(DDL_CREDENTIALS)
        .execute(&mut *conn)
        .await
        .map_err(merr)?;
    Ok(())
}

/// v4 → v5：**破坏性**迁移（用户拍板 2026-10-21，见模块文档）。单事务内：
///
/// 1. `DROP TABLE IF EXISTS messages`（连带其全部索引，含非唯一的
///    `idx_messages_run_seq`；旧消息历史丢弃，存量脏数据一并消失）；
/// 2. 以最终 DDL 重建 `messages`（`content_zst BLOB NOT NULL` +
///    `meta_json TEXT`；无任何表 FK 引用 messages，重建无级联风险，
///    亦无需临时表 + RENAME）；
/// 3. 建 messages 两个索引（uq + execution_node_id）；
/// 4. `runs` 加 `input_tokens` / `output_tokens` 聚合列（column_exists
///    守卫，幂等；存量 run 无 meta → 保持默认 0，不回填）；
/// 5. `attachments` / `transcript_entries` 建表建索引（IF NOT EXISTS）。
///
/// `PRAGMA user_version = 5` 由 `run_migrations` 驱动器在本事务内写入；
/// 步进事务提交后驱动器做一次 `wal_checkpoint(TRUNCATE)` 回收 WAL
/// （迁移 O(1) DDL，无数据复制耗时风险）。
pub(crate) async fn migrate_v4_to_v5(conn: &mut SqliteConnection) -> Result<(), StoreError> {
    let merr = |e: sqlx::Error| StoreError::MigrationError(e.to_string());

    // 1. 破坏性 DROP（历史丢弃，用户已授权）
    query("DROP TABLE IF EXISTS messages")
        .execute(&mut *conn)
        .await
        .map_err(merr)?;

    // 2. 以最终 DDL 重建（IF NOT EXISTS：全新库路径若基线已建成同形态则跳过）
    query(DDL_MESSAGES)
        .execute(&mut *conn)
        .await
        .map_err(merr)?;

    // 3. messages 索引
    query(IDX_MESSAGES_RUN_SEQ_UQ)
        .execute(&mut *conn)
        .await
        .map_err(merr)?;
    query(IDX_MESSAGES_EXECUTION_NODE_ID)
        .execute(&mut *conn)
        .await
        .map_err(merr)?;

    // 4. runs 聚合列（幂等守卫）
    if !column_exists(conn, "runs", "input_tokens").await? {
        query("ALTER TABLE runs ADD COLUMN input_tokens INTEGER NOT NULL DEFAULT 0")
            .execute(&mut *conn)
            .await
            .map_err(merr)?;
    }
    if !column_exists(conn, "runs", "output_tokens").await? {
        query("ALTER TABLE runs ADD COLUMN output_tokens INTEGER NOT NULL DEFAULT 0")
            .execute(&mut *conn)
            .await
            .map_err(merr)?;
    }

    // 5. 新表 + 索引（IF NOT EXISTS，幂等）
    query(DDL_ATTACHMENTS)
        .execute(&mut *conn)
        .await
        .map_err(merr)?;
    query(IDX_ATTACHMENTS_SHA256)
        .execute(&mut *conn)
        .await
        .map_err(merr)?;
    query(DDL_TRANSCRIPT_ENTRIES)
        .execute(&mut *conn)
        .await
        .map_err(merr)?;
    query(IDX_TRANSCRIPT_RUN_SEQ)
        .execute(&mut *conn)
        .await
        .map_err(merr)?;

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
    const CREDENTIAL_KIND_CHECK: &str = "CHECK (kind IN ('env','file'))";

    /// CHECK 约束常量与 DDL 内联文本保持一致（防止两处漂移）。
    #[test]
    fn status_and_role_checks_embedded_in_ddl() {
        assert!(DDL_RUNS.contains(RUN_STATUS_CHECK));
        assert!(DDL_MESSAGES.contains(MESSAGE_ROLE_CHECK));
        assert!(DDL_CREDENTIALS.contains(CREDENTIAL_KIND_CHECK));
        assert!(!DDL_RUNS.contains("cwd"), "基线 DDL 不应再包含 cwd 死列");
        assert!(!index_statements()
            .iter()
            .any(|idx| idx.contains("idx_runs_cwd")));
    }
}
