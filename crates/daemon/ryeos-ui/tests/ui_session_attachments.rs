mod test_state;

use std::sync::Arc;

use ryeos_app::handler_context::HandlerContext;
use ryeos_ui::browser_session::{BindingAttachmentCoordinate, LaunchContext};
use ryeos_ui::state::get_ui_state;

use test_state::{build_test_state, launch_context, mint_launch};

fn context(surface: &str) -> LaunchContext {
    launch_context(
        surface,
        None,
        ryeos_ui::compiled_binding::EffectiveUiPosture::Interactive,
        None,
    )
}

fn request(
    coordinate: &BindingAttachmentCoordinate,
    producer: &ryeos_client_base::ui::UiPresentationProducer,
) -> serde_json::Value {
    serde_json::json!({
        "binding_attachment_id": coordinate.binding_attachment_id,
        "binding_generation": coordinate.binding_generation,
        "binding_digest": coordinate.binding_digest,
        "producer": producer,
    })
}

fn activate(state: &ryeos_app::state::AppState, launch: LaunchContext) -> (String, HandlerContext) {
    let (session_id, token) = mint_launch(state, launch);
    get_ui_state(state)
        .unwrap()
        .browser_sessions
        .consume_launch_token(&token)
        .expect("activate session");
    let handler = HandlerContext::new(format!("session:{session_id}"), vec![], false);
    (session_id, handler)
}

#[tokio::test]
#[ignore = "requires populated signed bundle handler binaries and development publisher key"]
async fn detach_refuses_root_attachment_with_current_producer() {
    let (_tmp, state) = test_state::build_test_state_with_live_bundles();
    let (_session_id, ctx, producer, coordinate) = test_state::live_presentation(&state).await;
    let result = (ryeos_ui::handlers::ui_session_attachments::DETACH_DESCRIPTOR.handler)(
        request(&coordinate, &producer),
        ctx,
        Arc::new(state),
    )
    .await;
    assert!(format!("{:#}", result.unwrap_err()).contains("root_attachment_immutable"));
}

#[tokio::test]
async fn surface_open_requires_existing_presentation_producer_and_rejects_browser_project_authority()
 {
    let (_tmp, state) = build_test_state();
    let (id, ctx) = activate(&state, context("surface:ryeos/ui/base"));
    let session = get_ui_state(&state)
        .unwrap()
        .browser_sessions
        .get_session(&id)
        .unwrap();
    let params = serde_json::json!({
        "creation_request_id": "00000000-0000-4000-8000-000000000002",
        "selection": {"kind":"authored", "surface_ref": "surface:ryeos/ui/base"},
    });
    let descriptor = &ryeos_ui::handlers::ui_session_attachments::OPEN_SURFACE_DESCRIPTOR;
    (descriptor.handler)(params.clone(), ctx.clone(), Arc::new(state.clone()))
        .await
        .expect_err("a cookie and root attachment do not create presentation producer authority");
    let mut predecessor = params.clone();
    predecessor["root_binding"] =
        serde_json::to_value(session.attachments[&session.root_attachment_id].coordinate())
            .unwrap();
    predecessor["producer"] = serde_json::json!({
        "presentation_id":"00000000-0000-4000-8000-000000000001",
        "thread_id":"T-not-a-seat", "producer_incarnation":"11".repeat(32)
    });
    let error = (descriptor.handler)(predecessor, ctx.clone(), Arc::new(state.clone()))
        .await
        .expect_err("predecessor browser-selected root/producer grammar is rejected");
    assert!(format!("{error:#}").contains("unknown field"));
    let mut project_params = params.clone();
    project_params["project_path"] = serde_json::json!("/another-project");
    let error = (descriptor.handler)(project_params, ctx, Arc::new(state.clone()))
        .await
        .expect_err("Surface open never accepts a browser-selected project path");
    assert!(format!("{error:#}").contains("unknown field"));
    (descriptor.handler)(
        params,
        HandlerContext::new("fp:operator".into(), vec!["*".into()], true),
        Arc::new(state.clone()),
    )
    .await
    .expect_err("operator scope is not a browser presentation association");
    let after = get_ui_state(&state)
        .unwrap()
        .browser_sessions
        .get_session(&id)
        .unwrap();
    assert_eq!(after.attachments.len(), 1);
    assert!(after.presentations.is_empty());
}

#[tokio::test]
#[ignore = "requires populated signed bundle handler binaries and development publisher key"]
async fn detach_is_session_exact_fences_dispatch_and_replays() {
    let (_tmp, state) = test_state::build_test_state_with_live_bundles();
    let (owner_id, owner_ctx, producer, coordinate) =
        test_state::live_presentation_with_surface(&state, "surface:ryeos/ui/base").await;
    let (_other_id, other_ctx, other_producer, _other_root) =
        test_state::live_presentation(&state).await;

    let mut missing = request(&coordinate, &producer);
    missing.as_object_mut().unwrap().remove("producer");
    (ryeos_ui::handlers::ui_session_attachments::DETACH_DESCRIPTOR.handler)(
        missing,
        owner_ctx.clone(),
        Arc::new(state.clone()),
    )
    .await
    .expect_err("attachment authority does not substitute a current producer proof");
    let mut stale = producer.clone();
    stale.producer_incarnation = "99".repeat(32);
    (ryeos_ui::handlers::ui_session_attachments::DETACH_DESCRIPTOR.handler)(
        request(&coordinate, &stale),
        owner_ctx.clone(),
        Arc::new(state.clone()),
    )
    .await
    .expect_err("noncurrent producer cannot revoke a live attachment");

    let other = (ryeos_ui::handlers::ui_session_attachments::DETACH_DESCRIPTOR.handler)(
        request(&coordinate, &other_producer),
        other_ctx,
        Arc::new(state.clone()),
    )
    .await;
    let other = other.expect("foreign coordinate is an opaque idempotent no-op");
    assert_eq!(other["detached"], false);
    assert!(
        get_ui_state(&state)
            .unwrap()
            .browser_sessions
            .resolve_attachment(&owner_id, &coordinate)
            .is_ok(),
        "another session's no-op must leave owner authority untouched"
    );

    let detached = (ryeos_ui::handlers::ui_session_attachments::DETACH_DESCRIPTOR.handler)(
        request(&coordinate, &producer),
        owner_ctx.clone(),
        Arc::new(state.clone()),
    )
    .await
    .expect("detach exact attachment");
    assert_eq!(detached["detached"], true);
    assert!(
        get_ui_state(&state)
            .unwrap()
            .browser_sessions
            .admit_attachment_dispatch(&owner_id, &coordinate)
            .is_err(),
        "revocation must fence future dispatch admission"
    );

    let replay = (ryeos_ui::handlers::ui_session_attachments::DETACH_DESCRIPTOR.handler)(
        request(&coordinate, &producer),
        owner_ctx.clone(),
        Arc::new(state.clone()),
    )
    .await
    .expect("lost-response replay");
    assert_eq!(replay["detached"], false);
    assert_eq!(
        state
            .state_store
            .get_thread(&producer.thread_id)
            .unwrap()
            .unwrap()
            .status,
        "running",
        "attachment revocation does not own presentation seat settlement"
    );
    let touched = (ryeos_ui::handlers::ui_seat::TOUCH_DESCRIPTOR.handler)(
        serde_json::to_value(&producer).unwrap(),
        owner_ctx,
        Arc::new(state),
    )
    .await
    .expect("same producer remains current after attachment revocation");
    assert_eq!(touched["touched"], true);
}
