//! OpenSlate 的 SQLite 存储实现。
//!
//! 提供 [`SqliteStore`]：连接管理、PRAGMA 配置、版本化迁移
//! （不使用 sqlx 宏）。
//!
//! 连接策略：journal_mode / synchronous / busy_timeout / foreign_keys 全部
//! 通过 [`sqlx::sqlite::SqliteConnectOptions`] 配置 —— 这些设置随连接选项
//! 在**每条**新连接上生效，连接池因 idle_timeout / max_lifetime 回收重建
//! 连接后不会静默回退到默认值。

use std::time::Duration;

use openslate_core::error::StoreError;
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous,
};
use sqlx::AssertSqlSafe;
use sqlx::{query, query_scalar, SqlitePool};

use crate::schema;

/// OpenSlate 运行数据的 SQLite 存储。
///
/// `Clone` 是廉价的：连接池基于 `Arc`，长生命周期的 recorder（如每个
/// run 一个的 `RunRecorder`）可以与 `AppContext` 各持一份句柄。
#[derive(Clone)]
pub struct SqliteStore {
    pool: SqlitePool,
}

/// PRAGMA 关键设置的快照（用于验证）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PragmaState {
    pub journal_mode: String,
    pub synchronous: String,
    pub foreign_keys: bool,
    pub busy_timeout: i64,
}

/// 收紧主库文件与已存在的 WAL/-shm 旁车文件的权限为仅属主可读写
/// (0o600)。库中保存明文的完整会话历史；默认 umask 会留下任何本地
/// 用户都可读的 0644。Best-effort：旁车文件尚不存在则跳过，失败仅告警
/// —— 存储两种情况下都继续工作。
#[cfg(unix)]
fn restrict_db_file_permissions(path: &str) {
    use std::os::unix::fs::PermissionsExt;

    for file in [
        std::path::PathBuf::from(path),
        std::path::PathBuf::from(format!("{path}-wal")),
        std::path::PathBuf::from(format!("{path}-shm")),
    ] {
        let Ok(metadata) = std::fs::metadata(&file) else {
            continue; // 旁车文件惰性出现；暂时无事可做
        };
        let mut perms = metadata.permissions();
        perms.set_mode(0o600);
        if let Err(e) = std::fs::set_permissions(&file, perms) {
            tracing::warn!(
                target: "openslate_store",
                "failed to restrict db file permissions for {}: {}",
                file.display(),
                e
            );
        }
    }
}

#[cfg(not(unix))]
fn restrict_db_file_permissions(_path: &str) {}

/// 文件库的连接选项：PRAGMA 全部走连接选项，确保每条（含回收重建的）
/// 连接一致。
///
/// 注意必须用 `filename()` + `create_if_missing(true)` 构造，而不是拼接
/// `sqlite:{path}?mode=rwc` URL 字符串 —— 路径里含 `?` / `#` 时 URL 解析
/// 会错乱。
fn file_connect_options(path: &str) -> SqliteConnectOptions {
    SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(5))
        .foreign_keys(true)
}

/// 内存库的连接选项（仅测试用途）。
fn in_memory_connect_options() -> SqliteConnectOptions {
    // 内存库不支持 WAL（journal_mode 保持默认 memory），不设置即可。
    SqliteConnectOptions::new()
        .in_memory(true)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(5))
        .foreign_keys(true)
}

impl SqliteStore {
    /// 创建一个内存 SQLite 数据库的存储。**仅测试用途。**
    ///
    /// `:memory:` 库的每条新连接都是全新的空库，因此这里保证单连接
    /// 永不被池回收（idle_timeout / max_lifetime 均为 None），
    /// 否则连接重建后整库"丢失"、查询报 no such table。
    pub async fn new_in_memory() -> Result<Self, StoreError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .min_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(in_memory_connect_options())
            .await
            .map_err(|e| StoreError::ConnectionError(e.to_string()))?;
        Ok(Self { pool })
    }

    /// 创建一个连接到文件数据库的存储。
    ///
    /// 池配置为单连接（SQLite 写并发收益有限，且避免 `database is
    /// locked`）；PRAGMA 走 [`SqliteConnectOptions`]，连接回收重建后
    /// 依然生效。
    pub async fn new(path: &str) -> Result<Self, StoreError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .min_connections(1)
            .connect_with(file_connect_options(path))
            .await
            .map_err(|e| StoreError::ConnectionError(e.to_string()))?;

        // 库文件（以及此时可能已出现的 WAL/-shm 旁车文件）已存在 ——
        // 在任何 run 数据落进去之前收紧权限。
        restrict_db_file_permissions(path);

        Ok(Self { pool })
    }

    /// 读取当前 `PRAGMA user_version`。
    pub async fn user_version(&self) -> Result<i64, StoreError> {
        query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(self.pool())
            .await
            .map_err(|e| StoreError::QueryError(e.to_string()))
    }

    /// 执行版本化迁移：按 [`schema::SCHEMA_VERSION`] 逐版本步进。
    ///
    /// 每个步进封装为独立事务，`PRAGMA user_version` 与 DDL 同事务
    /// 提交 —— 崩溃后重跑只会从上次已提交的版本继续，不会出现半迁移。
    pub async fn run_migrations(&self) -> Result<(), StoreError> {
        let mut version = self.user_version().await?;
        if version > schema::SCHEMA_VERSION {
            return Err(StoreError::MigrationError(format!(
                "数据库 schema 版本 {version} 高于本程序支持的版本 {}，请先升级程序",
                schema::SCHEMA_VERSION
            )));
        }

        while version < schema::SCHEMA_VERSION {
            let mut tx = self
                .pool
                .begin()
                .await
                .map_err(|e| StoreError::MigrationError(e.to_string()))?;

            match version {
                0 => schema::migrate_v0_to_v1(&mut tx).await?,
                1 => schema::migrate_v1_to_v2(&mut tx).await?,
                v => {
                    return Err(StoreError::MigrationError(format!(
                        "未知的 schema 版本步进起点: v{v}"
                    )))
                }
            }

            // user_version 不支持绑定参数；值为内部 i64，无注入风险
            // （AssertSqlSafe 仅为通过 sqlx 的动态 SQL 审计）。
            query(AssertSqlSafe(format!(
                "PRAGMA user_version = {}",
                version + 1
            )))
                .execute(&mut *tx)
                .await
                .map_err(|e| StoreError::MigrationError(e.to_string()))?;

            tx.commit()
                .await
                .map_err(|e| StoreError::MigrationError(e.to_string()))?;
            version += 1;
        }
        Ok(())
    }

    /// 验证 PRAGMA 设置是否正确。
    pub async fn verify_pragma(&self) -> Result<PragmaState, StoreError> {
        let journal_mode: String = query_scalar::<_, String>("PRAGMA journal_mode")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| StoreError::QueryError(e.to_string()))?;

        let sync_val: i64 = query_scalar::<_, i64>("PRAGMA synchronous")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| StoreError::QueryError(e.to_string()))?;

        let fk: i64 = query_scalar::<_, i64>("PRAGMA foreign_keys")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| StoreError::QueryError(e.to_string()))?;

        let busy_timeout: i64 = query_scalar::<_, i64>("PRAGMA busy_timeout")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| StoreError::QueryError(e.to_string()))?;

        let synchronous = match sync_val {
            0 => "OFF".to_owned(),
            1 => "NORMAL".to_owned(),
            2 => "FULL".to_owned(),
            other => other.to_string(),
        };

        Ok(PragmaState {
            journal_mode,
            synchronous,
            foreign_keys: fk != 0,
            busy_timeout,
        })
    }

    /// 获取底层连接池的引用。
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_in_memory_db_creates() {
        let store = SqliteStore::new_in_memory().await;
        assert!(store.is_ok(), "new_in_memory should succeed");
    }

    #[tokio::test]
    async fn test_all_seven_tables_created() {
        let store = SqliteStore::new_in_memory().await.expect("store created");
        store.run_migrations().await.expect("migrations run");

        let tables: Vec<String> = query_scalar(
            "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .fetch_all(store.pool())
        .await
        .expect("query sqlite_master");

        let mut expected: Vec<&str> = schema::TABLE_NAMES.to_vec();
        expected.sort();

        let mut actual: Vec<String> = tables;
        actual.sort();

        assert_eq!(actual, expected, "all 7 tables should exist");
    }

    #[tokio::test]
    async fn test_pragma_foreign_keys_on() {
        let store = SqliteStore::new_in_memory().await.expect("store created");
        let pragma = store.verify_pragma().await.expect("pragma verified");
        assert!(pragma.foreign_keys, "foreign_keys should be ON");
    }

    #[tokio::test]
    async fn test_pragma_synchronous_normal() {
        let store = SqliteStore::new_in_memory().await.expect("store created");
        let pragma = store.verify_pragma().await.expect("pragma verified");
        assert_eq!(pragma.synchronous, "NORMAL", "synchronous should be NORMAL");
    }

    #[tokio::test]
    async fn test_foreign_key_enforcement() {
        let store = SqliteStore::new_in_memory().await.expect("store created");
        store.run_migrations().await.expect("migrations run");

        // Insert a step referencing a non-existent run_id → should fail.
        let result = query(
            "INSERT INTO steps (id, run_id, execution_node_id, agent_id, kind, data_json, started_at) \
             VALUES ('s1', 'nonexistent_run', 'nonexistent_node', 'a1', 'model_call', '{}', 1)",
        )
        .execute(store.pool())
        .await;

        assert!(
            result.is_err(),
            "INSERT with invalid foreign key should fail"
        );
    }

    #[tokio::test]
    async fn test_migrations_idempotent() {
        let store = SqliteStore::new_in_memory().await.expect("store created");
        store.run_migrations().await.expect("first migration");
        store.run_migrations().await.expect("second migration");

        let tables: Vec<String> = query_scalar(
            "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .fetch_all(store.pool())
        .await
        .expect("query sqlite_master");

        assert_eq!(tables.len(), 7, "should still have exactly 7 tables");
    }

    #[tokio::test]
    async fn test_migrations_triple_run() {
        let store = SqliteStore::new_in_memory().await.expect("store created");
        store.run_migrations().await.expect("first migration");
        store.run_migrations().await.expect("second migration");
        store.run_migrations().await.expect("third migration");

        let tables: Vec<String> = query_scalar(
            "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .fetch_all(store.pool())
        .await
        .expect("query sqlite_master");

        assert_eq!(tables.len(), 7, "should still have exactly 7 tables");

        let indexes: Vec<String> = query_scalar(
            "SELECT name FROM sqlite_master WHERE type='index' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .fetch_all(store.pool())
        .await
        .expect("query sqlite_master for indexes");

        assert_eq!(
            indexes.len(),
            schema::INDEX_NAMES.len(),
            "should have exactly the expected number of indexes"
        );

        assert_eq!(
            store.user_version().await.expect("user_version"),
            schema::SCHEMA_VERSION
        );
    }

    #[tokio::test]
    async fn test_migration_sets_user_version() {
        let store = SqliteStore::new_in_memory().await.expect("store created");
        assert_eq!(
            store.user_version().await.expect("user_version"),
            0,
            "全新库 user_version 应为 0"
        );
        store.run_migrations().await.expect("migrations run");
        assert_eq!(
            store.user_version().await.expect("user_version"),
            schema::SCHEMA_VERSION
        );
    }

    #[tokio::test]
    async fn test_runs_cwd_dead_column_dropped() {
        let store = SqliteStore::new_in_memory().await.expect("store created");
        store.run_migrations().await.expect("migrations run");

        let columns: Vec<String> =
            query_scalar("SELECT name FROM pragma_table_info('runs') ORDER BY name")
                .fetch_all(store.pool())
                .await
                .expect("query pragma_table_info");

        assert!(
            !columns.iter().any(|c| c == "cwd"),
            "runs 表不应再有 'cwd' 死列, got: {columns:?}"
        );

        let indexes: Vec<String> = query_scalar(
            "SELECT name FROM sqlite_master WHERE type='index' AND name = 'idx_runs_cwd'",
        )
        .fetch_all(store.pool())
        .await
        .expect("query sqlite_master");
        assert!(indexes.is_empty(), "idx_runs_cwd 死索引应被清理");
    }

    #[tokio::test]
    async fn test_runs_table_has_title_column() {
        let store = SqliteStore::new_in_memory().await.expect("store created");
        store.run_migrations().await.expect("migrations run");

        let columns: Vec<String> =
            query_scalar("SELECT name FROM pragma_table_info('runs') ORDER BY name")
                .fetch_all(store.pool())
                .await
                .expect("query pragma_table_info");

        assert!(
            columns.iter().any(|c| c == "title"),
            "runs 表应有 'title' 列, got: {columns:?}"
        );
    }

    #[tokio::test]
    async fn test_runs_table_has_cost_usd_column() {
        let store = SqliteStore::new_in_memory().await.expect("store created");
        store.run_migrations().await.expect("migrations run");

        let columns: Vec<String> =
            query_scalar("SELECT name FROM pragma_table_info('runs') ORDER BY name")
                .fetch_all(store.pool())
                .await
                .expect("query pragma_table_info");

        assert!(
            columns.iter().any(|c| c == "cost_usd"),
            "runs 表应有 'cost_usd' 列 (P2-3), got: {columns:?}"
        );
    }

    #[tokio::test]
    async fn test_messages_and_steps_have_seq_columns() {
        let store = SqliteStore::new_in_memory().await.expect("store created");
        store.run_migrations().await.expect("migrations run");

        let msg_columns: Vec<String> =
            query_scalar("SELECT name FROM pragma_table_info('messages')")
                .fetch_all(store.pool())
                .await
                .expect("query pragma_table_info(messages)");
        assert!(
            msg_columns.iter().any(|c| c == "seq"),
            "messages 应有 'seq' 列, got: {msg_columns:?}"
        );

        let step_columns: Vec<String> = query_scalar("SELECT name FROM pragma_table_info('steps')")
            .fetch_all(store.pool())
            .await
            .expect("query pragma_table_info(steps)");
        assert!(
            step_columns.iter().any(|c| c == "seq"),
            "steps 应有 'seq' 列, got: {step_columns:?}"
        );
    }

    #[tokio::test]
    async fn test_status_and_role_check_constraints_enforced() {
        let store = SqliteStore::new_in_memory().await.expect("store created");
        store.run_migrations().await.expect("migrations run");

        query(
            "INSERT INTO runs (id, root_agent_id, status, input_json, started_at) \
             VALUES ('bad-status', 'root', 'bogus', '{}', 1)",
        )
        .execute(store.pool())
        .await
        .expect_err("非法 status 应被 CHECK 拒绝");

        query("INSERT INTO runs (id, root_agent_id, status, input_json, started_at) \
               VALUES ('ok-run', 'root', 'running', '{}', 1)")
        .execute(store.pool())
        .await
        .expect("合法 status 应通过");

        query(
            "INSERT INTO messages (id, run_id, execution_node_id, role, content_json, created_at) \
             VALUES ('bad-role', 'ok-run', 'enode-x', 'narrator', '{}', 1)",
        )
        .execute(store.pool())
        .await
        .expect_err("非法 role 应被 CHECK 拒绝");
    }

    #[tokio::test]
    async fn test_messages_run_seq_unique_enforced() {
        let store = SqliteStore::new_in_memory().await.expect("store created");
        store.run_migrations().await.expect("migrations run");

        query("INSERT INTO runs (id, root_agent_id, status, input_json, started_at) \
               VALUES ('uq-run', 'root', 'running', '{}', 1)")
        .execute(store.pool())
        .await
        .expect("insert run");
        query(
            "INSERT INTO execution_nodes (id, run_id, agent_id, status, input_json, started_at) \
             VALUES ('uq-enode', 'uq-run', 'root', 'running', '{}', 1)",
        )
        .execute(store.pool())
        .await
        .expect("insert node");

        query(
            "INSERT INTO messages (id, run_id, execution_node_id, role, content_json, seq, created_at) \
             VALUES ('uq-m1', 'uq-run', 'uq-enode', 'user', '{}', 1, 1)",
        )
        .execute(store.pool())
        .await
        .expect("first message");

        let dup = query(
            "INSERT INTO messages (id, run_id, execution_node_id, role, content_json, seq, created_at) \
             VALUES ('uq-m2', 'uq-run', 'uq-enode', 'user', '{}', 1, 2)",
        )
        .execute(store.pool())
        .await;
        assert!(dup.is_err(), "同 run 内重复 seq 应被唯一索引拒绝");
    }

    #[tokio::test]
    async fn test_file_based_db_creates() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("test.db");
        let path_str = path.to_str().expect("valid utf-8 path");

        let store = SqliteStore::new(path_str).await;
        assert!(store.is_ok(), "file-based store should create successfully");
    }

    /// 路径含 `?` / `#` 时也必须能正常打开（URL 拼接时代的回归测试）。
    #[tokio::test]
    async fn test_file_based_db_creates_with_special_chars_in_path() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("we?ird#name.db");
        let path_str = path.to_str().expect("valid utf-8 path");

        let store = SqliteStore::new(path_str).await.expect("store created");
        store.run_migrations().await.expect("migrations run");
        store
            .insert_run("q-run", None, "root", "running", "{}", 1)
            .await
            .expect("insert run");
        assert_eq!(store.count_runs().await.expect("count"), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_file_based_db_permissions_owner_only() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("test_perms.db");
        let path_str = path.to_str().expect("valid utf-8 path");

        let _store = SqliteStore::new(path_str).await.expect("store created");

        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path)
            .expect("db file exists")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "db file must be owner-only (conversation plaintext)"
        );
    }

    #[tokio::test]
    async fn test_file_based_pragma_wal() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("test_wal.db");
        let path_str = path.to_str().expect("valid utf-8 path");

        let store = SqliteStore::new(path_str).await.expect("store created");
        let pragma = store.verify_pragma().await.expect("pragma verified");
        assert_eq!(
            pragma.journal_mode, "wal",
            "file-based store should use WAL journal mode"
        );
    }

    #[tokio::test]
    async fn test_all_indexes_created() {
        let store = SqliteStore::new_in_memory().await.expect("store created");
        store.run_migrations().await.expect("migrations run");

        let indexes: Vec<String> = query_scalar(
            "SELECT name FROM sqlite_master WHERE type='index' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .fetch_all(store.pool())
        .await
        .expect("query sqlite_master for indexes");

        let mut expected: Vec<&str> = schema::INDEX_NAMES.to_vec();
        expected.sort();

        let mut actual: Vec<String> = indexes;
        actual.sort();

        assert_eq!(actual, expected, "all expected indexes should exist");
    }

    #[tokio::test]
    async fn test_index_idempotent() {
        let store = SqliteStore::new_in_memory().await.expect("store created");
        store.run_migrations().await.expect("first migration");
        store.run_migrations().await.expect("second migration");

        let indexes: Vec<String> = query_scalar(
            "SELECT name FROM sqlite_master WHERE type='index' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .fetch_all(store.pool())
        .await
        .expect("query sqlite_master for indexes");

        assert_eq!(
            indexes.len(),
            schema::INDEX_NAMES.len(),
            "should have exactly the expected number of indexes"
        );
    }

    #[tokio::test]
    async fn test_in_memory_pragma_synchronous_normal() {
        let store = SqliteStore::new_in_memory().await.expect("store created");
        let pragma = store.verify_pragma().await.expect("pragma verified");
        assert_eq!(pragma.synchronous, "NORMAL");
        assert!(pragma.foreign_keys, "foreign_keys should be ON");
    }

    #[tokio::test]
    async fn test_file_based_pragma_all() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("test_pragma_all.db");
        let path_str = path.to_str().expect("valid utf-8 path");

        let store = SqliteStore::new(path_str).await.expect("store created");
        let pragma = store.verify_pragma().await.expect("pragma verified");

        assert_eq!(pragma.journal_mode, "wal", "should use WAL");
        assert_eq!(pragma.synchronous, "NORMAL", "synchronous should be NORMAL");
        assert!(pragma.foreign_keys, "foreign_keys should be ON");
        assert_eq!(pragma.busy_timeout, 5000, "busy_timeout should be 5000ms");
    }

    /// 回归测试（问题 1）：连接被池回收重建后，PRAGMA 必须依然是
    /// 连接选项里配置的值，而不是回退 SQLite/sqlx 默认值。
    ///
    /// 用与 [`SqliteStore::new`] 相同的连接选项，但把 idle_timeout /
    /// max_lifetime 压到 1ms，强制回收后重新取连接验证。
    #[tokio::test]
    async fn test_pragma_survives_connection_recycle() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("recycle.db");
        let path_str = path.to_str().expect("valid utf-8 path");

        // 先建库，确保文件存在且 WAL 已启用。
        {
            let store = SqliteStore::new(path_str).await.expect("store created");
            store.run_migrations().await.expect("migrations");
        }

        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .min_connections(1)
            .idle_timeout(Duration::from_millis(1))
            .max_lifetime(Duration::from_millis(1))
            .connect_with(file_connect_options(path_str))
            .await
            .expect("pool created");

        // 让 reaper 关闭空闲连接；随后取到的一定是新建连接。
        tokio::time::sleep(Duration::from_millis(80)).await;

        let sync: i64 = query_scalar("PRAGMA synchronous")
            .fetch_one(&pool)
            .await
            .expect("synchronous");
        let fk: i64 = query_scalar("PRAGMA foreign_keys")
            .fetch_one(&pool)
            .await
            .expect("foreign_keys");
        let journal: String = query_scalar("PRAGMA journal_mode")
            .fetch_one(&pool)
            .await
            .expect("journal_mode");

        assert_eq!(sync, 1, "回收重建后 synchronous 必须仍是 NORMAL(1)");
        assert_eq!(fk, 1, "回收重建后 foreign_keys 必须仍是 ON");
        assert_eq!(journal, "wal", "回收重建后 journal_mode 必须仍是 wal");
        pool.close().await;
    }

    /// 内存库数据在池的空闲周期后依然存在（单连接永不回收的回归测试）。
    #[tokio::test]
    async fn test_in_memory_data_survives_idle() {
        let store = SqliteStore::new_in_memory().await.expect("store created");
        store.run_migrations().await.expect("migrations run");
        store
            .insert_run("mem-run", None, "root", "running", "{}", 1)
            .await
            .expect("insert run");

        tokio::time::sleep(Duration::from_millis(100)).await;

        assert_eq!(
            store.count_runs().await.expect("count"),
            1,
            "内存库在空闲后不应被回收重建（否则整库丢失）"
        );
    }

    // -----------------------------------------------------------------------
    // 存量 v0 库升级测试（手工建旧 schema → run_migrations → 校验）
    // -----------------------------------------------------------------------

    /// HEAD 时代的旧 schema：runs 含 cwd 死列，user_version=0。
    async fn build_head_era_v0_db(path: &str) {
        let conn = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(conn)
            .await
            .expect("open legacy db");

        query(
            "CREATE TABLE runs (
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
            )",
        )
        .execute(&pool)
        .await
        .expect("legacy runs");

        query(
            "CREATE TABLE execution_nodes (
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
            )",
        )
        .execute(&pool)
        .await
        .expect("legacy execution_nodes");

        query(
            "CREATE TABLE messages (
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
            )",
        )
        .execute(&pool)
        .await
        .expect("legacy messages");

        query("CREATE INDEX idx_runs_cwd ON runs(cwd)")
            .execute(&pool)
            .await
            .expect("legacy idx_runs_cwd");
        query("CREATE INDEX idx_messages_run_seq ON messages(run_id, seq)")
            .execute(&pool)
            .await
            .expect("legacy idx_messages_run_seq");

        // 存量数据：1 个 run + 1 个 node + 2 条消息（seq 唯一）
        query("INSERT INTO runs (id, title, root_agent_id, status, cwd, input_json, started_at) \
               VALUES ('legacy-run', '旧标题', 'root', 'completed', '/old/cwd', '{}', 1000)")
            .execute(&pool)
            .await
            .expect("legacy run data");
        query(
            "INSERT INTO execution_nodes (id, run_id, agent_id, status, input_json, started_at) \
             VALUES ('legacy-enode', 'legacy-run', 'root', 'completed', '{}', 1100)",
        )
        .execute(&pool)
        .await
        .expect("legacy node data");
        for (i, id) in [("legacy-m1", 1i64), ("legacy-m2", 2)].into_iter() {
            query(
                "INSERT INTO messages (id, run_id, execution_node_id, role, content_json, seq, created_at) \
                 VALUES (?, 'legacy-run', 'legacy-enode', 'user', '\"x\"', ?, 1200)",
            )
            .bind(id)
            .bind(i)
            .execute(&pool)
            .await
            .expect("legacy message data");
        }
        // 其余表留给基线 DDL 的 IF NOT EXISTS 补建
        pool.close().await;
    }

    #[tokio::test]
    async fn test_migrates_head_era_v0_db_to_v2() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("legacy_head.sqlite");
        let path_str = path.to_str().expect("valid utf-8 path");
        build_head_era_v0_db(path_str).await;

        let store = SqliteStore::new(path_str).await.expect("open store");
        store.run_migrations().await.expect("migrate to v2");

        // user_version 正确
        assert_eq!(store.user_version().await.expect("version"), 2);

        // title 列存在且数据完好
        let run = store.get_run("legacy-run").await.expect("get").expect("run");
        assert_eq!(run.title.as_deref(), Some("旧标题"));
        assert_eq!(run.status, "completed");

        // cwd 死列与死索引被清理
        let columns: Vec<String> =
            query_scalar("SELECT name FROM pragma_table_info('runs') ORDER BY name")
                .fetch_all(store.pool())
                .await
                .expect("columns");
        assert!(
            !columns.iter().any(|c| c == "cwd"),
            "cwd 死列应被删除, got: {columns:?}"
        );
        let idx: Option<String> = query_scalar(
            "SELECT name FROM sqlite_master WHERE type='index' AND name='idx_runs_cwd'",
        )
        .fetch_optional(store.pool())
        .await
        .expect("index check");
        assert!(idx.is_none(), "idx_runs_cwd 应被删除");

        // 唯一索引存在
        let uq: Option<String> = query_scalar(
            "SELECT name FROM sqlite_master WHERE type='index' AND name='idx_messages_run_seq_uq'",
        )
        .fetch_optional(store.pool())
        .await
        .expect("uq check");
        assert!(uq.is_some(), "唯一索引应已创建");

        // 存量消息完好
        assert_eq!(
            store.count_messages_by_run("legacy-run").await.expect("cnt"),
            2
        );

        // 重复迁移幂等
        store.run_migrations().await.expect("idempotent rerun");
    }

    /// 更旧的 v0 库（无 title、无 cost_usd、messages 无 seq）也能升级。
    #[tokio::test]
    async fn test_migrates_pre_title_v0_db() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("legacy_pre_title.sqlite");
        let path_str = path.to_str().expect("valid utf-8 path");

        {
            let pool = SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(
                    SqliteConnectOptions::new()
                        .filename(path_str)
                        .create_if_missing(true),
                )
                .await
                .expect("open");
            query(
                "CREATE TABLE runs (
                    id TEXT PRIMARY KEY,
                    root_agent_id TEXT NOT NULL,
                    status TEXT NOT NULL,
                    cwd TEXT,
                    input_json TEXT NOT NULL,
                    output_json TEXT,
                    started_at INTEGER NOT NULL,
                    finished_at INTEGER
                )",
            )
            .execute(&pool)
            .await
            .expect("old runs");
            query(
                "CREATE TABLE execution_nodes (
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
                )",
            )
            .execute(&pool)
            .await
            .expect("old execution_nodes");
            query(
                "INSERT INTO runs (id, root_agent_id, status, input_json, started_at) \
                 VALUES ('old-run', 'root', 'completed', '{}', 1)",
            )
            .execute(&pool)
            .await
            .expect("old data");
            pool.close().await;
        }

        let store = SqliteStore::new(path_str).await.expect("open store");
        store.run_migrations().await.expect("migrate");

        assert_eq!(store.user_version().await.expect("version"), 2);

        let columns: Vec<String> =
            query_scalar("SELECT name FROM pragma_table_info('runs') ORDER BY name")
                .fetch_all(store.pool())
                .await
                .expect("columns");
        for expected in ["title", "cost_usd"] {
            assert!(
                columns.iter().any(|c| c == expected),
                "runs 应补齐 '{expected}' 列, got: {columns:?}"
            );
        }

        let run = store.get_run("old-run").await.expect("get").expect("run");
        assert_eq!(run.title, None);
        assert_eq!(run.cost_usd, 0.0);
    }

    /// 存量数据存在重复 seq 时，唯一索引创建失败仅告警跳过，迁移不阻断。
    #[tokio::test]
    async fn test_v2_unique_index_skipped_on_duplicate_seq() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("legacy_dup_seq.sqlite");
        let path_str = path.to_str().expect("valid utf-8 path");

        {
            let pool = SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(
                    SqliteConnectOptions::new()
                        .filename(path_str)
                        .create_if_missing(true),
                )
                .await
                .expect("open");
            for ddl in [
                "CREATE TABLE runs (id TEXT PRIMARY KEY, root_agent_id TEXT NOT NULL, status TEXT NOT NULL, input_json TEXT NOT NULL, started_at INTEGER NOT NULL)",
                "CREATE TABLE execution_nodes (id TEXT PRIMARY KEY, run_id TEXT NOT NULL, agent_id TEXT NOT NULL, status TEXT NOT NULL, input_json TEXT NOT NULL, started_at INTEGER NOT NULL)",
                "CREATE TABLE messages (id TEXT PRIMARY KEY, run_id TEXT NOT NULL, execution_node_id TEXT NOT NULL, role TEXT NOT NULL, content_json TEXT NOT NULL, seq INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL)",
            ] {
                query(ddl).execute(&pool).await.expect("legacy ddl");
            }
            query("INSERT INTO runs VALUES ('dup-run', 'root', 'running', '{}', 1)")
                .execute(&pool)
                .await
                .expect("run");
            query("INSERT INTO execution_nodes VALUES ('dup-enode', 'dup-run', 'root', 'running', '{}', 1)")
                .execute(&pool)
                .await
                .expect("node");
            // 两条消息同 seq —— 唯一索引必然创建失败
            for id in ["dup-m1", "dup-m2"] {
                query("INSERT INTO messages VALUES (?, 'dup-run', 'dup-enode', 'user', '\"x\"', 7, 1)")
                    .bind(id)
                    .execute(&pool)
                    .await
                    .expect("dup message");
            }
            pool.close().await;
        }

        let store = SqliteStore::new(path_str).await.expect("open store");
        store
            .run_migrations()
            .await
            .expect("重复 seq 只能导致索引跳过，不能阻断迁移");

        assert_eq!(store.user_version().await.expect("version"), 2);
        let uq: Option<String> = query_scalar(
            "SELECT name FROM sqlite_master WHERE type='index' AND name='idx_messages_run_seq_uq'",
        )
        .fetch_optional(store.pool())
        .await
        .expect("uq check");
        assert!(uq.is_none(), "重复数据下唯一索引应被跳过");
    }

    /// 高于支持版本的库必须拒绝迁移。
    #[tokio::test]
    async fn test_future_version_rejected() {
        let store = SqliteStore::new_in_memory().await.expect("store");
        store.run_migrations().await.expect("migrate");
        query(AssertSqlSafe(format!(
            "PRAGMA user_version = {}",
            schema::SCHEMA_VERSION + 1
        )))
        .execute(store.pool())
        .await
        .expect("bump version");
        let err = store.run_migrations().await.expect_err("must reject");
        assert!(err.to_string().contains("高于"), "got: {err}");
    }
}
