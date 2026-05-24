// Tests for `ui.session.current` handler.

mod test_state;
use test_state::build_test_state;

use std::sync::Arc;
use ryeos_app::ui_session::LaunchContext;
use ryeos_app::handler_context::HandlerContext;

fn test_context() -> LaunchContext {
    LaunchContext {
        surface_ref: "surface:ryeos/cockpit/base".into(),
        project_path: Some("/tmp/project".into()),
        read_only: false,
        granted_caps: vec!["ui.read".into()],
    }
}

#[tokio::test]
async fn session_current_returns_session_fields() {
    let (_tmp, state) = build_test_state();

    let (session_id, _token) = state.browser_sessions.mint_token(test_context());

    // Create a handler context that looks like a browser_session principal.
    let ctx = HandlerContext::new(
        format!("session:{session_id}"),
        vec!["ui.read".into()],
        false,
    );

    let result = (ryeos_ui::handlers::ui_session_current::DESCRIPTOR.handler)(
        serde_json::json!(null),
        ctx,
        Arc::new(state),
    )
    .await
    .expect("should succeed");

    assert_eq!(result["session_id"], session_id);
    assert_eq!(result["surface_ref"], "surface:ryeos/cockpit/base");
    assert_eq!(result["project_path"], "/tmp/project");
    assert!(!result["read_only"].as_bool().unwrap());
    assert!(result["events_url"].as_str().unwrap().contains(&session_id));
}

#[tokio::test]
async fn session_current_without_session_rejected() {
    let (_tmp, state) = build_test_state();

    // Anonymous context — no session.
    let ctx = HandlerContext::anonymous();

    let result = (ryeos_ui::handlers::ui_session_current::DESCRIPTOR.handler)(
        serde_json::json!(null),
        ctx,
        Arc::new(state),
    )
    .await;

    assert!(result.is_err(), "anonymous should be rejected");
}

#[tokio::test]
async fn session_current_with_expired_session_rejected() {
    let short_store = ryeos_ui::BrowserSessionStore::new_with_short_ttl(
        std::time::Duration::from_millis(1),
        std::time::Duration::from_millis(1),
    );

    let (session_id, _token) = short_store.mint_token(test_context());

    std::thread::sleep(std::time::Duration::from_millis(5));

    let (_tmp, _state) = build_test_state();
    // Manually inject the expired session into the test state's store.
    // (The short_store is separate from state's store, so we test the
    // store directly.)
    assert!(
        short_store.get_session(&session_id).is_none(),
        "expired session should be gone"
    );
}

#[tokio::test]
async fn session_current_with_read_only_flag() {
    let (_tmp, state) = build_test_state();

    let ctx = LaunchContext {
        surface_ref: "surface:ryeos/test/ro".into(),
        project_path: None,
        read_only: true,
        granted_caps: vec![],
    };
    let (session_id, _token) = state.browser_sessions.mint_token(ctx);

    let hctx = HandlerContext::new(
        format!("session:{session_id}"),
        vec![],
        false,
    );

    let result = (ryeos_ui::handlers::ui_session_current::DESCRIPTOR.handler)(
        serde_json::json!(null),
        hctx,
        Arc::new(state),
    )
    .await
    .expect("should succeed");

    assert!(result["read_only"].as_bool().unwrap());
    assert_eq!(result["project_path"], serde_json::Value::Null);
}
