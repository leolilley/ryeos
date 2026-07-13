// Tests for `ui.invocations.dispatch` handler.

mod test_state;
use test_state::build_test_state;

use ryeos_app::handler_context::HandlerContext;
use ryeos_ui::browser_session::LaunchContext;
use ryeos_ui::state::get_ui_state;
use std::sync::Arc;

fn test_context() -> LaunchContext {
    LaunchContext {
        surface_ref: "surface:ryeos/ui/base".into(),
        project_path: None,
        read_only: false,
        granted_caps: vec!["ui.read".into()],
        user_principal_id: None,
    }
}

fn read_only_context() -> LaunchContext {
    LaunchContext {
        surface_ref: "surface:ryeos/ui/base".into(),
        project_path: None,
        read_only: true,
        granted_caps: vec!["ui.read".into()],
        user_principal_id: None,
    }
}

#[tokio::test]
async fn arbitrary_event_targets_are_rejected() {
    let (_tmp, state) = build_test_state();
    let (session_id, _token) = get_ui_state(&state)
        .unwrap()
        .browser_sessions
        .mint_token(test_context());

    let ctx = HandlerContext::new(
        format!("session:{session_id}"),
        vec!["ui.read".into()],
        false,
    );

    let result = (ryeos_ui::handlers::ui_invocations_dispatch::DESCRIPTOR.handler)(
        serde_json::json!({
            "target": { "kind": "ref", "ref": "custom.event" },
            "params": { "key": "val" }
        }),
        ctx,
        Arc::new(state.clone()),
    )
    .await;

    assert!(result.is_err(), "arbitrary UI event dispatch should fail");
    let msg = format!("{:#}", result.unwrap_err());
    assert!(
        msg.contains("canonical ref"),
        "expected canonical ref message, got: {msg}"
    );
}

#[tokio::test]
async fn read_only_session_rejects_nonlocal_invocation() {
    let (_tmp, state) = build_test_state();
    let (session_id, _token) = get_ui_state(&state)
        .unwrap()
        .browser_sessions
        .mint_token(read_only_context());

    let ctx = HandlerContext::new(
        format!("session:{session_id}"),
        vec!["ui.read".into()],
        false,
    );

    let result = (ryeos_ui::handlers::ui_invocations_dispatch::DESCRIPTOR.handler)(
        serde_json::json!({
            "target": { "kind": "ref", "ref": "service:commands/submit" },
        }),
        ctx,
        Arc::new(state),
    )
    .await;

    assert!(
        result.is_err(),
        "read-only should reject nonlocal invocations"
    );
    let msg = format!("{:#}", result.unwrap_err());
    assert!(
        msg.contains("read-only"),
        "expected read-only mention, got: {msg}"
    );
}

#[tokio::test]
async fn session_cookie_required() {
    let (_tmp, state) = build_test_state();

    let ctx = HandlerContext::anonymous();

    let result = (ryeos_ui::handlers::ui_invocations_dispatch::DESCRIPTOR.handler)(
        serde_json::json!({
            "target": { "kind": "ref", "ref": "service:ui/seat/close" },
            "params": { "thread_id": "T-1" }
        }),
        ctx,
        Arc::new(state),
    )
    .await;

    assert!(result.is_err(), "anonymous should be rejected");
    let msg = format!("{:#}", result.unwrap_err());
    assert!(
        msg.contains("session"),
        "expected session mention, got: {msg}"
    );
}

#[tokio::test]
async fn session_local_invocation_publishes_to_session_bus() {
    let (_tmp, state) = build_test_state();
    let (session_id, _token) = get_ui_state(&state)
        .unwrap()
        .browser_sessions
        .mint_token(test_context());

    // Subscribe to the session bus before invoking.
    let mut rx = get_ui_state(&state)
        .unwrap()
        .session_bus
        .subscribe(&session_id);

    let ctx = HandlerContext::new(
        format!("session:{session_id}"),
        vec!["ui.read".into()],
        false,
    );

    let result = (ryeos_ui::handlers::ui_invocations_dispatch::DESCRIPTOR.handler)(
        serde_json::json!({
            "target": { "kind": "ref", "ref": "service:ui/seat/close" },
            "params": { "thread_id": "T-1" }
        }),
        ctx,
        Arc::new(state),
    )
    .await
    .expect("should succeed");

    let invocation_id = result["invocation_id"].as_str().unwrap();

    // The session bus should have received the event.
    let event = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
        .await
        .expect("timeout")
        .expect("recv error");

    assert_eq!(event.event_type, "invocation.dispatched");
    assert_eq!(event.payload["target"]["kind"], "ref");
    assert_eq!(event.payload["target"]["ref"], "service:ui/seat/close");
    assert_eq!(event.payload["invocation_id"], invocation_id);
}
