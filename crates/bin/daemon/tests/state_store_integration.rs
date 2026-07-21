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

    use ryeos_app::state_store::{
        FinalizeThreadRecord, NewArtifactRecord, NewThreadRecord, StateStore,
    };
    use ryeos_app::write_barrier::WriteBarrier;
    use tempfile::TempDir;

    fn captured_policy(item_ref: &str) -> ryeos_state::objects::CapturedThreadHistoryPolicy {
        let hash = "a".repeat(64);
        ryeos_state::objects::CapturedThreadHistoryPolicy {
            retention: ryeos_state::objects::ThreadHistoryRetention::Durable,
            canonical_item_ref: item_ref.to_string(),
            item_content_hash: hash.clone(),
            item_signer_fingerprint: Some(hash.clone()),
            item_trust_class: ryeos_state::objects::CapturedItemTrustClass::Trusted,
            kind_schema_content_hash: hash,
            resolved_from: ryeos_state::objects::CapturedPolicyProvenance::NodeDefault {
                node_policy:
                    ryeos_state::objects::CapturedNodeHistoryPolicyProvenance::MissingConfig,
            },
        }
    }

    fn setup_state_store() -> (TempDir, Arc<StateStore>) {
        let tmpdir = TempDir::new().unwrap();
        let runtime_state_dir = tmpdir.path().join(".ai").join("state");
        let runtime_db_path = tmpdir.path().join("runtime.sqlite3");

        let test_key_path = tmpdir.path().join("test_key.pem");
        let identity = ryeos_app::identity::NodeIdentity::create(&test_key_path)
            .expect("test identity creation should succeed");
        let signer = Arc::new(ryeos_app::state_store::NodeIdentitySigner::from_identity(
            &identity,
        ));
        let mut head_trust = ryeos_state::refs::TrustStore::new();
        head_trust.insert(
            identity.fingerprint().to_string(),
            *identity.verifying_key(),
        );

        let write_barrier = WriteBarrier::new();

        let store = StateStore::new_with_head_trust(
            tmpdir.path().to_path_buf(),
            runtime_state_dir,
            runtime_db_path,
            signer,
            write_barrier,
            Arc::new(head_trust),
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
            launch_mode: "wait".to_string(),
            current_site_id: "site:test".to_string(),
            origin_site_id: "site:test".to_string(),
            upstream_thread_id: upstream.map(|s| s.to_string()),
            requested_by: Some("user:test".to_string()),
            project_root: None,
            project_authority: ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS,
            base_project_snapshot_hash: None,
            usage_subject: None,
            usage_subject_asserted_by: None,
            captured_history_policy: (thread_id == chain_root_id)
                .then(|| captured_policy(item_ref)),
        }
    }

    fn make_project_thread(
        thread_id: &str,
        chain_root_id: &str,
        kind: &str,
        item_ref: &str,
        upstream: Option<&str>,
    ) -> NewThreadRecord {
        let mut thread = make_thread(thread_id, chain_root_id, kind, item_ref, upstream);
        let project_root = std::env::temp_dir();
        thread.project_root = Some(project_root.clone());
        thread.project_authority = ryeos_state::objects::ExecutionProjectAuthority::live(
            project_root.clone(),
            format!("local:{}", project_root.display()),
            ryeos_state::objects::LiveProjectAccess::ReadWrite,
            ryeos_state::objects::LiveFilesystemConfinement::standard_descriptor_rooted(),
            ryeos_state::objects::EnvironmentAuthority::None,
            Vec::new(),
        )
        .unwrap();
        thread
    }

    fn seed_follow_parent(store: &StateStore) {
        store
            .create_thread_for_test(&make_thread("P", "P", "graph", "graph:test/graph", None))
            .expect("seed authoritative follow parent");
    }

    fn follow_seed(follow_key: &str) -> ryeos_app::runtime_db::NewFollowWaiter {
        ryeos_app::runtime_db::NewFollowWaiter {
            follow_key: follow_key.to_string(),
            parent_thread_id: "P".to_string(),
            parent_chain_root_id: "P".to_string(),
            follow_node: "node-a".to_string(),
            graph_run_id: "gr-1".to_string(),
            step_count: 0,
            frontier_id: None,
            fanout: false,
            expected_children: 1,
            child_project_authority: None,
        }
    }

    fn set_follow_child(store: &StateStore, follow_key: &str, child: &str, root: &str) {
        let sealed =
            ryeos_app::thread_lifecycle::SealedRootExecutionRequest::storage_test_fixture();
        let item_ref = sealed.item_ref();
        let spec_hash = ryeos_app::runtime_db::follow_child_spec_hash(
            item_ref,
            &std::collections::BTreeMap::new(),
            &serde_json::json!({}),
            None,
        )
        .unwrap();
        store
            .set_follow_child(follow_key, 0, item_ref, &spec_hash, child, root, &sealed)
            .unwrap();
    }

    #[test]
    fn follow_reserve_is_idempotent_and_rejects_seed_conflict() {
        let (_tmp, store) = setup_state_store();
        seed_follow_parent(&store);
        let seed = follow_seed("P/gr-1/node-a/0");
        let w = store.reserve_follow(&seed).unwrap();
        assert_eq!(w.phase, ryeos_app::runtime_db::follow_phase::RESERVED);
        // Same seed → get-or-create returns the same reserved row.
        let again = store.reserve_follow(&seed).unwrap();
        assert_eq!(again.follow_key, w.follow_key);
        assert_eq!(again.phase, ryeos_app::runtime_db::follow_phase::RESERVED);
        // Same key, conflicting seed (different frontier) → rejected, not reused.
        let mut conflicting = follow_seed("P/gr-1/node-a/0");
        conflicting.frontier_id = Some("frontier-x".to_string());
        assert!(store.reserve_follow(&conflicting).is_err());
    }

    #[test]
    fn follow_reserve_rejects_cohort_size_conflict() {
        let (_tmp, store) = setup_state_store();
        seed_follow_parent(&store);
        store.reserve_follow(&follow_seed("size-conflict")).unwrap();

        let mut conflicting = follow_seed("size-conflict");
        conflicting.fanout = true;
        conflicting.expected_children = 2;
        assert!(store.reserve_follow(&conflicting).is_err());

        let waiter = store
            .get_follow_waiter_by_key("size-conflict")
            .unwrap()
            .unwrap();
        assert!(!waiter.fanout);
        assert_eq!(waiter.expected_children, 1);
    }

    #[test]
    fn follow_set_child_refuses_overwrite() {
        let (_tmp, store) = setup_state_store();
        seed_follow_parent(&store);
        store.reserve_follow(&follow_seed("k1")).unwrap();
        set_follow_child(&store, "k1", "C", "C");
        // Idempotent: the identical child is a no-op.
        set_follow_child(&store, "k1", "C", "C");
        // A different child would strand the original → refused.
        let sealed =
            ryeos_app::thread_lifecycle::SealedRootExecutionRequest::storage_test_fixture();
        let item_ref = sealed.item_ref();
        let hash = ryeos_app::runtime_db::follow_child_spec_hash(
            item_ref,
            &std::collections::BTreeMap::new(),
            &serde_json::json!({}),
            None,
        )
        .unwrap();
        assert!(store
            .set_follow_child("k1", 0, item_ref, &hash, "C2", "C2", &sealed)
            .is_err());
        let w = store.get_follow_waiter_by_key("k1").unwrap().unwrap();
        assert_eq!(w.children.len(), 1);
        assert_eq!(w.children[0].child_thread_id, "C");
    }

    #[test]
    fn follow_set_parent_successor_refuses_overwrite() {
        let (_tmp, store) = setup_state_store();
        seed_follow_parent(&store);
        store.reserve_follow(&follow_seed("k2")).unwrap();
        store.set_follow_parent_successor("k2", "S").unwrap();
        store.set_follow_parent_successor("k2", "S").unwrap(); // idempotent
        assert!(store.set_follow_parent_successor("k2", "S2").is_err());
    }

    #[test]
    fn follow_mark_waiting_requires_child_and_successor() {
        let (_tmp, store) = setup_state_store();
        seed_follow_parent(&store);
        store.reserve_follow(&follow_seed("k3")).unwrap();
        // Neither child nor successor recorded → cannot mark waiting.
        assert!(store.mark_follow_waiting("k3").is_err());
        set_follow_child(&store, "k3", "C", "C");
        // Child but no successor → still cannot.
        assert!(store.mark_follow_waiting("k3").is_err());
        store.set_follow_parent_successor("k3", "S").unwrap();
        store.mark_follow_waiting("k3").unwrap();
        assert_eq!(
            store.get_follow_waiter_by_key("k3").unwrap().unwrap().phase,
            ryeos_app::runtime_db::follow_phase::WAITING
        );
        // Idempotent on waiting.
        store.mark_follow_waiting("k3").unwrap();
    }

    #[test]
    fn follow_child_terminal_transitions_waiting_to_ready_once() {
        let (_tmp, store) = setup_state_store();
        seed_follow_parent(&store);
        store.reserve_follow(&follow_seed("k4")).unwrap();
        set_follow_child(&store, "k4", "C", "Croot");
        store.set_follow_parent_successor("k4", "S").unwrap();
        store.mark_follow_waiting("k4").unwrap();

        let envelope = serde_json::json!({ "status": "completed", "result": { "x": 1 } });
        // First terminal for the child chain: waiting → ready, returns true.
        let first = store
            .mark_follow_child_terminal("Croot", "C", "completed", &envelope)
            .unwrap();
        assert!(first);
        let w = store.get_follow_waiter_by_key("k4").unwrap().unwrap();
        assert_eq!(w.phase, ryeos_app::runtime_db::follow_phase::READY);
        assert_eq!(w.children[0].terminal_status.as_deref(), Some("completed"));
        // Duplicate identical terminal is a no-op (already ready) → false.
        let second = store
            .mark_follow_child_terminal("Croot", "C", "completed", &envelope)
            .unwrap();
        assert!(!second);
        // An unknown child chain has no waiter → false, not an error.
        assert!(!store
            .mark_follow_child_terminal("unknown-chain", "C", "completed", &envelope)
            .unwrap());
    }

    /// Make `id` a RUNNING native-resume source with a captured ResumeContext —
    /// the complete precondition `create_follow_resume_successor` requires.
    fn seed_continuable(store: &StateStore, id: &str, kind: &str) {
        use ryeos_app::launch_metadata::{ResumeContext, RuntimeLaunchMetadata};
        use ryeos_engine::contracts::{
            EffectivePrincipal, ExecutionHints, Principal, ProjectContext,
        };
        let item_ref = match kind {
            "directive" => "directive:test/item",
            "graph" => "graph:test/graph",
            other => panic!("unsupported continuable fixture kind: {other}"),
        };
        store.mark_thread_running(id, None).unwrap();
        store
            .seed_launch_metadata(
                id,
                &RuntimeLaunchMetadata::default()
                    .with_native_resume(ryeos_engine::contracts::NativeResumeSpec::default())
                    .with_resume_context(ResumeContext {
                        kind: kind.into(),
                        item_ref: item_ref.into(),
                        ref_bindings: std::collections::BTreeMap::new(),
                        launch_mode: "wait".into(),
                        parameters: serde_json::json!({}),
                        project_context: ProjectContext::LocalPath {
                            path: std::env::temp_dir(),
                        },
                        project_authority: ryeos_state::objects::ExecutionProjectAuthority::live(
                            std::env::temp_dir(),
                            format!("local:{}", std::env::temp_dir().display()),
                            ryeos_state::objects::LiveProjectAccess::ReadWrite,
                            ryeos_state::objects::LiveFilesystemConfinement::standard_descriptor_rooted(),
                            ryeos_state::objects::EnvironmentAuthority::None,
                            Vec::new(),
                        )
                        .unwrap(),
                        lifecycle_authority:
                            ryeos_state::objects::ExecutionLifecycleAuthority::DAEMON_RESTARTABLE,
                        stable_project_identity: Some(
                            ryeos_app::launch_metadata::StableProjectIdentity::from_path(
                                &std::env::temp_dir(),
                                "site:test",
                            )
                            .unwrap(),
                        ),
                        local_overlay_root: Some(std::env::temp_dir()),
                        original_snapshot_hash: None,
                        original_pushed_head_ref: None,
                        state_root: None,
                        current_site_id: "site:test".into(),
                        origin_site_id: "site:test".into(),
                        requested_by: EffectivePrincipal::Local(Principal {
                            fingerprint: "fp".into(),
                            scopes: vec![],
                        }),
                        execution_hints: ExecutionHints::default(),
                        effective_caps: vec![],
                        parent_delegation_caps: None,
                        executor_ref: None,
                        runtime_ref: None,
                    }),
            )
            .unwrap();
    }

    fn seed_projectless_continuable(store: &StateStore, id: &str, kind: &str) {
        use ryeos_app::launch_metadata::{ResumeContext, RuntimeLaunchMetadata};
        use ryeos_engine::contracts::{
            EffectivePrincipal, ExecutionHints, Principal, ProjectContext,
        };
        let item_ref = match kind {
            "directive" => "directive:test/item",
            "graph" => "graph:test/graph",
            other => panic!("unsupported continuable fixture kind: {other}"),
        };
        store.mark_thread_running(id, None).unwrap();
        store
            .seed_launch_metadata(
                id,
                &RuntimeLaunchMetadata::default()
                    .with_native_resume(ryeos_engine::contracts::NativeResumeSpec::default())
                    .with_launch_driver(ryeos_state::objects::ExecutionLaunchDriver::ManagedRuntime)
                    .with_resume_context(ResumeContext {
                        kind: kind.into(),
                        item_ref: item_ref.into(),
                        ref_bindings: std::collections::BTreeMap::new(),
                        launch_mode: "wait".into(),
                        parameters: serde_json::json!({}),
                        project_context: ProjectContext::None,
                        project_authority:
                            ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS,
                        lifecycle_authority:
                            ryeos_state::objects::ExecutionLifecycleAuthority::DAEMON_RESTARTABLE,
                        stable_project_identity: None,
                        local_overlay_root: None,
                        original_snapshot_hash: None,
                        original_pushed_head_ref: None,
                        state_root: None,
                        current_site_id: "site:test".into(),
                        origin_site_id: "site:test".into(),
                        requested_by: EffectivePrincipal::Local(Principal {
                            fingerprint: "fp".into(),
                            scopes: vec![],
                        }),
                        execution_hints: ExecutionHints::default(),
                        effective_caps: vec![],
                        parent_delegation_caps: None,
                        executor_ref: None,
                        runtime_ref: None,
                    }),
            )
            .unwrap();
    }

    #[test]
    fn follow_spawn_sequence_reaches_waiting_with_child_and_successor() {
        let (_tmp, store) = setup_state_store();
        // Running parent with captured launch identity.
        store
            .create_thread_for_test(&make_thread("P", "P", "graph", "graph:test/graph", None))
            .unwrap();
        seed_projectless_continuable(&store, "P", "graph");

        // The ordered spawn sequence, as the handler drives it.
        store
            .reserve_follow(&follow_seed("P/gr-1/node-a/0"))
            .unwrap();
        // Child is a FRESH ROOT: its own chain root, no upstream braid.
        store
            .create_thread_for_test(&make_thread("C", "C", "graph", "graph:test/graph", None))
            .unwrap();
        set_follow_child(&store, "P/gr-1/node-a/0", "C", "C");
        // Parent follow-resume successor: created (not launched), settles parent.
        store
            .create_follow_resume_successor(
                &make_thread("S", "P", "graph", "graph:test/graph", Some("P")),
                "P",
                "P",
            )
            .unwrap();
        // The signed continuation edge is committed with successor birth,
        // before the operational waiter is linked. Live recovery uses this as
        // the zero-gap ownership proof while the child is still running.
        assert!(store.is_follow_resume_successor("P", "S").unwrap());
        assert!(store.get_follow_waiter_by_successor("S").unwrap().is_none());
        store
            .set_follow_parent_successor("P/gr-1/node-a/0", "S")
            .unwrap();
        store.mark_follow_waiting("P/gr-1/node-a/0").unwrap();

        // The composite state the handler guarantees before the detached launch.
        let w = store
            .get_follow_waiter_by_key("P/gr-1/node-a/0")
            .unwrap()
            .unwrap();
        assert_eq!(w.phase, ryeos_app::runtime_db::follow_phase::WAITING);
        assert_eq!(w.children.len(), 1);
        assert_eq!(w.children[0].child_thread_id, "C");
        assert_eq!(w.parent_successor_thread_id.as_deref(), Some("S"));
        // Child is a fresh root.
        let child = store.get_thread("C").unwrap().unwrap();
        assert_eq!(child.chain_root_id, "C");
        assert!(child.upstream_thread_id.is_none());
        // Parent settled `continued`; successor `created` (not launched).
        assert_eq!(store.get_thread("P").unwrap().unwrap().status, "continued");
        let succ = store.get_thread("S").unwrap().unwrap();
        assert_eq!(succ.status, "created");
        assert_eq!(succ.upstream_thread_id.as_deref(), Some("P"));
        // The successor carries the follow-resume marker used for later wakeup.
        assert!(store.is_follow_resume_successor("P", "S").unwrap());
    }

    #[test]
    fn sequential_follow_successors_preserve_native_resume_eligibility() {
        use ryeos_engine::contracts::{CancellationMode, NativeResumeSpec};

        let (_tmp, store) = setup_state_store();
        store
            .create_thread_for_test(&make_project_thread(
                "P",
                "P",
                "graph",
                "graph:test/graph",
                None,
            ))
            .unwrap();
        seed_continuable(&store, "P", "graph");

        let policy = NativeResumeSpec {
            checkpoint_interval_secs: 19,
            max_auto_resume_attempts: 3,
        };
        let mut parent_metadata = store.get_launch_metadata("P").unwrap().unwrap();
        parent_metadata.cancellation_mode = Some(CancellationMode::Hard);
        parent_metadata.native_resume = Some(policy.clone());
        let resume_context = parent_metadata.resume_context.clone().unwrap();
        store.seed_launch_metadata("P", &parent_metadata).unwrap();

        store
            .create_follow_resume_successor(
                &make_project_thread("S1", "P", "graph", "graph:test/graph", Some("P")),
                "P",
                "P",
            )
            .unwrap();
        let first_metadata = store.get_launch_metadata("S1").unwrap().unwrap();
        assert_eq!(first_metadata.native_resume, Some(policy.clone()));
        assert_eq!(
            first_metadata.cancellation_mode,
            Some(CancellationMode::Hard)
        );
        assert_eq!(first_metadata.resume_context, Some(resume_context.clone()));
        assert_eq!(
            first_metadata.continuation_source_thread_id.as_deref(),
            Some("P")
        );
        assert!(first_metadata.checkpoint_dir.is_none());

        // Model the first resumed segment reaching its second follow callback.
        // Its authoritative seed must itself satisfy the same follow-resume
        // contract; no live predecessor metadata is consulted on this hop.
        store.mark_thread_running("S1", None).unwrap();
        store
            .create_follow_resume_successor(
                &make_project_thread("S2", "P", "graph", "graph:test/graph", Some("S1")),
                "S1",
                "P",
            )
            .unwrap();
        let second_metadata = store.get_launch_metadata("S2").unwrap().unwrap();
        assert_eq!(second_metadata.native_resume, Some(policy));
        assert_eq!(
            second_metadata.cancellation_mode,
            Some(CancellationMode::Hard)
        );
        assert_eq!(second_metadata.resume_context, Some(resume_context));
        assert_eq!(
            second_metadata.continuation_source_thread_id.as_deref(),
            Some("S1")
        );
        assert!(second_metadata.checkpoint_dir.is_none());
    }

    #[test]
    fn follow_resume_rejects_source_without_native_resume_before_mutation() {
        let (_tmp, store) = setup_state_store();
        store
            .create_thread_for_test(&make_project_thread(
                "P-no-resume",
                "P-no-resume",
                "graph",
                "graph:test/graph",
                None,
            ))
            .unwrap();
        seed_continuable(&store, "P-no-resume", "graph");
        let mut metadata = store.get_launch_metadata("P-no-resume").unwrap().unwrap();
        metadata.native_resume = None;
        store
            .seed_launch_metadata("P-no-resume", &metadata)
            .unwrap();

        let error = store
            .create_follow_resume_successor(
                &make_project_thread(
                    "S-refused",
                    "P-no-resume",
                    "graph",
                    "graph:test/graph",
                    Some("P-no-resume"),
                ),
                "P-no-resume",
                "P-no-resume",
            )
            .expect_err("follow resume requires a native-resume source");

        assert!(error.to_string().contains("does not declare native_resume"));
        assert_eq!(
            store.get_thread("P-no-resume").unwrap().unwrap().status,
            "running"
        );
        assert!(store.get_thread("S-refused").unwrap().is_none());
        assert!(store.get_launch_metadata("S-refused").unwrap().is_none());
    }

    #[test]
    fn follow_adopts_existing_successor_when_waiter_missing_it() {
        let (_tmp, store) = setup_state_store();
        store
            .create_thread_for_test(&make_project_thread(
                "P",
                "P",
                "graph",
                "graph:test/graph",
                None,
            ))
            .unwrap();
        seed_continuable(&store, "P", "graph");
        store.reserve_follow(&follow_seed("k")).unwrap();
        set_follow_child(&store, "k", "C", "C");
        // A prior attempt created the follow-resume successor (parent now
        // continued) but crashed before recording it on the waiter.
        store
            .create_follow_resume_successor(
                &make_project_thread("S", "P", "graph", "graph:test/graph", Some("P")),
                "P",
                "P",
            )
            .unwrap();
        assert!(store
            .get_follow_waiter_by_key("k")
            .unwrap()
            .unwrap()
            .parent_successor_thread_id
            .is_none());

        // Recovery: the existing successor is the follow-resume one → adopt it
        // rather than re-create (which would fail: parent no longer running).
        assert!(store.is_follow_resume_successor("P", "S").unwrap());
        store.set_follow_parent_successor("k", "S").unwrap();
        store.mark_follow_waiting("k").unwrap();
        let w = store.get_follow_waiter_by_key("k").unwrap().unwrap();
        assert_eq!(w.phase, ryeos_app::runtime_db::follow_phase::WAITING);
        assert_eq!(w.parent_successor_thread_id.as_deref(), Some("S"));
    }

    #[test]
    fn state_store_can_create_root_thread() {
        let (_tmpdir, store) = setup_state_store();

        let thread = make_thread(
            "T-root-1",
            "T-root-1",
            "directive",
            "directive:test/directive",
            None,
        );
        let persisted = store
            .create_thread_for_test(&thread)
            .expect("create_thread should succeed");

        assert!(!persisted.is_empty(), "Should return persisted events");
        assert_eq!(persisted[0].event_type, "thread_created");
    }

    #[test]
    fn state_store_can_mark_thread_running() {
        let (_tmpdir, store) = setup_state_store();

        let thread = make_thread(
            "T-running-1",
            "T-running-1",
            "directive",
            "directive:test/item",
            None,
        );
        store
            .create_thread_for_test(&thread)
            .expect("create_thread should succeed");

        let persisted = store
            .mark_thread_running("T-running-1", None)
            .expect("mark_thread_running should succeed");
        assert!(!persisted.is_empty());
    }

    #[test]
    fn state_store_can_attach_process() {
        let (_tmpdir, store) = setup_state_store();

        let thread = make_thread(
            "T-proc-1",
            "T-proc-1",
            "directive",
            "directive:test/item",
            None,
        );
        store
            .create_thread_for_test(&thread)
            .expect("create_thread should succeed");

        store
            .attach_thread_process(
                "T-proc-1",
                12345,
                67890,
                &ryeos_app::process::ExecutionProcessIdentity {
                    schema_version: ryeos_app::process::PROCESS_IDENTITY_SCHEMA_VERSION,
                    boot_id: "test-boot".to_string(),
                    target_pid: 12345,
                    target_start_time_ticks: 10,
                    group_leader_pid: 67890,
                    group_leader_start_time_ticks: 20,
                },
                &ryeos_app::launch_metadata::RuntimeLaunchMetadata::default(),
                None,
            )
            .expect("attach_thread_process should succeed");
    }

    #[test]
    fn state_store_can_list_threads() {
        let (_tmpdir, store) = setup_state_store();

        let root = make_thread(
            "T-list-1",
            "T-list-1",
            "directive",
            "directive:test/item",
            None,
        );
        store
            .create_thread_for_test(&root)
            .expect("create_thread 1 should succeed");

        let child = make_thread(
            "T-list-2",
            "T-list-1",
            "tool",
            "tool:test/tool",
            Some("T-list-1"),
        );
        store
            .create_thread_for_test(&child)
            .expect("create_thread 2 should succeed");

        let threads = store
            .list_threads(100)
            .expect("list_threads should succeed");
        assert!(threads.len() >= 2, "Should have at least 2 threads");
    }

    #[test]
    fn state_store_can_finalize_thread() {
        let (_tmpdir, store) = setup_state_store();

        let thread = make_thread(
            "T-final-1",
            "T-final-1",
            "directive",
            "directive:test/item",
            None,
        );
        store
            .create_thread_for_test(&thread)
            .expect("create_thread should succeed");
        store
            .mark_thread_running("T-final-1", None)
            .expect("mark_thread_running should succeed");

        let finalize = FinalizeThreadRecord {
            status: "completed".to_string(),
            outcome_code: Some("success".to_string()),
            result_json: Some(serde_json::json!({"message": "done"})),
            error_json: None,
            artifacts: vec![],
            managed_envelope: None,
            result_project_snapshot_hash: None,
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

        // The terminal outcome_code and result must survive finalization and
        // be readable back, not just returned live.
        let result = store
            .get_thread_result("T-final-1")
            .expect("get_thread_result should succeed")
            .expect("thread result row should exist");
        assert_eq!(result.outcome_code.as_deref(), Some("success"));
        assert_eq!(result.result, Some(serde_json::json!({"message": "done"})));
    }

    #[test]
    fn state_store_reads_back_structured_error_as_object() {
        let (_tmpdir, store) = setup_state_store();

        let thread = make_thread(
            "T-err-1",
            "T-err-1",
            "directive",
            "directive:test/item",
            None,
        );
        store
            .create_thread_for_test(&thread)
            .expect("create_thread");
        store
            .mark_thread_running("T-err-1", None)
            .expect("mark_thread_running");

        let err = serde_json::json!({
            "code": "required_secret_missing",
            "env_var": "ZEN_API_KEY"
        });
        let finalize = FinalizeThreadRecord {
            status: "failed".to_string(),
            outcome_code: Some("required_secret_missing".to_string()),
            result_json: None,
            error_json: Some(err.clone()),
            artifacts: vec![],
            managed_envelope: None,
            result_project_snapshot_hash: None,
            final_cost: None,
        };
        store
            .finalize_thread("T-err-1", &finalize)
            .expect("finalize_thread");

        // The structured error must read back as the original object, not a
        // stringified blob.
        let result = store
            .get_thread_result("T-err-1")
            .expect("get_thread_result")
            .expect("thread result row should exist");
        assert_eq!(
            result.outcome_code.as_deref(),
            Some("required_secret_missing")
        );
        assert_eq!(result.error, Some(err));
    }

    // ── CAS-truth derived projection tests ──────────────────────────

    #[test]
    fn edge_derived_from_snapshot_upstream() {
        let (_tmpdir, store) = setup_state_store();

        // Create root thread
        let root = make_thread(
            "T-edge-root",
            "T-edge-root",
            "directive",
            "directive:test/item",
            None,
        );
        store
            .create_thread_for_test(&root)
            .expect("create root should succeed");

        // Create child thread with upstream_thread_id
        let child = make_thread(
            "T-edge-child",
            "T-edge-root",
            "tool",
            "tool:test/tool",
            Some("T-edge-root"),
        );
        store
            .create_thread_for_test(&child)
            .expect("create child should succeed");

        // Verify edge exists in projection (derived from snapshot's upstream_thread_id)
        let edges = store
            .list_chain_edges("T-edge-root")
            .expect("list_chain_edges should succeed");
        assert_eq!(edges.len(), 1, "should have one derived edge");
        assert_eq!(edges[0].source_thread_id, "T-edge-root");
        assert_eq!(edges[0].target_thread_id, "T-edge-child");
        assert_eq!(edges[0].edge_type, "spawned");
    }

    #[test]
    fn artifact_derived_from_event() {
        let (_tmpdir, store) = setup_state_store();

        let thread = make_thread(
            "T-art-1",
            "T-art-1",
            "directive",
            "directive:test/item",
            None,
        );
        store
            .create_thread_for_test(&thread)
            .expect("create_thread should succeed");
        store
            .mark_thread_running("T-art-1", None)
            .expect("mark_thread_running should succeed");

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
        let artifacts = store
            .list_thread_artifacts("T-art-1")
            .expect("list_thread_artifacts should succeed");
        assert_eq!(artifacts.len(), 1, "should have one derived artifact");
        assert_eq!(artifacts[0].artifact_type, "output");
    }

    #[test]
    fn artifact_derived_on_finalize() {
        let (_tmpdir, store) = setup_state_store();

        let thread = make_thread(
            "T-art-fin",
            "T-art-fin",
            "directive",
            "directive:test/item",
            None,
        );
        store
            .create_thread_for_test(&thread)
            .expect("create_thread should succeed");
        store
            .mark_thread_running("T-art-fin", None)
            .expect("mark_thread_running should succeed");

        let finalize = FinalizeThreadRecord {
            status: "completed".to_string(),
            outcome_code: Some("success".to_string()),
            result_json: Some(serde_json::json!({"value": 42})),
            error_json: None,
            artifacts: vec![NewArtifactRecord {
                artifact_type: "result".to_string(),
                uri: "file:///tmp/result.json".to_string(),
                content_hash: Some("deadbeef".to_string()),
                metadata: None,
            }],
            managed_envelope: None,
            result_project_snapshot_hash: None,
            final_cost: None,
        };

        store
            .finalize_thread("T-art-fin", &finalize)
            .expect("finalize_thread should succeed");

        // Verify artifact was derived from artifact_published event during finalize
        let artifacts = store
            .list_thread_artifacts("T-art-fin")
            .expect("list_thread_artifacts should succeed");
        // Finalize publishes artifact_published events, which should derive artifact rows
        assert_eq!(
            artifacts.len(),
            1,
            "should have one derived artifact from finalize"
        );
        assert_eq!(artifacts[0].artifact_type, "result");
    }

    #[test]
    fn continuation_sets_upstream_and_derives_edge() {
        let (_tmpdir, store) = setup_state_store();

        let thread = make_thread(
            "T-cont-1",
            "T-cont-1",
            "directive",
            "directive:test/item",
            None,
        );
        store
            .create_thread_for_test(&thread)
            .expect("create_thread should succeed");
        store
            .mark_thread_running("T-cont-1", None)
            .expect("mark_thread_running should succeed");

        // Create a successor via continuation
        let successor = make_thread(
            "T-cont-2",
            "T-cont-1",
            "directive",
            "directive:test/item2",
            None,
        );
        let events = store
            .create_continuation_for_test(&successor, "T-cont-1", "T-cont-1", Some("retry"))
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
        let edges = store
            .list_chain_edges("T-cont-1")
            .expect("list_chain_edges should succeed");
        assert_eq!(edges.len(), 1, "should have one edge from continuation");
        assert_eq!(edges[0].source_thread_id, "T-cont-1");
        assert_eq!(edges[0].target_thread_id, "T-cont-2");

        // Read contract: the settled source advertises its successor so a graph
        // reconciler / client can follow the continuation without scraping
        // event payloads.
        assert_eq!(
            source_detail.successor_thread_id.as_deref(),
            Some("T-cont-2"),
            "continued source must expose successor_thread_id"
        );
    }

    #[test]
    fn continuation_onto_completed_source_preserves_its_result() {
        // The operator follow-up path braids a successor onto an already-settled
        // turn. That must NOT rewrite the predecessor's terminal snapshot (which
        // would erase its result) — it stays `completed`, keeps its result, and
        // still advertises the successor.
        let (_tmpdir, store) = setup_state_store();

        let thread = make_thread(
            "T-done-1",
            "T-done-1",
            "directive",
            "directive:test/item",
            None,
        );
        store
            .create_thread_for_test(&thread)
            .expect("create_thread");
        store
            .mark_thread_running("T-done-1", None)
            .expect("mark_thread_running");
        store
            .finalize_thread(
                "T-done-1",
                &FinalizeThreadRecord {
                    status: "completed".to_string(),
                    outcome_code: Some("success".to_string()),
                    result_json: Some(serde_json::json!({"answer": 4})),
                    error_json: None,
                    artifacts: vec![],
                    managed_envelope: None,
                    result_project_snapshot_hash: None,
                    final_cost: None,
                },
            )
            .expect("finalize_thread");

        // Braid a successor onto the completed turn.
        let successor = make_thread(
            "T-done-2",
            "T-done-1",
            "directive",
            "directive:test/item",
            None,
        );
        store
            .create_continuation_for_test(&successor, "T-done-1", "T-done-1", Some("follow-up"))
            .expect("create_continuation onto completed source");

        // Predecessor keeps its terminal status and result.
        let source = store
            .get_thread("T-done-1")
            .expect("get_thread")
            .expect("source exists");
        assert_eq!(
            source.status, "completed",
            "an already-terminal source must not be rewritten to continued"
        );
        assert_eq!(
            source.successor_thread_id.as_deref(),
            Some("T-done-2"),
            "settled source must still expose its successor"
        );
        let result = store
            .get_thread_result("T-done-1")
            .expect("get_thread_result")
            .expect("result row preserved");
        assert_eq!(result.result, Some(serde_json::json!({"answer": 4})));
        assert_eq!(result.outcome_code.as_deref(), Some("success"));

        // Successor inherits the chain and links upstream.
        let succ = store
            .get_thread("T-done-2")
            .expect("get_thread")
            .expect("successor exists");
        assert_eq!(succ.chain_root_id, "T-done-1");
        assert_eq!(succ.upstream_thread_id.as_deref(), Some("T-done-1"));
    }

    #[test]
    fn continuation_is_single_successor_guarded() {
        // A predecessor is continued at most once. A completed source stays
        // continuable, so a second continuation is caught by the
        // single-successor guard (not the terminal-status check) — proving a
        // double-submit/race cannot mint sibling successors.
        let (_tmpdir, store) = setup_state_store();

        let thread = make_thread(
            "T-once-1",
            "T-once-1",
            "directive",
            "directive:test/item",
            None,
        );
        store
            .create_thread_for_test(&thread)
            .expect("create_thread");
        store
            .mark_thread_running("T-once-1", None)
            .expect("mark_thread_running");
        store
            .finalize_thread(
                "T-once-1",
                &FinalizeThreadRecord {
                    status: "completed".to_string(),
                    outcome_code: Some("success".to_string()),
                    result_json: Some(serde_json::json!({"a": 1})),
                    error_json: None,
                    artifacts: vec![],
                    managed_envelope: None,
                    result_project_snapshot_hash: None,
                    final_cost: None,
                },
            )
            .expect("finalize_thread");

        let first = make_thread(
            "T-once-2",
            "T-once-1",
            "directive",
            "directive:test/item",
            None,
        );
        store
            .create_continuation_for_test(&first, "T-once-1", "T-once-1", Some("first"))
            .expect("first continuation");

        let dup = make_thread(
            "T-once-3",
            "T-once-1",
            "directive",
            "directive:test/item",
            None,
        );
        let err = store
            .create_continuation_for_test(&dup, "T-once-1", "T-once-1", Some("second"))
            .expect_err("second continuation of the same source must be refused");
        assert!(
            err.to_string().contains("already continued"),
            "expected single-successor guard, got: {err}"
        );

        // The first successor remains the one exposed; no sibling was created.
        let source = store
            .get_thread("T-once-1")
            .expect("get_thread")
            .expect("source exists");
        assert_eq!(source.successor_thread_id.as_deref(), Some("T-once-2"));
        assert!(
            store.get_thread("T-once-3").expect("get_thread").is_none(),
            "the rejected duplicate successor must not have been persisted"
        );
    }

    #[test]
    fn machine_continuation_rejects_non_running_source() {
        // The machine handoff is a CUT-OFF of a still-running source. A terminal
        // source is the operator follow-up path; `create_machine_continuation`
        // must refuse it under the lock and mint no successor.
        let (_tmpdir, store) = setup_state_store();

        let thread = make_thread(
            "T-mc-done-1",
            "T-mc-done-1",
            "directive",
            "directive:test/item",
            None,
        );
        store
            .create_thread_for_test(&thread)
            .expect("create_thread");
        store
            .mark_thread_running("T-mc-done-1", None)
            .expect("mark_thread_running");
        store
            .finalize_thread(
                "T-mc-done-1",
                &FinalizeThreadRecord {
                    status: "completed".to_string(),
                    outcome_code: Some("success".to_string()),
                    result_json: Some(serde_json::json!({"a": 1})),
                    error_json: None,
                    artifacts: vec![],
                    managed_envelope: None,
                    result_project_snapshot_hash: None,
                    final_cost: None,
                },
            )
            .expect("finalize_thread");

        let successor = make_thread(
            "T-mc-done-2",
            "T-mc-done-1",
            "directive",
            "directive:test/item",
            None,
        );
        let err = store
            .create_machine_continuation(&successor, "T-mc-done-1", "T-mc-done-1", Some("limit"))
            .expect_err("machine continuation must refuse a terminal source");
        assert!(
            err.to_string().contains("requires a running source"),
            "expected running-source guard, got: {err}"
        );

        // No successor was minted; the source keeps its terminal result.
        assert!(
            store
                .get_thread("T-mc-done-2")
                .expect("get_thread")
                .is_none(),
            "a refused machine continuation must not persist a successor"
        );
        let source = store
            .get_thread("T-mc-done-1")
            .expect("get_thread")
            .expect("source exists");
        assert_eq!(source.status, "completed");
    }

    #[test]
    fn machine_continuation_requires_captured_resume_context() {
        // A successor we cannot launch is worse than none. With no spawn-time
        // `ResumeContext` seeded on the source, the handoff must fail and leave
        // the source RUNNING (not `continued`) so the runner fails terminal.
        let (_tmpdir, store) = setup_state_store();

        let thread = make_thread(
            "T-mc-run-1",
            "T-mc-run-1",
            "directive",
            "directive:test/item",
            None,
        );
        store
            .create_thread_for_test(&thread)
            .expect("create_thread");
        store
            .mark_thread_running("T-mc-run-1", None)
            .expect("mark_thread_running");
        // Deliberately do NOT seed launch metadata.

        let successor = make_thread(
            "T-mc-run-2",
            "T-mc-run-1",
            "directive",
            "directive:test/item",
            None,
        );
        let err = store
            .create_machine_continuation(&successor, "T-mc-run-1", "T-mc-run-1", Some("limit"))
            .expect_err("machine continuation must require a captured ResumeContext");
        assert!(
            err.to_string().contains("no captured ResumeContext"),
            "expected ResumeContext guard, got: {err}"
        );

        // The source must remain running — never stranded as `continued` behind
        // an unlaunchable successor.
        let source = store
            .get_thread("T-mc-run-1")
            .expect("get_thread")
            .expect("source exists");
        assert_eq!(
            source.status, "running",
            "a failed handoff must leave the source running, not continued"
        );
        assert!(
            store
                .get_thread("T-mc-run-2")
                .expect("get_thread")
                .is_none(),
            "no successor should exist after a refused handoff"
        );
    }

    #[test]
    fn machine_continuation_birth_preserves_prepublication_launch_claim() {
        let (_tmpdir, store) = setup_state_store();
        let source_id = "T-mc-claimed-source";
        let successor_id = "T-mc-claimed-successor";
        store
            .create_thread_for_test(&make_thread(
                source_id,
                source_id,
                "directive",
                "directive:test/item",
                None,
            ))
            .expect("create source");
        seed_projectless_continuable(&store, source_id, "directive");

        assert_eq!(
            store
                .reserve_fresh_thread_launch(successor_id, "claim-before-birth", "daemon:test")
                .expect("reserve successor launch before publication"),
            ryeos_app::runtime_db::LaunchClaimOutcome::Claimed
        );
        assert!(store
            .get_thread(successor_id)
            .expect("inspect unpublished successor")
            .is_none());

        store
            .create_machine_continuation(
                &make_thread(
                    successor_id,
                    source_id,
                    "directive",
                    "directive:test/item",
                    Some(source_id),
                ),
                source_id,
                source_id,
                Some("turn_limit"),
            )
            .expect("publish claimed machine successor");

        let successor = store
            .get_thread(successor_id)
            .expect("read successor")
            .expect("successor was published");
        assert_eq!(successor.status, "created");
        let claim = store
            .get_launch_claim(successor_id)
            .expect("read successor claim")
            .expect("prepublication claim survives successor birth");
        assert_eq!(claim.claim_id, "claim-before-birth");
    }

    #[test]
    fn prepared_machine_successor_cannot_drop_source_execution_policy() {
        use ryeos_app::launch_metadata::RuntimeLaunchMetadata;

        let (_tmpdir, store) = setup_state_store();
        store
            .create_thread_for_test(&make_project_thread(
                "T-policy-source",
                "T-policy-source",
                "directive",
                "directive:test/item",
                None,
            ))
            .unwrap();
        seed_continuable(&store, "T-policy-source", "directive");
        let source_metadata = store
            .get_launch_metadata("T-policy-source")
            .unwrap()
            .unwrap();
        let resume_context = source_metadata.resume_context.clone().unwrap();
        let incomplete_prepared =
            RuntimeLaunchMetadata::default().with_resume_context(resume_context.clone());

        let error = store
            .create_machine_continuation_with_events(
                &make_project_thread(
                    "T-policy-successor",
                    "T-policy-source",
                    "directive",
                    "directive:test/item",
                    Some("T-policy-source"),
                ),
                "T-policy-source",
                "T-policy-source",
                Some("turn_limit"),
                &resume_context,
                &incomplete_prepared,
                Vec::new(),
            )
            .expect_err("prepared successor must carry the source execution policy");

        assert!(error
            .to_string()
            .contains("execution policy differs from its source"));
        assert_eq!(
            store.get_thread("T-policy-source").unwrap().unwrap().status,
            "running"
        );
        assert!(store.get_thread("T-policy-successor").unwrap().is_none());
        assert!(store
            .get_launch_metadata("T-policy-successor")
            .unwrap()
            .is_none());
    }

    #[test]
    fn create_or_get_continuation_dedups_by_fingerprint() {
        use ryeos_app::state_store::ContinuationOutcome;
        let (_tmpdir, store) = setup_state_store();
        let thread = make_thread("T-fp-1", "T-fp-1", "directive", "directive:test/item", None);
        store
            .create_thread_for_test(&thread)
            .expect("create_thread");
        store
            .mark_thread_running("T-fp-1", None)
            .expect("mark_thread_running");
        store
            .finalize_thread(
                "T-fp-1",
                &FinalizeThreadRecord {
                    status: "completed".to_string(),
                    outcome_code: Some("success".to_string()),
                    result_json: Some(serde_json::json!({"a": 1})),
                    error_json: None,
                    artifacts: vec![],
                    managed_envelope: None,
                    result_project_snapshot_hash: None,
                    final_cost: None,
                },
            )
            .expect("finalize_thread");

        // First follow-up: creates the successor + persists fingerprint fp-A.
        let succ = make_thread("T-fp-2", "T-fp-1", "directive", "directive:test/item", None);
        let outcome = store
            .create_or_get_continuation_for_test(
                &succ,
                "T-fp-1",
                "T-fp-1",
                Some("follow-up"),
                "sha256:fp-A",
                None,
            )
            .expect("first create_or_get");
        assert!(
            matches!(outcome, ContinuationOutcome::Created(_)),
            "first submit creates a successor"
        );
        assert_eq!(
            store
                .get_thread("T-fp-1")
                .unwrap()
                .unwrap()
                .successor_thread_id
                .as_deref(),
            Some("T-fp-2")
        );
        // The fingerprint is persisted on the edge (not re-derived), so dedup
        // works even before/without any runtime-emitted input.
        assert_eq!(
            store
                .get_continuation_fingerprint("T-fp-1")
                .unwrap()
                .as_deref(),
            Some("sha256:fp-A")
        );

        // Duplicate submit (same fingerprint, even a different candidate id) must
        // return the EXISTING successor and mint no sibling.
        let dup = make_thread("T-fp-3", "T-fp-1", "directive", "directive:test/item", None);
        let outcome = store
            .create_or_get_continuation_for_test(
                &dup,
                "T-fp-1",
                "T-fp-1",
                Some("follow-up"),
                "sha256:fp-A",
                None,
            )
            .expect("duplicate create_or_get");
        match outcome {
            ContinuationOutcome::Existing {
                successor_thread_id,
            } => {
                assert_eq!(successor_thread_id, "T-fp-2")
            }
            other => panic!("expected Existing, got {other:?}"),
        }
        assert!(
            store.get_thread("T-fp-3").unwrap().is_none(),
            "duplicate submit must not mint a sibling successor"
        );

        // A DIFFERENT fingerprint onto the already-continued source is a conflict.
        let conflicting = make_thread("T-fp-4", "T-fp-1", "directive", "directive:test/item", None);
        let outcome = store
            .create_or_get_continuation_for_test(
                &conflicting,
                "T-fp-1",
                "T-fp-1",
                Some("follow-up"),
                "sha256:fp-B",
                None,
            )
            .expect("conflicting create_or_get");
        match outcome {
            ContinuationOutcome::Conflict {
                successor_thread_id,
            } => {
                assert_eq!(successor_thread_id, "T-fp-2")
            }
            other => panic!("expected Conflict, got {other:?}"),
        }
        assert!(
            store.get_thread("T-fp-4").unwrap().is_none(),
            "conflicting submit must not mint a sibling successor"
        );
    }

    #[test]
    fn machine_continuation_chain_depth_cap() {
        use ryeos_app::launch_metadata::{ResumeContext, RuntimeLaunchMetadata};
        use ryeos_engine::contracts::{
            EffectivePrincipal, ExecutionHints, Principal, ProjectContext,
        };
        let (_tmpdir, store) = setup_state_store();
        let max = ryeos_app::thread_lifecycle::MAX_CONTINUATION_CHAIN_DEPTH;

        let resume_ctx = || ResumeContext {
            kind: "directive".into(),
            item_ref: "directive:test/item".into(),
            ref_bindings: std::collections::BTreeMap::new(),
            launch_mode: "wait".into(),
            parameters: serde_json::json!({}),
            project_context: ProjectContext::LocalPath {
                path: std::env::temp_dir(),
            },
            project_authority: ryeos_state::objects::ExecutionProjectAuthority::live(
                std::env::temp_dir(),
                format!("local:{}", std::env::temp_dir().display()),
                ryeos_state::objects::LiveProjectAccess::ReadWrite,
                ryeos_state::objects::LiveFilesystemConfinement::standard_descriptor_rooted(),
                ryeos_state::objects::EnvironmentAuthority::None,
                Vec::new(),
            )
            .unwrap(),
            lifecycle_authority:
                ryeos_state::objects::ExecutionLifecycleAuthority::DAEMON_RESTARTABLE,
            stable_project_identity: Some(
                ryeos_app::launch_metadata::StableProjectIdentity::from_path(
                    &std::env::temp_dir(),
                    "site:test",
                )
                .unwrap(),
            ),
            local_overlay_root: Some(std::env::temp_dir()),
            original_snapshot_hash: None,
            original_pushed_head_ref: None,
            state_root: None,
            current_site_id: "site:test".into(),
            origin_site_id: "site:test".into(),
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: "fp".into(),
                scopes: vec![],
            }),
            execution_hints: ExecutionHints::default(),
            effective_caps: vec![],
            parent_delegation_caps: None,
            executor_ref: None,
            runtime_ref: None,
        };
        // A machine continuation requires the source be RUNNING with a captured
        // ResumeContext, so make each successor continuable before extending.
        let make_continuable = |id: &str| {
            store
                .mark_thread_running(id, None)
                .expect("mark_thread_running");
            store
                .seed_launch_metadata(
                    id,
                    &RuntimeLaunchMetadata::default()
                        .with_native_resume(ryeos_engine::contracts::NativeResumeSpec::default())
                        .with_resume_context(resume_ctx()),
                )
                .expect("seed launch metadata");
        };

        let root = make_project_thread("D0", "D0", "directive", "directive:test/item", None);
        store.create_thread_for_test(&root).expect("create root");
        make_continuable("D0");

        // `max` consecutive machine continuations — all allowed (links #1..#max).
        let mut source = "D0".to_string();
        for i in 1..=max {
            let id = format!("D{i}");
            let succ =
                make_project_thread(&id, "D0", "directive", "directive:test/item", Some(&source));
            store
                .create_machine_continuation(&succ, &source, "D0", Some("turn_limit"))
                .unwrap_or_else(|e| panic!("machine link #{i} must be allowed: {e}"));
            make_continuable(&id);
            source = id;
        }

        // The next machine continuation (link #max+1) is refused — the chain is at
        // the cap. No successor is persisted; the source stays running so the
        // runtime fails terminal.
        let over = make_project_thread(
            "D-over",
            "D0",
            "directive",
            "directive:test/item",
            Some(&source),
        );
        let err = store
            .create_machine_continuation(&over, &source, "D0", Some("turn_limit"))
            .expect_err("continuation past the cap must be refused");
        assert!(
            err.to_string().contains("continuation depth limit reached"),
            "got: {err}"
        );
        assert!(
            store.get_thread("D-over").unwrap().is_none(),
            "the refused successor must not be persisted"
        );
        assert_eq!(
            store.get_thread(&source).unwrap().unwrap().status,
            "running",
            "the source stays running for terminal-fail, not continued"
        );

        // A follow-resume successor IS allowed at the machine-depth cap: it is
        // structural progress, not an autonomous segment-cut, so the cap does not
        // apply. It is created (not launched) and settles the source `continued`.
        let follow_succ = make_project_thread(
            "D-follow",
            "D0",
            "directive",
            "directive:test/item",
            Some(&source),
        );
        store
            .create_follow_resume_successor(&follow_succ, &source, "D0")
            .expect("follow-resume must be allowed at the machine cap");
        let fs = store
            .get_thread("D-follow")
            .unwrap()
            .expect("follow successor persisted");
        assert_eq!(
            fs.status, "created",
            "follow successor is created, not running"
        );
        assert_eq!(fs.upstream_thread_id.as_deref(), Some(source.as_str()));
        assert_eq!(
            store.get_thread(&source).unwrap().unwrap().status,
            "continued",
            "the follow-resume settles the source continued"
        );
    }

    #[test]
    fn follow_resume_and_marker_scrubbing() {
        use ryeos_app::launch_metadata::{ResumeContext, RuntimeLaunchMetadata};
        use ryeos_engine::contracts::{
            EffectivePrincipal, ExecutionHints, Principal, ProjectContext,
        };
        let (_tmpdir, store) = setup_state_store();

        let resume_ctx = || ResumeContext {
            kind: "directive".into(),
            item_ref: "directive:test/item".into(),
            ref_bindings: std::collections::BTreeMap::new(),
            launch_mode: "wait".into(),
            parameters: serde_json::json!({}),
            project_context: ProjectContext::LocalPath {
                path: std::env::temp_dir(),
            },
            project_authority: ryeos_state::objects::ExecutionProjectAuthority::live(
                std::env::temp_dir(),
                format!("local:{}", std::env::temp_dir().display()),
                ryeos_state::objects::LiveProjectAccess::ReadWrite,
                ryeos_state::objects::LiveFilesystemConfinement::standard_descriptor_rooted(),
                ryeos_state::objects::EnvironmentAuthority::None,
                Vec::new(),
            )
            .unwrap(),
            lifecycle_authority:
                ryeos_state::objects::ExecutionLifecycleAuthority::DAEMON_RESTARTABLE,
            stable_project_identity: Some(
                ryeos_app::launch_metadata::StableProjectIdentity::from_path(
                    &std::env::temp_dir(),
                    "site:test",
                )
                .unwrap(),
            ),
            local_overlay_root: Some(std::env::temp_dir()),
            original_snapshot_hash: None,
            original_pushed_head_ref: None,
            state_root: None,
            current_site_id: "site:test".into(),
            origin_site_id: "site:test".into(),
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: "fp".into(),
                scopes: vec![],
            }),
            execution_hints: ExecutionHints::default(),
            effective_caps: vec![],
            parent_delegation_caps: None,
            executor_ref: None,
            runtime_ref: None,
        };
        let make_continuable = |id: &str| {
            store
                .mark_thread_running(id, None)
                .expect("mark_thread_running");
            store
                .seed_launch_metadata(
                    id,
                    &RuntimeLaunchMetadata::default()
                        .with_native_resume(ryeos_engine::contracts::NativeResumeSpec::default())
                        .with_resume_context(resume_ctx()),
                )
                .expect("seed launch metadata");
        };
        let edge = |id: &str| -> (Option<String>, Option<String>) {
            store
                .with_state_db(|db| {
                    Ok::<_, anyhow::Error>(
                        ryeos_state::queries::continuation_edge(db.projection(), id)?
                            .map(|(_, reason, fp)| (reason, fp))
                            .unwrap_or((None, None)),
                    )
                })
                .unwrap()
        };

        // Machine scrubs ALL reserved markers from a runtime-supplied reason, so a
        // runtime can forge neither an operator reset nor a depth-exempt follow.
        for spoof in ["operator_follow_up", "graph_follow_resume"] {
            let src = format!("M-{spoof}");
            store
                .create_thread_for_test(&make_project_thread(
                    &src,
                    &src,
                    "directive",
                    "directive:test/item",
                    None,
                ))
                .expect("create root");
            make_continuable(&src);
            let succ = format!("M-{spoof}-s");
            store
                .create_machine_continuation(
                    &make_project_thread(
                        &succ,
                        &src,
                        "directive",
                        "directive:test/item",
                        Some(&src),
                    ),
                    &src,
                    &src,
                    Some(spoof),
                )
                .expect("machine continuation");
            assert_eq!(
                edge(&src).0,
                None,
                "reserved marker '{spoof}' must be scrubbed from a runtime reason"
            );
        }

        // Follow-resume successor invariants.
        store
            .create_thread_for_test(&make_project_thread(
                "F-root",
                "F-root",
                "directive",
                "directive:test/item",
                None,
            ))
            .expect("create root");
        make_continuable("F-root");
        store
            .create_follow_resume_successor(
                &make_project_thread(
                    "F-succ",
                    "F-root",
                    "directive",
                    "directive:test/item",
                    Some("F-root"),
                ),
                "F-root",
                "F-root",
            )
            .expect("follow-resume");

        let fs = store
            .get_thread("F-succ")
            .unwrap()
            .expect("successor persisted");
        assert_eq!(fs.status, "created", "successor is created, not running");
        assert_eq!(fs.upstream_thread_id.as_deref(), Some("F-root"));
        assert_eq!(
            store.get_thread("F-root").unwrap().unwrap().status,
            "continued",
            "source settled continued"
        );
        let (reason, fp) = edge("F-root");
        assert_eq!(reason.as_deref(), Some("graph_follow_resume"));
        assert!(
            fp.is_none(),
            "follow-resume edge has no request fingerprint"
        );
        assert!(store
            .get_continuation_fingerprint("F-root")
            .unwrap()
            .is_none());
        assert!(
            store
                .get_launch_metadata("F-succ")
                .unwrap()
                .and_then(|m| m.resume_context)
                .is_some(),
            "follow successor must have the source resume context seeded"
        );
        // The reconcile guard's discriminator: a follow-resume edge is detected
        // (so reconcile leaves the successor pending) ONLY for the actual edge
        // target — another created row naming the same upstream does not match —
        // and a machine edge is never matched.
        assert!(
            store
                .is_follow_resume_successor("F-root", "F-succ")
                .unwrap(),
            "the follow edge F-root -> F-succ must read as a follow-resume successor"
        );
        assert!(
            !store
                .is_follow_resume_successor("F-root", "F-other")
                .unwrap(),
            "a different successor naming the same upstream must NOT match"
        );
        assert!(
            !store
                .is_follow_resume_successor("M-operator_follow_up", "M-operator_follow_up-s")
                .unwrap(),
            "a machine edge must NOT read as a follow-resume successor"
        );
        // A second continuation off the now-continued source is rejected.
        assert!(
            store
                .create_follow_resume_successor(
                    &make_project_thread(
                        "F-dup",
                        "F-root",
                        "directive",
                        "directive:test/item",
                        Some("F-root")
                    ),
                    "F-root",
                    "F-root",
                )
                .is_err(),
            "a second successor off a settled source must be rejected"
        );

        // Source must be running.
        store
            .create_thread_for_test(&make_project_thread(
                "R-cr",
                "R-cr",
                "directive",
                "directive:test/item",
                None,
            ))
            .expect("create");
        assert!(
            store
                .create_machine_continuation(
                    &make_project_thread(
                        "R-cr-s",
                        "R-cr",
                        "directive",
                        "directive:test/item",
                        Some("R-cr")
                    ),
                    "R-cr",
                    "R-cr",
                    Some("turn_limit"),
                )
                .is_err(),
            "machine continuation requires a running source"
        );

        // Missing resume context fails BEFORE the source settles.
        store
            .create_thread_for_test(&make_thread(
                "R-nr",
                "R-nr",
                "directive",
                "directive:test/item",
                None,
            ))
            .expect("create");
        store
            .mark_thread_running("R-nr", None)
            .expect("mark running");
        assert!(
            store
                .create_machine_continuation(
                    &make_project_thread(
                        "R-nr-s",
                        "R-nr",
                        "directive",
                        "directive:test/item",
                        Some("R-nr")
                    ),
                    "R-nr",
                    "R-nr",
                    Some("turn_limit"),
                )
                .is_err(),
            "missing source ResumeContext must fail"
        );
        assert_eq!(
            store.get_thread("R-nr").unwrap().unwrap().status,
            "running",
            "source stays running when continuation fails"
        );

        // Successor preconditions are checked BEFORE any runtime-db write, so a
        // rejection leaves no orphan row and the source untouched.
        store
            .create_thread_for_test(&make_project_thread(
                "G-root",
                "G-root",
                "directive",
                "directive:test/item",
                None,
            ))
            .expect("create");
        make_continuable("G-root");
        // A successor in a FOREIGN chain is rejected.
        assert!(
            store
                .create_follow_resume_successor(
                    &make_project_thread(
                        "G-bad-chain",
                        "OTHER",
                        "directive",
                        "directive:test/item",
                        Some("G-root")
                    ),
                    "G-root",
                    "G-root",
                )
                .is_err(),
            "successor with a foreign chain root must be rejected"
        );
        assert!(store.get_thread("G-bad-chain").unwrap().is_none());
        assert!(
            store.get_launch_metadata("G-bad-chain").unwrap().is_none(),
            "rejected successor must leave no orphan runtime row"
        );
        // A successor declaring a DIFFERENT upstream is rejected.
        assert!(
            store
                .create_follow_resume_successor(
                    &make_project_thread(
                        "G-bad-up",
                        "G-root",
                        "directive",
                        "directive:test/item",
                        Some("ELSEWHERE")
                    ),
                    "G-root",
                    "G-root",
                )
                .is_err(),
            "successor declaring a foreign upstream must be rejected"
        );
        assert!(store.get_thread("G-bad-up").unwrap().is_none());
        assert_eq!(
            store.get_thread("G-root").unwrap().unwrap().status,
            "running",
            "a rejected continuation leaves the source running"
        );
    }

    #[test]
    fn rebuild_recovers_edges_from_cas() {
        let (_tmpdir, store) = setup_state_store();

        let root = make_thread(
            "T-reb-root",
            "T-reb-root",
            "directive",
            "directive:test/item",
            None,
        );
        store
            .create_thread_for_test(&root)
            .expect("create root should succeed");

        let child = make_thread(
            "T-reb-child",
            "T-reb-root",
            "tool",
            "tool:test/tool",
            Some("T-reb-root"),
        );
        store
            .create_thread_for_test(&child)
            .expect("create child should succeed");

        // Verify edges exist before rebuild
        let edges_before = store
            .list_chain_edges("T-reb-root")
            .expect("list_chain_edges should succeed");
        assert_eq!(edges_before.len(), 1);

        // Now delete the projection and rebuild from CAS
        store
            .with_state_db(|db| {
                let cas_root = db.cas_root().to_path_buf();
                let refs_root = db.refs_root().to_path_buf();

                // Clear edges from projection
                db.projection()
                    .connection()
                    .execute_batch("DELETE FROM thread_edges; DELETE FROM projection_meta;")
                    .expect("clear projection should succeed");

                // Rebuild from CAS
                let report = ryeos_state::rebuild::rebuild_projection(
                    db.projection(),
                    &cas_root,
                    &refs_root,
                    db.head_trust_store(),
                )
                .expect("rebuild should succeed");

                assert_eq!(report.chains_rebuilt, 1);
                assert_eq!(report.threads_restored, 2);

                Ok::<_, anyhow::Error>(())
            })
            .expect("with_state_db should succeed");

        // Verify edges were recovered from snapshot upstream_thread_id during rebuild
        let edges_after = store
            .list_chain_edges("T-reb-root")
            .expect("list_chain_edges should succeed");
        assert_eq!(
            edges_after.len(),
            1,
            "edge should be recovered after rebuild"
        );
        assert_eq!(edges_after[0].source_thread_id, "T-reb-root");
        assert_eq!(edges_after[0].target_thread_id, "T-reb-child");
    }

    #[test]
    fn rebuild_recovers_artifacts_from_cas() {
        let (_tmpdir, store) = setup_state_store();

        let thread = make_thread(
            "T-reb-art",
            "T-reb-art",
            "directive",
            "directive:test/item",
            None,
        );
        store
            .create_thread_for_test(&thread)
            .expect("create_thread should succeed");
        store
            .mark_thread_running("T-reb-art", None)
            .expect("mark_thread_running should succeed");

        let finalize = FinalizeThreadRecord {
            status: "completed".to_string(),
            outcome_code: Some("success".to_string()),
            result_json: None,
            error_json: None,
            artifacts: vec![NewArtifactRecord {
                artifact_type: "output_file".to_string(),
                uri: "file:///tmp/out.txt".to_string(),
                content_hash: Some("hash123".to_string()),
                metadata: Some(serde_json::json!({"lines": 10})),
            }],
            managed_envelope: None,
            result_project_snapshot_hash: None,
            final_cost: None,
        };

        store
            .finalize_thread("T-reb-art", &finalize)
            .expect("finalize should succeed");

        // Clear artifacts from projection
        store
            .with_state_db(|db| {
                db.projection()
                    .connection()
                    .execute_batch("DELETE FROM thread_artifacts; DELETE FROM projection_meta;")
                    .expect("clear artifacts should succeed");
                Ok::<_, anyhow::Error>(())
            })
            .expect("with_state_db should succeed");

        // Rebuild from CAS
        store
            .with_state_db(|db| {
                let cas_root = db.cas_root().to_path_buf();
                let refs_root = db.refs_root().to_path_buf();

                let report = ryeos_state::rebuild::rebuild_projection(
                    db.projection(),
                    &cas_root,
                    &refs_root,
                    db.head_trust_store(),
                )
                .expect("rebuild should succeed");

                assert_eq!(report.chains_rebuilt, 1);

                Ok::<_, anyhow::Error>(())
            })
            .expect("with_state_db should succeed");

        // Verify artifact was recovered from artifact_published event during rebuild
        let artifacts = store
            .list_thread_artifacts("T-reb-art")
            .expect("list_thread_artifacts should succeed");
        assert_eq!(
            artifacts.len(),
            1,
            "artifact should be recovered after rebuild"
        );
        assert_eq!(artifacts[0].artifact_type, "output_file");
    }

    #[test]
    fn named_chain_repair_recovers_new_state() {
        let (_tmpdir, store) = setup_state_store();

        let thread = make_thread(
            "T-catchup",
            "T-catchup",
            "directive",
            "directive:test/item",
            None,
        );
        store
            .create_thread_for_test(&thread)
            .expect("create_thread should succeed");

        // Simulate projection drift: update projection_meta to a fake old hash
        store
            .with_state_db(|db| {
                let meta = ryeos_state::projection::ProjectionMeta {
                    chain_root_id: "T-catchup".to_string(),
                    indexed_chain_state_hash: "deadbeef".repeat(4),
                    updated_at: "2020-01-01T00:00:00Z".to_string(),
                };
                db.projection()
                    .update_projection_meta(&meta)
                    .expect("update meta should succeed");

                // Clear threads table to simulate drift
                db.projection()
                    .connection()
                    .execute_batch("DELETE FROM threads")
                    .expect("clear threads should succeed");

                Ok::<_, anyhow::Error>(())
            })
            .expect("with_state_db should succeed");

        // Repair only the named chain; current-generation recovery never scans
        // the global head directory.
        store
            .with_state_db(|db| {
                let report = db
                    .repair_one_chain("T-catchup")
                    .expect("named-chain repair should succeed");

                assert_eq!(report.chains_updated, 1, "chain should be repaired");

                Ok::<_, anyhow::Error>(())
            })
            .expect("with_state_db should succeed");

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
        let root = make_thread("T-e2e", "T-e2e", "directive", "directive:test/e2e", None);
        store.create_thread_for_test(&root).expect("create root");
        store
            .mark_thread_running("T-e2e", None)
            .expect("mark running");

        // 2. Spawn child thread
        let child = make_thread(
            "T-e2e-child",
            "T-e2e",
            "tool",
            "tool:test/child",
            Some("T-e2e"),
        );
        store.create_thread_for_test(&child).expect("create child");

        // 3. Finalize root with artifacts
        let finalize = FinalizeThreadRecord {
            status: "completed".to_string(),
            outcome_code: Some("done".to_string()),
            result_json: Some(serde_json::json!({"answer": 42})),
            error_json: None,
            artifacts: vec![NewArtifactRecord {
                artifact_type: "summary".to_string(),
                uri: "file:///tmp/summary.json".to_string(),
                content_hash: None,
                metadata: None,
            }],
            managed_envelope: None,
            result_project_snapshot_hash: None,
            final_cost: Some(ryeos_engine::contracts::FinalCost {
                turns: 3,
                input_tokens: 1500,
                output_tokens: 500,
                spend: 0.05,
                provider: None,
                basis: None,
                metadata: None,
            }),
        };
        store.finalize_thread("T-e2e", &finalize).expect("finalize");

        // 4. Verify everything in projection
        let threads = store
            .list_chain_threads("T-e2e")
            .expect("list_chain_threads");
        assert_eq!(threads.len(), 2);

        let edges = store.list_chain_edges("T-e2e").expect("list_chain_edges");
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].source_thread_id, "T-e2e");
        assert_eq!(edges[0].target_thread_id, "T-e2e-child");

        let artifacts = store
            .list_thread_artifacts("T-e2e")
            .expect("list_artifacts");
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].artifact_type, "summary");

        let root_detail = store
            .get_thread("T-e2e")
            .expect("get_thread")
            .expect("thread exists");
        assert_eq!(root_detail.status, "completed");

        // 5. Delete projection and rebuild — everything should recover
        store
            .with_state_db(|db| {
                db.projection()
                    .connection()
                    .execute_batch(
                        "DELETE FROM thread_edges;
                 DELETE FROM thread_artifacts;
                 DELETE FROM threads;
                 DELETE FROM events;
                 DELETE FROM projection_meta;",
                    )
                    .expect("clear projection");

                let cas_root = db.cas_root().to_path_buf();
                let refs_root = db.refs_root().to_path_buf();
                let report = ryeos_state::rebuild::rebuild_projection(
                    db.projection(),
                    &cas_root,
                    &refs_root,
                    db.head_trust_store(),
                )
                .expect("rebuild");

                assert!(report.chains_rebuilt >= 1);

                Ok::<_, anyhow::Error>(())
            })
            .expect("with_state_db");

        // 6. Verify everything recovered
        let threads_after = store
            .list_chain_threads("T-e2e")
            .expect("list_chain_threads after");
        assert_eq!(threads_after.len(), 2, "all threads should be recovered");

        let edges_after = store.list_chain_edges("T-e2e").expect("list_edges after");
        assert_eq!(edges_after.len(), 1, "edge should be recovered");

        let artifacts_after = store
            .list_thread_artifacts("T-e2e")
            .expect("list_artifacts after");
        assert_eq!(artifacts_after.len(), 1, "artifact should be recovered");

        let root_after = store
            .get_thread("T-e2e")
            .expect("get_thread after")
            .expect("thread exists after");
        assert_eq!(root_after.status, "completed", "status should be recovered");
    }

    // ── Cancel / attach_process status guard tests ────────────────

    #[test]
    fn attach_process_rejects_terminal_thread() {
        let (_tmpdir, store) = setup_state_store();

        let thread = make_thread(
            "T-skip-attach",
            "T-skip-attach",
            "directive",
            "directive:test/item",
            None,
        );
        store
            .create_thread_for_test(&thread)
            .expect("create_thread");
        store
            .mark_thread_running("T-skip-attach", None)
            .expect("mark running");

        // Finalize as cancelled.
        let finalize = FinalizeThreadRecord {
            status: "cancelled".to_string(),
            outcome_code: Some("cancelled".to_string()),
            result_json: None,
            error_json: None,
            artifacts: vec![],
            managed_envelope: None,
            result_project_snapshot_hash: None,
            final_cost: None,
        };
        store
            .finalize_thread("T-skip-attach", &finalize)
            .expect("finalize");

        let error = store
            .attach_thread_process(
                "T-skip-attach",
                99999,
                99999,
                &ryeos_app::process::ExecutionProcessIdentity {
                    schema_version: ryeos_app::process::PROCESS_IDENTITY_SCHEMA_VERSION,
                    boot_id: "test-boot".to_string(),
                    target_pid: 99999,
                    target_start_time_ticks: 10,
                    group_leader_pid: 99999,
                    group_leader_start_time_ticks: 10,
                },
                &ryeos_app::launch_metadata::RuntimeLaunchMetadata::default(),
                None,
            )
            .expect_err("attach_process on a terminal thread must fail loudly");
        assert_eq!(
            error.to_string(),
            "refusing to attach process 99999/99999 to terminal thread T-skip-attach (cancelled)"
        );

        // Verify the rejected attachment did not write a stale PGID.
        let detail = store
            .get_thread("T-skip-attach")
            .expect("get_thread")
            .expect("thread exists");
        assert_eq!(detail.status, "cancelled");
        // runtime.pgid should still be None (never attached before finalize).
        assert!(
            detail.runtime.pgid.is_none(),
            "PGID should be None on a terminal thread that was never attached, got: {:?}",
            detail.runtime.pgid
        );
    }

    #[test]
    fn finalize_as_cancelled_works() {
        let (_tmpdir, store) = setup_state_store();

        let thread = make_thread(
            "T-cancel-1",
            "T-cancel-1",
            "directive",
            "directive:test/item",
            None,
        );
        store
            .create_thread_for_test(&thread)
            .expect("create_thread");
        store
            .mark_thread_running("T-cancel-1", None)
            .expect("mark running");

        let finalize = FinalizeThreadRecord {
            status: "cancelled".to_string(),
            outcome_code: Some("cancelled".to_string()),
            result_json: None,
            error_json: Some(serde_json::json!({"reason": "test_cancel"})),
            artifacts: vec![],
            managed_envelope: None,
            result_project_snapshot_hash: None,
            final_cost: None,
        };

        let persisted = store
            .finalize_thread("T-cancel-1", &finalize)
            .expect("finalize");
        assert!(!persisted.is_empty());

        // The terminal event should be thread_cancelled.
        let terminal_events: Vec<_> = persisted
            .iter()
            .filter(|e| e.event_type == "thread_cancelled")
            .collect();
        assert_eq!(
            terminal_events.len(),
            1,
            "should have one thread_cancelled event"
        );

        let detail = store
            .get_thread("T-cancel-1")
            .expect("get_thread")
            .expect("exists");
        assert_eq!(detail.status, "cancelled");
    }

    #[test]
    fn finalize_as_cancelled_rejects_already_terminal() {
        let (_tmpdir, store) = setup_state_store();

        let thread = make_thread(
            "T-double",
            "T-double",
            "directive",
            "directive:test/item",
            None,
        );
        store
            .create_thread_for_test(&thread)
            .expect("create_thread");
        store
            .mark_thread_running("T-double", None)
            .expect("mark running");

        let finalize = FinalizeThreadRecord {
            status: "completed".to_string(),
            outcome_code: Some("success".to_string()),
            result_json: None,
            error_json: None,
            artifacts: vec![],
            managed_envelope: None,
            result_project_snapshot_hash: None,
            final_cost: None,
        };
        store
            .finalize_thread("T-double", &finalize)
            .expect("first finalize");

        // Second finalize should fail.
        let cancel_finalize = FinalizeThreadRecord {
            status: "cancelled".to_string(),
            outcome_code: Some("cancelled".to_string()),
            result_json: None,
            error_json: None,
            artifacts: vec![],
            managed_envelope: None,
            result_project_snapshot_hash: None,
            final_cost: None,
        };
        let err = store
            .finalize_thread("T-double", &cancel_finalize)
            .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("invalid status transition"), "got: {msg}");
    }

    #[test]
    fn cancel_created_thread_without_pgid() {
        // A thread in "created" status (never started) can be finalized
        // as cancelled — no PGID needed.
        let (_tmpdir, store) = setup_state_store();

        let thread = make_thread(
            "T-created-cancel",
            "T-created-cancel",
            "directive",
            "directive:test/item",
            None,
        );
        store
            .create_thread_for_test(&thread)
            .expect("create_thread");
        // Don't mark running — stays in "created".

        let finalize = FinalizeThreadRecord {
            status: "cancelled".to_string(),
            outcome_code: Some("cancelled".to_string()),
            result_json: None,
            error_json: None,
            artifacts: vec![],
            managed_envelope: None,
            result_project_snapshot_hash: None,
            final_cost: None,
        };

        let persisted = store
            .finalize_thread("T-created-cancel", &finalize)
            .expect("finalize created");
        assert!(!persisted.is_empty());

        let detail = store
            .get_thread("T-created-cancel")
            .expect("get")
            .expect("exists");
        assert_eq!(detail.status, "cancelled");
    }
}
