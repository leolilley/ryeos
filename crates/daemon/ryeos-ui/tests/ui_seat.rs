mod test_state;
use test_state::{build_test_state_with_live_bundles, launch_context};

use ryeos_app::handler_context::HandlerContext;
use ryeos_ui::state::get_ui_state;
use std::sync::Arc;

// Exercise the same verifier + canonical service invoker used by the signed
// HTTP routes. Calling the handler alone misses dispatch-class mismatches.
async fn invoke_seat_route(
    descriptor: &ryeos_api::registry::ServiceDescriptor,
    input: serde_json::Value,
    context: HandlerContext,
    state: Arc<ryeos_app::state::AppState>,
) -> anyhow::Result<serde_json::Value> {
    use ryeos_api::routes::invocation::{
        CompiledRouteInvocation, RouteInvocationContext, RouteInvocationResult,
    };
    let mut headers = axum::http::HeaderMap::new();
    let session_id = context.fingerprint.strip_prefix("session:").unwrap();
    headers.insert("cookie", format!("ryeos_session={session_id}").parse()?);
    let mut invocation = RouteInvocationContext {
        route_id: descriptor.service_ref.into(),
        method: axum::http::Method::POST,
        uri: "/ui/api/session/seat/open".parse()?,
        captures: Default::default(),
        headers,
        body_raw: Vec::new(),
        input,
        principal: None,
        workspace_lifeline: None,
        launch_timings: None,
        state: state.as_ref().clone(),
        webhook_dedupe: Arc::new(Default::default()),
    };
    let verifier = ryeos_ui::invokers::browser_session_invocation::CompiledBrowserSessionVerifier {
        ui: get_ui_state(&state).unwrap().clone(),
    };
    // Auth ignores the input; retain it for the service invocation.
    let input = invocation.input.clone();
    let auth = verifier.invoke(invocation).await?;
    let RouteInvocationResult::Principal(principal) = auth else {
        panic!("auth verifier must return a principal");
    };
    invocation = RouteInvocationContext {
        route_id: descriptor.service_ref.into(),
        method: axum::http::Method::POST,
        uri: "/ui/api/session/seat/open".parse()?,
        captures: Default::default(),
        headers: Default::default(),
        body_raw: Vec::new(),
        input,
        principal: Some(principal),
        workspace_lifeline: None,
        launch_timings: None,
        state: state.as_ref().clone(),
        webhook_dedupe: Arc::new(Default::default()),
    };
    let invoker = ryeos_api::routes::invokers::service_invocation::CompiledServiceInvocation {
        service_ref: descriptor.service_ref.into(),
        subject_kind: "service".into(),
        endpoint: descriptor.endpoint.into(),
    };
    match invoker.invoke(invocation).await? {
        RouteInvocationResult::Json { value, .. } => Ok(value),
        _ => panic!("seat route must return JSON"),
    }
}

fn session_context(user_principal_id: Option<String>) -> ryeos_ui::browser_session::LaunchContext {
    launch_context(
        "surface:ryeos/ui/base",
        None,
        ryeos_ui::compiled_binding::EffectiveUiPosture::ObservationOnly,
        user_principal_id,
    )
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
    let (session_id, token) = get_ui_state(&state)
        .unwrap()
        .browser_sessions
        .mint_token(session_context(Some("fp:user-1".into())));
    assert_eq!(
        get_ui_state(&state)
            .unwrap()
            .browser_sessions
            .consume_launch_token(&token),
        Some(session_id.clone())
    );
    let ctx = handler_context(&session_id);

    let first = invoke_seat_route(
        &ryeos_ui::handlers::ui_seat::OPEN_DESCRIPTOR,
        serde_json::json!({}),
        ctx.clone(),
        Arc::new(state.clone()),
    )
    .await
    .expect("open seat");
    let second = invoke_seat_route(
        &ryeos_ui::handlers::ui_seat::OPEN_DESCRIPTOR,
        serde_json::json!({}),
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
    assert_eq!(
        detail.requested_by.as_deref(),
        Some(format!("session:{session_id}").as_str())
    );
}

#[tokio::test]
async fn same_principal_sessions_cannot_reattach_each_others_seats() {
    let (_tmp, state) = build_test_state_with_live_bundles();
    let sessions = &get_ui_state(&state).unwrap().browser_sessions;
    let (first_id, first_token) = sessions.mint_token(session_context(Some("fp:user-1".into())));
    let (second_id, second_token) = sessions.mint_token(session_context(Some("fp:user-1".into())));
    assert_eq!(
        sessions.consume_launch_token(&first_token),
        Some(first_id.clone())
    );
    assert_eq!(
        sessions.consume_launch_token(&second_token),
        Some(second_id.clone())
    );

    let first = invoke_seat_route(
        &ryeos_ui::handlers::ui_seat::OPEN_DESCRIPTOR,
        serde_json::json!({}),
        handler_context(&first_id),
        Arc::new(state.clone()),
    )
    .await
    .expect("open first session seat");
    let second = invoke_seat_route(
        &ryeos_ui::handlers::ui_seat::OPEN_DESCRIPTOR,
        serde_json::json!({}),
        handler_context(&second_id),
        Arc::new(state.clone()),
    )
    .await
    .expect("open second session seat");

    assert_ne!(first["thread_id"], second["thread_id"]);
    assert_eq!(second["reattached"], false);
}

#[tokio::test]
async fn ui_seat_append_replay_and_close_round_trip() {
    let (_tmp, state) = build_test_state_with_live_bundles();
    let (session_id, token) = get_ui_state(&state)
        .unwrap()
        .browser_sessions
        .mint_token(session_context(None));
    assert_eq!(
        get_ui_state(&state)
            .unwrap()
            .browser_sessions
            .consume_launch_token(&token),
        Some(session_id.clone())
    );
    let ctx = handler_context(&session_id);
    let state = Arc::new(state);

    let opened = invoke_seat_route(
        &ryeos_ui::handlers::ui_seat::OPEN_DESCRIPTOR,
        serde_json::json!({}),
        ctx.clone(),
        state.clone(),
    )
    .await
    .expect("open seat");
    let thread_id = opened["thread_id"].as_str().unwrap();

    let touched = invoke_seat_route(
        &ryeos_ui::handlers::ui_seat::TOUCH_DESCRIPTOR,
        serde_json::json!({ "thread_id": thread_id }),
        ctx.clone(),
        state.clone(),
    )
    .await
    .expect("touch seat through authenticated route");
    assert_eq!(touched["touched"], true);

    let project_action = ryeos_ui::handlers::ALL
        .iter()
        .find(|descriptor| descriptor.service_ref == "service:projects/open")
        .unwrap();
    let denied = invoke_seat_route(
        project_action,
        serde_json::json!({}),
        ctx.clone(),
        state.clone(),
    )
    .await
    .expect_err("a cookie must not bypass compiled-binding dispatch for project actions");
    assert!(
        denied
            .to_string()
            .contains("cannot invoke session-local service")
    );

    let appended = invoke_seat_route(
        &ryeos_ui::handlers::ui_seat::APPEND_DESCRIPTOR,
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

    let replay = invoke_seat_route(
        &ryeos_ui::handlers::ui_seat::REPLAY_DESCRIPTOR,
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

    let closed = invoke_seat_route(
        &ryeos_ui::handlers::ui_seat::CLOSE_DESCRIPTOR,
        serde_json::json!({ "thread_id": thread_id }),
        ctx,
        state.clone(),
    )
    .await
    .expect("close seat");
    assert_eq!(closed["status"], "completed");
}
