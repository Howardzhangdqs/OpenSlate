//! Integration test: file-based SQLite store roundtrip and persistence.
//!
//! The inline unit tests in this crate only use `:memory:` databases.
//! This test verifies that data persists to a real file on disk and
//! survives a close/reopen cycle.

use openslate_store_sqlite::store::SqliteStore;

/// Insert a run, close the store, reopen the file, and verify the run persists.
#[tokio::test]
async fn file_db_run_persists_across_reopen() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("test.sqlite");
    let db_path_str = db_path.to_str().unwrap();

    // Phase 1: create, migrate, insert
    {
        let store = SqliteStore::new(db_path_str).await.expect("store created");
        store.run_migrations().await.expect("migrations run");

        store
            .insert_run(
                "run-persist-1",
                Some("Persistence Test"),
                "root-agent",
                "running",
                r#"{"prompt":"hello"}"#,
                1000,
            )
            .await
            .expect("insert run");

        store
            .insert_run(
                "run-persist-2",
                None,
                "root-agent",
                "completed",
                r#"{"prompt":"world"}"#,
                2000,
            )
            .await
            .expect("insert run 2");

        // Verify while still open
        let run = store.get_run("run-persist-1").await.expect("query").unwrap();
        assert_eq!(run.title.as_deref(), Some("Persistence Test"));
        assert_eq!(run.status, "running");

        // Store is dropped here → file handle released
    }

    // Phase 2: reopen the same file and verify data persisted
    {
        let store = SqliteStore::new(db_path_str).await.expect("store reopened");
        // No need to run_migrations again — schema is already on disk

        let run1 = store.get_run("run-persist-1").await.expect("query").unwrap();
        assert_eq!(run1.id, "run-persist-1");
        assert_eq!(run1.title.as_deref(), Some("Persistence Test"));
        assert_eq!(run1.root_agent_id, "root-agent");
        assert_eq!(run1.status, "running");
        assert_eq!(run1.input_json, r#"{"prompt":"hello"}"#);
        assert_eq!(run1.started_at, 1000);

        let run2 = store.get_run("run-persist-2").await.expect("query").unwrap();
        assert_eq!(run2.id, "run-persist-2");
        assert_eq!(run2.status, "completed");
        assert_eq!(run2.started_at, 2000);
    }
}

/// Insert runs, update status, verify the update persists on disk.
#[tokio::test]
async fn file_db_update_status_persists() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("update_test.sqlite");

    {
        let store = SqliteStore::new(db_path.to_str().unwrap())
            .await
            .expect("store created");
        store.run_migrations().await.expect("migrations");

        store
            .insert_run("run-upd", Some("Update Test"), "agent", "running", "{}", 500)
            .await
            .expect("insert");

        store
            .update_run_status("run-upd", "completed", Some(r#"{"result":"ok"}"#), Some(600))
            .await
            .expect("update");
    }

    // Reopen and verify
    {
        let store = SqliteStore::new(db_path.to_str().unwrap())
            .await
            .expect("reopen");
        let run = store.get_run("run-upd").await.expect("query").unwrap();
        assert_eq!(run.status, "completed");
        assert_eq!(run.output_json.as_deref(), Some(r#"{"result":"ok"}"#));
        assert_eq!(run.finished_at, Some(600));
    }
}

/// Verify list_runs ordering and pagination on a file DB.
#[tokio::test]
async fn file_db_list_runs_pagination() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("pagination_test.sqlite");

    {
        let store = SqliteStore::new(db_path.to_str().unwrap())
            .await
            .expect("store");
        store.run_migrations().await.expect("migrations");

        for i in 0..5 {
            store
                .insert_run(
                    &format!("run-{i}"),
                    Some(&format!("Run {i}")),
                    "agent",
                    "completed",
                    "{}",
                    1000 + i as i64 * 100,
                )
                .await
                .expect("insert");
        }
    }

    {
        let store = SqliteStore::new(db_path.to_str().unwrap())
            .await
            .expect("reopen");

        let page1 = store.list_runs(3, 0).await.expect("page 1");
        assert_eq!(page1.len(), 3);
        // Ordered by started_at DESC
        assert_eq!(page1[0].id, "run-4");
        assert_eq!(page1[1].id, "run-3");
        assert_eq!(page1[2].id, "run-2");

        let page2 = store.list_runs(3, 3).await.expect("page 2");
        assert_eq!(page2.len(), 2);
        assert_eq!(page2[0].id, "run-1");
        assert_eq!(page2[1].id, "run-0");
    }
}

/// Full write → query roundtrip for execution nodes, steps, and messages.
#[tokio::test]
async fn file_db_full_write_query_roundtrip() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("roundtrip.sqlite");

    {
        let store = SqliteStore::new(db_path.to_str().unwrap())
            .await
            .expect("store");
        store.run_migrations().await.expect("migrations");

        // Insert run
        store
            .insert_run("rr-run", None, "root", "running", "{}", 1000)
            .await
            .expect("insert run");

        // Insert execution node
        store
            .insert_execution_node(
                "rr-en",
                "rr-run",
                "root",
                None,
                None,
                "running",
                r#"{"task":"do"}"#,
                1100,
            )
            .await
            .expect("insert exec node");

        // Insert step
        store
            .insert_step(
                "rr-step",
                "rr-run",
                "rr-en",
                "root",
                "model_call",
                r#"{"model":"test"}"#,
                1200,
            )
            .await
            .expect("insert step");

        // Insert messages
        store
            .insert_message(
                "rr-msg-1",
                "rr-run",
                "rr-en",
                Some("root"),
                "user",
                r#""hello""#,
                1300,
            )
            .await
            .expect("insert msg 1");

        store
            .insert_message(
                "rr-msg-2",
                "rr-run",
                "rr-en",
                Some("root"),
                "assistant",
                r#""world""#,
                1400,
            )
            .await
            .expect("insert msg 2");
    }

    // Reopen and verify all data
    {
        let store = SqliteStore::new(db_path.to_str().unwrap())
            .await
            .expect("reopen");

        let en = store.get_execution_node("rr-en").await.expect("query").unwrap();
        assert_eq!(en.run_id, "rr-run");
        assert_eq!(en.agent_id, "root");
        assert_eq!(en.input_json, r#"{"task":"do"}"#);

        let steps = store.list_steps("rr-run").await.expect("steps");
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].kind, "model_call");

        let msgs = store.list_messages("rr-en").await.expect("messages");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[1].role, "assistant");
    }
}

/// Verify WAL mode is active on file-based databases.
#[tokio::test]
async fn file_db_wal_mode_active() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("wal_test.sqlite");

    let store = SqliteStore::new(db_path.to_str().unwrap())
        .await
        .expect("store");
    store.run_migrations().await.expect("migrations");

    let pragma = store.verify_pragma().await.expect("verify pragma");

    // WAL mode should be enabled (journal_mode = "wal")
    assert!(
        pragma.journal_mode.eq_ignore_ascii_case("wal"),
        "expected WAL mode, got: {}",
        pragma.journal_mode
    );

    // A WAL file should exist alongside the database
    let wal_path = db_path.with_extension("sqlite-wal");
    assert!(
        wal_path.exists(),
        "WAL file should exist at {}",
        wal_path.display()
    );
}
