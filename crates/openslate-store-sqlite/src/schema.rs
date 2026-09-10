//! Schema DDL for the OpenSlate SQLite store.
//!
//! Contains the CREATE TABLE statements for all 7 tables:
//! `runs`, `execution_nodes`, `steps`, `messages`,
//! `prompt_snapshots`, `audit_log`, `trace_events`.
//!
//! Also provides index DDL for common query patterns.

/// Returns the ordered list of DDL statements for all tables.
///
/// The order matters: tables referenced by foreign keys must be created first.
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

/// Returns the list of ALTER TABLE statements for schema evolution
/// (columns added after initial schema).
///
/// Each statement is designed to be idempotent: callers should
/// ignore "duplicate column name" errors.
pub fn alter_statements() -> Vec<&'static str> {
    vec![
        ALTER_RUNS_ADD_CWD,
        ALTER_RUNS_ADD_COST_USD,
        ALTER_MESSAGES_ADD_SEQ,
        ALTER_STEPS_ADD_SEQ,
    ]
}

/// Returns the ordered list of CREATE INDEX statements.
///
/// All statements use `IF NOT EXISTS` so they are idempotent.
pub fn index_statements() -> Vec<&'static str> {
    vec![
        IDX_RUNS_STATUS,
        IDX_RUNS_CWD,
        IDX_RUNS_STARTED_AT,
        IDX_STEPS_RUN_ID,
        IDX_MESSAGES_EXECUTION_NODE_ID,
        IDX_MESSAGES_RUN_SEQ,
        IDX_TRACE_EVENTS_RUN_ID,
        IDX_AUDIT_LOG_RUN_ID,
    ]
}

// ---------------------------------------------------------------------------
// Table DDL
// ---------------------------------------------------------------------------

const DDL_RUNS: &str = r#"
CREATE TABLE IF NOT EXISTS runs (
    id TEXT PRIMARY KEY,
    title TEXT,
    root_agent_id TEXT NOT NULL,
    status TEXT NOT NULL,
    cwd TEXT,
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
    role TEXT NOT NULL,
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
// ALTER TABLE DDL (schema evolution)
// ---------------------------------------------------------------------------

/// Add `cwd` column to the `runs` table for efficient CWD-based queries.
///
/// For fresh databases the column is included in `DDL_RUNS`.
/// For existing databases this ALTER TABLE migrates the schema.
const ALTER_RUNS_ADD_CWD: &str = "ALTER TABLE runs ADD COLUMN cwd TEXT";

/// Add `cost_usd` column to the `runs` table (P2-3 cost tracking): the
/// run's accumulated model spend in USD (0 = unpriced or no usage).
///
/// For fresh databases the column is included in `DDL_RUNS`.
const ALTER_RUNS_ADD_COST_USD: &str =
    "ALTER TABLE runs ADD COLUMN cost_usd REAL NOT NULL DEFAULT 0";

/// Add `seq` column to the `messages` table: a per-run monotonically
/// increasing sequence number assigned by the writer.
///
/// Ordering by `created_at` alone is unstable for messages inserted in the
/// same instant (batch inserts share a timestamp), and conversation order is
/// load-bearing — a provider rejects history whose `tool_calls` are shuffled
/// away from their tool results. `seq` gives `ORDER BY` a total order.
///
/// For fresh databases the column is included in `DDL_MESSAGES`.
const ALTER_MESSAGES_ADD_SEQ: &str =
    "ALTER TABLE messages ADD COLUMN seq INTEGER NOT NULL DEFAULT 0";

/// Add `seq` column to the `steps` table (same rationale as
/// [`ALTER_MESSAGES_ADD_SEQ`]: `started_at` alone cannot order same-batch
/// inserts).
const ALTER_STEPS_ADD_SEQ: &str = "ALTER TABLE steps ADD COLUMN seq INTEGER NOT NULL DEFAULT 0";

// ---------------------------------------------------------------------------
// Index DDL
// ---------------------------------------------------------------------------

/// Index for filtering runs by status (e.g. `get_last_interrupted_run`).
const IDX_RUNS_STATUS: &str = "CREATE INDEX IF NOT EXISTS idx_runs_status ON runs(status)";

/// Index for listing runs by working directory (`list_runs_by_cwd`).
const IDX_RUNS_CWD: &str = "CREATE INDEX IF NOT EXISTS idx_runs_cwd ON runs(cwd)";

/// Index for ordering runs by time (`list_runs`, `list_runs_by_cwd`).
const IDX_RUNS_STARTED_AT: &str =
    "CREATE INDEX IF NOT EXISTS idx_runs_started_at ON runs(started_at)";

/// Index for listing steps belonging to a run (`list_steps`).
const IDX_STEPS_RUN_ID: &str = "CREATE INDEX IF NOT EXISTS idx_steps_run_id ON steps(run_id)";

/// Index for listing messages by execution node (`list_messages`).
const IDX_MESSAGES_EXECUTION_NODE_ID: &str =
    "CREATE INDEX IF NOT EXISTS idx_messages_execution_node_id ON messages(execution_node_id)";

/// Index for loading a run's full conversation in `seq` order
/// (`list_messages_by_run` — the resume path).
const IDX_MESSAGES_RUN_SEQ: &str =
    "CREATE INDEX IF NOT EXISTS idx_messages_run_seq ON messages(run_id, seq)";

/// Index for listing trace events by run (`list_trace_events`).
const IDX_TRACE_EVENTS_RUN_ID: &str =
    "CREATE INDEX IF NOT EXISTS idx_trace_events_run_id ON trace_events(run_id)";

/// Index for listing audit log entries by run (`list_audit_logs`).
const IDX_AUDIT_LOG_RUN_ID: &str =
    "CREATE INDEX IF NOT EXISTS idx_audit_log_run_id ON audit_log(run_id)";

// ---------------------------------------------------------------------------
// Constants for verification
// ---------------------------------------------------------------------------

/// Expected table names in creation order.
pub const TABLE_NAMES: &[&str] = &[
    "runs",
    "execution_nodes",
    "steps",
    "messages",
    "prompt_snapshots",
    "audit_log",
    "trace_events",
];

/// Expected index names (for verification in tests).
pub const INDEX_NAMES: &[&str] = &[
    "idx_runs_status",
    "idx_runs_cwd",
    "idx_runs_started_at",
    "idx_steps_run_id",
    "idx_messages_execution_node_id",
    "idx_messages_run_seq",
    "idx_trace_events_run_id",
    "idx_audit_log_run_id",
];
