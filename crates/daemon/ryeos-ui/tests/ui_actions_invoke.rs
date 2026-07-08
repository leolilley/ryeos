// Tests for `ui.actions.invoke` handler.

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
async fn unknown_command_dispatched_to_session_bus() {
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

    let result = (ryeos_ui::handlers::ui_actions_invoke::DESCRIPTOR.handler)(
        serde_json::json!({
            "command_id": "custom.action",
            "args": { "key": "val" }
        }),
        ctx,
        Arc::new(state.clone()),
    )
    .await
    .expect("should succeed");

    assert_eq!(result["status"], "dispatched");
    assert_eq!(result["command_id"], "custom.action");
    assert!(result["invocation_id"].is_string());
}

#[tokio::test]
async fn read_only_session_rejects_action() {
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

    let result = (ryeos_ui::handlers::ui_actions_invoke::DESCRIPTOR.handler)(
        serde_json::json!({
            "command_id": "some.action",
        }),
        ctx,
        Arc::new(state),
    )
    .await;

    assert!(result.is_err(), "read-only should reject actions");
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

    let result = (ryeos_ui::handlers::ui_actions_invoke::DESCRIPTOR.handler)(
        serde_json::json!({
            "command_id": "some.action",
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
async fn action_publishes_to_session_bus() {
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

    let result = (ryeos_ui::handlers::ui_actions_invoke::DESCRIPTOR.handler)(
        serde_json::json!({
            "command_id": "test.action",
            "args": { "foo": "bar" }
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

    assert_eq!(event.event_type, "action.invoked");
    assert_eq!(event.payload["command_id"], "test.action");
    assert_eq!(event.payload["invocation_id"], invocation_id);
    assert_eq!(event.payload["args"]["foo"], "bar");
}
