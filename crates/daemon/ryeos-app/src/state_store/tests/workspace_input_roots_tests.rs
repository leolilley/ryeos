//! A detached child's captured workspace input as a GC root.
use super::*;

fn input_pair(snapshot: &str, capture: &str) -> ryeos_state::objects::WorkspaceGenerationPair {
    ryeos_state::objects::WorkspaceGenerationPair {
        snapshot_hash: snapshot.repeat(64),
        output_capture_hash: Some(capture.repeat(64)),
    }
}

/// Reserve a detached action intent that captured `pair`, leaving its
/// workspace operation `Quiesced` (the phase the capture itself ends in).
fn quiesced_detached_input(
    store: &StateStore,
    child: &str,
    pair: &ryeos_state::objects::WorkspaceGenerationPair,
) -> String {
    use runtime_db::RuntimeWorkspaceOperationPhase::{Quiescing, Reserved};
    let invocation = ryeos_runtime::workload_client::WorkloadInvocationSource::Cli {
        external_request_id: format!("detached-{}", child.to_ascii_lowercase()),
    };
    let grant = "c".repeat(64);
    let boot = "a".repeat(64);
    let authority = "b".repeat(64);
    let operation_id = invocation.runtime_operation_id(&grant).unwrap();
    let g = store.lock().unwrap();
    g.runtime_db
        .reserve_runtime_action_intent_with_workspace(
            &operation_id,
            "T-workspace-root",
            "T-workspace-root",
            runtime_db::RuntimeActionMode::Detached,
            &"e".repeat(64),
            child,
            None,
            Some(&runtime_db::NewRuntimeWorkspaceOperation {
                workspace_id: "workspace-parent",
                access: ryeos_engine::kind_registry::WorkspaceAccess::ImmutableCurrentGeneration,
                worker_instance_id: "test-worker",
                worker_boot_epoch: 1,
                worker_boot_identity_hash: &boot,
                project_authority_digest: &authority,
                workload_client_grant_digest: &grant,
                invocation,
            }),
        )
        .unwrap();
    g.runtime_db
        .transition_runtime_workspace_operation(&operation_id, &[Reserved], Quiescing)
        .unwrap();
    g.runtime_db
        .bind_runtime_workspace_input_generation(&operation_id, pair)
        .unwrap();
    operation_id
}

fn release_operation(store: &StateStore, operation_id: &str) {
    use runtime_db::RuntimeWorkspaceOperationPhase::{Quiesced, Released};
    store
        .lock()
        .unwrap()
        .runtime_db
        .transition_runtime_workspace_operation(operation_id, &[Quiesced], Released)
        .unwrap();
}

fn birth_detached_child(
    store: &StateStore,
    child: &str,
    input: Option<ryeos_state::objects::WorkspaceGenerationPair>,
) {
    let mut resume = continuation_resume_context(ProjectContext::LocalPath {
        path: PathBuf::from("/work/project"),
    });
    resume.workspace_input = input;
    let launch_metadata = crate::launch_metadata::RuntimeLaunchMetadata::default()
        .with_launch_driver(ryeos_state::objects::ExecutionLaunchDriver::ManagedRuntime)
        .with_resume_context(resume);
    store
        .create_root_thread_with_events_and_launch_metadata(
            &thread_record(child, child),
            Vec::new(),
            Some(&launch_metadata),
        )
        .expect("detached child thread");
}

fn rooted(
    store: &StateStore,
    pair: &ryeos_state::objects::WorkspaceGenerationPair,
) -> (bool, bool) {
    let roots = store.active_resume_snapshot_roots().unwrap();
    (
        roots.contains(&pair.snapshot_hash),
        roots.contains(pair.output_capture_hash.as_ref().unwrap()),
    )
}

/// The input is rooted at every point from the capture until the child ends:
/// by the quiesced operation, then by the released intent while no child row
/// exists, then by the child's own sealed resume authority, and it is dropped
/// when the child reaches a terminal state.
#[test]
fn a_detached_childs_workspace_input_is_rooted_from_capture_until_the_child_ends() {
    let store = test_store();
    let pair = input_pair("c", "d");
    assert_eq!(rooted(&store, &pair), (false, false));

    let operation_id = quiesced_detached_input(&store, "T-detached", &pair);
    assert_eq!(rooted(&store, &pair), (true, true), "quiesced operation");

    // The window between capture and child birth: the parent is no longer
    // fenced and no thread row carries the input yet.
    release_operation(&store, &operation_id);
    assert!(store.get_thread("T-detached").unwrap().is_none());
    assert_eq!(
        rooted(&store, &pair),
        (true, true),
        "a sweep forced between capture and child creation keeps the input"
    );

    // Created, then running: the child's sealed context holds the input even
    // though its intent is released and no longer does.
    birth_detached_child(&store, "T-detached", Some(pair.clone()));
    assert_eq!(rooted(&store, &pair), (true, true), "created child");
    store.mark_thread_running("T-detached", None).unwrap();
    assert_eq!(rooted(&store, &pair), (true, true), "running child");

    finish_workspace_test_thread(&store, "T-detached");
    assert_eq!(
        rooted(&store, &pair),
        (false, false),
        "the input is released with the child's terminal state"
    );
}

/// A released intent whose child exists and carries no input roots nothing:
/// the clause bridges only the window before the child's birth.
#[test]
fn a_released_intent_roots_its_input_only_until_its_child_exists() {
    let store = test_store();
    let pair = input_pair("e", "f");
    let operation_id = quiesced_detached_input(&store, "T-bare-child", &pair);
    release_operation(&store, &operation_id);
    assert_eq!(rooted(&store, &pair), (true, true));
    birth_detached_child(&store, "T-bare-child", None);
    assert_eq!(rooted(&store, &pair), (false, false));
}

const HOUR_MS: i64 = 60 * 60 * 1000;

/// An abandoned spawn: the capture released the parent's workspace and the
/// child was never created. The intent keeps rooting the captured input until
/// it is old enough to retire; the retirement then lets a sweep collect it.
#[test]
fn an_abandoned_detached_intent_is_retired_and_its_input_is_no_longer_rooted() {
    let store = test_store();
    let pair = input_pair("1", "2");
    let operation_id = quiesced_detached_input(&store, "T-never-born", &pair);
    release_operation(&store, &operation_id);
    assert!(store.get_thread("T-never-born").unwrap().is_none());
    assert_eq!(rooted(&store, &pair), (true, true), "no child yet");

    let now = lillux::time::timestamp_millis();
    let age = abandoned_detached_intent_age(None);
    // A young intent is kept: its spawn may still be running.
    let young = store.retire_abandoned_detached_intents(now, age).unwrap();
    assert!(young.retired.is_empty());
    assert_eq!(rooted(&store, &pair), (true, true), "young intent kept");

    // Past the bound it is retired, and a sweep no longer roots the input.
    let old = store
        .retire_abandoned_detached_intents(now + 2 * HOUR_MS, age)
        .unwrap();
    assert_eq!(old.retired, vec![operation_id.clone()]);
    assert!(
        store
            .get_runtime_action_intent(&operation_id)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        rooted(&store, &pair),
        (false, false),
        "a sweep after the retirement collects the input"
    );
    // Retiring again finds nothing.
    let again = store
        .retire_abandoned_detached_intents(now + 2 * HOUR_MS, age)
        .unwrap();
    assert!(again.retired.is_empty());
}

/// An intent whose child exists is never retired, however old: the child's
/// own authority owns its input from then on.
#[test]
fn an_old_detached_intent_with_a_child_is_not_retired() {
    let store = test_store();
    let pair = input_pair("3", "4");
    let operation_id = quiesced_detached_input(&store, "T-born", &pair);
    release_operation(&store, &operation_id);
    birth_detached_child(&store, "T-born", Some(pair.clone()));
    let report = store
        .retire_abandoned_detached_intents(
            lillux::time::timestamp_millis() + 48 * HOUR_MS,
            abandoned_detached_intent_age(None),
        )
        .unwrap();
    assert!(report.retired.is_empty());
    assert!(
        store
            .get_runtime_action_intent(&operation_id)
            .unwrap()
            .is_some()
    );
    assert_eq!(rooted(&store, &pair), (true, true));
}

#[test]
fn the_abandoned_intent_age_defaults_and_clamps() {
    use std::time::Duration;
    assert_eq!(
        abandoned_detached_intent_age(None),
        Duration::from_secs(3600)
    );
    assert_eq!(
        abandoned_detached_intent_age(Some(Duration::from_secs(60))),
        Duration::from_secs(60)
    );
    assert_eq!(
        abandoned_detached_intent_age(Some(Duration::from_secs(10 * 24 * 3600))),
        Duration::from_secs(24 * 3600)
    );
}
