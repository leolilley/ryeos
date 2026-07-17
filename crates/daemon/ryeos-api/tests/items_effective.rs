// Tests for `items.effective` handler.
//
// Error-path tests with an empty engine. The kind mismatch check
// happens before resolution, so wrong_kind works without real items.

use std::sync::Arc;

use ryeos_app::handler_context::HandlerContext;

mod test_state;
use test_state::{build_test_state, build_test_state_with_bundles};

async fn call_effective(
    state: &ryeos_app::state::AppState,
    req_json: serde_json::Value,
) -> Result<serde_json::Value, anyhow::Error> {
    let req: ryeos_api::handlers::items_effective::Request = serde_json::from_value(req_json)?;
    let ctx = HandlerContext::anonymous();
    ryeos_api::handlers::items_effective::handle(req, ctx, Arc::new(state.clone())).await
}

#[tokio::test]
async fn nonexistent_ref_returns_not_found() {
    let (_tmp, state) = build_test_state_with_bundles();
    let result = call_effective(
        &state,
        serde_json::json!({
            "canonical_ref": "client:nonexistent/item",
        }),
    )
    .await;
    let err = result.expect_err("should fail for nonexistent ref");
    let msg = format!("{err:#}");
    assert!(msg.contains("not found"), "expected not_found, got: {msg}");
}

#[tokio::test]
async fn wrong_kind_returns_typed_error_code() {
    let (_tmp, state) = build_test_state();
    // The engine checks expected_kind against item_ref.kind before
    // resolution. surface: ref with expected_kind=client produces
    // EffectiveItemWrongKind immediately.
    let result = call_effective(
        &state,
        serde_json::json!({
            "canonical_ref": "surface:ryeos/ui/base",
            "expected_kind": "client"
        }),
    )
    .await;
    let err = result.expect_err("should fail with wrong_kind");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("wrong_kind:"),
        "expected 'wrong_kind:' error code prefix, got: {msg}"
    );
    assert!(
        msg.contains("expected `client`"),
        "should mention expected kind, got: {msg}"
    );
    assert!(
        msg.contains("got `surface`"),
        "should mention found kind, got: {msg}"
    );
}

#[tokio::test]
async fn invalid_canonical_ref_returns_bad_request() {
    let (_tmp, state) = build_test_state();
    let result = call_effective(
        &state,
        serde_json::json!({
            "canonical_ref": "not-a-valid-ref",
        }),
    )
    .await;
    let err = result.expect_err("should fail for invalid ref");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("invalid canonical ref"),
        "expected invalid ref error, got: {msg}"
    );
}

#[tokio::test]
async fn same_kind_passes_validation_but_not_found() {
    let (_tmp, state) = build_test_state_with_bundles();
    // surface: ref with expected_kind=surface passes the kind check
    // but fails at resolution after the real registry establishes the kind.
    let result = call_effective(
        &state,
        serde_json::json!({
            "canonical_ref": "surface:nonexistent/surface",
            "expected_kind": "surface"
        }),
    )
    .await;
    let err = result.expect_err("should fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("not found"),
        "expected not_found after kind check passed, got: {msg}"
    );
    assert!(
        !msg.contains("wrong_kind"),
        "should NOT contain wrong_kind, got: {msg}"
    );
}
