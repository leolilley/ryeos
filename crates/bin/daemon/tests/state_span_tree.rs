//! Trace-capture test for `StateStore` write spans.
//!
//! Runs in its own integration-test binary so the per-thread
//! `ryeos_tracing::test::capture_traces` subscriber is the only
//! subscriber active on the test thread.
//!
//! Asserts that root creation followed by mark_thread_running →
//! attach_thread_process → finalize_thread emits the complete current
//! state-write span sequence and preserves the expected wrapper nesting.

use std::sync::Arc;

use ryeos_app::launch_metadata::RuntimeLaunchMetadata;
use ryeos_app::state_store::{FinalizeThreadRecord, NewThreadRecord, StateStore};
use ryeos_app::write_barrier::WriteBarrier;
use tempfile::TempDir;

fn captured_policy() -> ryeos_state::objects::CapturedThreadHistoryPolicy {
    let hash = "a".repeat(64);
    ryeos_state::objects::CapturedThreadHistoryPolicy {
        retention: ryeos_state::objects::ThreadHistoryRetention::Durable,
        canonical_item_ref: "directive:test/directive".to_string(),
        item_content_hash: hash.clone(),
        item_signer_fingerprint: Some(hash.clone()),
        item_trust_class: ryeos_state::objects::CapturedItemTrustClass::Trusted,
        kind_schema_content_hash: hash,
        resolved_from: ryeos_state::objects::CapturedPolicyProvenance::NodeDefault {
            node_policy: ryeos_state::objects::CapturedNodeHistoryPolicyProvenance::MissingConfig,
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

fn make_thread(thread_id: &str, chain_root_id: &str) -> NewThreadRecord {
    NewThreadRecord {
        thread_id: thread_id.to_string(),
        chain_root_id: chain_root_id.to_string(),
        kind: "directive".to_string(),
        item_ref: "directive:test/directive".to_string(),
        executor_ref: "test/executor".to_string(),
        launch_mode: "inline".to_string(),
        current_site_id: "site:test".to_string(),
        origin_site_id: "site:test".to_string(),
        upstream_thread_id: None,
        requested_by: Some("user:test".to_string()),
        project_root: None,
        base_project_snapshot_hash: None,
        usage_subject: None,
        usage_subject_asserted_by: None,
        captured_history_policy: (thread_id == chain_root_id).then(captured_policy),
    }
}

#[test]
fn state_store_write_path_emits_state_spans() {
    ryeos_tracing::test::prime_callsites();
    let (_tmpdir, store) = setup_state_store();

    let (_, spans) = ryeos_tracing::test::capture_traces(|| {
        let thread = make_thread("T-trace-1", "T-trace-1");
        store
            .create_thread_for_test(&thread)
            .expect("create_thread");
        store
            .mark_thread_running("T-trace-1", None)
            .expect("mark_thread_running");
        store
            .attach_thread_process(
                "T-trace-1",
                111,
                222,
                &ryeos_app::process::ExecutionProcessIdentity {
                    schema_version: ryeos_app::process::PROCESS_IDENTITY_SCHEMA_VERSION,
                    boot_id: "test-boot".to_string(),
                    target_pid: 111,
                    target_start_time_ticks: 10,
                    group_leader_pid: 222,
                    group_leader_start_time_ticks: 20,
                },
                &RuntimeLaunchMetadata::default(),
                None,
            )
            .expect("attach_thread_process");
        store
            .finalize_thread(
                "T-trace-1",
                &FinalizeThreadRecord {
                    status: "completed".into(),
                    outcome_code: None,
                    result_json: Some(serde_json::json!({"ok": true})),
                    error_json: None,
                    artifacts: vec![],
                    final_cost: None,
                    managed_envelope: None,
                    result_project_snapshot_hash: None,
                },
            )
            .expect("finalize_thread");
    });

    fn collect_names(s: &ryeos_tracing::test::RecordedSpan, out: &mut Vec<String>) {
        out.push(s.name.clone());
        for c in &s.children {
            collect_names(c, out);
        }
    }
    let mut names: Vec<String> = Vec::new();
    for s in &spans {
        collect_names(s, &mut names);
    }

    assert_eq!(
        names,
        [
            "state:project_event",
            "state:mark_thread_running",
            "state:project_event",
            "state:attach_thread_process",
            "state:thread_attach",
            "state:finalize_thread",
            "state:project_event",
        ]
        .map(str::to_string)
        .to_vec(),
        "state lifecycle span ordering drifted"
    );

    let created = ryeos_tracing::test::find_span(&spans, "state:project_event")
        .expect("root creation must project thread_created");
    assert_eq!(created.field("thread_id"), Some("T-trace-1"));
    assert_eq!(created.field("event_type"), Some("thread_created"));

    let running = ryeos_tracing::test::find_span(&spans, "state:mark_thread_running")
        .unwrap_or_else(|| panic!("expected state:mark_thread_running; got {:?}", names));
    assert_eq!(running.field("thread_id"), Some("T-trace-1"));
    let started = ryeos_tracing::test::find_child(running, "state:project_event")
        .expect("mark_thread_running must project thread_started");
    assert_eq!(started.field("thread_id"), Some("T-trace-1"));
    assert_eq!(started.field("event_type"), Some("thread_started"));

    let attach = ryeos_tracing::test::find_span(&spans, "state:attach_thread_process")
        .unwrap_or_else(|| panic!("expected state:attach_thread_process; got {:?}", names));
    assert_eq!(attach.field("thread_id"), Some("T-trace-1"));
    assert_eq!(attach.field("pid"), Some("111"));

    let finalize = ryeos_tracing::test::find_span(&spans, "state:finalize_thread")
        .unwrap_or_else(|| panic!("expected state:finalize_thread; got {:?}", names));
    assert_eq!(finalize.field("thread_id"), Some("T-trace-1"));
    assert_eq!(finalize.field("status"), Some("completed"));
    let completed = ryeos_tracing::test::find_child(finalize, "state:project_event")
        .expect("finalize_thread must project thread_completed");
    assert_eq!(completed.field("thread_id"), Some("T-trace-1"));
    assert_eq!(completed.field("event_type"), Some("thread_completed"));

    // The state_store process wrapper must retain the runtime-db attachment as
    // a descendant; lifecycle event projections are asserted above as children
    // of the transition that authored them.
    assert!(
        ryeos_tracing::test::find_child(attach, "state:thread_attach").is_some(),
        "expected state:thread_attach under state:attach_thread_process; got {:?}",
        names
    );
}

#[test]
fn resume_attempts_bump_emits_state_span() {
    ryeos_tracing::test::prime_callsites();
    let (_tmpdir, store) = setup_state_store();

    // Need a thread row in runtime_db for bump_resume_attempts to act on.
    store
        .create_thread_for_test(&make_thread("T-bump-1", "T-bump-1"))
        .expect("create_thread");

    let (post, spans) = ryeos_tracing::test::capture_traces(|| {
        store
            .bump_resume_attempts("T-bump-1")
            .expect("bump_resume_attempts")
    });

    assert_eq!(post, 1, "first bump should yield post-increment value of 1");

    let _bump = ryeos_tracing::test::find_span(&spans, "state:resume_attempts_bump")
        .unwrap_or_else(|| {
            let names: Vec<&str> = spans.iter().map(|s| s.name.as_str()).collect();
            panic!("expected state:resume_attempts_bump; got {:?}", names)
        });
}
