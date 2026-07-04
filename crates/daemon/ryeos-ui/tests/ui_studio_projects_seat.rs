//! Seat-auth gate coverage for the Studio project registry handlers.
//!
//! The project registry is node-global and its gates accept both browser
//! sessions and verified operators through `seat_auth::require_seat_caller`.

mod test_state;
use test_state::build_test_state;

use ryeos_app::handler_context::HandlerContext;
use serde_json::json;
use std::sync::Arc;

#[tokio::test]
async fn verified_operator_passes_projects_read_gate() {
    let (_tmp, state) = build_test_state();
    let operator_ctx = HandlerContext::new("fp:local-trust".into(), vec!["*".into()], true);

    let listed = ryeos_ui::handlers::ui_studio_projects::handle_projects_list(
        json!(null),
        operator_ctx,
        Arc::new(state),
    )
    .await
    .expect("verified operator should pass the projects read gate");

    assert_eq!(listed["version"], 1);
    assert!(listed["projects"].as_array().unwrap().is_empty());
}
