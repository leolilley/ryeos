// A seat's thread is launched by the key its browser session was minted for.
// Reading follows that key: any session of the key reads it, through the
// session's own dispatch or another's; another key's session and a retired
// key read `NotFound`, exactly like an absent thread.

use std::sync::Arc;

use serde_json::json;

use ryeos_app::handler_context::HandlerContext;
use ryeos_app::state::AppState;
use ryeos_app::state_store::NewThreadRecord;
use ryeos_engine::trust::{
    SignerRecord, SignerRole, SignerScope, SignerStatus, TrustEvent, TrustEventKind, TrustStore,
    compute_fingerprint,
};
use ryeos_state::objects::{
    CapturedItemTrustClass, CapturedNodeHistoryPolicyProvenance, CapturedPolicyProvenance,
    CapturedThreadHistoryPolicy, ExecutionProjectAuthority, ThreadHistoryRetention,
};
use ryeos_ui::browser_session::{AdmittedBindingAttachment, BrowserSession};
use ryeos_ui::state::get_ui_state;
use ryeos_ui::test_support as test_state;

const BETA: &str = "fp:b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2";

fn captured_policy() -> CapturedThreadHistoryPolicy {
    let hash = "a".repeat(64);
    CapturedThreadHistoryPolicy {
        retention: ThreadHistoryRetention::Durable,
        canonical_item_ref: "directive:test/own-session".to_string(),
        item_content_hash: hash.clone(),
        item_signer_fingerprint: Some(hash.clone()),
        item_trust_class: CapturedItemTrustClass::Trusted,
        kind_schema_content_hash: hash,
        resolved_from: CapturedPolicyProvenance::NodeDefault {
            node_policy: CapturedNodeHistoryPolicyProvenance::test_policy(),
        },
    }
}

/// A running projectless thread whose launcher is `requested_by`.
fn create_thread(state: &AppState, thread_id: &str, requested_by: &str) {
    state
        .state_store
        .create_thread_for_test(&NewThreadRecord {
            thread_id: thread_id.to_string(),
            chain_root_id: thread_id.to_string(),
            kind: "directive".to_string(),
            item_ref: "directive:test/own-session".to_string(),
            executor_ref: "test/executor".to_string(),
            launch_mode: "wait".to_string(),
            current_site_id: "site:test".to_string(),
            origin_site_id: "site:test".to_string(),
            upstream_thread_id: None,
            requested_by: Some(requested_by.to_string()),
            project_root: None,
            project_authority: ExecutionProjectAuthority::PROJECTLESS,
            base_project_snapshot_hash: None,
            usage_subject: None,
            usage_subject_asserted_by: None,
            captured_history_policy: Some(captured_policy()),
            owner: None,
        })
        .expect("create fixture thread");
    state
        .state_store
        .mark_thread_running(thread_id, None)
        .expect("mark fixture thread running");
}

/// A live projectless session minted for `key`, with its root attachment.
fn session_for(state: &AppState, key: &str) -> (BrowserSession, Arc<AdmittedBindingAttachment>) {
    let mut launch = test_state::launch_context(
        "surface:ryeos/ui/base",
        None,
        ryeos_ui::compiled_binding::EffectiveUiPosture::ObservationOnly,
        Some(key.to_string()),
    );
    Arc::make_mut(&mut launch.compiled_binding)
        .binding
        .principal_id = key.to_string();
    let (session_id, token) = test_state::mint_launch(state, launch);
    let ui = get_ui_state(state).expect("UI state");
    assert_eq!(
        ui.browser_sessions.consume_launch_token(&token),
        Some(session_id.clone())
    );
    let session = ui
        .browser_sessions
        .get_session(&session_id)
        .expect("active session");
    assert_eq!(session.principal_id, key);
    let attachment = session.attachments[&session.root_attachment_id].clone();
    (session, attachment)
}

async fn inspect(
    state: &Arc<AppState>,
    session: &BrowserSession,
    attachment: Arc<AdmittedBindingAttachment>,
    thread_id: &str,
) -> anyhow::Result<serde_json::Value> {
    let ctx = HandlerContext::new(
        "fp:compiled-source".to_string(),
        vec!["ui.read".into()],
        true,
    );
    test_state::with_session_dispatch(
        session,
        attachment,
        (ryeos_ui::handlers::ui_threads::INSPECT_DESCRIPTOR.handler)(
            json!({"thread_id": thread_id, "event_limit": 1}),
            ctx,
            state.clone(),
        ),
    )
    .await
}

fn is_not_found(result: &anyhow::Result<serde_json::Value>) -> bool {
    matches!(
        result
            .as_ref()
            .err()
            .and_then(|error| error.downcast_ref::<ryeos_app::handler_error::HandlerError>()),
        Some(ryeos_app::handler_error::HandlerError::NotFound)
    )
}

#[tokio::test]
async fn every_session_of_the_launching_key_reads_its_seat_thread_and_no_other_key_does() {
    let (_tmp, _owner, state) = test_state::build_test_state();
    let key = test_state::FIXTURE_KEY;
    let (own, own_attachment) = session_for(&state, key);
    let (sibling, sibling_attachment) = session_for(&state, key);
    let (foreign, foreign_attachment) = session_for(&state, BETA);
    create_thread(&state, "T-seat", key);
    let state = Arc::new(state);

    let read = inspect(&state, &own, own_attachment, "T-seat").await;
    assert!(
        read.is_ok(),
        "the launching session reads its seat thread: {read:?}"
    );

    let read = inspect(&state, &sibling, sibling_attachment, "T-seat").await;
    assert!(
        read.is_ok(),
        "another session of the same key reads it: {read:?}"
    );

    let read = inspect(&state, &foreign, foreign_attachment, "T-seat").await;
    assert!(is_not_found(&read), "another key's session: {read:?}");
}

fn record(key: &lillux::crypto::SigningKey, status: SignerStatus) -> SignerRecord {
    SignerRecord::new(
        &key.verifying_key(),
        "own-session-test",
        SignerRole::ProjectAuthor,
        SignerScope::authoring_default(),
        status,
    )
}

#[tokio::test]
async fn a_session_of_a_retired_key_reads_nothing_it_launched() {
    let (_tmp, _owner, mut state) = test_state::build_test_state();
    let retired = lillux::crypto::SigningKey::from_bytes(&[2u8; 32]);
    let successor = lillux::crypto::SigningKey::from_bytes(&[3u8; 32]);
    let retired_fp = compute_fingerprint(&retired.verifying_key());
    let successor_fp = compute_fingerprint(&successor.verifying_key());
    let succeeded = TrustEvent {
        schema: "x".into(),
        seq: 1,
        previous: None,
        kind: TrustEventKind::Succeeded,
        signer: successor_fp.clone(),
        record_digest: String::new(),
        request_digest: String::new(),
        operator: String::new(),
        predecessor: Some(retired_fp.clone()),
    };
    let trust = TrustStore::from_registry(
        lillux::crypto::SigningKey::from_bytes(&[1u8; 32]).verifying_key(),
        vec![
            record(&retired, SignerStatus::Retired),
            record(&successor, SignerStatus::Active),
        ],
    )
    .unwrap()
    .with_succession_chain(std::slice::from_ref(&succeeded));
    state.engine = Arc::new((*state.engine).clone().with_trust_store(trust));

    let (old, old_attachment) = session_for(&state, &format!("fp:{retired_fp}"));
    let (now, now_attachment) = session_for(&state, &format!("fp:{successor_fp}"));
    create_thread(&state, "T-old-seat", &format!("fp:{retired_fp}"));
    create_thread(&state, "T-now-seat", &format!("fp:{successor_fp}"));
    let state = Arc::new(state);

    let read = inspect(&state, &now, now_attachment, "T-now-seat").await;
    assert!(
        read.is_ok(),
        "the active key's session reads its seat thread: {read:?}"
    );
    let read = inspect(&state, &old, old_attachment, "T-old-seat").await;
    assert!(is_not_found(&read), "the retired key's session: {read:?}");
}
