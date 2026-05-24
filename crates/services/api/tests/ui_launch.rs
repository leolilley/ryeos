//! Slice 0: Tests for `ui.launch` handler.
//!
//! Pins current behavior so Slice 3 can refactor with a net.

mod test_state;
use test_state::build_test_state;

use std::sync::Arc;
use ryeos_app::handler_context::HandlerContext;

#[tokio::test]
async fn invalid_token_rejected() {
    let (_tmp, state) = build_test_state();

    let result = (ryeos_api::handlers::ui_launch::DESCRIPTOR.handler)(
        serde_json::json!({ "token": "nonexistent-token" }),
        HandlerContext::anonymous(),
        Arc::new(state),
    )
    .await;

    let err = result.expect_err("should fail for invalid token");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("invalid") || msg.contains("expired"),
        "expected rejection, got: {msg}"
    );
}

#[tokio::test]
async fn valid_token_consumed_and_session_returned() {
    let (_tmp, state) = build_test_state();

    let (session_id, token) = state
        .browser_sessions
        .create_session(vec!["ui.read".into()], None);

    let result = (ryeos_api::handlers::ui_launch::DESCRIPTOR.handler)(
        serde_json::json!({ "token": token }),
        HandlerContext::anonymous(),
        Arc::new(state),
    )
    .await;

    let val = result.expect("valid token should succeed");
    assert_eq!(val["session_id"], session_id);
    assert_eq!(val["redirect"], "/ui");
    assert_eq!(val["cookie"]["name"], "ryeos_session");
}

#[tokio::test]
async fn consumed_token_cannot_be_reused() {
    let (_tmp, state) = build_test_state();

    let (_, token) = state
        .browser_sessions
        .create_session(vec![], None);

    // First consume succeeds.
    let result1 = (ryeos_api::handlers::ui_launch::DESCRIPTOR.handler)(
        serde_json::json!({ "token": token.clone() }),
        HandlerContext::anonymous(),
        Arc::new(state.clone()),
    )
    .await;
    assert!(result1.is_ok(), "first consume should succeed");

    // Second consume fails.
    let result2 = (ryeos_api::handlers::ui_launch::DESCRIPTOR.handler)(
        serde_json::json!({ "token": token }),
        HandlerContext::anonymous(),
        Arc::new(state),
    )
    .await;
    assert!(result2.is_err(), "reused token should fail");
}

#[tokio::test]
async fn expired_token_rejected() {
    let (_tmp, state) = build_test_state();

    let (_, token) = state.browser_sessions.create_session_with_ttl(
        vec![],
        None,
        std::time::Duration::from_millis(0),
    );

    // Wait for expiry.
    std::thread::sleep(std::time::Duration::from_millis(2));

    let result = (ryeos_api::handlers::ui_launch::DESCRIPTOR.handler)(
        serde_json::json!({ "token": token }),
        HandlerContext::anonymous(),
        Arc::new(state),
    )
    .await;
    assert!(result.is_err(), "expired token should fail");
}

#[tokio::test]
async fn browser_session_auth_rejects_missing_cookie() {
    use axum::http::HeaderMap;
    use ryeos_api::routes::invokers::browser_session_invocation::extract_session_cookie_for_test;

    let headers = HeaderMap::new();
    assert!(
        extract_session_cookie_for_test(&headers).is_none(),
        "missing cookie should produce None"
    );
}
