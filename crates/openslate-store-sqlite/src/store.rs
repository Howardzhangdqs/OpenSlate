//! SQLite store implementation for OpenSlate.
//!
//! Provides [`SqliteStore`] with connection management, PRAGMA setup,
//! and explicit migration (no sqlx macros).

use openslate_core::error::StoreError;
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::{query, query_scalar, SqlitePool};

use crate::schema;

/// SQLite-backed store for OpenSlate run data.
pub struct SqliteStore {
    pool: SqlitePool,
}

/// Snapshot of key PRAGMA settings for verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PragmaState {
    pub journal_mode: String,
    pub synchronous: String,
    pub foreign_keys: bool,
    pub busy_timeout: i64,
}

impl SqliteStore {
    /// Create a new store with an in-memory SQLite database (for testing).
    pub async fn new_in_memory() -> Result<Self, StoreError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .map_err(|e| StoreError::ConnectionError(e.to_string()))?;

        // In-memory: WAL and busy_timeout don't apply.
        query("PRAGMA foreign_keys = ON")
            .execute(&pool)
            .await
            .map_err(|e| StoreError::ConnectionError(e.to_string()))?;

        query("PRAGMA synchronous = NORMAL")
            .execute(&pool)
            .await
            .map_err(|e| StoreError::ConnectionError(e.to_string()))?;

        Ok(Self { pool })
    }

    /// Create a new store connected to a file-based database.
    pub async fn new(path: &str) -> Result<Self, StoreError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&format!("sqlite:{path}?mode=rwc"))
            .await
            .map_err(|e| StoreError::ConnectionError(e.to_string()))?;

        query("PRAGMA journal_mode = WAL")
            .execute(&pool)
            .await
            .map_err(|e| StoreError::ConnectionError(e.to_string()))?;

        query("PRAGMA synchronous = NORMAL")
            .execute(&pool)
            .await
            .map_err(|e| StoreError::ConnectionError(e.to_string()))?;

        query("PRAGMA foreign_keys = ON")
            .execute(&pool)
            .await
            .map_err(|e| StoreError::ConnectionError(e.to_string()))?;

        query("PRAGMA busy_timeout = 5000")
            .execute(&pool)
            .await
            .map_err(|e| StoreError::ConnectionError(e.to_string()))?;

        Ok(Self { pool })
    }

    /// Run all migrations (create tables if not exist).
    pub async fn run_migrations(&self) -> Result<(), StoreError> {
        for ddl in schema::ddl_statements() {
            query(ddl)
                .execute(&self.pool)
                .await
                .map_err(|e| StoreError::MigrationError(e.to_string()))?;
        }
        Ok(())
    }

    /// Verify PRAGMA settings are correct.
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

    /// Get a reference to the underlying pool.
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
    async fn test_file_based_db_creates() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("test.db");
        let path_str = path.to_str().expect("valid utf-8 path");

        let store = SqliteStore::new(path_str).await;
        assert!(store.is_ok(), "file-based store should create successfully");
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
}
