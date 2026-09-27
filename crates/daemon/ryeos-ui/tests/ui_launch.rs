// Tests for `ui.launch` handler.
//
// Pins current behavior so Slice 3 can refactor with a net.

mod test_state;
use test_state::{build_test_state, launch_context, mint_launch};

use ryeos_app::handler_context::HandlerContext;
use ryeos_ui::state::get_ui_state;
use std::sync::Arc;
use std::time::Duration;

fn test_context() -> ryeos_ui::browser_session::LaunchContext {
    launch_context(
        "surface:ryeos/ui/base",
        None,
        ryeos_ui::compiled_binding::EffectiveUiPosture::Interactive,
        None,
    )
}

#[tokio::test]
async fn invalid_token_rejected() {
    let (_tmp, state) = build_test_state();

    let result = (ryeos_ui::handlers::ui_launch::DESCRIPTOR.handler)(
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

    let (session_id, token) = mint_launch(&state, test_context());

    let result = (ryeos_ui::handlers::ui_launch::DESCRIPTOR.handler)(
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
async fn committed_activation_replays_the_same_session() {
    let (_tmp, state) = build_test_state();

    let (session_id, token) = mint_launch(&state, test_context());

    // First consume succeeds.
    let result1 = (ryeos_ui::handlers::ui_launch::DESCRIPTOR.handler)(
        serde_json::json!({ "token": token.clone() }),
        HandlerContext::anonymous(),
        Arc::new(state.clone()),
    )
    .await;
    assert!(result1.is_ok(), "first consume should succeed");

    // A response-loss retry returns the exact same committed session.
    let result2 = (ryeos_ui::handlers::ui_launch::DESCRIPTOR.handler)(
        serde_json::json!({ "token": token }),
        HandlerContext::anonymous(),
        Arc::new(state),
    )
    .await;
    let replay = result2.expect("activation replay should recover committed result");
    assert_eq!(replay["session_id"], session_id);
}

#[tokio::test]
async fn expired_token_rejected() {
    let short_store = ryeos_ui::BrowserSessionStore::new_with_short_ttl(
        Duration::from_millis(1),
        Duration::from_millis(1),
    );

    let (_, token, _) = short_store
        .mint_token(test_context(), 16, &"22".repeat(32))
        .expect("mint short-lived session");

    std::thread::sleep(Duration::from_millis(5));

    assert!(
        short_store.consume_launch_token(&token).is_none(),
        "expired token should be rejected"
    );
}

#[tokio::test]
async fn browser_session_auth_rejects_missing_cookie() {
    use axum::http::HeaderMap;
    use ryeos_ui::invokers::browser_session_invocation::extract_session_cookie_for_test;

    let headers = HeaderMap::new();
    assert!(
        extract_session_cookie_for_test(&headers).is_none(),
        "missing cookie should produce None"
    );
}
