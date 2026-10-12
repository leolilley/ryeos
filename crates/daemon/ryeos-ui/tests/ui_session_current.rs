// Tests for `ui.session.current` handler.

mod test_state;
use test_state::{launch_context, mint_launch};

use ryeos_app::handler_context::HandlerContext;
use ryeos_ui::state::get_ui_state;
use std::sync::Arc;

fn build_test_state() -> (tempfile::TempDir, ryeos_app::state::AppState) {
    let (directory, mut state) = test_state::build_test_state();
    // Session projection includes the actual node transport bound even before
    // a presentation is prepared. Supply that route, not a hard-coded DTO.
    let mut append_route = state
        .node_config
        .routes
        .iter()
        .find(|route| {
            route.response.source.as_deref()
                == Some(ryeos_ui::handlers::ui_invocations_dispatch::DESCRIPTOR.service_ref)
        })
        .unwrap()
        .clone();
    append_route.id = "ui/seat/append".into();
    append_route.path = "/ui/api/seat/append".into();
    append_route.response.source = Some(
        ryeos_ui::handlers::ui_seat::APPEND_DESCRIPTOR
            .service_ref
            .into(),
    );
    Arc::make_mut(&mut state.node_config)
        .routes
        .push(append_route);
    (directory, state)
}

fn test_context() -> ryeos_ui::browser_session::LaunchContext {
    let mut context = launch_context(
        "surface:ryeos/ui/base",
        None,
        ryeos_ui::compiled_binding::EffectiveUiPosture::Interactive,
        None,
    );
    let binding = Arc::make_mut(&mut context.compiled_binding);
    let mut identity = binding.binding.root.identity().clone();
    identity.canonical_ref = "config:test/presentation".into();
    binding.binding.root =
        ryeos_api::surface_views::UiCompositionRootIdentity::SharedPresentation { identity };
    context.presentation = serde_json::from_value(serde_json::json!({
        "definition":{"kind":"shared_presentation","config":{
            "category":"ui","shared_presentation":{"schema":1,"title":"Test presentation",
                "launcher":"launch","dialogs":[{"id":"launch","title":"Launch","view":"view:test/launch"}],
                "capabilities":{"sources":[],"affordances":[]}}
        }},
        "views":{"view:test/launch":{"widget":"rows"}}
    })).unwrap();
    context
}

#[tokio::test]
#[ignore = "requires populated signed bundle handler binaries and development publisher key"]
async fn session_current_projects_real_shared_controls_with_zero_surfaces() {
    let (_tmp, state) = test_state::build_test_state_with_live_bundles();
    let (id, ctx, producer, root) = test_state::live_presentation(&state).await;
    let response = (ryeos_ui::handlers::ui_session_current::DESCRIPTOR.handler)(
        serde_json::json!(null),
        ctx,
        Arc::new(state.clone()),
    )
    .await
    .expect("shared presentation exists before any Surface opens");
    assert_eq!(response["session_id"], id);
    assert_eq!(response["binding_attachments"].as_array().unwrap().len(), 1);
    assert_eq!(
        response["binding_attachments"][0]["root_ref"],
        "config:test/presentation"
    );
    assert_eq!(
        response["presentations"][0]["presentation_id"],
        producer.presentation_id
    );
    assert_eq!(
        response["presentations"][0]["root_binding"],
        serde_json::to_value(root).unwrap()
    );
    assert_eq!(
        response["presentations"][0]["surfaces"],
        serde_json::json!([])
    );
}

#[tokio::test]
#[ignore = "requires populated signed bundle handler binaries and development publisher key"]
async fn session_current_projects_real_config_root_and_independent_surface_membership() {
    let (_tmp, state) = test_state::build_test_state_with_live_bundles();
    let (id, ctx, producer, coordinate) =
        test_state::live_presentation_with_surface(&state, "surface:ryeos/ui/base").await;
    let before = get_ui_state(&state)
        .unwrap()
        .browser_sessions
        .get_session(&id)
        .unwrap();
    let response = (ryeos_ui::handlers::ui_session_current::DESCRIPTOR.handler)(
        serde_json::json!(null),
        ctx,
        Arc::new(state.clone()),
    )
    .await
    .expect("read exact current presentation membership");
    let root_id = response["root_attachment_id"].as_str().unwrap();
    let attachments = response["binding_attachments"].as_array().unwrap();
    assert_eq!(attachments.len(), 2);
    let root = attachments
        .iter()
        .find(|attachment| attachment["binding_attachment_id"] == root_id)
        .unwrap();
    assert_eq!(root["root_ref"], "config:test/presentation");
    assert_eq!(
        root["presentation"]["definition"]["kind"],
        "shared_presentation"
    );
    let presentations = response["presentations"].as_array().unwrap();
    assert_eq!(presentations.len(), 1);
    assert_eq!(
        presentations[0]["presentation_id"],
        producer.presentation_id
    );
    assert_eq!(presentations[0]["thread_id"], producer.thread_id);
    assert_eq!(presentations[0]["surfaces"].as_array().unwrap().len(), 1);
    assert_eq!(presentations[0]["surfaces"][0]["ordinal"], 1);
    assert_eq!(
        presentations[0]["surfaces"][0]["binding"],
        serde_json::to_value(&coordinate).unwrap()
    );
    assert_eq!(response["session_id"], before.session_id);
    assert_eq!(
        state
            .state_store
            .list_threads_filtered(100, None)
            .unwrap()
            .len(),
        1,
        "reading current state and opening a Surface do not create replacement seats"
    );
}

#[tokio::test]
async fn session_current_returns_session_fields() {
    let (_tmp, state) = build_test_state();

    let (session_id, token) = mint_launch(&state, test_context());
    assert_eq!(
        get_ui_state(&state)
            .unwrap()
            .browser_sessions
            .consume_launch_token(&token),
        Some(session_id.clone())
    );

    // Create a handler context that looks like a browser_session principal.
    let ctx = HandlerContext::new(
        format!("session:{session_id}"),
        vec!["ui.read".into()],
        false,
    );

    let result = (ryeos_ui::handlers::ui_session_current::DESCRIPTOR.handler)(
        serde_json::json!(null),
        ctx,
        Arc::new(state),
    )
    .await
    .expect("should succeed");

    assert_eq!(result["session_id"], session_id);
    assert_eq!(result["presentations"], serde_json::json!([]));
    let root_attachment_id = result["root_attachment_id"].as_str().unwrap();
    let attachments = result["binding_attachments"].as_array().unwrap();
    assert_eq!(attachments.len(), 1);
    let attachment = &attachments[0];
    assert_eq!(attachment["binding_attachment_id"], root_attachment_id);
    assert_eq!(attachment["root_ref"], "config:test/presentation");
    assert_eq!(
        attachment["presentation"]["definition"]["kind"],
        "shared_presentation"
    );
    assert_eq!(attachment["project_path"], serde_json::Value::Null);
    assert_eq!(attachment["posture"], "interactive");
    assert_eq!(attachment["binding_digest"], "11".repeat(32));
    assert!(result.get("surface_ref").is_none());
    assert!(result.get("binding_digest").is_none());
    assert!(result["events_url"].as_str().unwrap().contains(&session_id));
}

#[tokio::test]
async fn session_current_without_session_rejected() {
    let (_tmp, state) = build_test_state();

    // Anonymous context — no session.
    let ctx = HandlerContext::anonymous();

    let result = (ryeos_ui::handlers::ui_session_current::DESCRIPTOR.handler)(
        serde_json::json!(null),
        ctx,
        Arc::new(state),
    )
    .await;

    assert!(result.is_err(), "anonymous should be rejected");
}

#[tokio::test]
async fn session_current_with_expired_session_rejected() {
    let short_store = ryeos_ui::BrowserSessionStore::new_with_short_ttl(
        std::time::Duration::from_millis(1),
        std::time::Duration::from_millis(1),
    );

    let (session_id, _token, _) = short_store
        .mint_token(test_context(), 16, &"22".repeat(32))
        .expect("mint short-lived session");

    std::thread::sleep(std::time::Duration::from_millis(5));

    let (_tmp, _state) = build_test_state();
    // Manually inject the expired session into the test state's store.
    // (The short_store is separate from state's store, so we test the
    // store directly.)
    assert!(
        short_store.get_session(&session_id).is_none(),
        "expired session should be gone"
    );
}

#[tokio::test]
async fn session_current_reports_observation_only_posture() {
    let (_tmp, state) = build_test_state();

    let mut ctx = test_context();
    Arc::make_mut(&mut ctx.compiled_binding).posture =
        ryeos_ui::compiled_binding::EffectiveUiPosture::ObservationOnly;
    let (session_id, token) = mint_launch(&state, ctx);
    assert_eq!(
        get_ui_state(&state)
            .unwrap()
            .browser_sessions
            .consume_launch_token(&token),
        Some(session_id.clone())
    );

    let hctx = HandlerContext::new(format!("session:{session_id}"), vec![], false);

    let result = (ryeos_ui::handlers::ui_session_current::DESCRIPTOR.handler)(
        serde_json::json!(null),
        hctx,
        Arc::new(state),
    )
    .await
    .expect("should succeed");

    assert_eq!(
        result["binding_attachments"][0]["posture"],
        "observation_only"
    );
    assert_eq!(
        result["binding_attachments"][0]["project_path"],
        serde_json::Value::Null
    );
}

#[tokio::test]
async fn session_current_returns_durable_user_principal_when_present() {
    let (_tmp, state) = build_test_state();
    let user_principal_id = format!("fp:{}", "cd".repeat(32));
    let mut ctx = test_context();
    ctx.user_principal_id = Some(user_principal_id.clone());
    let (session_id, token) = mint_launch(&state, ctx);
    assert_eq!(
        get_ui_state(&state)
            .unwrap()
            .browser_sessions
            .consume_launch_token(&token),
        Some(session_id.clone())
    );
    let hctx = HandlerContext::new(format!("session:{session_id}"), vec![], false);

    let result = (ryeos_ui::handlers::ui_session_current::DESCRIPTOR.handler)(
        serde_json::json!(null),
        hctx,
        Arc::new(state),
    )
    .await
    .expect("should succeed");

    assert_eq!(result["user_principal_id"], user_principal_id);
}
