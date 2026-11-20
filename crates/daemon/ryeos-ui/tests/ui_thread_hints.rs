// A thread hint names a thread and its status, so a browser session receives
// it only when its own authorization may observe that thread: the owner for a
// projectless seat, the live project for a project-attached seat. An
// undecidable thread produces no hint, and an activity hint carries only the
// threads, and the count of events, the session may observe.

use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::broadcast::Receiver;

use ryeos_app::state::AppState;
use ryeos_app::state_store::NewThreadRecord;
use ryeos_app::stream_envelope::RouteStreamEnvelope;
use ryeos_state::objects::{
    CapturedItemTrustClass, CapturedNodeHistoryPolicyProvenance, CapturedPolicyProvenance,
    CapturedThreadHistoryPolicy, EnvironmentAuthority, ExecutionProjectAuthority,
    LiveFilesystemConfinement, LiveProjectAccess, ThreadHistoryRetention,
};
use ryeos_ui::state::get_ui_state;
use ryeos_ui::test_support::{build_test_state, launch_context, mint_launch};
use ryeos_ui::{ThreadHintFanout, UiState};

fn captured_policy() -> CapturedThreadHistoryPolicy {
    let hash = "a".repeat(64);
    CapturedThreadHistoryPolicy {
        retention: ThreadHistoryRetention::Durable,
        canonical_item_ref: "directive:test/hints".to_string(),
        item_content_hash: hash.clone(),
        item_signer_fingerprint: Some(hash.clone()),
        item_trust_class: CapturedItemTrustClass::Trusted,
        kind_schema_content_hash: hash,
        resolved_from: CapturedPolicyProvenance::NodeDefault {
            node_policy: CapturedNodeHistoryPolicyProvenance::test_policy(),
        },
    }
}

fn create_thread(
    state: &AppState,
    thread_id: &str,
    requested_by: &str,
    project_root: Option<&std::path::Path>,
) {
    let (project_root, project_authority) = match project_root {
        None => (None, ExecutionProjectAuthority::PROJECTLESS),
        Some(root) => {
            let root = root.canonicalize().expect("canonical fixture project");
            let authority = ExecutionProjectAuthority::live(
                root.clone(),
                format!("local:{}", root.display()),
                LiveProjectAccess::ReadOnly,
                LiveFilesystemConfinement::standard_fixed_parents(),
                EnvironmentAuthority::None,
                Vec::new(),
            )
            .expect("live fixture project authority");
            (Some(root), authority)
        }
    };
    state
        .state_store
        .create_thread_for_test(&NewThreadRecord {
            thread_id: thread_id.to_string(),
            chain_root_id: thread_id.to_string(),
            kind: "directive".to_string(),
            item_ref: "directive:test/hints".to_string(),
            executor_ref: "test/executor".to_string(),
            launch_mode: "wait".to_string(),
            current_site_id: "site:test".to_string(),
            origin_site_id: "site:test".to_string(),
            upstream_thread_id: None,
            requested_by: Some(requested_by.to_string()),
            project_root,
            project_authority,
            base_project_snapshot_hash: None,
            usage_subject: None,
            usage_subject_asserted_by: None,
            captured_history_policy: Some(captured_policy()),
        })
        .unwrap();
}

/// A live session of `principal`, attached to `project` when given, with a
/// receiver on its bus.
fn session(
    state: &AppState,
    principal: &str,
    project: Option<&std::path::Path>,
) -> (String, Receiver<RouteStreamEnvelope>) {
    let mut context = launch_context(
        "surface:ryeos/ui/base",
        project.map(|path| path.to_str().expect("utf8 project path")),
        ryeos_ui::compiled_binding::EffectiveUiPosture::Interactive,
        None,
    );
    Arc::make_mut(&mut context.compiled_binding)
        .binding
        .principal_id = principal.to_string();
    let (session_id, token) = mint_launch(state, context);
    let ui = get_ui_state(state).unwrap();
    assert_eq!(
        ui.browser_sessions.consume_launch_token(&token),
        Some(session_id.clone())
    );
    let receiver = ui.session_bus.subscribe(&session_id);
    (session_id, receiver)
}

fn lifecycle(thread_id: &str) -> Value {
    json!({
        "kind": "thread",
        "thread_id": thread_id,
        "chain_root_id": thread_id,
        "event_type": "thread_started",
        "status": "running",
        "updated_at": "now",
    })
}

async fn send_lifecycle(
    fanout: &ThreadHintFanout,
    state: &AppState,
    ui: &UiState,
    thread_id: &str,
    chain_root_id: &str,
) {
    fanout
        .publish_lifecycle(state, ui, thread_id, chain_root_id, lifecycle(thread_id))
        .await;
}

fn received(receiver: &mut Receiver<RouteStreamEnvelope>) -> Vec<Value> {
    let mut out = Vec::new();
    while let Ok(envelope) = receiver.try_recv() {
        assert_eq!(envelope.event_type, "thread.hint");
        out.push(envelope.payload);
    }
    out
}

#[tokio::test]
async fn each_principal_receives_hints_for_its_own_threads_only() {
    let (_tmp, _owner, state) = build_test_state();
    let ui = get_ui_state(&state).unwrap().clone();
    let (_a, mut a_rx) = session(&state, "fp:alice", None);
    let (_b, mut b_rx) = session(&state, "fp:bob", None);
    create_thread(&state, "T-alice", "fp:alice", None);
    create_thread(&state, "T-bob", "fp:bob", None);
    let fanout = ThreadHintFanout::new();

    send_lifecycle(&fanout, &state, &ui, "T-alice", "T-alice").await;
    send_lifecycle(&fanout, &state, &ui, "T-bob", "T-bob").await;

    let a = received(&mut a_rx);
    let b = received(&mut b_rx);
    assert_eq!(a.len(), 1, "{a:?}");
    assert_eq!(a[0]["thread_id"], "T-alice");
    assert_eq!(b.len(), 1, "{b:?}");
    assert_eq!(b[0]["thread_id"], "T-bob");
    // Nothing of the other principal's thread rides on what either received.
    for payload in a.iter().chain(&b) {
        let text = payload.to_string();
        let other = if payload["thread_id"] == "T-alice" {
            "T-bob"
        } else {
            "T-alice"
        };
        assert!(!text.contains(other), "{text}");
    }
    assert_eq!(fanout.undecided_count(), 0);
}

#[tokio::test]
async fn a_local_operator_session_receives_what_its_principal_owns_and_no_live_project_threads() {
    // A projectless seat of the node operator is admitted exactly as a seat
    // read of that thread is: its own projectless threads, not another
    // principal's, and not a live project's thread it is not attached to.
    let (_tmp, _owner, state) = build_test_state();
    let ui = get_ui_state(&state).unwrap().clone();
    let project = tempfile::TempDir::new().unwrap();
    let (_op, mut op_rx) = session(&state, "fp:operator", None);
    create_thread(&state, "T-op", "fp:operator", None);
    create_thread(&state, "T-other", "fp:other", None);
    create_thread(&state, "T-op-live", "fp:operator", Some(project.path()));
    let fanout = ThreadHintFanout::new();
    for id in ["T-op", "T-other", "T-op-live"] {
        send_lifecycle(&fanout, &state, &ui, id, id).await;
    }
    let got = received(&mut op_rx);
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0]["thread_id"], "T-op");
}

#[tokio::test]
async fn a_project_attached_seat_receives_only_its_projects_threads() {
    let (_tmp, _owner, state) = build_test_state();
    let ui = get_ui_state(&state).unwrap().clone();
    let project_a = tempfile::TempDir::new().unwrap();
    let project_b = tempfile::TempDir::new().unwrap();
    let (_s, mut rx) = session(&state, "fp:alice", Some(project_a.path()));
    create_thread(&state, "T-in-a", "fp:alice", Some(project_a.path()));
    // A teammate's thread in the same project is the project's thread.
    create_thread(&state, "T-in-a-team", "fp:teammate", Some(project_a.path()));
    create_thread(&state, "T-in-b", "fp:alice", Some(project_b.path()));
    // The seat's own projectless thread belongs to no project it is on.
    create_thread(&state, "T-free", "fp:alice", None);
    let fanout = ThreadHintFanout::new();
    for id in ["T-in-a", "T-in-a-team", "T-in-b", "T-free"] {
        send_lifecycle(&fanout, &state, &ui, id, id).await;
    }
    let ids: Vec<String> = received(&mut rx)
        .iter()
        .map(|payload| payload["thread_id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids, vec!["T-in-a".to_owned(), "T-in-a-team".to_owned()]);
}

#[tokio::test]
async fn an_undecidable_thread_produces_no_hint_and_is_counted() {
    let (_tmp, _owner, state) = build_test_state();
    let ui = get_ui_state(&state).unwrap().clone();
    let (_s, mut rx) = session(&state, "fp:alice", None);
    create_thread(&state, "T-alice", "fp:alice", None);
    let fanout = ThreadHintFanout::new();

    // No signed subject for the thread.
    send_lifecycle(&fanout, &state, &ui, "T-absent", "T-absent").await;
    assert_eq!(fanout.undecided_count(), 1);
    // An event whose chain root contradicts the signed subject.
    send_lifecycle(&fanout, &state, &ui, "T-alice", "T-elsewhere").await;
    assert_eq!(fanout.undecided_count(), 2);
    assert!(received(&mut rx).is_empty());

    // Activity of an undecidable thread is withheld the same way.
    let counts = std::collections::HashMap::from([("T-absent".to_owned(), ("T-absent".to_owned(), 3))]);
    fanout.publish_activity(&state, &ui, &counts).await;
    assert!(received(&mut rx).is_empty());
    assert_eq!(fanout.undecided_count(), 3);

    // The decided thread still flows.
    send_lifecycle(&fanout, &state, &ui, "T-alice", "T-alice").await;
    assert_eq!(received(&mut rx).len(), 1);
}

#[tokio::test]
async fn activity_hints_carry_only_the_sessions_own_threads_and_count() {
    let (_tmp, _owner, state) = build_test_state();
    let ui = get_ui_state(&state).unwrap().clone();
    let (_a, mut a_rx) = session(&state, "fp:alice", None);
    let (_b, mut b_rx) = session(&state, "fp:bob", None);
    let (_c, mut c_rx) = session(&state, "fp:carol", None);
    create_thread(&state, "T-alice", "fp:alice", None);
    create_thread(&state, "T-bob-1", "fp:bob", None);
    create_thread(&state, "T-bob-2", "fp:bob", None);
    let fanout = ThreadHintFanout::new();
    let counts = std::collections::HashMap::from([
        ("T-alice".to_owned(), ("T-alice".to_owned(), 2)),
        ("T-bob-1".to_owned(), ("T-bob-1".to_owned(), 5)),
        ("T-bob-2".to_owned(), ("T-bob-2".to_owned(), 7)),
    ]);
    fanout.publish_activity(&state, &ui, &counts).await;

    let a = received(&mut a_rx);
    assert_eq!(
        a,
        vec![json!({"kind": "activity", "thread_ids": ["T-alice"], "event_count": 2})]
    );
    let b = received(&mut b_rx);
    assert_eq!(b.len(), 1);
    assert_eq!(b[0]["event_count"], 12);
    assert_eq!(b[0]["thread_ids"], json!(["T-bob-1", "T-bob-2"]));
    // A session that may observe none of the interval's threads is not even
    // told that there was activity.
    assert!(received(&mut c_rx).is_empty());
}

#[tokio::test]
async fn a_session_with_no_attachment_receives_nothing() {
    let (_tmp, _owner, state) = build_test_state();
    let ui = get_ui_state(&state).unwrap().clone();
    let (session_id, mut rx) = session(&state, "fp:alice", None);
    create_thread(&state, "T-alice", "fp:alice", None);
    let current = ui.browser_sessions.get_session(&session_id).unwrap();
    let root = current.attachments[&current.root_attachment_id].coordinate();
    // Before revocation the hint arrives; after it, the seat has no
    // authority left and receives nothing.
    let fanout = ThreadHintFanout::new();
    send_lifecycle(&fanout, &state, &ui, "T-alice", "T-alice").await;
    assert_eq!(received(&mut rx).len(), 1);
    ui.browser_sessions
        .revoke_attachment(&session_id, &root)
        .expect("the store revokes the root attachment");
    send_lifecycle(&fanout, &state, &ui, "T-alice", "T-alice").await;
    assert!(received(&mut rx).is_empty());
}
