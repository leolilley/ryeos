//! Seat-auth gate coverage for the RyeOS UI project registry handlers.
//!
//! The project registry is node-global and its gates accept both browser
//! sessions and verified operators through `seat_auth::require_seat_caller`.

mod test_state;
use test_state::{build_test_state, build_test_state_with_live_bundles, local_operator_context};

use ryeos_app::handler_context::HandlerContext;
use serde_json::json;
use std::sync::Arc;

#[tokio::test]
async fn verified_operator_passes_projects_read_gate() {
    let (_tmp, state) = build_test_state();
    let operator_ctx = local_operator_context(&state, vec!["*".into()]);

    let listed = ryeos_ui::handlers::ui_projects::handle_projects_list(
        json!(null),
        operator_ctx,
        Arc::new(state),
    )
    .await
    .expect("verified operator should pass the projects read gate");

    assert_eq!(listed["version"], 1);
    assert!(listed["projects"].as_array().unwrap().is_empty());
}

#[tokio::test]
#[ignore = "requires populated handler binaries via scripts/populate-bundles.sh"]
async fn opening_another_project_mints_an_immutable_successor_session() {
    let (_tmp, state) = build_test_state_with_live_bundles();
    let first = tempfile::TempDir::new().expect("first project");
    let second = tempfile::TempDir::new().expect("second project");
    let operator_ctx = local_operator_context(&state, vec!["*".into()]);
    let principal = operator_ctx.fingerprint.clone();
    let state = Arc::new(state);

    let mut project_ids = Vec::new();
    for root in [first.path(), second.path()] {
        let added = ryeos_ui::handlers::ui_projects::handle_projects_add(
            json!({"root": root}),
            operator_ctx.clone(),
            state.clone(),
        )
        .await
        .expect("register project");
        project_ids.push(
            added["project"]["local_id"]
                .as_str()
                .expect("registered local id")
                .to_string(),
        );
    }

    let minted = ryeos_ui::handlers::ui_launch_mint::handle(
        ryeos_ui::handlers::ui_launch_mint::Request {
            ui_binding_contract_revision: ryeos_ui::UI_BINDING_CONTRACT_REVISION.to_string(),
            surface_ref: "surface:ryeos/ui/base".into(),
            project_path: Some(first.path().display().to_string()),
            user_principal_id: Some(principal),
        },
        operator_ctx,
        state.clone(),
    )
    .await
    .expect("mint first project session");
    let first_session_id = minted["session_id"].as_str().unwrap();
    assert_eq!(
        ryeos_ui::state::get_ui_state(&state)
            .unwrap()
            .browser_sessions
            .consume_launch_token(minted["token"].as_str().unwrap()),
        Some(first_session_id.to_string())
    );
    let first_session = ryeos_ui::state::get_ui_state(&state)
        .unwrap()
        .browser_sessions
        .get_session(first_session_id)
        .expect("first session retained");
    let first_binding_digest = first_session.compiled_binding.binding_digest.clone();
    let predecessor_context = HandlerContext::new(
        format!("session:{first_session_id}"),
        vec!["*".into()],
        false,
    );
    let predecessor_seat = (ryeos_ui::handlers::ui_seat::OPEN_DESCRIPTOR.handler)(
        json!({}),
        predecessor_context.clone(),
        state.clone(),
    )
    .await
    .expect("open predecessor seat");

    let opened = ryeos_ui::handlers::ui_projects::handle_projects_open(
        json!({"local_id": project_ids[1]}),
        predecessor_context,
        state.clone(),
    )
    .await
    .expect("open second project");
    assert_eq!(opened["ui_transition"]["kind"], "replace_session");
    let successor_id = opened["ui_transition"]["session_id"]
        .as_str()
        .expect("successor id");
    assert_ne!(successor_id, first_session_id);

    let ui = ryeos_ui::state::get_ui_state(&state).unwrap();
    assert!(ui.browser_sessions.get_session(successor_id).is_none());
    let successor_token = opened["ui_transition"]["launch_url"]
        .as_str()
        .unwrap()
        .strip_prefix("/ui/launch/")
        .unwrap();
    assert_eq!(
        ui.browser_sessions.consume_launch_token(successor_token),
        Some(successor_id.to_string())
    );
    let successor = ui
        .browser_sessions
        .get_session(successor_id)
        .expect("successor retained");
    assert_eq!(
        successor.project_root.as_deref(),
        Some(
            second
                .path()
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .as_ref()
        )
    );
    assert_ne!(
        successor.compiled_binding.binding_digest,
        first_binding_digest
    );
    assert!(
        ui.browser_sessions.get_session(first_session_id).is_none(),
        "committed successor activation must retire the predecessor"
    );
    let predecessor_seat = state
        .state_store
        .get_thread(predecessor_seat["thread_id"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(predecessor_seat.status, "completed");
}
