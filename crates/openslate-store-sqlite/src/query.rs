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
    /// run 级 token 聚合（v5，来自 `RunRecorder::finish_with` 的
    /// `RunFinal`）。存量 run（v5 前）无 meta → 保持 0，不回填。
    pub input_tokens: i64,
    pub output_tokens: i64,
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
///
/// `content_json` 是 `content_zst` 列**解压后**的 JSON 文本（解压在
/// store 侧完成后交付；损坏帧经 [`crate::codec::decode_content`] 报带
/// 消息 id 的错误，绝不静默丢行）。`meta_json` 为 per-message 元数据
/// 旁路通道（assistant 行的 `MessageMeta` serde JSON；user/tool 行 None）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageRecord {
    pub id: String,
    pub run_id: String,
    pub execution_node_id: String,
    pub agent_id: Option<String>,
    pub role: String,
    pub content_json: String,
    pub meta_json: Option<String>,
    /// Per-run monotonic sequence number (writer-assigned). Readers order by
    /// it — `created_at` alone cannot order same-batch inserts, and a
    /// provider rejects shuffled `tool_calls`/tool-result pairs.
    pub seq: i64,
    pub created_at: i64,
}

/// An attachment record (v5). 元数据行——字节在 `files/{sha256}.{ext}`
/// （content-addressed）；本记录是引用形状，wire/DB 同构（设计 §3.1/§6.1）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentRecord {
    /// `att-<uuid>`。
    pub id: String,
    /// `None` = 已导入未绑定（composer 暂存；Submit 时 `bind_attachments`）。
    pub run_id: Option<String>,
    pub sha256: String,
    /// 归一化后的 mime（ingest 层保证白名单）。
    pub mime: String,
    pub filename: Option<String>,
    /// 归一化后字节数。
    pub size_bytes: i64,
    pub created_at: i64,
}

/// A transcript entry record (v5). `entry_json` 为 `EntryDto` 的 serde
/// JSON **透传**——store 不解析，重建/回放语义在 mobile/protocol 层。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptEntryRecord {
    pub id: String,
    pub run_id: String,
    pub seq: i64,
    pub entry_json: String,
    pub created_at: i64,
}

/// A tool output record (v6; v7 加 `ref`). 元数据行——压缩字节在
/// `files/outputs/{sha256}.zst`（content-addressed，跨 run 共享）；
/// `orig_bytes` / `line_count` 是**压缩前**的原文计量（详情弹窗全文
/// 闭环按 `orig_bytes` 校验解压完整性）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolOutputRecord {
    /// `out-<uuid simple>`（行 id，行级唯一；与 ref 载体不同）。
    pub id: String,
    pub run_id: String,
    /// 会话内唯一短 id（`out-N`；消息 content 里 `[STORED OUTPUT
    /// ref="out-…"` 标记携带，由 storer 按 run 内自增序号分配）。
    pub ref_id: String,
    /// 全 64 hex（文件名与内容寻址键）。
    pub sha256: String,
    pub tool_name: String,
    /// 原文字节数（压缩前）。
    pub orig_bytes: i64,
    /// 原文行数（`str::lines()` 语义）。
    pub line_count: i64,
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

/// An ask_user pending-question record（v3；ask-user 设计 r4/r5）。
/// `status` 用小写词表（pending/answered/declined/superseded/expired），
/// 与协议 `QuestionStatus` 的映射由调用方（mobile 装配层）负责。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingQuestionRecord {
    /// 幂等主键（host call id）。
    pub request_id: String,
    /// 关联消息历史中的悬挂 tool call（assistant `tool_calls[].id`）。
    pub tool_call_id: String,
    /// 问卷负载（§2 归一化后的 questions 数组 JSON，协议透传不解释）。
    pub questions_json: String,
    pub status: String,
    pub version: i64,
    pub created_at: i64,
}

/// ask_user 持久凭据行（设置页列表 / vault 镜像管理的**无明文**视图）。
///
/// 刻意不含 `value`：列表/管理路径只走元数据，明文只在 vault 取值时经
/// [`SqliteStore::get_credential_value`] / [`SqliteStore::load_credentials_with_values`]
/// 按需取（防明文随 Debug/日志意外扩散）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialRecord {
    /// 凭据键：env 变量名或 `file:<path>`。
    pub name: String,
    /// 落地形态（env|file）。
    pub kind: String,
    /// 变量名 / 文件路径。
    pub target: String,
    pub created_at: i64,
    pub updated_at: i64,
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
    i64,            // input_tokens
    i64,            // output_tokens
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
    Vec<u8>,        // content_zst（解压前字节）
    Option<String>, // meta_json
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

type PendingQuestionRow = (
    String, // request_id
    String, // tool_call_id
    String, // questions_json
    String, // status
    i64,    // version
    i64,    // created_at
);

type CredentialRow = (
    String, // name
    String, // kind
    String, // target
    i64,    // created_at
    i64,    // updated_at
);

type AttachmentRow = (
    String,         // id
    Option<String>, // run_id
    String,         // sha256
    String,         // mime
    Option<String>, // filename
    i64,            // size_bytes
    i64,            // created_at
);

type TranscriptEntryRow = (
    String, // id
    String, // run_id
    i64,    // seq
    String, // entry_json
    i64,    // created_at
);

type ToolOutputRow = (
    String, // id
    String, // run_id
    String, // ref
    String, // sha256
    String, // tool_name
    i64,    // orig_bytes
    i64,    // line_count
    i64,    // created_at
);

/// vault 启动镜像加载的凭据行（**含明文**；仅 vault 内部消费，绝不
/// derive Debug / 打日志）。
pub struct CredentialSecretEntry {
    pub name: String,
    pub kind: String,
    pub target: String,
    pub value: String,
}

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
        input_tokens: row.9,
        output_tokens: row.10,
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

/// 解压 `content_zst` 并组装 `MessageRecord`。损坏帧报带消息 id 的错误。
fn row_to_message(row: MessageRow) -> Result<MessageRecord, StoreError> {
    let (id, run_id, execution_node_id, agent_id, role, content_zst, meta_json, seq, created_at) = row;
    Ok(MessageRecord {
        content_json: crate::codec::decode_content(&id, &content_zst)?,
        id,
        run_id,
        execution_node_id,
        agent_id,
        role,
        meta_json,
        seq,
        created_at,
    })
}

fn row_to_attachment(row: AttachmentRow) -> AttachmentRecord {
    AttachmentRecord {
        id: row.0,
        run_id: row.1,
        sha256: row.2,
        mime: row.3,
        filename: row.4,
        size_bytes: row.5,
        created_at: row.6,
    }
}

fn row_to_transcript_entry(row: TranscriptEntryRow) -> TranscriptEntryRecord {
    TranscriptEntryRecord {
        id: row.0,
        run_id: row.1,
        seq: row.2,
        entry_json: row.3,
        created_at: row.4,
    }
}

fn row_to_tool_output(row: ToolOutputRow) -> ToolOutputRecord {
    ToolOutputRecord {
        id: row.0,
        run_id: row.1,
        ref_id: row.2,
        sha256: row.3,
        tool_name: row.4,
        orig_bytes: row.5,
        line_count: row.6,
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

fn row_to_pending_question(row: PendingQuestionRow) -> PendingQuestionRecord {
    PendingQuestionRecord {
        request_id: row.0,
        tool_call_id: row.1,
        questions_json: row.2,
        status: row.3,
        version: row.4,
        created_at: row.5,
    }
}

fn row_to_credential(row: CredentialRow) -> CredentialRecord {
    CredentialRecord {
        name: row.0,
        kind: row.1,
        target: row.2,
        created_at: row.3,
        updated_at: row.4,
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
            "SELECT id, title, root_agent_id, status, input_json, output_json, started_at, finished_at, cost_usd, \
                    input_tokens, output_tokens \
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
            "SELECT id, title, root_agent_id, status, input_json, output_json, started_at, finished_at, cost_usd, \
                    input_tokens, output_tokens \
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
            "SELECT id, run_id, execution_node_id, agent_id, role, content_zst, meta_json, seq, created_at \
             FROM messages WHERE execution_node_id = ? ORDER BY seq",
        )
        .bind(execution_node_id)
        .fetch_all(pool)
        .await
        .map_err(qerr)?;

        rows.into_iter().map(row_to_message).collect()
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
            "SELECT id, run_id, execution_node_id, agent_id, role, content_zst, meta_json, seq, created_at \
             FROM messages WHERE run_id = ? ORDER BY seq",
        )
        .bind(run_id)
        .fetch_all(pool)
        .await
        .map_err(qerr)?;

        rows.into_iter().map(row_to_message).collect()
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
            "SELECT id, run_id, execution_node_id, agent_id, role, content_zst, meta_json, seq, created_at \
             FROM messages WHERE run_id = ? AND seq > ? ORDER BY seq ASC LIMIT ?",
        )
        .bind(run_id)
        .bind(after_seq)
        .bind(limit)
        .fetch_all(pool)
        .await
        .map_err(qerr)?;

        rows.into_iter().map(row_to_message).collect()
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
            "SELECT id, title, root_agent_id, status, input_json, output_json, started_at, finished_at, cost_usd, \
                    input_tokens, output_tokens \
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
            "SELECT id, title, root_agent_id, status, input_json, output_json, started_at, finished_at, cost_usd, \
                    input_tokens, output_tokens \
             FROM runs WHERE status != 'failed' \
             ORDER BY started_at DESC, id DESC LIMIT 1",
        )
        .fetch_optional(pool)
        .await
        .map_err(qerr)?;

        Ok(row.map(row_to_run))
    }

    /// List pending ask_user questionnaires（snapshot 回填 / 迟到答案
    /// 校验的数据源），按 `created_at` 升序（下发顺序稳定）。
    pub async fn list_pending_questions(&self) -> Result<Vec<PendingQuestionRecord>, StoreError> {
        let pool = self.pool();
        let rows: Vec<PendingQuestionRow> = query_as(
            "SELECT request_id, tool_call_id, questions_json, status, version, created_at \
             FROM pending_questions WHERE status = 'pending' ORDER BY created_at",
        )
        .fetch_all(pool)
        .await
        .map_err(qerr)?;

        Ok(rows.into_iter().map(row_to_pending_question).collect())
    }

    /// 取单条问卷（任意状态；迟到答案路径用它做 pending 前置校验，
    /// 避免先 UPDATE 再读的竞态——真正的首答者赢由
    /// [`crate::write::SqliteStore::transition_pending_question`] 原子保证）。
    pub async fn get_pending_question(
        &self,
        request_id: &str,
    ) -> Result<Option<PendingQuestionRecord>, StoreError> {
        let pool = self.pool();
        let row: Option<PendingQuestionRow> = query_as(
            "SELECT request_id, tool_call_id, questions_json, status, version, created_at \
             FROM pending_questions WHERE request_id = ?",
        )
        .bind(request_id)
        .fetch_optional(pool)
        .await
        .map_err(qerr)?;

        Ok(row.map(row_to_pending_question))
    }

    // ── ask_user 持久凭据（v4）────────────────────────────────────────

    /// 全部持久凭据的**无明文**清单（设置页 / 管理面），按 name 字典序。
    pub async fn list_credentials(&self) -> Result<Vec<CredentialRecord>, StoreError> {
        let pool = self.pool();
        let rows: Vec<CredentialRow> = query_as(
            "SELECT name, kind, target, created_at, updated_at \
             FROM credentials ORDER BY name",
        )
        .fetch_all(pool)
        .await
        .map_err(qerr)?;

        Ok(rows.into_iter().map(row_to_credential).collect())
    }

    /// 单条凭据明文（vault 注入/重写文件用；不存在返回 None）。
    pub async fn get_credential_value(
        &self,
        name: &str,
    ) -> Result<Option<String>, StoreError> {
        let pool = self.pool();
        let value: Option<String> = query_scalar("SELECT value FROM credentials WHERE name = ?")
            .bind(name)
            .fetch_optional(pool)
            .await
            .map_err(qerr)?;
        Ok(value)
    }

    /// vault 启动镜像加载：全部凭据行（含明文）。调用方仅 vault 内部，
    /// 结果不得进日志/Debug（见 [`CredentialSecretEntry`]）。
    pub async fn load_credentials_with_values(
        &self,
    ) -> Result<Vec<CredentialSecretEntry>, StoreError> {
        let pool = self.pool();
        let rows: Vec<(String, String, String, String)> = query_as(
            "SELECT name, kind, target, value FROM credentials ORDER BY name",
        )
        .fetch_all(pool)
        .await
        .map_err(qerr)?;

        Ok(rows
            .into_iter()
            .map(|(name, kind, target, value)| CredentialSecretEntry {
                name,
                kind,
                target,
                value,
            })
            .collect())
    }

    // ── 附件（v5）───────────────────────────────────────────────────

    /// 取单条附件元数据（不存在返回 `None`）。
    pub async fn get_attachment(
        &self,
        id: &str,
    ) -> Result<Option<AttachmentRecord>, StoreError> {
        let pool = self.pool();
        let row: Option<AttachmentRow> = query_as(
            "SELECT id, run_id, sha256, mime, filename, size_bytes, created_at \
             FROM attachments WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(qerr)?;
        Ok(row.map(row_to_attachment))
    }

    /// 列出一个 run 的全部附件（按 `created_at, id` 稳定序）。
    pub async fn list_run_attachments(
        &self,
        run_id: &str,
    ) -> Result<Vec<AttachmentRecord>, StoreError> {
        let pool = self.pool();
        let rows: Vec<AttachmentRow> = query_as(
            "SELECT id, run_id, sha256, mime, filename, size_bytes, created_at \
             FROM attachments WHERE run_id = ? ORDER BY created_at, id",
        )
        .bind(run_id)
        .fetch_all(pool)
        .await
        .map_err(qerr)?;
        Ok(rows.into_iter().map(row_to_attachment).collect())
    }

    // ── 转写条目（v5）───────────────────────────────────────────────

    /// 列出一个 run 的全部转写条目，按 `seq` 升序（行级顺序即转写顺序；
    /// `entry_json` 透传不解析）。空结果 = v5 前的 run 或无条目——调用方
    /// 回退 message 投影（设计 §4 两级策略）。
    pub async fn list_transcript_entries(
        &self,
        run_id: &str,
    ) -> Result<Vec<TranscriptEntryRecord>, StoreError> {
        let pool = self.pool();
        let rows: Vec<TranscriptEntryRow> = query_as(
            "SELECT id, run_id, seq, entry_json, created_at \
             FROM transcript_entries WHERE run_id = ? ORDER BY seq",
        )
        .bind(run_id)
        .fetch_all(pool)
        .await
        .map_err(qerr)?;
        Ok(rows.into_iter().map(row_to_transcript_entry).collect())
    }

    // ── 工具长输出（v6 表 / v7 ref 列）──────────────────────────────

    /// `ref` 形状校验（v7）：`out-` + **1..16 位 hex**（新序号 `out-3`
    /// 与手工造的长形态都收；格式非法直接报错，bind 传参，值不进 SQL
    /// 文本）。
    fn validate_tool_output_ref(ref_id: &str) -> Result<(), StoreError> {
        match ref_id.strip_prefix("out-") {
            Some(hex)
                if !hex.is_empty()
                    && hex.len() <= 16
                    && hex.bytes().all(|b| b.is_ascii_hexdigit()) =>
            {
                Ok(())
            }
            _ => Err(StoreError::QueryError(format!(
                "invalid tool output ref (expected \"out-\" followed by 1-16 hex chars): '{ref_id}'"
            ))),
        }
    }

    /// 按 `(run_id, ref)` 精确查长输出行（详情弹窗全文闭环 / output_query
    /// 的数据源）。
    ///
    /// **严格会话隔离**（用户拍板 2026-10-22）：必须传 `run_id`，`WHERE
    /// run_id = ? AND ref = ?` 精确匹配——一个 run 的 ref 不能被其他
    /// run 解析，跨 run 同 ref（各自 `out-1`）互不可见；无「当前 run
    /// 优先 / 任意 run 回落」路径。未命中返回 `None`（调用方回落消息
    /// content 原文或报未知 ref）。
    pub async fn find_tool_output(
        &self,
        run_id: &str,
        ref_id: &str,
    ) -> Result<Option<ToolOutputRecord>, StoreError> {
        Self::validate_tool_output_ref(ref_id)?;
        let pool = self.pool();
        let row: Option<ToolOutputRow> = query_as(
            "SELECT id, run_id, ref, sha256, tool_name, orig_bytes, line_count, created_at \
             FROM tool_outputs \
             WHERE run_id = ? AND ref = ?",
        )
        .bind(run_id)
        .bind(ref_id)
        .fetch_optional(pool)
        .await
        .map_err(qerr)?;
        Ok(row.map(row_to_tool_output))
    }

    /// 按 `(run_id, sha256)` 查长输出行——storer 的幂等路径：同 run 同
    /// 内容重复落盘时**复用既有行的 ref**（同内容同 ref，序号不因幂等
    /// 跳号）。sha 必须全 64 hex。
    pub async fn find_tool_output_by_sha(
        &self,
        run_id: &str,
        sha256: &str,
    ) -> Result<Option<ToolOutputRecord>, StoreError> {
        if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(StoreError::QueryError(format!(
                "invalid tool output sha256 (expected 64 hex chars): '{sha256}'"
            )));
        }
        let pool = self.pool();
        let row: Option<ToolOutputRow> = query_as(
            "SELECT id, run_id, ref, sha256, tool_name, orig_bytes, line_count, created_at \
             FROM tool_outputs \
             WHERE run_id = ? AND sha256 = ?",
        )
        .bind(run_id)
        .bind(sha256)
        .fetch_optional(pool)
        .await
        .map_err(qerr)?;
        Ok(row.map(row_to_tool_output))
    }

    /// 该 run 现有 ref 序号的最大值（`out-N` 的 N；无行 / 无可解析序号
    /// → 0）。storer 分配下一个 ref 用 `max + 1`。
    ///
    /// **并发约定**：store 的 tool_outputs 写入目前都在 mobile storer 的
    /// 串行路径上（单回合内工具结果顺序落盘），max+1 分配无竞态窗口；
    /// 真并发撞号由 `idx_tool_outputs_run_ref` 唯一索引兜底（insert 报
    /// 错 → storer 回落旧截断路径，不产生损坏数据）。
    pub async fn max_tool_output_ref_num(&self, run_id: &str) -> Result<i64, StoreError> {
        let pool = self.pool();
        let refs: Vec<String> = query_scalar("SELECT ref FROM tool_outputs WHERE run_id = ?")
            .bind(run_id)
            .fetch_all(pool)
            .await
            .map_err(qerr)?;
        Ok(refs
            .iter()
            .filter_map(|r| r.strip_prefix("out-"))
            .filter(|tail| !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()))
            .filter_map(|tail| tail.parse::<i64>().ok())
            .max()
            .unwrap_or(0))
    }

    /// 读长输出压缩字节（`files/outputs/{sha256}.zst`）。
    ///
    /// `sha256` 必须为全 64 hex（防路径拼接，bind 场景外的文件名直接
    /// 来自该值）；文件缺失返回 `Ok(None)`（崩溃窗口 / 手动清理后的
    /// 正常回落路径），其他 IO 错误报错。**返回压缩字节**——解压与
    /// `orig_bytes` 校验由调用方（mobile 全文闭环）负责。内存库无
    /// files_dir → `Ok(None)`。
    pub async fn read_tool_output_bytes(
        &self,
        sha256: &str,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(StoreError::QueryError(format!(
                "invalid tool output sha256 (expected 64 hex chars): '{sha256}'"
            )));
        }
        let Some(outputs_dir) = self.files_dir().map(|d| d.join("outputs")) else {
            return Ok(None);
        };
        match std::fs::read(outputs_dir.join(format!("{sha256}.zst"))) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(StoreError::QueryError(format!(
                "read tool output file for sha256 '{sha256}' failed: {e}"
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::SqlitePool;

    /// 测试插入用的合法压缩帧（读取端会真解码 content_zst）。
    fn zst(s: &str) -> Vec<u8> {
        crate::codec::encode_content(s)
    }

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
            "INSERT INTO messages (id, run_id, execution_node_id, agent_id, role, content_zst, seq, created_at) \
             VALUES ('msg-1', 'run-1', 'en-1', 'root', 'user', ?, 1, 1200)",
        )
        .bind(zst("\"hello\""))
        .execute(pool)
        .await
        .expect("insert msg 1");

        sqlx::query(
            "INSERT INTO messages (id, run_id, execution_node_id, agent_id, role, content_zst, seq, created_at) \
             VALUES ('msg-2', 'run-1', 'en-1', 'root', 'assistant', ?, 2, 1200)",
        )
        .bind(zst("\"world\""))
        .execute(pool)
        .await
        .expect("insert msg 2");

        let msgs = store.list_messages("en-1").await.expect("list_messages");
        assert_eq!(msgs.len(), 2);
        // Ordered by seq (created_at is identical here)
        assert_eq!(msgs[0].id, "msg-1");
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[0].seq, 1);
        assert_eq!(msgs[0].content_json, "\"hello\"", "解压后内容零损");
        assert!(msgs[0].meta_json.is_none(), "未写 meta 的行为 NULL");
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
        for (id, node, role, content, seq) in [
            ("m-1", "en-1", "user", "\"q\"", 1i64),
            ("m-2", "en-2", "assistant", "\"a\"", 2),
            ("m-3", "en-1", "tool", "\"t\"", 3),
        ] {
            sqlx::query(
                "INSERT INTO messages (id, run_id, execution_node_id, agent_id, role, content_zst, seq, created_at) \
                 VALUES (?, 'run-1', ?, 'root', ?, ?, ?, 1200)",
            )
            .bind(id)
            .bind(node)
            .bind(role)
            .bind(zst(content))
            .bind(seq)
            .execute(pool)
            .await
            .expect("insert interleaved message");
        }

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
            "INSERT INTO messages (id, run_id, execution_node_id, agent_id, role, content_zst, seq, created_at) \
             VALUES ('m-9', 'run-2', 'en-3', 'root', 'user', ?, 1, 2200)",
        )
        .bind(zst("\"other\""))
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
                "INSERT INTO messages (id, run_id, execution_node_id, agent_id, role, content_zst, seq, created_at) \
                 VALUES (?, 'run-1', 'en-1', 'root', 'user', ?, ?, 1200)",
            )
            .bind(id)
            .bind(zst("\"x\""))
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
                "INSERT INTO messages (id, run_id, execution_node_id, agent_id, role, content_zst, seq, created_at) \
                 VALUES (?, 'run-1', 'en-1', 'root', 'user', ?, ?, 1200)",
            )
            .bind(format!("m-{i}"))
            .bind(zst("\"x\""))
            .bind(seq)
            .execute(pool)
            .await
            .expect("insert run-1 message");
        }
        sqlx::query(
            "INSERT INTO messages (id, run_id, execution_node_id, agent_id, role, content_zst, seq, created_at) \
             VALUES ('other-1', 'run-2', 'en-2', 'root', 'user', ?, 1, 2200)",
        )
        .bind(zst("\"y\""))
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
                "INSERT INTO messages (id, run_id, execution_node_id, agent_id, role, content_zst, seq, created_at) \
                 VALUES (?, 'run-1', 'en-1', 'root', 'user', ?, ?, 1200)",
            )
            .bind(format!("cm-{i}"))
            .bind(zst("\"x\""))
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

    /// 损坏的 content_zst 帧：读取报带消息 id 的错误，绝不静默丢行。
    #[tokio::test]
    async fn test_corrupted_content_zst_fails_with_message_id() {
        let store = setup_store().await;
        let pool = store.pool();

        insert_run(pool, "run-1", None, "root", "running", "{}", 1000).await;
        insert_execution_node(
            pool, "en-1", "run-1", "root", None, None, "running", "{}", 1100,
        )
        .await;
        sqlx::query(
            "INSERT INTO messages (id, run_id, execution_node_id, agent_id, role, content_zst, seq, created_at) \
             VALUES ('corrupt-1', 'run-1', 'en-1', 'root', 'user', x'00', 1, 1200)",
        )
        .execute(pool)
        .await
        .expect("insert corrupted blob");

        let err = store
            .list_messages_by_run("run-1")
            .await
            .expect_err("corrupted frame must surface");
        assert!(
            err.to_string().contains("corrupt-1"),
            "错误信息应携带消息 id, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_attachment_and_transcript_entry_reads() {
        let store = setup_store().await;
        insert_run(&store.pool(), "run-1", None, "root", "running", "{}", 1000).await;

        // 内存库没有 files_dir（insert_attachment 拒绝），读取测试直接塞行
        sqlx::query(
            "INSERT INTO attachments (id, run_id, sha256, mime, filename, size_bytes, created_at) \
             VALUES ('att-1', 'run-1', 'abc', 'image/png', 'a.png', 3, 2000)",
        )
        .execute(store.pool())
        .await
        .expect("seed attachment row");

        let att = store
            .get_attachment("att-1")
            .await
            .expect("get")
            .expect("exists");
        assert_eq!(att.run_id.as_deref(), Some("run-1"));
        assert_eq!(att.sha256, "abc");
        assert_eq!(att.mime, "image/png");
        assert_eq!(att.filename.as_deref(), Some("a.png"));
        assert_eq!(att.size_bytes, 3);

        assert!(
            store.get_attachment("ghost").await.expect("get").is_none(),
            "不存在的附件返回 None"
        );

        let listed = store.list_run_attachments("run-1").await.expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "att-1");

        for (id, seq, json) in [
            ("te-1", 1i64, r#"{"kind":"user"}"#),
            ("te-2", 2, r#"{"kind":"assistant"}"#),
            ("te-0", 3, r#"{"kind":"meta"}"#),
        ] {
            store
                .insert_transcript_entry(id, "run-1", seq, json, 3000)
                .await
                .expect("insert transcript entry");
        }
        let entries = store
            .list_transcript_entries("run-1")
            .await
            .expect("list transcript");
        assert_eq!(
            entries.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            vec!["te-1", "te-2", "te-0"],
            "按 seq 升序而非插入序"
        );
        assert_eq!(entries[0].entry_json, r#"{"kind":"user"}"#);
        assert!(
            store
                .list_transcript_entries("no-run")
                .await
                .expect("empty")
                .is_empty()
        );
    }

    // -----------------------------------------------------------------------
    // 工具长输出（v6 表 / v7 ref）：find 严格会话隔离 / 按 sha 幂等 /
    // 序号 max / 校验 / 字节读取
    // -----------------------------------------------------------------------

    /// 严格会话隔离：`(run_id, ref)` 精确匹配，跨 run 互不可见。
    #[tokio::test]
    async fn test_find_tool_output_strict_run_scoping() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("to_query.sqlite");
        let store = SqliteStore::new(path.to_str().expect("utf-8"))
            .await
            .expect("file store");
        store.run_migrations().await.expect("migrations");
        for (run, at) in [("run-a", 1000i64), ("run-b", 2000)] {
            insert_run(&store.pool(), run, None, "root", "completed", "{}", at).await;
        }

        // 两个 run 各自的 out-1（不同 sha）；run-a 另有 out-2 与一条
        // 长形态 ref（16 hex）——格式上界一并覆盖。
        let sha_a: String = "a".repeat(64);
        let sha_b: String = "b".repeat(64);
        let sha_d: String = "d".repeat(64);
        let sha_legacy: String = "a1b2".to_owned() + &"c".repeat(60);
        let legacy_ref = format!("out-{}", &sha_legacy[..16]);
        for (id, run, ref_id, sha, at) in [
            ("out-a", "run-a", "out-1", sha_a.as_str(), 1500i64),
            ("out-b", "run-b", "out-1", sha_b.as_str(), 2500),
            ("out-c", "run-a", legacy_ref.as_str(), sha_legacy.as_str(), 3500),
            ("out-d", "run-a", "out-2", sha_d.as_str(), 4500),
        ] {
            sqlx::query(
                "INSERT INTO tool_outputs (id, run_id, ref, sha256, tool_name, orig_bytes, line_count, created_at) \
                 VALUES (?, ?, ?, ?, 'bash', 10, 2, ?)",
            )
            .bind(id)
            .bind(run)
            .bind(ref_id)
            .bind(sha)
            .bind(at)
            .execute(store.pool())
            .await
            .expect("seed tool output row");
        }

        // run 内精确命中
        let record = store
            .find_tool_output("run-a", "out-1")
            .await
            .expect("find")
            .expect("hit");
        assert_eq!(record.run_id, "run-a");
        assert_eq!(record.ref_id, "out-1");
        assert_eq!(record.sha256, sha_a);
        assert_eq!(record.tool_name, "bash");
        assert_eq!(record.orig_bytes, 10);
        assert_eq!(record.line_count, 2);

        // 同 ref 不同 run → 各自的行（互不可见 ≠ 互相遮挡）
        let record = store
            .find_tool_output("run-b", "out-1")
            .await
            .expect("find")
            .expect("hit");
        assert_eq!(record.run_id, "run-b");
        assert_eq!(record.sha256, sha_b);

        // 长形态 ref（16 hex 上界）同样精确命中
        let record = store
            .find_tool_output("run-a", &legacy_ref)
            .await
            .expect("find")
            .expect("hit");
        assert_eq!(record.sha256, sha_legacy);

        // **跨会话隔离**：run-a 独有的 ref 在 run-b 不可解析
        assert!(
            store
                .find_tool_output("run-b", "out-2")
                .await
                .expect("find")
                .is_none(),
            "run-a 的 out-2 在 run-b 不可解析"
        );
        assert!(
            store
                .find_tool_output("run-b", &legacy_ref)
                .await
                .expect("find")
                .is_none(),
            "run-a 的 legacy ref 在 run-b 不可解析"
        );
        // 双方都没有的 ref → None
        assert!(
            store
                .find_tool_output("run-a", "out-9")
                .await
                .expect("find")
                .is_none()
        );

        // 非法形状 → Err（缺前缀 / 空 hex / 超 16 位 / 非 hex）
        assert!(store.find_tool_output("run-a", "out-").await.is_err());
        assert!(store.find_tool_output("run-a", "1").await.is_err());
        assert!(
            store
                .find_tool_output("run-a", &format!("out-{}", "a".repeat(17)))
                .await
                .is_err()
        );
        assert!(store.find_tool_output("run-a", "out-zzzz").await.is_err());
    }

    /// 按 sha 幂等查找 + ref 序号 max：storer 的两条支撑查询。
    #[tokio::test]
    async fn test_find_by_sha_and_max_ref_num() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("to_sha.sqlite");
        let store = SqliteStore::new(path.to_str().expect("utf-8"))
            .await
            .expect("file store");
        store.run_migrations().await.expect("migrations");
        for (run, at) in [("run-a", 1000i64), ("run-b", 2000)] {
            insert_run(&store.pool(), run, None, "root", "completed", "{}", at).await;
        }

        assert_eq!(
            store.max_tool_output_ref_num("run-a").await.expect("max"),
            0,
            "无行 → 0（storer 从 out-1 起分配）"
        );

        let sha_a: String = "c".repeat(64);
        let sha_b: String = "d".repeat(64);
        for (id, run, ref_id, sha) in [
            ("out-a", "run-a", "out-1", sha_a.as_str()),
            ("out-b", "run-a", "out-7", sha_b.as_str()),
            ("out-c", "run-b", "out-3", sha_a.as_str()),
        ] {
            sqlx::query(
                "INSERT INTO tool_outputs (id, run_id, ref, sha256, tool_name, orig_bytes, line_count, created_at) \
                 VALUES (?, ?, ?, ?, 'bash', 10, 2, 1000)",
            )
            .bind(id)
            .bind(run)
            .bind(ref_id)
            .bind(sha)
            .execute(store.pool())
            .await
            .expect("seed");
        }

        // 按 (run, sha) 命中 → 带回该行真实 ref；跨 run 同 sha 各自的行
        let hit = store
            .find_tool_output_by_sha("run-a", &sha_a)
            .await
            .expect("find")
            .expect("hit");
        assert_eq!(hit.ref_id, "out-1");
        let hit_b = store
            .find_tool_output_by_sha("run-b", &sha_a)
            .await
            .expect("find")
            .expect("hit");
        assert_eq!(hit_b.ref_id, "out-3", "run-b 的同 sha 行独立 ref");
        assert!(
            store
                .find_tool_output_by_sha("run-b", &sha_b)
                .await
                .expect("find")
                .is_none(),
            "run-b 没有 sha_b"
        );

        // max 序号：run-a = 7（下一个分配 out-8）；run-b = 3；未知 run = 0
        assert_eq!(store.max_tool_output_ref_num("run-a").await.expect("max"), 7);
        assert_eq!(store.max_tool_output_ref_num("run-b").await.expect("max"), 3);
        assert_eq!(
            store.max_tool_output_ref_num("no-run").await.expect("max"),
            0
        );

        // sha 形状校验
        assert!(store.find_tool_output_by_sha("run-a", "abc").await.is_err());
        assert!(
            store
                .find_tool_output_by_sha("run-a", &"z".repeat(64))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn test_read_tool_output_bytes_roundtrip_and_missing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("to_read.sqlite");
        let store = SqliteStore::new(path.to_str().expect("utf-8"))
            .await
            .expect("file store");
        store.run_migrations().await.expect("migrations");
        insert_run(&store.pool(), "run-1", None, "root", "running", "{}", 1000).await;

        let sha: String = "ab".repeat(32);
        let data = zst("long output body");
        let effective = store
            .insert_tool_output("out-1", "run-1", "out-1", &sha, "bash", 16, 1, &data, 2000)
            .await
            .expect("insert");
        assert_eq!(effective, "out-1", "新行返回传入的 ref");

        let bytes = store
            .read_tool_output_bytes(&sha)
            .await
            .expect("read")
            .expect("file exists");
        assert_eq!(bytes, data, "压缩字节零损往返");

        // 文件缺失（行在、文件被手动清）→ Ok(None)
        std::fs::remove_file(
            store
                .files_dir()
                .expect("dir")
                .join("outputs")
                .join(format!("{sha}.zst")),
        )
        .expect("remove");
        assert!(
            store
                .read_tool_output_bytes(&sha)
                .await
                .expect("read")
                .is_none(),
            "文件缺失返回 None（调用方回落）"
        );

        // 非法 sha 形状 → Err（防路径拼接）
        assert!(store.read_tool_output_bytes("../etc/passwd").await.is_err());
        assert!(store.read_tool_output_bytes("ab").await.is_err());
    }
}
