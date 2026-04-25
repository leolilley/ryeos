//! Trace-capture test for `StateStore` write spans.
//!
//! Runs in its own integration-test binary so the per-thread
//! `ryeos_tracing::test::capture_traces` subscriber is the only
//! subscriber active on the test thread.
//!
//! Asserts that a create_thread → mark_thread_running →
//! attach_thread_process → finalize_thread sequence emits the
//! corresponding `state:*` spans we instrumented in
//! `ryeosd/src/state_store.rs`.

use std::sync::Arc;

use ryeosd::launch_metadata::RuntimeLaunchMetadata;
use ryeosd::state_store::{
    FinalizeThreadRecord, NewThreadRecord, StateStore,
};
use ryeosd::write_barrier::WriteBarrier;
use tempfile::TempDir;

fn setup_state_store() -> (TempDir, Arc<StateStore>) {
    let tmpdir = TempDir::new().unwrap();
    let state_root = tmpdir.path().join(".state");
    let runtime_db_path = tmpdir.path().join("runtime.sqlite3");

    let test_key_path = tmpdir.path().join("test_key.pem");
    let identity = ryeosd::identity::NodeIdentity::create(&test_key_path)
        .expect("test identity creation should succeed");
    let signer = Arc::new(ryeosd::state_store::NodeIdentitySigner::from_identity(
        &identity,
    ));

    let write_barrier = WriteBarrier::new();

    let store = StateStore::new(state_root, runtime_db_path, signer, write_barrier)
        .expect("StateStore creation should succeed");

    (tmpdir, Arc::new(store))
}

fn make_thread(thread_id: &str, chain_root_id: &str) -> NewThreadRecord {
    NewThreadRecord {
        thread_id: thread_id.to_string(),
        chain_root_id: chain_root_id.to_string(),
        kind: "directive".to_string(),
        item_ref: "test/directive".to_string(),
        executor_ref: "test/executor".to_string(),
        launch_mode: "inline".to_string(),
        current_site_id: "site:test".to_string(),
        origin_site_id: "site:test".to_string(),
        upstream_thread_id: None,
        requested_by: Some("user:test".to_string()),
    }
}

#[test]
fn state_store_write_path_emits_state_spans() {
    ryeos_tracing::test::prime_callsites();
    let (_tmpdir, store) = setup_state_store();

    let (_, spans) = ryeos_tracing::test::capture_traces(|| {
        let thread = make_thread("T-trace-1", "T-trace-1");
        store.create_thread(&thread).expect("create_thread");
        store
            .mark_thread_running("T-trace-1", None)
            .expect("mark_thread_running");
        store
            .attach_thread_process(
                "T-trace-1",
                111,
                222,
                &RuntimeLaunchMetadata::default(),
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

    let create = ryeos_tracing::test::find_span(&spans, "state:create_thread")
        .unwrap_or_else(|| panic!("expected state:create_thread; got {:?}", names));
    assert_eq!(create.field("thread_id"), Some("T-trace-1"));
    assert_eq!(create.field("chain_root_id"), Some("T-trace-1"));

    let running = ryeos_tracing::test::find_span(&spans, "state:mark_thread_running")
        .unwrap_or_else(|| panic!("expected state:mark_thread_running; got {:?}", names));
    assert_eq!(running.field("thread_id"), Some("T-trace-1"));

    let attach = ryeos_tracing::test::find_span(&spans, "state:attach_thread_process")
        .unwrap_or_else(|| panic!("expected state:attach_thread_process; got {:?}", names));
    assert_eq!(attach.field("thread_id"), Some("T-trace-1"));
    assert_eq!(attach.field("pid"), Some("111"));

    let finalize = ryeos_tracing::test::find_span(&spans, "state:finalize_thread")
        .unwrap_or_else(|| panic!("expected state:finalize_thread; got {:?}", names));
    assert_eq!(finalize.field("thread_id"), Some("T-trace-1"));
    assert_eq!(finalize.field("status"), Some("completed"));

    // The state_store wrappers must nest the inner ryeos-state +
    // runtime_db spans as descendants:
    //   state:create_thread → state:chain_create + state:chain_append
    //   state:attach_thread_process → state:thread_attach (runtime_db)
    assert!(
        ryeos_tracing::test::find_child(create, "state:chain_create").is_some(),
        "expected state:chain_create under state:create_thread; got {:?}",
        names
    );
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
        .create_thread(&make_thread("T-bump-1", "T-bump-1"))
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
