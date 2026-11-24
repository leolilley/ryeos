// Two principals on one checkout. A path is not authority over threads: a
// seat of principal B bound to A's checkout reads none of A's threads through
// a thread read route, whether A's thread states A's project identity or
// none, and B still reads the thread it launched there itself.

use std::sync::Arc;

use serde_json::json;

use ryeos_app::handler_context::HandlerContext;
use ryeos_app::state::AppState;
use ryeos_app::state_store::NewThreadRecord;
use ryeos_state::objects::{
    CapturedItemTrustClass, CapturedNodeHistoryPolicyProvenance, CapturedPolicyProvenance,
    CapturedThreadHistoryPolicy, EnvironmentAuthority, ExecutionProjectAuthority,
    LiveFilesystemConfinement, LiveProjectAccess, ThreadHistoryRetention,
};
use ryeos_state::project_identity::{LogicalProject, ProjectIdentity};
use ryeos_ui::state::get_ui_state;
use ryeos_ui::test_support as test_state;

/// Two keys with no registry record: each acts for its own solo principal.
const ALPHA: &str = "fp:a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
const BETA: &str = "fp:b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2";

fn principal_of(key: &str) -> ryeos_state::principal_id::PrincipalId {
    ryeos_engine::trust::SuccessionGraph::default()
        .ownership_of(key.strip_prefix("fp:").unwrap())
        .unwrap()
        .principal
}

fn captured_policy() -> CapturedThreadHistoryPolicy {
    let hash = "a".repeat(64);
    CapturedThreadHistoryPolicy {
        retention: ThreadHistoryRetention::Durable,
        canonical_item_ref: "directive:test/two-principals".to_string(),
        item_content_hash: hash.clone(),
        item_signer_fingerprint: Some(hash.clone()),
        item_trust_class: CapturedItemTrustClass::Trusted,
        kind_schema_content_hash: hash,
        resolved_from: CapturedPolicyProvenance::NodeDefault {
            node_policy: CapturedNodeHistoryPolicyProvenance::test_policy(),
        },
    }
}

/// A live thread `requested_by` launched in `root`, sealing `signer_project`.
fn create_thread(
    state: &AppState,
    thread_id: &str,
    requested_by: &str,
    root: &std::path::Path,
    signer_project: LogicalProject,
) {
    let root = root.canonicalize().expect("canonical fixture checkout");
    let mut project_authority = ExecutionProjectAuthority::live(
        root.clone(),
        format!("local:{}", root.display()),
        LiveProjectAccess::ReadOnly,
        LiveFilesystemConfinement::standard_fixed_parents(),
        EnvironmentAuthority::None,
        Vec::new(),
    )
    .expect("live fixture project authority");
    if let ExecutionProjectAuthority::LiveProject {
        signer_project: sealed,
        ..
    } = &mut project_authority
    {
        *sealed = signer_project;
    }
    state
        .state_store
        .create_thread_for_test(&NewThreadRecord {
            thread_id: thread_id.to_string(),
            chain_root_id: thread_id.to_string(),
            kind: "directive".to_string(),
            item_ref: "directive:test/two-principals".to_string(),
            executor_ref: "test/executor".to_string(),
            launch_mode: "wait".to_string(),
            current_site_id: "site:test".to_string(),
            origin_site_id: "site:test".to_string(),
            upstream_thread_id: None,
            requested_by: Some(requested_by.to_string()),
            project_root: Some(root),
            project_authority,
            base_project_snapshot_hash: None,
            usage_subject: None,
            usage_subject_asserted_by: None,
            captured_history_policy: Some(captured_policy()),
        })
        .expect("create fixture thread");
    state
        .state_store
        .mark_thread_running(thread_id, None)
        .expect("mark fixture thread running");
}

/// A seat of `principal` bound to `root`. The binding seals no project
/// identity: `principal` registered nothing there.
fn seat_on(
    state: &AppState,
    principal: &str,
    root: &std::path::Path,
) -> Arc<ryeos_ui::browser_session::AdmittedBindingAttachment> {
    let root = root.canonicalize().expect("canonical seat checkout");
    let mut launch = test_state::launch_context(
        "surface:ryeos/ui/base",
        Some(root.to_string_lossy().as_ref()),
        ryeos_ui::compiled_binding::EffectiveUiPosture::ObservationOnly,
        None,
    );
    Arc::make_mut(&mut launch.compiled_binding)
        .binding
        .principal_id = principal.to_string();
    launch.project_authority = Some(Arc::new(
        lillux::PinnedDirectory::open(&root)
            .expect("open seat checkout")
            .expect("seat checkout exists"),
    ));
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
    session.attachments[&session.root_attachment_id].clone()
}

async fn inspect(
    state: &Arc<AppState>,
    attachment: Arc<ryeos_ui::browser_session::AdmittedBindingAttachment>,
    thread_id: &str,
) -> anyhow::Result<serde_json::Value> {
    let ctx = HandlerContext::new(
        "fp:compiled-source".to_string(),
        vec!["ui.read".into()],
        true,
    );
    ryeos_ui::seat_auth::with_compiled_ui_attachment(
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
async fn another_principals_seat_on_the_same_checkout_reads_none_of_its_threads() {
    let (_tmp, _owner, state) = test_state::build_test_state();
    let checkout = tempfile::TempDir::new().unwrap();
    let alpha_project = ProjectIdentity::new(principal_of(ALPHA), "alpha".parse().unwrap());
    create_thread(
        &state,
        "T-alpha-stated",
        ALPHA,
        checkout.path(),
        LogicalProject::Stated(alpha_project),
    );
    create_thread(
        &state,
        "T-alpha-unstated",
        ALPHA,
        checkout.path(),
        LogicalProject::Absent(ryeos_state::project_identity::LogicalProjectAbsence::NotRegistered),
    );
    create_thread(
        &state,
        "T-beta",
        BETA,
        checkout.path(),
        LogicalProject::Absent(ryeos_state::project_identity::LogicalProjectAbsence::NotRegistered),
    );
    let beta_seat = seat_on(&state, BETA, checkout.path());
    let alpha_seat = seat_on(&state, ALPHA, checkout.path());
    let state = Arc::new(state);

    for thread in ["T-alpha-stated", "T-alpha-unstated"] {
        let read = inspect(&state, beta_seat.clone(), thread).await;
        assert!(
            is_not_found(&read),
            "{thread} must be NotFound to B: {read:?}"
        );
    }
    let own = inspect(&state, beta_seat, "T-beta").await;
    assert!(own.is_ok(), "B reads its own launch: {own:?}");

    // A's unstated launch is A's, from A's seat on the same checkout; B's is
    // not.
    let alpha_own = inspect(&state, alpha_seat.clone(), "T-alpha-unstated").await;
    assert!(alpha_own.is_ok(), "A reads its own launch: {alpha_own:?}");
    let read = inspect(&state, alpha_seat, "T-beta").await;
    assert!(
        is_not_found(&read),
        "T-beta must be NotFound to A: {read:?}"
    );
}
