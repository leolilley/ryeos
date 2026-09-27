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
async fn request_project_path_does_not_create_project_authority() {
    let (_tmp, state) = build_test_state();
    let requested = tempfile::TempDir::new().expect("request project");
    let operator_ctx = local_operator_context(&state, vec!["*".into()]);

    let listed = ryeos_ui::handlers::ui_projects::handle_projects_list(
        json!({"project_path": requested.path()}),
        operator_ctx,
        Arc::new(state),
    )
    .await
    .expect("verified operator should pass the projects read gate");

    assert!(
        listed["projects"].as_array().unwrap().is_empty(),
        "request parameters cannot synthesize a current project"
    );
}

#[tokio::test]
#[ignore = "requires populated handler binaries via scripts/populate-bundles.sh"]
async fn opening_another_project_admits_an_independent_attachment() {
    let (_tmp, state) = build_test_state_with_live_bundles();
    let first = tempfile::TempDir::new().expect("first project");
    let second = tempfile::TempDir::new().expect("second project");
    let operator_ctx = local_operator_context(&state, vec!["*".into()]);
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
            user_principal_id: None,
        },
        operator_ctx,
        state.clone(),
    )
    .await
    .expect("mint first project session");
    let first_session_id = minted["session_id"].as_str().unwrap();
    let activated = ryeos_ui::handlers::ui_launch::handle(
        json!({"token": minted["token"]}),
        HandlerContext::anonymous(),
        state.clone(),
    )
    .await
    .expect("activate first project session");
    assert_eq!(activated["session_id"], first_session_id);
    let first_session = ryeos_ui::state::get_ui_state(&state)
        .unwrap()
        .browser_sessions
        .get_session(first_session_id)
        .expect("first session retained");
    let first_coordinate = first_session
        .attachments
        .get(&first_session.surface_attachment_id)
        .expect("first attachment")
        .coordinate();
    let predecessor_context = HandlerContext::new(
        format!("session:{first_session_id}"),
        vec!["*".into()],
        false,
    );
    let predecessor_seat = (ryeos_ui::handlers::ui_seat::OPEN_DESCRIPTOR.handler)(
        json!({
            "binding_attachment_id": first_coordinate.binding_attachment_id,
            "binding_generation": first_coordinate.binding_generation,
            "binding_digest": first_coordinate.binding_digest,
        }),
        predecessor_context.clone(),
        state.clone(),
    )
    .await
    .expect("open predecessor seat");

    let opened = (ryeos_ui::handlers::ui_invocations_dispatch::DESCRIPTOR.handler)(
        json!({
            "binding_attachment_id": first_coordinate.binding_attachment_id,
            "binding_generation": first_coordinate.binding_generation,
            "binding_digest": first_coordinate.binding_digest,
            "coordinate": {
                "kind": "affordance",
                "view_ref": "view:ryeos/projects/list",
                "affordance_id": "open-project"
            },
            "payload": {
                "kind": "selection",
                "record": {"local_id": project_ids[1]}
            }
        }),
        predecessor_context,
        state.clone(),
    )
    .await
    .expect("open second project");
    let transition = &opened["result"]["ui_transition"];
    assert_eq!(transition["kind"], "admit_binding_attachment");
    let attachment_id = transition["attachment"]["binding_attachment_id"]
        .as_str()
        .expect("attachment id");
    let ui = ryeos_ui::state::get_ui_state(&state).unwrap();
    let retained = ui
        .browser_sessions
        .get_session(first_session_id)
        .expect("session retained");
    let successor = retained
        .attachments
        .get(attachment_id)
        .expect("new attachment");
    assert_eq!(
        successor.project_query_identity.as_deref(),
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
        first_coordinate.binding_digest
    );
    let predecessor_seat = state
        .state_store
        .get_thread(predecessor_seat["thread_id"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(predecessor_seat.status, "running");
}
