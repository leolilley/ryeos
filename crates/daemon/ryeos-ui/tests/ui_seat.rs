mod test_state;
use test_state::{build_test_state_with_live_bundles, launch_context};

use ryeos_app::handler_context::HandlerContext;
use ryeos_ui::state::get_ui_state;
use serde_json::{Value, json};
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

fn open_request(state: &ryeos_app::state::AppState, session_id: &str) -> Value {
    let session = get_ui_state(state)
        .unwrap()
        .browser_sessions
        .get_session(session_id)
        .expect("active session");
    let coordinate = session
        .attachments
        .get(&session.surface_attachment_id)
        .expect("surface attachment")
        .coordinate();
    serde_json::json!({
        "binding_attachment_id": coordinate.binding_attachment_id,
        "binding_generation": coordinate.binding_generation,
        "binding_digest": coordinate.binding_digest,
    })
}

fn append_request(
    opened: &Value,
    operation_id: &str,
    first_engine_seq: u64,
    payloads: &[Value],
) -> Value {
    let events = payloads
        .iter()
        .enumerate()
        .map(|(offset, payload)| {
            json!({
                "engine_seq": first_engine_seq + offset as u64,
                "event_type": "seat.facet",
                "payload": payload,
            })
        })
        .collect::<Vec<_>>();
    let payload_digest = ryeos_ui::handlers::ui_seat::seat_payload_digest(&json!(events)).unwrap();
    json!({
        "thread_id": opened["thread_id"],
        "producer_incarnation": opened["producer_incarnation"],
        "operation_id": operation_id,
        "first_engine_seq": first_engine_seq,
        "last_engine_seq": first_engine_seq + payloads.len() as u64 - 1,
        "event_count": payloads.len(),
        "payload_digest": payload_digest,
        "events": events,
    })
}

#[tokio::test]
async fn ui_seat_open_reattaches_running_session_seat() {
    let (_tmp, state) = build_test_state_with_live_bundles();
    let (session_id, token) =
        test_state::mint_launch(&state, session_context(Some("fp:user-1".into())));
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
        open_request(&state, &session_id),
        ctx.clone(),
        Arc::new(state.clone()),
    )
    .await
    .expect("open seat");
    let second = invoke_seat_route(
        &ryeos_ui::handlers::ui_seat::OPEN_DESCRIPTOR,
        open_request(&state, &session_id),
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
async fn ui_seat_open_rejects_stale_attachment_policy_generation() {
    let (_tmp, state) = build_test_state_with_live_bundles();
    let stale_digest = "99".repeat(32);
    let mut context = session_context(None);
    Arc::make_mut(&mut context.compiled_binding)
        .binding
        .node_policy_generation_digest = stale_digest.clone();
    let (session_id, token, _) = get_ui_state(&state)
        .unwrap()
        .browser_sessions
        .mint_token(context, 16, &stale_digest)
        .expect("mint stale-policy fixture");
    assert_eq!(
        get_ui_state(&state)
            .unwrap()
            .browser_sessions
            .consume_launch_token(&token),
        Some(session_id.clone())
    );

    let error = invoke_seat_route(
        &ryeos_ui::handlers::ui_seat::OPEN_DESCRIPTOR,
        open_request(&state, &session_id),
        handler_context(&session_id),
        Arc::new(state),
    )
    .await
    .expect_err("stale node-policy attachment must not create a seat");
    assert!(format!("{error:?}").contains("ui_binding_stale"));
}

#[tokio::test]
async fn same_principal_sessions_cannot_reattach_each_others_seats() {
    let (_tmp, state) = build_test_state_with_live_bundles();
    let (first_id, first_token) =
        test_state::mint_launch(&state, session_context(Some("fp:user-1".into())));
    let (second_id, second_token) =
        test_state::mint_launch(&state, session_context(Some("fp:user-1".into())));
    let sessions = &get_ui_state(&state).unwrap().browser_sessions;
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
        open_request(&state, &first_id),
        handler_context(&first_id),
        Arc::new(state.clone()),
    )
    .await
    .expect("open first session seat");
    let second = invoke_seat_route(
        &ryeos_ui::handlers::ui_seat::OPEN_DESCRIPTOR,
        open_request(&state, &second_id),
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
    let (session_id, token) = test_state::mint_launch(&state, session_context(None));
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
        open_request(&state, &session_id),
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
        append_request(
            &opened,
            "00000000-0000-4000-8000-000000000001",
            0,
            &[json!({ "key": "selection", "value": { "item": "T-1" } })],
        ),
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

#[tokio::test]
async fn ui_seat_append_is_exactly_idempotent_and_rejects_contradictions_and_gaps() {
    let (_tmp, state) = build_test_state_with_live_bundles();
    let (session_id, token) = test_state::mint_launch(&state, session_context(None));
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
        open_request(&state, &session_id),
        ctx.clone(),
        state.clone(),
    )
    .await
    .expect("open seat");
    assert_eq!(opened["next_engine_seq"], "0");

    let operation_id = "00000000-0000-4000-8000-000000000002";
    let request = append_request(&opened, operation_id, 0, &[json!({ "é": "😀", "a": "𐀀" })]);
    // Cross-language vector for the browser/server typed digest protocol.
    assert_eq!(
        request["payload_digest"],
        "40442791210ae66e4e67d762a8102f444250935db90a1976d743cb9952eed572"
    );
    let first = invoke_seat_route(
        &ryeos_ui::handlers::ui_seat::APPEND_DESCRIPTOR,
        request.clone(),
        ctx.clone(),
        state.clone(),
    )
    .await
    .expect("first append");
    let replayed_ack = invoke_seat_route(
        &ryeos_ui::handlers::ui_seat::APPEND_DESCRIPTOR,
        request,
        ctx.clone(),
        state.clone(),
    )
    .await
    .expect("lost-response-equivalent exact replay");
    assert_eq!(replayed_ack, first);

    let replay = invoke_seat_route(
        &ryeos_ui::handlers::ui_seat::REPLAY_DESCRIPTOR,
        json!({ "chain_root_id": opened["thread_id"] }),
        ctx.clone(),
        state.clone(),
    )
    .await
    .expect("replay seat");
    assert_eq!(replay["events"].as_array().unwrap().len(), 1);

    invoke_seat_route(
        &ryeos_ui::handlers::ui_seat::APPEND_DESCRIPTOR,
        append_request(
            &opened,
            operation_id,
            0,
            &[json!({ "key": "selection", "value": "different" })],
        ),
        ctx.clone(),
        state.clone(),
    )
    .await
    .expect_err("operation identity must bind the exact request");

    invoke_seat_route(
        &ryeos_ui::handlers::ui_seat::APPEND_DESCRIPTOR,
        append_request(
            &opened,
            "00000000-0000-4000-8000-000000000003",
            2,
            &[json!({ "key": "selection", "value": "gap" })],
        ),
        ctx.clone(),
        state.clone(),
    )
    .await
    .expect_err("sequence gaps must be refused");

    invoke_seat_route(
        &ryeos_ui::handlers::ui_seat::APPEND_DESCRIPTOR,
        append_request(
            &opened,
            "00000000-0000-4000-8000-000000000007",
            0,
            &[json!({ "key": "selection", "value": "overlap" })],
        ),
        ctx.clone(),
        state.clone(),
    )
    .await
    .expect_err("sequence overlaps must be refused");

    let mut malformed = append_request(
        &opened,
        "00000000-0000-4000-8000-000000000004",
        1,
        &[json!({ "key": "selection", "value": "bad digest" })],
    );
    malformed["payload_digest"] = json!("not-a-digest");
    invoke_seat_route(
        &ryeos_ui::handlers::ui_seat::APPEND_DESCRIPTOR,
        malformed,
        ctx.clone(),
        state.clone(),
    )
    .await
    .expect_err("malformed digest must be refused");

    let float_append = invoke_seat_route(
        &ryeos_ui::handlers::ui_seat::APPEND_DESCRIPTOR,
        append_request(
            &opened,
            "00000000-0000-4000-8000-000000000006",
            1,
            &[json!({ "key": "fraction", "value": 0.000001 })],
        ),
        ctx.clone(),
        state.clone(),
    )
    .await
    .expect("typed digest accepts a float across JS/serde exponent spelling thresholds");
    assert_eq!(float_append["appended"], 1);

    let rotated = invoke_seat_route(
        &ryeos_ui::handlers::ui_seat::OPEN_DESCRIPTOR,
        open_request(&state, &session_id),
        ctx.clone(),
        state.clone(),
    )
    .await
    .expect("rotate the producer after the accepted operation");
    assert_ne!(
        rotated["producer_incarnation"],
        opened["producer_incarnation"]
    );
    let rotated_replay = invoke_seat_route(
        &ryeos_ui::handlers::ui_seat::APPEND_DESCRIPTOR,
        append_request(&opened, operation_id, 0, &[json!({ "é": "😀", "a": "𐀀" })]),
        ctx.clone(),
        state.clone(),
    )
    .await
    .expect("an accepted operation remains exactly replayable after producer rotation");
    assert_eq!(rotated_replay, first);

    invoke_seat_route(
        &ryeos_ui::handlers::ui_seat::CLOSE_DESCRIPTOR,
        json!({ "thread_id": opened["thread_id"] }),
        ctx.clone(),
        state.clone(),
    )
    .await
    .expect("close seat after committed operation");
    let settled_replay = invoke_seat_route(
        &ryeos_ui::handlers::ui_seat::APPEND_DESCRIPTOR,
        append_request(&opened, operation_id, 0, &[json!({ "é": "😀", "a": "𐀀" })]),
        ctx,
        state,
    )
    .await
    .expect("durable receipt remains replayable after seat settlement");
    assert_eq!(settled_replay, first);
}

#[tokio::test]
async fn ui_seat_new_open_invalidates_stale_and_foreign_producers() {
    let (_tmp, state) = build_test_state_with_live_bundles();
    let (owner_id, owner_token) = test_state::mint_launch(&state, session_context(None));
    let (foreign_id, foreign_token) = test_state::mint_launch(&state, session_context(None));
    let sessions = &get_ui_state(&state).unwrap().browser_sessions;
    assert_eq!(
        sessions.consume_launch_token(&owner_token),
        Some(owner_id.clone())
    );
    assert_eq!(
        sessions.consume_launch_token(&foreign_token),
        Some(foreign_id.clone())
    );
    let state = Arc::new(state);
    let owner_ctx = handler_context(&owner_id);
    let first = invoke_seat_route(
        &ryeos_ui::handlers::ui_seat::OPEN_DESCRIPTOR,
        open_request(&state, &owner_id),
        owner_ctx.clone(),
        state.clone(),
    )
    .await
    .expect("open seat");
    let second = invoke_seat_route(
        &ryeos_ui::handlers::ui_seat::OPEN_DESCRIPTOR,
        open_request(&state, &owner_id),
        owner_ctx.clone(),
        state.clone(),
    )
    .await
    .expect("reattach seat");
    assert_ne!(
        first["producer_incarnation"],
        second["producer_incarnation"]
    );

    let stale_request = append_request(
        &first,
        "00000000-0000-4000-8000-000000000005",
        0,
        &[json!({ "key": "selection", "value": "stale" })],
    );
    invoke_seat_route(
        &ryeos_ui::handlers::ui_seat::APPEND_DESCRIPTOR,
        stale_request,
        owner_ctx,
        state.clone(),
    )
    .await
    .expect_err("previous open's producer must be stale");

    invoke_seat_route(
        &ryeos_ui::handlers::ui_seat::APPEND_DESCRIPTOR,
        append_request(
            &second,
            "00000000-0000-4000-8000-000000000006",
            0,
            &[json!({ "key": "selection", "value": "foreign" })],
        ),
        handler_context(&foreign_id),
        state,
    )
    .await
    .expect_err("foreign session must not append an owned seat");
}
