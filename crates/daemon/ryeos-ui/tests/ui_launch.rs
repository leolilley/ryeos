// Tests for `ui.launch` handler.
//
// Pins current behavior so Slice 3 can refactor with a net.

use ryeos_ui::test_support as test_state;
use test_state::{build_test_state, launch_context, mint_launch};

use ryeos_app::handler_context::HandlerContext;
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
    let (_tmp, _state_owner, state) = build_test_state();

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
    let (_tmp, _state_owner, state) = build_test_state();

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
async fn a_second_use_of_a_valid_token_is_refused() {
    let (_tmp, _state_owner, state) = build_test_state();
    let (_session_id, token) = mint_launch(&state, test_context());
    let state = Arc::new(state);

    let first = (ryeos_ui::handlers::ui_launch::DESCRIPTOR.handler)(
        serde_json::json!({ "token": token.clone() }),
        HandlerContext::anonymous(),
        state.clone(),
    )
    .await;
    assert!(first.is_ok(), "first use should succeed");

    let second = (ryeos_ui::handlers::ui_launch::DESCRIPTOR.handler)(
        serde_json::json!({ "token": token }),
        HandlerContext::anonymous(),
        state,
    )
    .await;
    assert!(second.is_err(), "a spent token must be refused");
}

#[tokio::test]
async fn unknown_spent_and_expired_tokens_get_the_same_refusal() {
    let (_tmp, _state_owner, state) = build_test_state();
    let state = Arc::new(state);

    let refusal = |token: String| {
        let state = state.clone();
        async move {
            let error = (ryeos_ui::handlers::ui_launch::DESCRIPTOR.handler)(
                serde_json::json!({ "token": token }),
                HandlerContext::anonymous(),
                state,
            )
            .await
            .expect_err("refused");
            format!("{error:#}")
        }
    };

    let unknown = refusal("not-a-token".to_string()).await;

    let (_, spent_token) = mint_launch(&state, test_context());
    (ryeos_ui::handlers::ui_launch::DESCRIPTOR.handler)(
        serde_json::json!({ "token": spent_token.clone() }),
        HandlerContext::anonymous(),
        state.clone(),
    )
    .await
    .expect("first use succeeds");
    let spent = refusal(spent_token).await;

    let mut short = test_context();
    short.timing = ryeos_ui::SessionTiming::new(
        Duration::from_millis(1),
        Duration::from_millis(1),
        Duration::from_millis(1),
    );
    let (_, expired_token) = mint_launch(&state, short);
    std::thread::sleep(Duration::from_millis(10));
    let expired = refusal(expired_token).await;

    assert_eq!(unknown, spent);
    assert_eq!(unknown, expired);
}

#[tokio::test]
async fn expired_token_rejected() {
    let short_store = ryeos_ui::BrowserSessionStore::new();
    let mut context = test_context();
    context.timing = ryeos_ui::SessionTiming::new(
        Duration::from_millis(1),
        Duration::from_millis(1),
        Duration::from_millis(1),
    );

    let (_, token, _) = short_store
        .mint_token(context, 16, &"22".repeat(32))
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

async fn activated_cookie(state: ryeos_app::state::AppState) -> serde_json::Value {
    let (session_id, token) = mint_launch(&state, test_context());
    let val = (ryeos_ui::handlers::ui_launch::DESCRIPTOR.handler)(
        serde_json::json!({ "token": token }),
        HandlerContext::anonymous(),
        Arc::new(state),
    )
    .await
    .expect("valid token should succeed");
    assert_eq!(val["session_id"], session_id);
    val["cookie"].clone()
}

#[tokio::test]
async fn cookie_is_not_secure_without_a_public_origin() {
    let (_tmp, _owner, state) = build_test_state();
    let cookie = activated_cookie(state).await;
    assert_eq!(cookie["secure"], false);
}

#[tokio::test]
async fn cookie_is_not_secure_for_a_plain_http_public_origin() {
    let (_tmp, _owner, state) =
        test_state::build_test_state_with_public_origin("http://localhost:8080");
    let cookie = activated_cookie(state).await;
    assert_eq!(cookie["secure"], false);
}

#[tokio::test]
async fn cookie_is_secure_for_an_https_public_origin() {
    let (_tmp, _owner, state) =
        test_state::build_test_state_with_public_origin("https://node.example.org");
    let cookie = activated_cookie(state).await;
    assert_eq!(cookie["secure"], true);
    assert_eq!(cookie["same_site"], "Strict");
    assert_eq!(cookie["http_only"], true);
}
