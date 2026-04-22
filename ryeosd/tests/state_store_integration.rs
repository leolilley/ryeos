//! End-to-end integration tests for StateStore with daemon

#[cfg(test)]
mod integration_tests {
    use std::sync::Arc;

    use ryeosd::state_store::{
        StateStore, NewThreadRecord, FinalizeThreadRecord, PersistedEventRecord,
    };
    use tempfile::TempDir;

    fn setup_state_store() -> (TempDir, Arc<StateStore>) {
        let tmpdir = TempDir::new().unwrap();
        let state_root = tmpdir.path().join(".state");
        let runtime_db_path = tmpdir.path().join("runtime.sqlite3");

        // Use a dummy signer — StateStore::new expects a NodeIdentitySigner,
        // but we can create one from a test identity.
        let test_key_path = tmpdir.path().join("test_key.pem");
        let identity = ryeosd::identity::NodeIdentity::create(&test_key_path)
            .expect("test identity creation should succeed");
        let signer = Arc::new(
            ryeosd::state_store::NodeIdentitySigner::from_identity(&identity)
        );

        let store = StateStore::new(
            state_root,
            runtime_db_path,
            signer,
        )
        .expect("StateStore creation should succeed");

        (tmpdir, Arc::new(store))
    }

    fn make_thread(
        thread_id: &str,
        chain_root_id: &str,
        kind: &str,
        item_ref: &str,
        upstream: Option<&str>,
    ) -> NewThreadRecord {
        NewThreadRecord {
            thread_id: thread_id.to_string(),
            chain_root_id: chain_root_id.to_string(),
            kind: kind.to_string(),
            item_ref: item_ref.to_string(),
            executor_ref: "test/executor".to_string(),
            launch_mode: "inline".to_string(),
            current_site_id: "site:test".to_string(),
            origin_site_id: "site:test".to_string(),
            upstream_thread_id: upstream.map(|s| s.to_string()),
            requested_by: Some("user:test".to_string()),
        }
    }

    #[test]
    fn state_store_can_create_root_thread() {
        let (_tmpdir, store) = setup_state_store();

        let thread = make_thread("T-root-1", "T-root-1", "directive", "test/directive", None);
        let persisted = store.create_thread(&thread).expect("create_thread should succeed");

        assert!(!persisted.is_empty(), "Should return persisted events");
        assert_eq!(persisted[0].event_type, "thread_created");
    }

    #[test]
    fn state_store_can_mark_thread_running() {
        let (_tmpdir, store) = setup_state_store();

        let thread = make_thread("T-running-1", "T-running-1", "directive", "test/item", None);
        store.create_thread(&thread).expect("create_thread should succeed");

        let persisted = store.mark_thread_running("T-running-1").expect("mark_thread_running should succeed");
        assert!(!persisted.is_empty());
    }

    #[test]
    fn state_store_can_attach_process() {
        let (_tmpdir, store) = setup_state_store();

        let thread = make_thread("T-proc-1", "T-proc-1", "directive", "test/item", None);
        store.create_thread(&thread).expect("create_thread should succeed");

        store
            .attach_thread_process("T-proc-1", 12345, 67890)
            .expect("attach_thread_process should succeed");
    }

    #[test]
    fn state_store_can_list_threads() {
        let (_tmpdir, store) = setup_state_store();

        let root = make_thread("T-list-1", "T-list-1", "directive", "test/item", None);
        store.create_thread(&root).expect("create_thread 1 should succeed");

        let child = make_thread("T-list-2", "T-list-1", "tool", "test/tool", Some("T-list-1"));
        store.create_thread(&child).expect("create_thread 2 should succeed");

        let threads = store.list_threads(100).expect("list_threads should succeed");
        assert!(threads.len() >= 2, "Should have at least 2 threads");
    }

    #[test]
    fn state_store_can_finalize_thread() {
        let (_tmpdir, store) = setup_state_store();

        let thread = make_thread("T-final-1", "T-final-1", "directive", "test/item", None);
        store.create_thread(&thread).expect("create_thread should succeed");
        store.mark_thread_running("T-final-1").expect("mark_thread_running should succeed");

        let finalize = FinalizeThreadRecord {
            status: "completed".to_string(),
            outcome_code: Some("success".to_string()),
            result_json: Some(serde_json::json!({"message": "done"})),
            error_json: None,
            artifacts: vec![],
            final_cost: None,
        };

        let persisted = store
            .finalize_thread("T-final-1", &finalize)
            .expect("finalize_thread should succeed");
        assert!(!persisted.is_empty());

        let detail = store
            .get_thread("T-final-1")
            .expect("get_thread should succeed")
            .expect("thread should exist");

        assert_eq!(detail.status, "completed");
    }
}
