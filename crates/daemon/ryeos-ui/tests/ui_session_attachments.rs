use ryeos_ui::test_support as test_state;

use std::sync::Arc;

use ryeos_app::handler_context::HandlerContext;
use ryeos_ui::browser_session::{
    BindingAttachmentCandidate, BindingAttachmentCoordinate, LaunchContext,
};
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

fn request(coordinate: &BindingAttachmentCoordinate) -> serde_json::Value {
    serde_json::json!({
        "binding_attachment_id": coordinate.binding_attachment_id,
        "binding_generation": coordinate.binding_generation,
        "binding_digest": coordinate.binding_digest,
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

fn secondary_attachment(
    state: &ryeos_app::state::AppState,
    session_id: &str,
) -> BindingAttachmentCoordinate {
    let ui = get_ui_state(state).unwrap();
    let session = ui.browser_sessions.get_session(session_id).unwrap();
    let origin = session
        .attachments
        .get(&session.root_attachment_id)
        .unwrap()
        .coordinate();
    let mut candidate = context("surface:ryeos/test/secondary");
    Arc::make_mut(&mut candidate.compiled_binding)
        .binding
        .node_policy_generation_digest = state.node_policy.generation_digest().to_owned();
    ui.browser_sessions
        .publish_attachment(
            session_id,
            &origin,
            BindingAttachmentCandidate {
                registered_project_id: None,
                compiled_binding: candidate.compiled_binding,
                presentation: candidate.presentation,
                project_authority: None,
            },
            16,
            state.node_policy.generation_digest(),
        )
        .unwrap()
        .coordinate()
}

#[tokio::test]
async fn detach_refuses_surface_attachment() {
    let (_tmp, _state_owner, state) = build_test_state();
    let (session_id, ctx) = activate(&state, context("surface:ryeos/ui/base"));
    let session = get_ui_state(&state)
        .unwrap()
        .browser_sessions
        .get_session(&session_id)
        .unwrap();
    let coordinate = session
        .attachments
        .get(&session.root_attachment_id)
        .unwrap()
        .coordinate();
    let result = (ryeos_ui::handlers::ui_session_attachments::DETACH_DESCRIPTOR.handler)(
        request(&coordinate),
        ctx,
        Arc::new(state),
    )
    .await;
    assert!(format!("{:#}", result.unwrap_err()).contains("surface_attachment_immutable"));
}

#[tokio::test]
async fn detach_is_session_exact_fences_dispatch_and_replays() {
    let (_tmp, _state_owner, state) = build_test_state();
    let (owner_id, owner_ctx) = activate(&state, context("surface:ryeos/ui/base"));
    let coordinate = secondary_attachment(&state, &owner_id);
    let (_other_id, other_ctx) = activate(&state, context("surface:ryeos/ui/base"));

    let other = (ryeos_ui::handlers::ui_session_attachments::DETACH_DESCRIPTOR.handler)(
        request(&coordinate),
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
        request(&coordinate),
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
        request(&coordinate),
        owner_ctx,
        Arc::new(state),
    )
    .await
    .expect("lost-response replay");
    assert_eq!(replay["detached"], false);
}

#[tokio::test]
async fn surface_open_requires_existing_presentation_producer_and_rejects_browser_project_authority()
 {
    let (_tmp, _state_owner, state) = build_test_state();
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
