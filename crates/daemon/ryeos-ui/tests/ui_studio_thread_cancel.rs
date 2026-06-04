mod test_state;

use ryeos_app::handler_context::HandlerContext;
use ryeos_app::state_store::NewThreadRecord;
use ryeos_ui::browser_session::LaunchContext;
use ryeos_ui::state::get_ui_state;
use std::sync::Arc;

const PRINCIPAL: &str = "fp:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OTHER_PRINCIPAL: &str = "fp:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn launch_context(read_only: bool, user_principal_id: Option<String>) -> LaunchContext {
    LaunchContext {
        surface_ref: "surface:ryeos/studio/base".into(),
        project_path: None,
        read_only,
        granted_caps: vec!["ui.read".into()],
        user_principal_id,
    }
}

fn browser_context(session_id: &str) -> HandlerContext {
    HandlerContext::new(
        format!("session:{session_id}"),
        vec!["ui.read".into()],
        false,
    )
}

fn create_thread(state: &ryeos_app::state::AppState, thread_id: &str, requested_by: &str) {
    state
        .state_store
        .create_thread(&NewThreadRecord {
            thread_id: thread_id.to_string(),
            chain_root_id: thread_id.to_string(),
            kind: "tool".to_string(),
            item_ref: "tool:demo/run".to_string(),
            executor_ref: "test/executor".to_string(),
            launch_mode: "inline".to_string(),
            current_site_id: "site:test".to_string(),
            origin_site_id: "site:test".to_string(),
            upstream_thread_id: None,
            requested_by: Some(requested_by.to_string()),
        })
        .expect("create thread");
}

#[tokio::test]
async fn studio_thread_cancel_delegates_to_real_cancel_service() {
    let (_tmp, state) = test_state::build_test_state();
    create_thread(&state, "T-cancel-studio", PRINCIPAL);
    let (session_id, _token) = get_ui_state(&state)
        .unwrap()
        .browser_sessions
        .mint_token(launch_context(false, Some(PRINCIPAL.to_string())));

    let result = (ryeos_ui::handlers::ui_studio_threads::CANCEL_DESCRIPTOR.handler)(
        serde_json::json!({ "thread_id": "T-cancel-studio" }),
        browser_context(&session_id),
        Arc::new(state.clone()),
    )
    .await
    .expect("cancel should succeed");

    assert_eq!(result["thread_id"], "T-cancel-studio");
    assert_eq!(result["status"], "cancelled");
    let thread = state
        .state_store
        .get_thread("T-cancel-studio")
        .expect("get thread")
        .expect("thread exists");
    assert_eq!(thread.status, "cancelled");
}

#[tokio::test]
async fn studio_thread_cancel_requires_writable_verified_session_principal() {
    let (_tmp, state) = test_state::build_test_state();
    create_thread(&state, "T-cancel-denied", PRINCIPAL);
    let (read_only_session, _token) = get_ui_state(&state)
        .unwrap()
        .browser_sessions
        .mint_token(launch_context(true, Some(PRINCIPAL.to_string())));
    let (missing_principal_session, _token) = get_ui_state(&state)
        .unwrap()
        .browser_sessions
        .mint_token(launch_context(false, None));

    let read_only = (ryeos_ui::handlers::ui_studio_threads::CANCEL_DESCRIPTOR.handler)(
        serde_json::json!({ "thread_id": "T-cancel-denied" }),
        browser_context(&read_only_session),
        Arc::new(state.clone()),
    )
    .await;
    assert!(read_only.is_err(), "read-only session should reject cancel");

    let missing_principal = (ryeos_ui::handlers::ui_studio_threads::CANCEL_DESCRIPTOR.handler)(
        serde_json::json!({ "thread_id": "T-cancel-denied" }),
        browser_context(&missing_principal_session),
        Arc::new(state.clone()),
    )
    .await;
    assert!(
        missing_principal.is_err(),
        "session without verified principal should reject cancel"
    );

    let thread = state
        .state_store
        .get_thread("T-cancel-denied")
        .expect("get thread")
        .expect("thread exists");
    assert_eq!(thread.status, "created");
}

#[tokio::test]
async fn studio_thread_cancel_rejects_cross_owner_thread() {
    let (_tmp, state) = test_state::build_test_state();
    create_thread(&state, "T-cancel-cross-owner", OTHER_PRINCIPAL);
    let (session_id, _token) = get_ui_state(&state)
        .unwrap()
        .browser_sessions
        .mint_token(launch_context(false, Some(PRINCIPAL.to_string())));

    let result = (ryeos_ui::handlers::ui_studio_threads::CANCEL_DESCRIPTOR.handler)(
        serde_json::json!({ "thread_id": "T-cancel-cross-owner" }),
        browser_context(&session_id),
        Arc::new(state.clone()),
    )
    .await;

    assert!(result.is_err(), "cross-owner cancel should reject");
    let thread = state
        .state_store
        .get_thread("T-cancel-cross-owner")
        .expect("get thread")
        .expect("thread exists");
    assert_eq!(thread.status, "created");
}
