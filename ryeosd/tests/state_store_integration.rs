//! End-to-end integration tests for StateStore with daemon
//!
//! Tests verify that:
//! - Thread edges are derived from snapshot upstream_thread_id (CAS-truth)
//! - Artifacts are derived from artifact_published events (CAS-truth)
//! - Projection rebuild recovers edges and artifacts from CAS
//! - create_continuation correctly sets upstream_thread_id
//! - No direct projection writes leak (projection drift is recoverable)

#[cfg(test)]
mod integration_tests {
    use std::sync::Arc;

    use ryeosd::state_store::{
        StateStore, NewThreadRecord, NewArtifactRecord, FinalizeThreadRecord,
    };
    use ryeosd::write_barrier::WriteBarrier;
    use tempfile::TempDir;

    fn setup_state_store() -> (TempDir, Arc<StateStore>) {
        let tmpdir = TempDir::new().unwrap();
        let state_root = tmpdir.path().join(".ai").join("state");
        let runtime_db_path = tmpdir.path().join("runtime.sqlite3");

        let test_key_path = tmpdir.path().join("test_key.pem");
        let identity = ryeosd::identity::NodeIdentity::create(&test_key_path)
            .expect("test identity creation should succeed");
        let signer = Arc::new(
            ryeosd::state_store::NodeIdentitySigner::from_identity(&identity)
        );

        let write_barrier = WriteBarrier::new();

        let store = StateStore::new(
            state_root,
            runtime_db_path,
            signer,
            write_barrier,
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

        let persisted = store.mark_thread_running("T-running-1", None).expect("mark_thread_running should succeed");
        assert!(!persisted.is_empty());
    }

    #[test]
    fn state_store_can_attach_process() {
        let (_tmpdir, store) = setup_state_store();

        let thread = make_thread("T-proc-1", "T-proc-1", "directive", "test/item", None);
        store.create_thread(&thread).expect("create_thread should succeed");

        store
            .attach_thread_process(
                "T-proc-1",
                12345,
                67890,
                &ryeosd::launch_metadata::RuntimeLaunchMetadata::default(),
            )
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
        store.mark_thread_running("T-final-1", None).expect("mark_thread_running should succeed");

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

    // ── CAS-truth derived projection tests ──────────────────────────

    #[test]
    fn edge_derived_from_snapshot_upstream() {
        let (_tmpdir, store) = setup_state_store();

        // Create root thread
        let root = make_thread("T-edge-root", "T-edge-root", "directive", "test/item", None);
        store.create_thread(&root).expect("create root should succeed");

        // Create child thread with upstream_thread_id
        let child = make_thread("T-edge-child", "T-edge-root", "tool", "test/tool", Some("T-edge-root"));
        store.create_thread(&child).expect("create child should succeed");

        // Verify edge exists in projection (derived from snapshot's upstream_thread_id)
        let edges = store.list_chain_edges("T-edge-root").expect("list_chain_edges should succeed");
        assert_eq!(edges.len(), 1, "should have one derived edge");
        assert_eq!(edges[0].source_thread_id, "T-edge-root");
        assert_eq!(edges[0].target_thread_id, "T-edge-child");
        assert_eq!(edges[0].edge_type, "spawned");
    }

    #[test]
    fn artifact_derived_from_event() {
        let (_tmpdir, store) = setup_state_store();

        let thread = make_thread("T-art-1", "T-art-1", "directive", "test/item", None);
        store.create_thread(&thread).expect("create_thread should succeed");
        store.mark_thread_running("T-art-1", None).expect("mark_thread_running should succeed");

        // Publish an artifact (directly, not via finalize)
        let artifact = NewArtifactRecord {
            artifact_type: "output".to_string(),
            uri: "file:///tmp/output.txt".to_string(),
            content_hash: Some("abc123".to_string()),
            metadata: Some(serde_json::json!({"size": 42})),
        };

        let (record, event) = store
            .publish_artifact("T-art-1", &artifact)
            .expect("publish_artifact should succeed");

        assert_eq!(record.artifact_type, "output");
        assert_eq!(event.event_type, "artifact_published");

        // Verify artifact is in the projection (derived from the event)
        let artifacts = store.list_thread_artifacts("T-art-1")
            .expect("list_thread_artifacts should succeed");
        assert_eq!(artifacts.len(), 1, "should have one derived artifact");
        assert_eq!(artifacts[0].artifact_type, "output");
    }

    #[test]
    fn artifact_derived_on_finalize() {
        let (_tmpdir, store) = setup_state_store();

        let thread = make_thread("T-art-fin", "T-art-fin", "directive", "test/item", None);
        store.create_thread(&thread).expect("create_thread should succeed");
        store.mark_thread_running("T-art-fin", None).expect("mark_thread_running should succeed");

        let finalize = FinalizeThreadRecord {
            status: "completed".to_string(),
            outcome_code: Some("success".to_string()),
            result_json: Some(serde_json::json!({"value": 42})),
            error_json: None,
            artifacts: vec![
                NewArtifactRecord {
                    artifact_type: "result".to_string(),
                    uri: "file:///tmp/result.json".to_string(),
                    content_hash: Some("deadbeef".to_string()),
                    metadata: None,
                },
            ],
            final_cost: None,
        };

        store.finalize_thread("T-art-fin", &finalize)
            .expect("finalize_thread should succeed");

        // Verify artifact was derived from artifact_published event during finalize
        let artifacts = store.list_thread_artifacts("T-art-fin")
            .expect("list_thread_artifacts should succeed");
        // Finalize publishes artifact_published events, which should derive artifact rows
        assert_eq!(artifacts.len(), 1, "should have one derived artifact from finalize");
        assert_eq!(artifacts[0].artifact_type, "result");
    }

    #[test]
    fn continuation_sets_upstream_and_derives_edge() {
        let (_tmpdir, store) = setup_state_store();

        let thread = make_thread("T-cont-1", "T-cont-1", "directive", "test/item", None);
        store.create_thread(&thread).expect("create_thread should succeed");
        store.mark_thread_running("T-cont-1", None).expect("mark_thread_running should succeed");

        // Create a successor via continuation
        let successor = make_thread("T-cont-2", "T-cont-1", "directive", "test/item2", None);
        let events = store
            .create_continuation(&successor, "T-cont-1", "T-cont-1", Some("retry"))
            .expect("create_continuation should succeed");

        assert!(!events.is_empty(), "should return persisted events");

        // Verify the source thread is now "continued"
        let source_detail = store
            .get_thread("T-cont-1")
            .expect("get_thread should succeed")
            .expect("source thread should exist");
        assert_eq!(source_detail.status, "continued");

        // Verify successor was created
        let successor_detail = store
            .get_thread("T-cont-2")
            .expect("get_thread should succeed")
            .expect("successor thread should exist");
        assert_eq!(successor_detail.status, "created");

        // Verify edge was derived from successor's upstream_thread_id
        let edges = store.list_chain_edges("T-cont-1")
            .expect("list_chain_edges should succeed");
        assert_eq!(edges.len(), 1, "should have one edge from continuation");
        assert_eq!(edges[0].source_thread_id, "T-cont-1");
        assert_eq!(edges[0].target_thread_id, "T-cont-2");
    }

    #[test]
    fn rebuild_recovers_edges_from_cas() {
        let (_tmpdir, store) = setup_state_store();

        let root = make_thread("T-reb-root", "T-reb-root", "directive", "test/item", None);
        store.create_thread(&root).expect("create root should succeed");

        let child = make_thread("T-reb-child", "T-reb-root", "tool", "test/tool", Some("T-reb-root"));
        store.create_thread(&child).expect("create child should succeed");

        // Verify edges exist before rebuild
        let edges_before = store.list_chain_edges("T-reb-root")
            .expect("list_chain_edges should succeed");
        assert_eq!(edges_before.len(), 1);

        // Now delete the projection and rebuild from CAS
        store.with_state_db(|db| {
            let cas_root = db.cas_root().to_path_buf();
            let refs_root = db.refs_root().to_path_buf();

            // Clear edges from projection
            db.projection().connection().execute_batch(
                "DELETE FROM thread_edges; DELETE FROM projection_meta;"
            ).expect("clear projection should succeed");

            // Rebuild from CAS
            let report = ryeos_state::rebuild::rebuild_projection(
                db.projection(), &cas_root, &refs_root,
            ).expect("rebuild should succeed");

            assert_eq!(report.chains_rebuilt, 1);
            assert_eq!(report.threads_restored, 2);

            Ok::<_, anyhow::Error>(())
        }).expect("with_state_db should succeed");

        // Verify edges were recovered from snapshot upstream_thread_id during rebuild
        let edges_after = store.list_chain_edges("T-reb-root")
            .expect("list_chain_edges should succeed");
        assert_eq!(edges_after.len(), 1, "edge should be recovered after rebuild");
        assert_eq!(edges_after[0].source_thread_id, "T-reb-root");
        assert_eq!(edges_after[0].target_thread_id, "T-reb-child");
    }

    #[test]
    fn rebuild_recovers_artifacts_from_cas() {
        let (_tmpdir, store) = setup_state_store();

        let thread = make_thread("T-reb-art", "T-reb-art", "directive", "test/item", None);
        store.create_thread(&thread).expect("create_thread should succeed");
        store.mark_thread_running("T-reb-art", None).expect("mark_thread_running should succeed");

        let finalize = FinalizeThreadRecord {
            status: "completed".to_string(),
            outcome_code: Some("success".to_string()),
            result_json: None,
            error_json: None,
            artifacts: vec![
                NewArtifactRecord {
                    artifact_type: "output_file".to_string(),
                    uri: "file:///tmp/out.txt".to_string(),
                    content_hash: Some("hash123".to_string()),
                    metadata: Some(serde_json::json!({"lines": 10})),
                },
            ],
            final_cost: None,
        };

        store.finalize_thread("T-reb-art", &finalize)
            .expect("finalize should succeed");

        // Clear artifacts from projection
        store.with_state_db(|db| {
            db.projection().connection().execute_batch(
                "DELETE FROM thread_artifacts; DELETE FROM projection_meta;"
            ).expect("clear artifacts should succeed");
            Ok::<_, anyhow::Error>(())
        }).expect("with_state_db should succeed");

        // Rebuild from CAS
        store.with_state_db(|db| {
            let cas_root = db.cas_root().to_path_buf();
            let refs_root = db.refs_root().to_path_buf();

            let report = ryeos_state::rebuild::rebuild_projection(
                db.projection(), &cas_root, &refs_root,
            ).expect("rebuild should succeed");

            assert_eq!(report.chains_rebuilt, 1);

            Ok::<_, anyhow::Error>(())
        }).expect("with_state_db should succeed");

        // Verify artifact was recovered from artifact_published event during rebuild
        let artifacts = store.list_thread_artifacts("T-reb-art")
            .expect("list_thread_artifacts should succeed");
        assert_eq!(artifacts.len(), 1, "artifact should be recovered after rebuild");
        assert_eq!(artifacts[0].artifact_type, "output_file");
    }

    #[test]
    fn catch_up_projection_recovers_new_state() {
        let (_tmpdir, store) = setup_state_store();

        let thread = make_thread("T-catchup", "T-catchup", "directive", "test/item", None);
        store.create_thread(&thread).expect("create_thread should succeed");

        // Simulate projection drift: update projection_meta to a fake old hash
        store.with_state_db(|db| {
            let meta = ryeos_state::projection::ProjectionMeta {
                chain_root_id: "T-catchup".to_string(),
                indexed_chain_state_hash: "deadbeef".repeat(4),
                updated_at: "2020-01-01T00:00:00Z".to_string(),
            };
            db.projection().update_projection_meta(&meta)
                .expect("update meta should succeed");

            // Clear threads table to simulate drift
            db.projection().connection()
                .execute_batch("DELETE FROM threads")
                .expect("clear threads should succeed");

            Ok::<_, anyhow::Error>(())
        }).expect("with_state_db should succeed");

        // Run catch-up
        store.with_state_db(|db| {
            let cas_root = db.cas_root().to_path_buf();
            let refs_root = db.refs_root().to_path_buf();

            let report = ryeos_state::rebuild::catch_up_projection(
                db.projection(), &cas_root, &refs_root,
            ).expect("catch_up should succeed");

            assert_eq!(report.chains_updated, 1, "chain should be caught up");

            Ok::<_, anyhow::Error>(())
        }).expect("with_state_db should succeed");

        // Verify thread was recovered
        let detail = store
            .get_thread("T-catchup")
            .expect("get_thread should succeed")
            .expect("thread should exist after catch-up");
        assert_eq!(detail.status, "created");
    }

    #[test]
    fn full_chain_lifecycle_e2e() {
        let (_tmpdir, store) = setup_state_store();

        // 1. Create root thread
        let root = make_thread("T-e2e", "T-e2e", "directive", "test/e2e", None);
        store.create_thread(&root).expect("create root");
        store.mark_thread_running("T-e2e", None).expect("mark running");

        // 2. Spawn child thread
        let child = make_thread("T-e2e-child", "T-e2e", "tool", "test/child", Some("T-e2e"));
        store.create_thread(&child).expect("create child");

        // 3. Finalize root with artifacts
        let finalize = FinalizeThreadRecord {
            status: "completed".to_string(),
            outcome_code: Some("done".to_string()),
            result_json: Some(serde_json::json!({"answer": 42})),
            error_json: None,
            artifacts: vec![
                NewArtifactRecord {
                    artifact_type: "summary".to_string(),
                    uri: "file:///tmp/summary.json".to_string(),
                    content_hash: None,
                    metadata: None,
                },
            ],
            final_cost: Some(vec![("tokens".to_string(), "1500".to_string())]),
        };
        store.finalize_thread("T-e2e", &finalize).expect("finalize");

        // 4. Verify everything in projection
        let threads = store.list_chain_threads("T-e2e").expect("list_chain_threads");
        assert_eq!(threads.len(), 2);

        let edges = store.list_chain_edges("T-e2e").expect("list_chain_edges");
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].source_thread_id, "T-e2e");
        assert_eq!(edges[0].target_thread_id, "T-e2e-child");

        let artifacts = store.list_thread_artifacts("T-e2e").expect("list_artifacts");
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].artifact_type, "summary");

        let root_detail = store.get_thread("T-e2e").expect("get_thread").expect("thread exists");
        assert_eq!(root_detail.status, "completed");

        // 5. Delete projection and rebuild — everything should recover
        store.with_state_db(|db| {
            db.projection().connection().execute_batch(
                "DELETE FROM thread_edges;
                 DELETE FROM thread_artifacts;
                 DELETE FROM threads;
                 DELETE FROM events;
                 DELETE FROM projection_meta;"
            ).expect("clear projection");

            let cas_root = db.cas_root().to_path_buf();
            let refs_root = db.refs_root().to_path_buf();
            let report = ryeos_state::rebuild::rebuild_projection(
                db.projection(), &cas_root, &refs_root,
            ).expect("rebuild");

            assert!(report.chains_rebuilt >= 1);

            Ok::<_, anyhow::Error>(())
        }).expect("with_state_db");

        // 6. Verify everything recovered
        let threads_after = store.list_chain_threads("T-e2e").expect("list_chain_threads after");
        assert_eq!(threads_after.len(), 2, "all threads should be recovered");

        let edges_after = store.list_chain_edges("T-e2e").expect("list_edges after");
        assert_eq!(edges_after.len(), 1, "edge should be recovered");

        let artifacts_after = store.list_thread_artifacts("T-e2e").expect("list_artifacts after");
        assert_eq!(artifacts_after.len(), 1, "artifact should be recovered");

        let root_after = store.get_thread("T-e2e").expect("get_thread after").expect("thread exists after");
        assert_eq!(root_after.status, "completed", "status should be recovered");
    }
}
