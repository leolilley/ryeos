//! End-to-end integration tests for StateStore with daemon

#[cfg(test)]
mod integration_tests {
    use std::fs;
    use std::path::Path;
    use std::sync::Arc;

    use ryeosd::db::{
        Database, NewThreadRecord, ThreadDetail, ThreadListItem, PersistedEventRecord,
        NewEventRecord,
    };
    use ryeosd::kind_profiles::KindProfileRegistry;
    use tempfile::TempDir;

    /// Create test configuration with temporary paths
    fn setup_test_dirs() -> Result<(TempDir, String), Box<dyn std::error::Error>> {
        let tmpdir = TempDir::new()?;
        let db_path = tmpdir.path().join("projection.sqlite3").to_string_lossy().to_string();
        Ok((tmpdir, db_path))
    }

    #[test]
    fn daemon_can_create_root_thread() -> Result<(), Box<dyn std::error::Error>> {
        let (_tmpdir, db_path) = setup_test_dirs()?;
        let kind_profiles = Arc::new(KindProfileRegistry::load_defaults());

        // Initialize database
        let db = Database::new(db_path, kind_profiles)?;

        // Create a root thread
        let thread = NewThreadRecord {
            thread_id: "T-root-1".to_string(),
            chain_root_id: "T-root-1".to_string(),
            kind: "directive".to_string(),
            status: "created".to_string(),
            item_ref: "test/directive".to_string(),
            executor_ref: "test/executor".to_string(),
            launch_mode: "inline".to_string(),
            current_site_id: "site:test".to_string(),
            origin_site_id: "site:test".to_string(),
            upstream_thread_id: None,
            requested_by: Some("user:test".to_string()),
            summary_json: None,
        };

        let persisted = db.create_thread(&thread, None)?;
        assert!(!persisted.is_empty(), "Should return persisted events");
        assert_eq!(
            persisted[0].event_type, "thread_created",
            "First event should be thread_created"
        );

        Ok(())
    }

    #[test]
    fn daemon_can_append_events() -> Result<(), Box<dyn std::error::Error>> {
        let (_tmpdir, db_path) = setup_test_dirs()?;
        let kind_profiles = Arc::new(KindProfileRegistry::load_defaults());

        let db = Database::new(db_path, kind_profiles)?;

        // Create thread
        let thread = NewThreadRecord {
            thread_id: "T-events-1".to_string(),
            chain_root_id: "T-events-1".to_string(),
            kind: "directive".to_string(),
            status: "created".to_string(),
            item_ref: "test/item".to_string(),
            executor_ref: "test/executor".to_string(),
            launch_mode: "inline".to_string(),
            current_site_id: "site:test".to_string(),
            origin_site_id: "site:test".to_string(),
            upstream_thread_id: None,
            requested_by: None,
            summary_json: None,
        };

        let _created = db.create_thread(&thread, None)?;

        // Append events
        let events = vec![
            NewEventRecord {
                event_type: "turn_start".to_string(),
                storage_class: "indexed".to_string(),
                payload: serde_json::json!({"turn": 1}),
            },
            NewEventRecord {
                event_type: "turn_complete".to_string(),
                storage_class: "indexed".to_string(),
                payload: serde_json::json!({"turn": 1, "tokens": 50}),
            },
        ];

        let persisted = db.append_events("T-events-1", "T-events-1", &events)?;
        assert!(!persisted.is_empty(), "Should persist events");
        assert_eq!(persisted.len(), 2, "Should have 2 persisted events");

        Ok(())
    }

    #[test]
    fn daemon_can_mark_thread_running() -> Result<(), Box<dyn std::error::Error>> {
        let (_tmpdir, db_path) = setup_test_dirs()?;
        let kind_profiles = Arc::new(KindProfileRegistry::load_defaults());

        let db = Database::new(db_path, kind_profiles)?;

        let thread = NewThreadRecord {
            thread_id: "T-running-1".to_string(),
            chain_root_id: "T-running-1".to_string(),
            kind: "directive".to_string(),
            status: "created".to_string(),
            item_ref: "test/item".to_string(),
            executor_ref: "test/executor".to_string(),
            launch_mode: "inline".to_string(),
            current_site_id: "site:test".to_string(),
            origin_site_id: "site:test".to_string(),
            upstream_thread_id: None,
            requested_by: None,
            summary_json: None,
        };

        db.create_thread(&thread, None)?;

        // Mark as running
        let persisted = db.mark_thread_running("T-running-1")?;
        assert!(!persisted.is_empty());

        Ok(())
    }

    #[test]
    fn daemon_can_attach_process() -> Result<(), Box<dyn std::error::Error>> {
        let (_tmpdir, db_path) = setup_test_dirs()?;
        let kind_profiles = Arc::new(KindProfileRegistry::load_defaults());

        let db = Database::new(db_path, kind_profiles)?;

        let thread = NewThreadRecord {
            thread_id: "T-proc-1".to_string(),
            chain_root_id: "T-proc-1".to_string(),
            kind: "directive".to_string(),
            status: "created".to_string(),
            item_ref: "test/item".to_string(),
            executor_ref: "test/executor".to_string(),
            launch_mode: "inline".to_string(),
            current_site_id: "site:test".to_string(),
            origin_site_id: "site:test".to_string(),
            upstream_thread_id: None,
            requested_by: None,
            summary_json: None,
        };

        db.create_thread(&thread, None)?;

        // Attach process
        db.attach_thread_process("T-proc-1", 12345, 67890, None)?;

        Ok(())
    }

    #[test]
    fn daemon_can_list_threads_in_chain() -> Result<(), Box<dyn std::error::Error>> {
        let (_tmpdir, db_path) = setup_test_dirs()?;
        let kind_profiles = Arc::new(KindProfileRegistry::load_defaults());

        let db = Database::new(db_path, kind_profiles)?;

        // Create root
        let root = NewThreadRecord {
            thread_id: "T-list-root".to_string(),
            chain_root_id: "T-list-root".to_string(),
            kind: "directive".to_string(),
            status: "created".to_string(),
            item_ref: "test/item".to_string(),
            executor_ref: "test/executor".to_string(),
            launch_mode: "inline".to_string(),
            current_site_id: "site:test".to_string(),
            origin_site_id: "site:test".to_string(),
            upstream_thread_id: None,
            requested_by: None,
            summary_json: None,
        };

        db.create_thread(&root, None)?;

        // Create child
        let child = NewThreadRecord {
            thread_id: "T-list-child".to_string(),
            chain_root_id: "T-list-root".to_string(),
            kind: "tool".to_string(),
            status: "created".to_string(),
            item_ref: "test/tool".to_string(),
            executor_ref: "test/executor".to_string(),
            launch_mode: "inline".to_string(),
            current_site_id: "site:test".to_string(),
            origin_site_id: "site:test".to_string(),
            upstream_thread_id: Some("T-list-root".to_string()),
            requested_by: None,
            summary_json: None,
        };

        db.create_thread(&child, None)?;

        // List threads in chain (limit=100)
        let threads = db.list_threads(100)?;
        assert!(threads.len() >= 2, "Should have at least 2 threads");

        Ok(())
    }

    #[test]
    fn daemon_can_finalize_thread() -> Result<(), Box<dyn std::error::Error>> {
        let (_tmpdir, db_path) = setup_test_dirs()?;
        let kind_profiles = Arc::new(KindProfileRegistry::load_defaults());

        let db = Database::new(db_path, kind_profiles)?;

        let thread = NewThreadRecord {
            thread_id: "T-finalize-1".to_string(),
            chain_root_id: "T-finalize-1".to_string(),
            kind: "directive".to_string(),
            status: "created".to_string(),
            item_ref: "test/item".to_string(),
            executor_ref: "test/executor".to_string(),
            launch_mode: "inline".to_string(),
            current_site_id: "site:test".to_string(),
            origin_site_id: "site:test".to_string(),
            upstream_thread_id: None,
            requested_by: None,
            summary_json: None,
        };

        db.create_thread(&thread, None)?;
        db.mark_thread_running("T-finalize-1")?;

        // Finalize
        let finalize = ryeosd::db::FinalizeThreadRecord {
            status: "completed".to_string(),
            outcome_code: Some("success".to_string()),
            result_json: Some(serde_json::json!({"message": "done"})),
            error_json: None,
            metadata: None,
            summary_json: None,
            actual_spend: Some(10.0),
            budget_status: None,
            budget_metadata: None,
            final_cost: None,
            artifacts: vec![],
        };

        let persisted = db.finalize_thread("T-finalize-1", &finalize)?;
        assert!(!persisted.is_empty());

        Ok(())
    }
}
