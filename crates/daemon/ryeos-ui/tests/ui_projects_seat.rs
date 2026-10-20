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
        test_state::live_presentation_with_surface(&state, "surface:ryeos/ui/base").await;
    let ui = ryeos_ui::state::get_ui_state(&state).unwrap();
    let session = ui.browser_sessions.get_session(&session_id).unwrap();
    let second = tempfile::TempDir::new().expect("second project");
    let operator = HandlerContext::new(
        session.principal_id.clone(),
        session.granted_caps.clone(),
        true,
    );
    let added = ryeos_ui::handlers::ui_projects::handle_projects_add(
        json!({"root":second.path()}),
        operator,
        state.clone(),
    )
    .await
    .expect("register project for the same retained principal");
    let local_id = added["project"]["local_id"].as_str().unwrap();
    let request_id = uuid::Uuid::new_v4().to_string();
    let request = json!({
        "presentation_operation_id":request_id,
        "producer":producer,
        "binding":{
            "binding_attachment_id":coordinate.binding_attachment_id,
            "binding_generation":coordinate.binding_generation,
            "binding_digest":coordinate.binding_digest,
            "coordinate":{"kind":"affordance","view_ref":"view:ryeos/projects/list","affordance_id":"open-project"},
            "payload":{"kind":"selection","record":{
                "local_id":local_id, "surface_ref":"surface:ryeos/ui/project",
                "creation_request_id":request_id
            }}
        }
    });
    let membership = &session.presentations[&producer.presentation_id];
    let origin = ryeos_client_base::ids::RyeOsViewInstanceKey::surface_tile(
        &ryeos_client_base::ids::PresentationId::try_new(producer.presentation_id.clone()).unwrap(),
        &ryeos_client_base::ids::SurfaceInstanceId::try_new(
            membership
                .surfaces
                .values()
                .find(|surface| surface.coordinate == coordinate)
                .unwrap()
                .surface_instance_id
                .clone(),
        )
        .unwrap(),
        ryeos_client_base::ids::TileId::new(1),
    );
    let maximum = state
        .node_config
        .routes
        .iter()
        .find(|route| {
            route.response.source.as_deref()
                == Some(ryeos_ui::handlers::ui_invocations_dispatch::DESCRIPTOR.service_ref)
        })
        .unwrap()
        .limits
        .body_bytes_max;
    let prepared = json!({"operation_id":request_id,"producer":producer,
    "root_binding":membership.root_coordinate,"body":{"kind":"admission","purpose":"open_surface","surface_open_target":{"selection":{"kind":"authored","surface_ref":"surface:ryeos/ui/project"},"placement":{"kind":"new_surface"}},"particular_selection":null,"invocation_origin":origin,
    "request":request["binding"],"success_notice":null}, "request_bounds":{
        "max_request_bytes":maximum, "max_input_bytes":maximum,
        "max_seat_append_bytes":state.node_config.routes.iter().find(|route| route.response.source.as_deref() == Some(ryeos_ui::handlers::ui_seat::APPEND_DESCRIPTOR.service_ref)).unwrap().limits.body_bytes_max},
    });
    let events = json!([{"engine_seq":0,"event_type":"seat.presentation_operation.prepared","payload":prepared}]);
    (ryeos_ui::handlers::ui_seat::APPEND_DESCRIPTOR.handler)(json!({
        "presentation_id":producer.presentation_id,"thread_id":producer.thread_id,
        "producer_incarnation":producer.producer_incarnation,"operation_id":uuid::Uuid::new_v4().to_string(),
        "first_engine_seq":0,"last_engine_seq":0,"event_count":1,
        "payload_digest":ryeos_client_base::ui::seat_payload_digest(&events).unwrap(),"events":events,
    }),ctx.clone(),state.clone()).await.expect("acknowledge real prepared operation before admission");
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
