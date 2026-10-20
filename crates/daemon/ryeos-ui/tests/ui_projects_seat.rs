//! Seat-auth gate coverage for the RyeOS UI project registry handlers.
//!
//! The project registry is node-global and its gates accept both browser
//! sessions and verified operators through `seat_auth::require_seat_caller`.

use ryeos_ui::test_support as test_state;
use test_state::{build_test_state, build_test_state_with_live_bundles, local_operator_context};

use serde_json::json;
use std::sync::Arc;

#[tokio::test]
async fn verified_operator_passes_projects_read_gate() {
    let (_tmp, _state_owner, state) = build_test_state();
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
    let (_tmp, _state_owner, state) = build_test_state();
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
#[ignore = "requires populated handler binaries, current signed project-open affordance and development publisher key"]
async fn opening_another_project_admits_an_independent_surface_without_replacing_the_seat() {
    let (_tmp, _state_owner, state) = build_test_state_with_live_bundles();
    let state = Arc::new(state);
    let (session_id, ctx, producer, coordinate) =
        test_state::live_shipped_presentation(&state).await;
    let ui = ryeos_ui::state::get_ui_state(&state).unwrap();
    let session = ui.browser_sessions.get_session(&session_id).unwrap();
    let second = tempfile::TempDir::new().expect("second project");
    // The session's principal is not the node operator: its registry is the
    // principal-owned one, reached only through its retained root attachment.
    let root_attachment = session.attachments[&session.root_attachment_id].clone();
    let added = ryeos_ui::seat_auth::with_compiled_ui_attachment(
        root_attachment,
        ryeos_ui::handlers::ui_projects::handle_projects_add(
            json!({"root":second.path()}),
            ctx.clone(),
            state.clone(),
        ),
    )
    .await
    .expect("register project for the same retained principal");
    let local_id = added["project"]["local_id"].as_str().unwrap();
    let membership = &session.presentations[&producer.presentation_id];
    let origin = ryeos_client_base::ids::RyeOsViewInstanceKey::shared_dialog(
        &ryeos_client_base::ids::PresentationId::try_new(producer.presentation_id.clone()).unwrap(),
        "projects",
    )
    .unwrap();
    let body_limit = |service_ref: &str| {
        state
            .node_config
            .routes
            .iter()
            .find(|route| route.response.source.as_deref() == Some(service_ref))
            .unwrap()
            .limits
            .body_bytes_max
    };
    let maximum = body_limit(ryeos_ui::handlers::ui_invocations_dispatch::DESCRIPTOR.service_ref);
    let mut prepared: ryeos_client_base::ui::admission_operation::UiPreparedPresentationOperation =
        serde_json::from_value(json!({
            "schema": 2, "prepared_engine_seq": 0,
            "operation_id": "00000000-0000-4000-8000-000000000000",
            "producer": producer,
            "root_binding": membership.root_coordinate,
            "request_bounds": {
                "max_request_bytes": maximum, "max_input_bytes": maximum,
                "max_seat_append_bytes": body_limit(
                    ryeos_ui::handlers::ui_seat::APPEND_DESCRIPTOR.service_ref),
            },
            "body": {
                "kind": "admission", "purpose": "open_surface",
                "surface_open_target": {
                    "selection": {"kind": "authored", "surface_ref": "surface:ryeos/ui/project"},
                    "placement": {"kind": "new_surface"},
                },
                "particular_selection": null,
                "invocation_origin": origin,
                "request": {
                    "binding_attachment_id": coordinate.binding_attachment_id,
                    "binding_generation": coordinate.binding_generation,
                    "binding_digest": coordinate.binding_digest,
                    "coordinate": {"kind": "affordance",
                        "view_ref": "view:ryeos/projects/list", "affordance_id": "open-project"},
                    "payload": {"kind": "selection", "record": {
                        "local_id": local_id, "surface_ref": "surface:ryeos/ui/project",
                    }},
                },
                "success_notice": null,
            },
        }))
        .unwrap();
    prepared.assign_identity().unwrap();
    let request_id = prepared.operation_id.clone();
    let request = json!({
        "presentation_operation_id": request_id,
        "producer": producer,
        "binding": prepared.admission().unwrap().request,
    });
    let events = json!([{
        "engine_seq": 0, "event_type": "seat.presentation_operation.prepared",
        "payload": prepared,
    }]);
    let payload_digest = ryeos_client_base::ui::seat_payload_digest(&events).unwrap();
    let operation_id = ryeos_client_base::ui::admission_operation::UiSeatAppendIdentity {
        presentation_id: producer.presentation_id.clone(),
        chain_root_id: producer.thread_id.clone(),
        thread_id: producer.thread_id.clone(),
        producer_incarnation: producer.producer_incarnation.clone(),
        first_engine_seq: 0,
        last_engine_seq: 0,
        event_count: 1,
        payload_digest: payload_digest.clone(),
    }
    .derive_identity()
    .unwrap();
    (ryeos_ui::handlers::ui_seat::APPEND_DESCRIPTOR.handler)(
        json!({
            "presentation_id": producer.presentation_id, "thread_id": producer.thread_id,
            "producer_incarnation": producer.producer_incarnation, "operation_id": operation_id,
            "first_engine_seq": 0, "last_engine_seq": 0, "event_count": 1,
            "payload_digest": payload_digest, "events": events,
        }),
        ctx.clone(),
        state.clone(),
    )
    .await
    .expect("acknowledge real prepared operation before admission");
    let opened = (ryeos_ui::handlers::ui_invocations_dispatch::DESCRIPTOR.handler)(
        request.clone(),
        ctx.clone(),
        state.clone(),
    )
    .await
    .expect("open explicitly selected Surface on registered second project");
    let transition = &opened["result"]["ui_transition"];
    assert_eq!(transition["kind"], "open_surface");
    assert_eq!(transition["presentation_id"], producer.presentation_id);
    let attachment_id = transition["attachment"]["binding_attachment_id"]
        .as_str()
        .unwrap();
    let retained = ui.browser_sessions.get_session(&session_id).unwrap();
    let successor = retained.attachments.get(attachment_id).unwrap();
    assert_eq!(successor.registered_project_id.as_deref(), Some(local_id));
    assert_eq!(
        successor.project_query_identity.as_deref(),
        Some(second.path().canonicalize().unwrap().to_str().unwrap())
    );
    assert!(
        retained
            .attachments
            .contains_key(&coordinate.binding_attachment_id)
    );
    let membership = &retained.presentations[&producer.presentation_id];
    assert_eq!(
        membership.thread_id.as_deref(),
        Some(producer.thread_id.as_str())
    );
    assert_eq!(membership.surfaces.len(), 2);
    assert_eq!(
        state
            .state_store
            .get_thread(&producer.thread_id)
            .unwrap()
            .unwrap()
            .status,
        "running"
    );
    let retried = (ryeos_ui::handlers::ui_invocations_dispatch::DESCRIPTOR.handler)(
        request,
        ctx,
        state.clone(),
    )
    .await
    .expect("exact creation retry");
    assert_eq!(
        retried["result"]["ui_transition"]["surface"],
        transition["surface"]
    );
    assert_eq!(
        retried["result"]["ui_transition"]["attachment"],
        transition["attachment"]
    );
}

#[tokio::test]
async fn project_open_refuses_the_predecessor_request_shape() {
    let (_tmp, _state_owner, state) = build_test_state();
    let operator_ctx = local_operator_context(&state, vec!["*".into()]);
    let error = ryeos_ui::handlers::ui_projects::handle_projects_open(
        json!({"local_id":"prj_x","surface_ref":"surface:ryeos/ui/base"}),
        operator_ctx.clone(),
        Arc::new(state.clone()),
    )
    .await
    .expect_err("surface_ref is not part of the opening request");
    assert!(format!("{error:#}").contains("unknown field"), "{error:#}");
    let error = ryeos_ui::handlers::ui_projects::handle_projects_open(
        json!({
            "local_id":"prj_x",
            "selection":{"kind":"authored","surface_ref":"surface:ryeos/ui/base"},
            "placement":{"kind":"new_surface"},
            "creation_request_id":"00000000-0000-4000-8000-000000000003",
        }),
        operator_ctx,
        Arc::new(state),
    )
    .await
    .expect_err("an operator is not a browser presentation association");
    assert!(
        format!("{error:#}").contains("compiled browser UI dispatch required"),
        "{error:#}"
    );
}
