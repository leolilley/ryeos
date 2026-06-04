// Tests for principal-aware Studio user-space storage.

mod test_state;
use test_state::build_test_state;

use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::{extract_handler_error, HandlerError};
use ryeos_ui::browser_session::LaunchContext;
use ryeos_ui::state::get_ui_state;
use serde_json::json;
use std::sync::Arc;

fn launch_context(user_principal_id: String) -> LaunchContext {
    LaunchContext {
        surface_ref: "surface:ryeos/studio/base".into(),
        project_path: None,
        read_only: false,
        granted_caps: vec!["ui.read".into()],
        user_principal_id: Some(user_principal_id),
    }
}

fn session_context(state: &ryeos_app::state::AppState, principal: &str) -> HandlerContext {
    let (session_id, _token) = get_ui_state(state)
        .unwrap()
        .browser_sessions
        .mint_token(launch_context(principal.to_string()));
    HandlerContext::new(
        format!("session:{session_id}"),
        vec!["ui.read".into()],
        false,
    )
}

#[tokio::test]
async fn studio_config_is_isolated_by_durable_user_principal() {
    let (tmp, state) = build_test_state();
    let principal_a = format!("fp:{}", "aa".repeat(32));
    let principal_b = format!("fp:{}", "bb".repeat(32));

    let ctx_a = session_context(&state, &principal_a);
    let updated = ryeos_ui::handlers::ui_studio_projects::handle_config_update(
        json!({ "theme": "dark" }),
        ctx_a.clone(),
        Arc::new(state.clone()),
    )
    .await
    .expect("principal A should update config");
    assert_eq!(updated["theme"], "dark");

    let loaded_a = ryeos_ui::handlers::ui_studio_projects::handle_config_get(
        json!(null),
        ctx_a,
        Arc::new(state.clone()),
    )
    .await
    .expect("principal A should read updated config");
    assert_eq!(loaded_a["theme"], "dark");

    let ctx_b = session_context(&state, &principal_b);
    let loaded_b = ryeos_ui::handlers::ui_studio_projects::handle_config_get(
        json!(null),
        ctx_b,
        Arc::new(state.clone()),
    )
    .await
    .expect("principal B should read default config");
    assert_eq!(loaded_b["theme"], "system");

    let principal_a_key = "aa".repeat(32);
    assert!(tmp
        .path()
        .join(".ai/principals")
        .join(principal_a_key)
        .join("user-space/.ai/config/studio.yaml")
        .is_file());
}

#[tokio::test]
async fn studio_dimension_exposes_session_user_principal() {
    let (_tmp, state) = build_test_state();
    let principal = format!("fp:{}", "cc".repeat(32));
    let ctx = session_context(&state, &principal);

    let dimension =
        ryeos_ui::handlers::ui_studio_dimension::handle(json!(null), ctx, Arc::new(state.clone()))
            .await
            .expect("studio dimension should load");

    assert_eq!(dimension["session"]["user_principal_id"], principal);
    assert_eq!(
        dimension["session"]["surface_ref"],
        "surface:ryeos/studio/base"
    );
}

#[tokio::test]
async fn launch_mint_rejects_invalid_user_principal_as_bad_request() {
    let (_tmp, state) = build_test_state();
    let req = ryeos_ui::handlers::ui_launch_mint::Request {
        surface_ref: "surface:ryeos/studio/base".into(),
        project_path: None,
        read_only: false,
        user_principal_id: Some("session:not-a-principal".into()),
    };

    let err = ryeos_ui::handlers::ui_launch_mint::handle(
        req,
        HandlerContext::new("fp:local-trust".into(), vec!["*".into()], true),
        Arc::new(state),
    )
    .await
    .expect_err("invalid principal should fail before launch route rendering");

    let handler_error = extract_handler_error(&err).expect("should preserve typed handler error");
    assert!(matches!(handler_error, HandlerError::BadRequest(_)));
}

#[tokio::test]
async fn launch_mint_rejects_mismatched_user_principal() {
    let (_tmp, state) = build_test_state();
    let req = ryeos_ui::handlers::ui_launch_mint::Request {
        surface_ref: "surface:ryeos/studio/base".into(),
        project_path: None,
        read_only: false,
        user_principal_id: Some(format!("fp:{}", "aa".repeat(32))),
    };

    let err = ryeos_ui::handlers::ui_launch_mint::handle(
        req,
        HandlerContext::new(format!("fp:{}", "bb".repeat(32)), vec!["*".into()], true),
        Arc::new(state),
    )
    .await
    .expect_err("caller must not bind launch to another durable principal");

    let handler_error = extract_handler_error(&err).expect("should preserve typed handler error");
    assert!(matches!(handler_error, HandlerError::Forbidden(_)));
}
