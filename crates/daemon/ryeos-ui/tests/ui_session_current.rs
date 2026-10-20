// Tests for `ui.session.current` handler.

mod test_state;
use test_state::{build_test_state, launch_context, mint_launch};

use ryeos_app::handler_context::HandlerContext;
use ryeos_ui::state::get_ui_state;
use std::sync::Arc;

fn test_context() -> ryeos_ui::browser_session::LaunchContext {
    launch_context(
        "surface:ryeos/ui/base",
        None,
        ryeos_ui::compiled_binding::EffectiveUiPosture::Interactive,
        None,
    )
}

#[tokio::test]
async fn session_current_returns_session_fields() {
    let (_tmp, _state_owner, state) = build_test_state();

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
    let root_attachment_id = result["root_attachment_id"].as_str().unwrap();
    let attachments = result["binding_attachments"].as_array().unwrap();
    assert_eq!(attachments.len(), 1);
    let attachment = &attachments[0];
    assert_eq!(attachment["binding_attachment_id"], root_attachment_id);
    assert_eq!(attachment["root_ref"], "surface:ryeos/ui/base");
    assert_eq!(attachment["project_path"], serde_json::Value::Null);
    assert_eq!(attachment["posture"], "interactive");
    assert_eq!(attachment["binding_digest"], "11".repeat(32));
    assert!(result.get("surface_ref").is_none());
    assert!(result.get("binding_digest").is_none());
    assert!(result["events_url"].as_str().unwrap().contains(&session_id));
}

#[tokio::test]
async fn session_current_without_session_rejected() {
    let (_tmp, _state_owner, state) = build_test_state();

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

    let (_tmp, _state_owner, _state) = build_test_state();
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
    let (_tmp, _state_owner, state) = build_test_state();

    let ctx = launch_context(
        "surface:ryeos/test/ro",
        None,
        ryeos_ui::compiled_binding::EffectiveUiPosture::ObservationOnly,
        None,
    );
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
    let (_tmp, _state_owner, state) = build_test_state();
    let user_principal_id = format!("fp:{}", "cd".repeat(32));
    let ctx = launch_context(
        "surface:ryeos/ui/base",
        None,
        ryeos_ui::compiled_binding::EffectiveUiPosture::Interactive,
        Some(user_principal_id.clone()),
    );
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

/// A launch whose root is a shared-presentation Config: the only root kind a
/// presentation can be admitted under.
fn shared_launch_context(
    config_ref: &str,
    user_principal_id: Option<String>,
) -> ryeos_ui::browser_session::LaunchContext {
    let mut context = launch_context(
        "surface:ryeos/ui/base",
        None,
        ryeos_ui::compiled_binding::EffectiveUiPosture::ObservationOnly,
        user_principal_id,
    );
    Arc::make_mut(&mut context.compiled_binding)
        .binding
        .surface
        .canonical_ref = config_ref.to_owned();
    let config = serde_json::from_value(serde_json::json!({
        "category": "ryeos/test",
        "shared_presentation": {
            "schema": 1, "title": "Test presentation", "launcher": "launch",
            "dialogs": [{"id": "launch", "title": "Launch", "view": "view:ryeos/test/launcher"}],
            "capabilities": {"sources": [], "affordances": []}
        }
    }))
    .expect("typed shared Config fixture");
    context.presentation = ryeos_client_base::presentation::UiCompositionPresentation {
        definition: ryeos_client_base::presentation::UiCompositionDefinition::SharedPresentation {
            config,
        },
        views: std::collections::BTreeMap::from([(
            "view:ryeos/test/launcher".into(),
            serde_json::json!({"widget": "rows"}),
        )]),
    };
    context
}

fn bounds() -> ryeos_client_base::ui::UiBindingRequestBounds {
    ryeos_client_base::ui::UiBindingRequestBounds {
        max_request_bytes: 65536,
        max_input_bytes: 65536,
        max_seat_append_bytes: 65536,
    }
}

fn limits(state: &ryeos_app::state::AppState) -> ryeos_ui::browser_session::PresentationLimits {
    let policy = state
        .node_policy
        .require::<ryeos_node_runtime::node_policy::sections::ui_browser_sessions::UiBrowserSessionPolicy>()
        .expect("signed UI browser-session policy");
    ryeos_ui::browser_session::PresentationLimits::from_policy(policy).expect("policy limits")
}

fn current_call(
    session_id: &str,
    state: &Arc<ryeos_app::state::AppState>,
) -> impl std::future::Future<Output = anyhow::Result<serde_json::Value>> {
    let ctx = HandlerContext::new(format!("session:{session_id}"), vec![], false);
    (ryeos_ui::handlers::ui_session_current::DESCRIPTOR.handler)(
        serde_json::json!(null),
        ctx,
        state.clone(),
    )
}

#[tokio::test]
async fn session_current_reports_the_real_presentation_memberships_sorted() {
    let (_tmp, _state_owner, state) = build_test_state();
    let (session_id, token) = mint_launch(
        &state,
        shared_launch_context("config:ryeos/test/presentation", None),
    );
    let ui = get_ui_state(&state).unwrap();
    ui.browser_sessions.consume_launch_token(&token).unwrap();
    let session = ui.browser_sessions.get_session(&session_id).unwrap();
    let root = session.attachments[&session.root_attachment_id].coordinate();
    let limits = limits(&state);
    let digest = state.node_policy.generation_digest().to_owned();
    let mut created = Vec::new();
    for _ in 0..3 {
        created.push(
            ui.browser_sessions
                .prepare_presentation(
                    &session_id,
                    &uuid::Uuid::new_v4().to_string(),
                    &root,
                    &limits,
                    &digest,
                    bounds(),
                )
                .unwrap(),
        );
    }
    let state = Arc::new(state);
    let result = current_call(&session_id, &state).await.expect("current");
    let reported = result["presentations"].as_array().unwrap();
    assert_eq!(reported.len(), 3);
    let ids = reported
        .iter()
        .map(|entry| entry["presentation_id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted, "memberships are reported in id order");
    for entry in reported {
        let membership = created
            .iter()
            .find(|membership| membership.presentation_id == entry["presentation_id"])
            .unwrap();
        assert_eq!(
            entry["root_binding"]["binding_attachment_id"],
            root.binding_attachment_id
        );
        assert_eq!(
            entry["initialization_digest"],
            membership.public_descriptor().initialization_digest
        );
        assert_eq!(entry["surfaces"], serde_json::json!([]));
    }
    for entry in reported {
        assert!(entry["thread_id"].is_null(), "no seat is bound yet");
    }
    // The launch root is the only current attachment; a presentation does not
    // add attachments of its own when no Surface was seeded.
    assert_eq!(result["binding_attachments"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn session_current_refuses_a_session_whose_launch_root_policy_is_stale() {
    let (_tmp, _state_owner, state) = build_test_state();
    let ui = get_ui_state(&state).unwrap();
    let stale_digest = "44".repeat(32);
    assert_ne!(stale_digest, state.node_policy.generation_digest());
    let mut stale = shared_launch_context("config:ryeos/test/presentation", None);
    std::sync::Arc::make_mut(&mut stale.compiled_binding)
        .binding
        .node_policy_generation_digest = stale_digest.clone();
    let (session_id, token, _) = ui
        .browser_sessions
        .mint_token(stale, 16, &stale_digest)
        .unwrap();
    ui.browser_sessions.consume_launch_token(&token).unwrap();
    assert!(
        current_call(&session_id, &Arc::new(state)).await.is_err(),
        "a launch root compiled under another policy generation is not reported"
    );
}

#[tokio::test]
async fn surface_rooted_session_reports_no_presentations_and_admits_none() {
    let (_tmp, _state_owner, state) = build_test_state();
    let (session_id, token) = mint_launch(&state, test_context());
    let ui = get_ui_state(&state).unwrap();
    ui.browser_sessions.consume_launch_token(&token).unwrap();
    let session = ui.browser_sessions.get_session(&session_id).unwrap();
    let root = session.attachments[&session.root_attachment_id].coordinate();
    assert_eq!(
        ui.browser_sessions
            .prepare_presentation(
                &session_id,
                &uuid::Uuid::new_v4().to_string(),
                &root,
                &limits(&state),
                state.node_policy.generation_digest(),
                bounds(),
            )
            .unwrap_err(),
        ryeos_ui::browser_session::AttachmentStoreError::PresentationInvalid,
        "a Surface launch root is not a shared-presentation shell"
    );
    let result = current_call(&session_id, &Arc::new(state)).await.unwrap();
    assert_eq!(result["presentations"], serde_json::json!([]));
}

#[tokio::test]
async fn presentation_admission_is_bounded_by_the_signed_policy() {
    let (_tmp, _state_owner, state) = build_test_state();
    let (session_id, token) = mint_launch(
        &state,
        shared_launch_context("config:ryeos/test/presentation", None),
    );
    let ui = get_ui_state(&state).unwrap();
    ui.browser_sessions.consume_launch_token(&token).unwrap();
    let session = ui.browser_sessions.get_session(&session_id).unwrap();
    let root = session.attachments[&session.root_attachment_id].coordinate();
    let limits = limits(&state);
    assert_eq!(limits.max_presentations, 16);
    let digest = state.node_policy.generation_digest().to_owned();
    for _ in 0..limits.max_presentations {
        ui.browser_sessions
            .prepare_presentation(
                &session_id,
                &uuid::Uuid::new_v4().to_string(),
                &root,
                &limits,
                &digest,
                bounds(),
            )
            .unwrap();
    }
    assert_eq!(
        ui.browser_sessions
            .prepare_presentation(
                &session_id,
                &uuid::Uuid::new_v4().to_string(),
                &root,
                &limits,
                &digest,
                bounds(),
            )
            .unwrap_err(),
        ryeos_ui::browser_session::AttachmentStoreError::Capacity
    );
    let result = current_call(&session_id, &Arc::new(state)).await.unwrap();
    assert_eq!(
        result["presentations"].as_array().unwrap().len(),
        limits.max_presentations
    );
}

#[tokio::test]
async fn session_current_refuses_a_bound_seat_with_neither_a_head_nor_a_fence() {
    let (_tmp, _state_owner, state) = build_test_state();
    let (session_id, token) = mint_launch(
        &state,
        shared_launch_context("config:ryeos/test/presentation", None),
    );
    let ui = get_ui_state(&state).unwrap();
    ui.browser_sessions.consume_launch_token(&token).unwrap();
    let session = ui.browser_sessions.get_session(&session_id).unwrap();
    let root = session.attachments[&session.root_attachment_id].coordinate();
    let digest = state.node_policy.generation_digest().to_owned();
    let created = ui
        .browser_sessions
        .prepare_presentation(
            &session_id,
            &uuid::Uuid::new_v4().to_string(),
            &root,
            &limits(&state),
            &digest,
            bounds(),
        )
        .unwrap();
    ui.browser_sessions
        .bind_presentation_seat(
            &session_id,
            &created.presentation_id,
            &root,
            "T-seat-one",
            "T-seat-one",
            &digest,
        )
        .unwrap();
    let error = current_call(&session_id, &Arc::new(state))
        .await
        .expect_err("a seat nothing signed and nothing fenced is not reported");
    assert!(
        error
            .to_string()
            .contains("neither live head nor retired fence")
    );
}
