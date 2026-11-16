// Browser session end, operator revoke and the revoked-session answer.

use std::sync::Arc;

use ryeos_api::route_error::RouteDispatchError;
use ryeos_api::routes::invocation::{
    CompiledRouteInvocation, RouteInvocationContext, RouteInvocationResult,
};
use ryeos_app::handler_context::HandlerContext;
use ryeos_ui::state::get_ui_state;
use ryeos_ui::test_support as test_state;
use test_state::{build_test_state, launch_context, mint_launch};

fn test_context() -> ryeos_ui::browser_session::LaunchContext {
    launch_context(
        "surface:ryeos/ui/base",
        None,
        ryeos_ui::compiled_binding::EffectiveUiPosture::Interactive,
        None,
    )
}

fn activated_session(state: &ryeos_app::state::AppState) -> String {
    let (session_id, token) = mint_launch(state, test_context());
    assert_eq!(
        get_ui_state(state)
            .unwrap()
            .browser_sessions
            .consume_launch_token(&token),
        Some(session_id.clone())
    );
    session_id
}

async fn authenticate(
    state: &ryeos_app::state::AppState,
    session_id: &str,
) -> Result<RouteInvocationResult, RouteDispatchError> {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "cookie",
        format!("ryeos_session={session_id}").parse().unwrap(),
    );
    let invocation = RouteInvocationContext {
        route_id: "ui/api/session/current".into(),
        method: axum::http::Method::GET,
        uri: "/ui/api/session/current".parse().unwrap(),
        captures: Default::default(),
        headers,
        body_raw: Vec::new(),
        input: serde_json::Value::Null,
        principal: None,
        workspace_lifeline: None,
        launch_timings: None,
        publication: None,
        state: state.clone(),
        webhook_dedupe: Arc::new(Default::default()),
    };
    ryeos_ui::invokers::browser_session_invocation::CompiledBrowserSessionVerifier {
        ui: get_ui_state(state).unwrap().clone(),
    }
    .invoke(invocation)
    .await
}

#[tokio::test]
async fn revoked_session_refused_with_code() {
    let (_tmp, _owner, state) = build_test_state();
    let session_id = activated_session(&state);
    authenticate(&state, &session_id)
        .await
        .expect("live session authenticates");

    assert!(
        get_ui_state(&state)
            .unwrap()
            .browser_sessions
            .revoke_session(&session_id)
    );

    let error = authenticate(&state, &session_id)
        .await
        .err()
        .expect("revoked session is refused");
    let RouteDispatchError::Structured {
        code, status, body, ..
    } = error
    else {
        panic!("revoked session must carry a structured answer");
    };
    assert_eq!(code, "browser_session_revoked");
    assert_eq!(status, 401);
    assert_eq!(body["error_code"], "browser_session_revoked");
}

#[tokio::test]
async fn unknown_session_stays_a_plain_unauthorized() {
    let (_tmp, _owner, state) = build_test_state();
    let error = authenticate(&state, "no-such-session")
        .await
        .err()
        .expect("unknown session is refused");
    assert!(matches!(error, RouteDispatchError::Unauthorized));
}

#[tokio::test]
async fn ending_a_session_clears_the_cookie_and_refuses_later_requests() {
    let (_tmp, _owner, state) = build_test_state();
    let session_id = activated_session(&state);
    let ctx = HandlerContext::new(format!("session:{session_id}"), Vec::new(), false);

    let response = (ryeos_ui::handlers::ui_session_end::DESCRIPTOR.handler)(
        serde_json::json!({}),
        ctx,
        Arc::new(state.clone()),
    )
    .await
    .expect("session ends itself");
    assert_eq!(response["ended"], true);
    assert_eq!(response["cookie"]["name"], "ryeos_session");
    assert_eq!(response["cookie"]["value"], "");
    assert_eq!(response["cookie"]["max_age"], 0);

    let error = authenticate(&state, &session_id)
        .await
        .err()
        .expect("ended session is refused");
    assert!(
        matches!(error, RouteDispatchError::Unauthorized),
        "ending your own session is not a revocation"
    );
}

#[tokio::test]
async fn session_end_requires_a_browser_session() {
    let (_tmp, _owner, state) = build_test_state();
    let error = (ryeos_ui::handlers::ui_session_end::DESCRIPTOR.handler)(
        serde_json::json!({}),
        HandlerContext::new("fp:someone".into(), vec!["*".into()], true),
        Arc::new(state),
    )
    .await
    .expect_err("a signed principal has no browser session to end");
    assert!(format!("{error:#}").contains("no browser session"));
}

#[tokio::test]
async fn session_admin_requires_the_configured_operator() {
    let (_tmp, _owner, state) = build_test_state();
    let session_id = activated_session(&state);
    let stranger = || HandlerContext::new("fp:stranger".into(), vec!["*".into()], true);

    let error = (ryeos_ui::handlers::ui_sessions_admin::LIST_DESCRIPTOR.handler)(
        serde_json::json!({}),
        stranger(),
        Arc::new(state.clone()),
    )
    .await
    .expect_err("list is operator-only");
    assert!(format!("{error:#}").contains("configured local operator"));

    let error = (ryeos_ui::handlers::ui_sessions_admin::REVOKE_DESCRIPTOR.handler)(
        serde_json::json!({ "session_id": session_id }),
        stranger(),
        Arc::new(state.clone()),
    )
    .await
    .expect_err("revoke is operator-only");
    assert!(format!("{error:#}").contains("configured local operator"));
    assert!(
        !get_ui_state(&state)
            .unwrap()
            .browser_sessions
            .is_revoked(&session_id),
        "a refused revoke leaves the session alone"
    );
}
