mod test_state;
use test_state::build_test_state_with_live_bundles;

use ryeos_app::handler_context::HandlerContext;
use ryeos_ui::browser_session::LaunchContext;
use ryeos_ui::state::get_ui_state;
use std::sync::Arc;

fn session_context(user_principal_id: Option<String>) -> LaunchContext {
    LaunchContext {
        surface_ref: "surface:ryeos/ui/base".into(),
        project_path: None,
        read_only: true,
        granted_caps: vec!["ui.read".into()],
        user_principal_id,
    }
}

fn handler_context(session_id: &str) -> HandlerContext {
    HandlerContext::new(
        format!("session:{session_id}"),
        vec!["ui.read".into()],
        false,
    )
}

#[tokio::test]
async fn ui_seat_open_reattaches_running_session_seat() {
    let (_tmp, state) = build_test_state_with_live_bundles();
    let (session_id, _token) = get_ui_state(&state)
        .unwrap()
        .browser_sessions
        .mint_token(session_context(Some("fp:user-1".into())));
    let ctx = handler_context(&session_id);

    let first = (ryeos_ui::handlers::ui_seat::OPEN_DESCRIPTOR.handler)(
        serde_json::json!({ "surface_ref": "surface:ryeos/ui/base" }),
        ctx.clone(),
        Arc::new(state.clone()),
    )
    .await
    .expect("open seat");
    let second = (ryeos_ui::handlers::ui_seat::OPEN_DESCRIPTOR.handler)(
        serde_json::json!({ "surface_ref": "surface:ryeos/ui/base" }),
        ctx,
        Arc::new(state.clone()),
    )
    .await
    .expect("reattach seat");

    assert_eq!(first["thread_id"], second["thread_id"]);
    assert_eq!(first["reattached"], false);
    assert_eq!(second["reattached"], true);

    let detail = state
        .state_store
        .get_thread(first["thread_id"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(detail.kind, "seat_session");
    assert_eq!(detail.requested_by.as_deref(), Some("fp:user-1"));
}

#[tokio::test]
async fn ui_seat_append_replay_and_close_round_trip() {
    let (_tmp, state) = build_test_state_with_live_bundles();
    let (session_id, _token) = get_ui_state(&state)
        .unwrap()
        .browser_sessions
        .mint_token(session_context(None));
    let ctx = handler_context(&session_id);
    let state = Arc::new(state);

    let opened = (ryeos_ui::handlers::ui_seat::OPEN_DESCRIPTOR.handler)(
        serde_json::json!({ "surface_ref": "surface:ryeos/ui/base" }),
        ctx.clone(),
        state.clone(),
    )
    .await
    .expect("open seat");
    let thread_id = opened["thread_id"].as_str().unwrap();

    let appended = (ryeos_ui::handlers::ui_seat::APPEND_DESCRIPTOR.handler)(
        serde_json::json!({
            "thread_id": thread_id,
            "events": [{
                "event_type": "seat.facet",
                "payload": { "seq": 0, "payload": { "key": "selection", "value": { "item": "T-1" } } }
            }]
        }),
        ctx.clone(),
        state.clone(),
    )
    .await
    .expect("append seat event");
    assert_eq!(appended["appended"], 1);

    let replay = (ryeos_ui::handlers::ui_seat::REPLAY_DESCRIPTOR.handler)(
        serde_json::json!({ "chain_root_id": thread_id }),
        ctx.clone(),
        state.clone(),
    )
    .await
    .expect("replay seat");
    assert_eq!(replay["events"].as_array().unwrap().len(), 1);
    assert_eq!(replay["events"][0]["event_type"], "seat.facet");
    assert_eq!(
        replay["events"][0]["payload"]["payload"]["key"],
        "selection"
    );

    let closed = (ryeos_ui::handlers::ui_seat::CLOSE_DESCRIPTOR.handler)(
        serde_json::json!({ "thread_id": thread_id }),
        ctx,
        state.clone(),
    )
    .await
    .expect("close seat");
    assert_eq!(closed["status"], "completed");
}

#[tokio::test]
async fn read_only_actions_allow_session_local_seat_services() {
    let (_tmp, state) = build_test_state_with_live_bundles();
    let (session_id, _token) = get_ui_state(&state)
        .unwrap()
        .browser_sessions
        .mint_token(session_context(None));
    let ctx = handler_context(&session_id);

    let result = (ryeos_ui::handlers::ui_invocations_dispatch::DESCRIPTOR.handler)(
        serde_json::json!({
            "target": { "kind": "ref", "ref": "service:ui/seat/open" },
            "params": { "surface_ref": "surface:ryeos/ui/base" }
        }),
        ctx,
        Arc::new(state),
    )
    .await
    .expect("read-only session may open local UI seat");

    assert_eq!(result["status"], "executed");
    assert!(result["result"]["thread_id"].is_string());
}
