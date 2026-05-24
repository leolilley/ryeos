//! Slice 0: Backfill tests for `ui.bootstrap` handler.
//!
//! These tests pin the current behavior of the ui.bootstrap service
//! handler so corrective slices can detect regressions.
//!
//! Error-path tests work with an empty engine. Happy-path tests
//! (resolving a real surface) require the full engine boot, which
//! needs boot-validation fixes first.

use std::sync::Arc;

use ryeos_app::handler_context::HandlerContext;

mod test_state;
use test_state::build_test_state;

/// Helper: call the ui.bootstrap handler with the given request JSON.
async fn call_bootstrap(
    state: &ryeos_app::state::AppState,
    req_json: serde_json::Value,
) -> Result<serde_json::Value, anyhow::Error> {
    let req: ryeos_api::handlers::ui_bootstrap::Request =
        serde_json::from_value(req_json)?;
    let ctx = HandlerContext::anonymous();
    ryeos_api::handlers::ui_bootstrap::handle(req, ctx, Arc::new(state.clone())).await
}

#[tokio::test]
async fn nonexistent_surface_returns_not_found() {
    let (_tmp, state) = build_test_state();
    let result = call_bootstrap(
        &state,
        serde_json::json!({
            "surface_ref": "surface:nonexistent/surface",
        }),
    )
    .await;
    let err = result.expect_err("should fail for nonexistent surface");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("not found") || msg.contains("not_found"),
        "expected not_found error, got: {msg}"
    );
}

#[tokio::test]
async fn unknown_renderer_rejected() {
    let (_tmp, state) = build_test_state();
    let result = call_bootstrap(
        &state,
        serde_json::json!({
            "surface_ref": "surface:ryeos/cockpit/base",
            "renderer": "vr_headset"
        }),
    )
    .await;
    let err = result.expect_err("should reject unknown renderer");
    let msg = format!("{err:#}");
    // The renderer validation happens before engine resolution,
    // so this should produce a "unknown renderer" error.
    assert!(
        msg.contains("unknown renderer"),
        "expected renderer rejection, got: {msg}"
    );
}

#[tokio::test]
async fn invalid_surface_ref_returns_bad_request() {
    let (_tmp, state) = build_test_state();
    let result = call_bootstrap(
        &state,
        serde_json::json!({
            "surface_ref": "not-a-valid-ref",
        }),
    )
    .await;
    let err = result.expect_err("should fail for invalid ref");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("invalid surface ref"),
        "expected invalid ref error, got: {msg}"
    );
}
