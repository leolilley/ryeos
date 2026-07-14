// Tests for `ui.intents.apply` handler.

mod test_state;
use test_state::build_test_state;

use ryeos_app::handler_context::HandlerContext;
use ryeos_ui::browser_session::LaunchContext;
use ryeos_ui::state::get_ui_state;
use std::sync::Arc;

fn test_context(read_only: bool) -> LaunchContext {
    LaunchContext {
        surface_ref: "surface:ryeos/ui/base".into(),
        project_path: None,
        read_only,
        granted_caps: vec!["ui.read".into()],
        user_principal_id: None,
    }
}

#[tokio::test]
async fn browser_session_applies_intent_to_self() {
    let (_tmp, state) = build_test_state();
    let (session_id, _token) = get_ui_state(&state)
        .unwrap()
        .browser_sessions
        .mint_token(test_context(false));
    let mut rx = get_ui_state(&state)
        .unwrap()
        .session_bus
        .subscribe(&session_id);

    let ctx = HandlerContext::new(
        format!("session:{session_id}"),
        vec!["ui.read".into()],
        false,
    );

    let result = (ryeos_ui::handlers::ui_intents_apply::DESCRIPTOR.handler)(
        serde_json::json!({
            "intent": "open_overlay",
            "payload": { "overlay_id": "views", "query": "threads" },
            "request_id": "req-1"
        }),
        ctx,
        Arc::new(state),
    )
    .await
    .expect("intent should apply");

    assert_eq!(result["status"], "applied");
    assert_eq!(result["session_id"], session_id);
    assert_eq!(result["intent"], "open_overlay");

    let event = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
        .await
        .expect("timeout")
        .expect("recv error");
    assert_eq!(event.event_type, "ui_intent.applied");
    assert_eq!(event.payload["session_id"], session_id);
    assert_eq!(event.payload["intent"], "open_overlay");
    assert_eq!(event.payload["payload"]["overlay_id"], "views");
    assert_eq!(event.payload["payload"]["query"], "threads");
    assert_eq!(event.payload["request_id"], "req-1");
}

#[tokio::test]
async fn read_only_browser_session_can_apply_safe_local_intent() {
    let (_tmp, state) = build_test_state();
    let (session_id, _token) = get_ui_state(&state)
        .unwrap()
        .browser_sessions
        .mint_token(test_context(true));

    let ctx = HandlerContext::new(
        format!("session:{session_id}"),
        vec!["ui.read".into()],
        false,
    );

    let result = (ryeos_ui::handlers::ui_intents_apply::DESCRIPTOR.handler)(
        serde_json::json!({
            "intent": "focus_input",
            "payload": {}
        }),
        ctx,
        Arc::new(state),
    )
    .await
    .expect("read-only UI-local intent should apply");

    assert_eq!(result["status"], "applied");
    assert_eq!(result["intent"], "focus_input");
}

#[tokio::test]
async fn browser_session_cannot_target_other_session() {
    let (_tmp, state) = build_test_state();
    let (session_id, _token) = get_ui_state(&state)
        .unwrap()
        .browser_sessions
        .mint_token(test_context(false));
    let (other_session_id, _other_token) = get_ui_state(&state)
        .unwrap()
        .browser_sessions
        .mint_token(test_context(false));

    let ctx = HandlerContext::new(
        format!("session:{session_id}"),
        vec!["ui.read".into()],
        false,
    );

    let result = (ryeos_ui::handlers::ui_intents_apply::DESCRIPTOR.handler)(
        serde_json::json!({
            "intent": "focus_input",
            "payload": {},
            "target_session_id": other_session_id
        }),
        ctx,
        Arc::new(state),
    )
    .await;

    assert!(result.is_err(), "cross-session browser control should fail");
    let msg = format!("{:#}", result.unwrap_err());
    assert!(msg.contains("caller session"), "got: {msg}");
}

#[tokio::test]
async fn signed_caller_requires_ui_session_control_capability() {
    let (_tmp, state) = build_test_state();
    let (session_id, _token) = get_ui_state(&state)
        .unwrap()
        .browser_sessions
        .mint_token(test_context(false));

    let ctx = HandlerContext::new("fp:operator".into(), vec![], true);

    let result = (ryeos_ui::handlers::ui_intents_apply::DESCRIPTOR.handler)(
        serde_json::json!({
            "intent": "focus_input",
            "payload": {},
            "target_session_id": session_id
        }),
        ctx,
        Arc::new(state),
    )
    .await;

    assert!(result.is_err(), "missing capability should fail");
    let msg = format!("{:#}", result.unwrap_err());
    assert!(
        msg.contains("ryeos.execute.service.ui/intents/apply"),
        "got: {msg}"
    );
}

#[tokio::test]
async fn signed_caller_with_capability_can_apply_to_target_session() {
    let (_tmp, state) = build_test_state();
    let (session_id, _token) = get_ui_state(&state)
        .unwrap()
        .browser_sessions
        .mint_token(test_context(false));

    let ctx = HandlerContext::new(
        "fp:operator".into(),
        vec!["ryeos.execute.service.ui/intents/apply".into()],
        true,
    );

    let result = (ryeos_ui::handlers::ui_intents_apply::DESCRIPTOR.handler)(
        serde_json::json!({
            "intent": "focus_input",
            "payload": {},
            "target_session_id": session_id
        }),
        ctx,
        Arc::new(state),
    )
    .await
    .expect("signed caller with capability should apply");

    assert_eq!(result["status"], "applied");
    assert_eq!(result["session_id"], session_id);
}

#[tokio::test]
async fn route_rejects_executable_fields() {
    let (_tmp, state) = build_test_state();
    let (session_id, _token) = get_ui_state(&state)
        .unwrap()
        .browser_sessions
        .mint_token(test_context(false));
    let ctx = HandlerContext::new(
        format!("session:{session_id}"),
        vec!["ui.read".into()],
        false,
    );

    let result = (ryeos_ui::handlers::ui_intents_apply::DESCRIPTOR.handler)(
        serde_json::json!({
            "intent": "focus_input",
            "payload": {},
            "command_id": "service:commands/submit"
        }),
        ctx,
        Arc::new(state),
    )
    .await;

    assert!(result.is_err(), "executable fields must be rejected");
    let msg = format!("{:#}", result.unwrap_err());
    assert!(msg.contains("unknown field `command_id`"), "got: {msg}");
}

#[tokio::test]
async fn route_rejects_service_ref_as_open_view() {
    let (_tmp, state) = build_test_state();
    let (session_id, _token) = get_ui_state(&state)
        .unwrap()
        .browser_sessions
        .mint_token(test_context(false));
    let ctx = HandlerContext::new(
        format!("session:{session_id}"),
        vec!["ui.read".into()],
        false,
    );

    let result = (ryeos_ui::handlers::ui_intents_apply::DESCRIPTOR.handler)(
        serde_json::json!({
            "intent": "open_view",
            "payload": { "view_ref": "service:commands/submit" }
        }),
        ctx,
        Arc::new(state),
    )
    .await;

    assert!(result.is_err(), "service refs must not pass as view refs");
    let msg = format!("{:#}", result.unwrap_err());
    assert!(msg.contains("view ref"), "got: {msg}");
}
