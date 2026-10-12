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
#[ignore = "requires populated handler binaries, current signed project-open affordance and development publisher key"]
async fn opening_another_project_admits_an_independent_surface_without_replacing_the_seat() {
    let (_tmp, state) = build_test_state_with_live_bundles();
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
async fn project_open_cannot_choose_a_project_with_operator_or_cookie_alone() {
    let (_tmp, state) = build_test_state();
    let state = Arc::new(state);
    let error = ryeos_ui::handlers::ui_projects::handle_projects_open(
        json!({"local_id":"registered", "selection":{"kind":"authored","surface_ref":"surface:test/development"},
            "placement":{"kind":"new_surface"},
            "creation_request_id":uuid::Uuid::new_v4().to_string()}),
        local_operator_context(&state, vec!["*".into()]),
        state.clone(),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error.downcast_ref::<ryeos_app::handler_error::HandlerError>(),
        Some(ryeos_app::handler_error::HandlerError::Forbidden(_))
    ));
    assert!(
        error
            .to_string()
            .contains("compiled browser UI dispatch required")
    );
    let (session_id, token) = test_state::mint_launch(
        &state,
        test_state::launch_context(
            "surface:test/development",
            None,
            ryeos_ui::compiled_binding::EffectiveUiPosture::Interactive,
            None,
        ),
    );
    assert_eq!(
        ryeos_ui::state::get_ui_state(&state)
            .unwrap()
            .browser_sessions
            .consume_launch_token(&token),
        Some(session_id.clone()),
    );
    let error = ryeos_ui::handlers::ui_projects::handle_projects_open(
        json!({"local_id":"registered", "selection":{"kind":"authored","surface_ref":"surface:test/development"},
            "placement":{"kind":"new_surface"},
            "creation_request_id":uuid::Uuid::new_v4().to_string()}),
        HandlerContext::new(format!("session:{session_id}"), vec!["*".into()], false),
        state,
    ).await.unwrap_err();
    assert!(matches!(
        error.downcast_ref::<ryeos_app::handler_error::HandlerError>(),
        Some(ryeos_app::handler_error::HandlerError::Forbidden(_))
    ));
    assert!(
        error
            .to_string()
            .contains("browser data access requires a compiled binding coordinate")
    );
}

#[tokio::test]
#[ignore = "requires populated handlers, freshly signed UI definitions and development publisher key"]
async fn particular_resume_reuses_the_acknowledged_original_operation_and_membership() {
    let (_tmp, state) = build_test_state_with_live_bundles();
    let state = Arc::new(state);
    let (session_id, ctx, producer, coordinate) =
        test_state::live_presentation_with_surface(&state, "surface:ryeos/ui/assistant").await;
    let ui = ryeos_ui::state::get_ui_state(&state).unwrap();
    let session = ui.browser_sessions.get_session(&session_id).unwrap();
    let project = tempfile::TempDir::new().unwrap();
    let operator = HandlerContext::new(
        session.principal_id.clone(),
        session.granted_caps.clone(),
        true,
    );
    let registered = ryeos_ui::handlers::ui_projects::handle_projects_add(
        json!({"root":project.path()}),
        operator.clone(),
        state.clone(),
    )
    .await
    .unwrap();
    let saved = json!({"id":"particular","name":"Particular project",
        "composition":{"schema":2,"id":"particular","title":"Particular project",
            "root":{"type":"group","views":["view:ryeos/project/files"],"active":0}},
        "relationships":[{"mount":{"kind":"tile","index":0},"source":{"mode":"follow_own_set"}}],
        "context":{"surface_ref":"surface:ryeos/ui/assistant","project":{"local_id":registered["project"]["local_id"]}}});
    ryeos_ui::handlers::ui_projects::handle_config_update(
        json!({"particular_view_set_library":{"expected_revision":0,"particular_view_sets":[saved]}}),
        operator.clone(),state.clone()).await.unwrap();
    let observed =
        ryeos_ui::handlers::ui_projects::handle_config_get(json!({}), operator, state.clone())
            .await
            .unwrap();
    let mut record = observed["particular_view_sets"][0].clone();
    assert_eq!(record["expected_revision"], 1);
    assert_eq!(record["surface_ref"], "surface:ryeos/ui/assistant");
    let operation_id = uuid::Uuid::new_v4().to_string();
    record["creation_request_id"] = json!(operation_id);
    let request = json!({"presentation_operation_id":operation_id,"producer":producer,
        "binding":{"binding_attachment_id":coordinate.binding_attachment_id,
            "binding_generation":coordinate.binding_generation,"binding_digest":coordinate.binding_digest,
            "coordinate":{"kind":"affordance","view_ref":"view:ryeos/view-sets/library","affordance_id":"resume-particular"},
            "payload":{"kind":"selection","record":record}}});
    let membership = &session.presentations[&producer.presentation_id];
    let surface_id = &membership
        .surfaces
        .values()
        .find(|surface| surface.coordinate == coordinate)
        .unwrap()
        .surface_instance_id;
    let origin = ryeos_client_base::ids::RyeOsViewInstanceKey::surface_tile(
        &ryeos_client_base::ids::PresentationId::try_new(producer.presentation_id.clone()).unwrap(),
        &ryeos_client_base::ids::SurfaceInstanceId::try_new(surface_id.clone()).unwrap(),
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
    let events = json!([{"engine_seq":0,"event_type":"seat.presentation_operation.prepared",
        "payload":{"operation_id":operation_id,"producer":producer,
            "root_binding":membership.root_coordinate,"body":{"kind":"admission","purpose":"resume_particular_view_set","surface_open_target":null,"particular_selection":{"kind":"view_set","id":"particular","expected_revision":1,"surface_ref":"surface:ryeos/ui/assistant"},"invocation_origin":origin,"request":request["binding"],"success_notice":null},
            "request_bounds":{"max_request_bytes":maximum,"max_input_bytes":maximum,
                "max_seat_append_bytes":state.node_config.routes.iter().find(|route| route.response.source.as_deref() == Some(ryeos_ui::handlers::ui_seat::APPEND_DESCRIPTOR.service_ref)).unwrap().limits.body_bytes_max}}}]);
    (ryeos_ui::handlers::ui_seat::APPEND_DESCRIPTOR.handler)(json!({"presentation_id":producer.presentation_id,
        "thread_id":producer.thread_id,"producer_incarnation":producer.producer_incarnation,
        "operation_id":uuid::Uuid::new_v4().to_string(),"first_engine_seq":0,"last_engine_seq":0,"event_count":1,
        "payload_digest":ryeos_client_base::ui::seat_payload_digest(&events).unwrap(),"events":events}),ctx.clone(),state.clone())
        .await.expect("acknowledge original saved resume operation");
    let opened = (ryeos_ui::handlers::ui_invocations_dispatch::DESCRIPTOR.handler)(
        request.clone(),
        ctx.clone(),
        state.clone(),
    )
    .await
    .expect("real retained particular admission");
    let retried = (ryeos_ui::handlers::ui_invocations_dispatch::DESCRIPTOR.handler)(
        request,
        ctx,
        state.clone(),
    )
    .await
    .expect("same prepared operation retries without new Surface");
    assert_eq!(opened["result"], retried["result"]);
    assert_eq!(
        opened["result"]["ui_transition"]["creation_request_id"],
        operation_id
    );
    assert_eq!(
        ui.browser_sessions
            .get_session(&session_id)
            .unwrap()
            .presentations[&producer.presentation_id]
            .surfaces
            .len(),
        2
    );
}

#[tokio::test]
async fn project_open_requires_explicit_surface_and_canonical_creation_identity() {
    let (_tmp, state) = build_test_state();
    let state = Arc::new(state);
    for input in [
        json!({"local_id":"registered"}),
        json!({"local_id":"registered","surface_ref":"surface:test/development","creation_request_id":uuid::Uuid::new_v4().to_string()}),
        json!({"local_id":"registered","selection":{"kind":"authored","surface_ref":"surface:test/development"},"creation_request_id":"not-a-uuid"}),
    ] {
        assert!(
            ryeos_ui::handlers::ui_projects::handle_projects_open(
                input,
                local_operator_context(&state, vec!["*".into()]),
                state.clone(),
            )
            .await
            .is_err()
        );
    }
}
