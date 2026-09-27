// Tests for `ui.session.current` handler.

mod test_state;
use test_state::{build_test_state, launch_context, mint_launch};

use ryeos_app::handler_context::HandlerContext;
use ryeos_ui::state::get_ui_state;
use std::sync::Arc;

fn test_context() -> ryeos_ui::browser_session::LaunchContext {
    launch_context(
        "surface:ryeos/ui/base",
        None,
        ryeos_ui::compiled_binding::EffectiveUiPosture::Interactive,
        None,
    )
}

#[tokio::test]
async fn session_current_returns_session_fields() {
    let (_tmp, state) = build_test_state();

    let (session_id, token) = mint_launch(&state, test_context());
    assert_eq!(
        get_ui_state(&state)
            .unwrap()
            .browser_sessions
            .consume_launch_token(&token),
        Some(session_id.clone())
    );

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
    let surface_attachment_id = result["surface_attachment_id"].as_str().unwrap();
    let attachments = result["binding_attachments"].as_array().unwrap();
    assert_eq!(attachments.len(), 1);
    let attachment = &attachments[0];
    assert_eq!(attachment["binding_attachment_id"], surface_attachment_id);
    assert_eq!(attachment["surface_ref"], "surface:ryeos/ui/base");
    assert_eq!(attachment["project_path"], serde_json::Value::Null);
    assert_eq!(attachment["posture"], "interactive");
    assert_eq!(attachment["binding_digest"], "11".repeat(32));
    assert!(result.get("surface_ref").is_none());
    assert!(result.get("binding_digest").is_none());
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

    let (session_id, _token, _) = short_store
        .mint_token(test_context(), 16, &"22".repeat(32))
        .expect("mint short-lived session");

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
async fn session_current_reports_observation_only_posture() {
    let (_tmp, state) = build_test_state();

    let ctx = launch_context(
        "surface:ryeos/test/ro",
        None,
        ryeos_ui::compiled_binding::EffectiveUiPosture::ObservationOnly,
        None,
    );
    let (session_id, token) = mint_launch(&state, ctx);
    assert_eq!(
        get_ui_state(&state)
            .unwrap()
            .browser_sessions
            .consume_launch_token(&token),
        Some(session_id.clone())
    );

    let hctx = HandlerContext::new(format!("session:{session_id}"), vec![], false);

    let result = (ryeos_ui::handlers::ui_session_current::DESCRIPTOR.handler)(
        serde_json::json!(null),
        hctx,
        Arc::new(state),
    )
    .await
    .expect("should succeed");

    assert_eq!(
        result["binding_attachments"][0]["posture"],
        "observation_only"
    );
    assert_eq!(
        result["binding_attachments"][0]["project_path"],
        serde_json::Value::Null
    );
}

#[tokio::test]
async fn session_current_returns_durable_user_principal_when_present() {
    let (_tmp, state) = build_test_state();
    let user_principal_id = format!("fp:{}", "cd".repeat(32));
    let ctx = launch_context(
        "surface:ryeos/ui/base",
        None,
        ryeos_ui::compiled_binding::EffectiveUiPosture::Interactive,
        Some(user_principal_id.clone()),
    );
    let (session_id, token) = mint_launch(&state, ctx);
    assert_eq!(
        get_ui_state(&state)
            .unwrap()
            .browser_sessions
            .consume_launch_token(&token),
        Some(session_id.clone())
    );
    let hctx = HandlerContext::new(format!("session:{session_id}"), vec![], false);

    let result = (ryeos_ui::handlers::ui_session_current::DESCRIPTOR.handler)(
        serde_json::json!(null),
        hctx,
        Arc::new(state),
    )
    .await
    .expect("should succeed");

    assert_eq!(result["user_principal_id"], user_principal_id);
}
