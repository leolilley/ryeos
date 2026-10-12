// Tests for principal-aware RyeOS UI user-space storage.

mod test_state;
use test_state::{build_test_state, launch_context};

use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::{HandlerError, extract_handler_error};
use ryeos_ui::state::get_ui_state;
use serde_json::json;
use std::sync::Arc;

fn principal_launch_context(user_principal_id: String) -> ryeos_ui::browser_session::LaunchContext {
    launch_context(
        "surface:ryeos/ui/base",
        None,
        ryeos_ui::compiled_binding::EffectiveUiPosture::Interactive,
        Some(user_principal_id),
    )
}

fn session_context(state: &ryeos_app::state::AppState, principal: &str) -> HandlerContext {
    let (session_id, token) =
        test_state::mint_launch(state, principal_launch_context(principal.to_string()));
    assert_eq!(
        get_ui_state(state)
            .unwrap()
            .browser_sessions
            .consume_launch_token(&token),
        Some(session_id.clone())
    );
    HandlerContext::new(
        format!("session:{session_id}"),
        vec!["ui.read".into()],
        false,
    )
}

#[tokio::test]
async fn ryeos_config_is_isolated_by_durable_user_principal() {
    // Handler-unit storage isolation only. Install each fixture's retained
    // attachment with its actual principal, not merely a user display label.
    // This is not evidence of signed capability/producer dispatch admission.
    fn config_context(state: &ryeos_app::state::AppState, principal: &str) -> HandlerContext {
        let mut launch = principal_launch_context(principal.to_owned());
        Arc::make_mut(&mut launch.compiled_binding)
            .binding
            .principal_id = principal.to_owned();
        let (id, token) = test_state::mint_launch(state, launch);
        assert_eq!(
            get_ui_state(state)
                .unwrap()
                .browser_sessions
                .consume_launch_token(&token),
            Some(id.clone())
        );
        HandlerContext::new(format!("session:{id}"), vec!["ui.read".into()], false)
    }
    async fn call_config(
        descriptor: ryeos_api::registry::ServiceDescriptor,
        parameters: serde_json::Value,
        context: HandlerContext,
        state: Arc<ryeos_app::state::AppState>,
    ) -> anyhow::Result<serde_json::Value> {
        let id = context.fingerprint.strip_prefix("session:").unwrap();
        let session = get_ui_state(&state)
            .unwrap()
            .browser_sessions
            .get_session(id)
            .unwrap();
        let attachment = session.attachments[&session.root_attachment_id].clone();
        ryeos_ui::seat_auth::with_compiled_ui_attachment(attachment, async {
            (descriptor.handler)(parameters, context, state).await
        })
        .await
    }
    let (tmp, state) = build_test_state();
    let principal_a = format!("fp:{}", "aa".repeat(32));
    let principal_b = format!("fp:{}", "bb".repeat(32));

    let ctx_a = config_context(&state, &principal_a);
    for (descriptor, parameters) in [
        (
            ryeos_ui::handlers::ui_projects::CONFIG_GET_DESCRIPTOR,
            json!(null),
        ),
        (
            ryeos_ui::handlers::ui_projects::CONFIG_UPDATE_DESCRIPTOR,
            json!({"theme":"dark"}),
        ),
    ] {
        let error = (descriptor.handler)(parameters, ctx_a.clone(), Arc::new(state.clone()))
            .await
            .expect_err("valid cookie cannot authorize principal storage access");
        assert!(matches!(
            extract_handler_error(&error),
            Some(HandlerError::Forbidden(_))
        ));
    }
    let updated = call_config(
        ryeos_ui::handlers::ui_projects::CONFIG_UPDATE_DESCRIPTOR,
        json!({ "theme": "dark" }),
        ctx_a.clone(),
        Arc::new(state.clone()),
    )
    .await
    .expect("principal A should update config");
    assert_eq!(updated["theme"], "dark");

    let loaded_a = call_config(
        ryeos_ui::handlers::ui_projects::CONFIG_GET_DESCRIPTOR,
        json!(null),
        ctx_a,
        Arc::new(state.clone()),
    )
    .await
    .expect("principal A should read updated config");
    assert_eq!(loaded_a["theme"], "dark");

    let ctx_b = config_context(&state, &principal_b);
    let loaded_b = call_config(
        ryeos_ui::handlers::ui_projects::CONFIG_GET_DESCRIPTOR,
        json!(null),
        ctx_b,
        Arc::new(state.clone()),
    )
    .await
    .expect("principal B should read default config");
    assert_eq!(loaded_b["theme"], "system");

    let principal_a_key = "aa".repeat(32);
    assert!(
        tmp.path()
            .join(".ai/principals")
            .join(principal_a_key)
            .join("space/.ai/config/ryeos-ui.yaml")
            .is_file()
    );
}

#[tokio::test]
async fn ryeos_dimension_rejects_a_valid_cookie_without_compiled_session_dispatch() {
    let (_tmp, state) = build_test_state();
    let principal = format!("fp:{}", "cc".repeat(32));
    let ctx = session_context(&state, &principal);

    let error = ryeos_ui::handlers::ui_dimension::handle(json!(null), ctx, Arc::new(state.clone()))
        .await
        .expect_err("a live cookie alone must not authorize a dimension source");
    assert!(matches!(
        extract_handler_error(&error),
        Some(HandlerError::Forbidden(_))
    ));
    assert!(format!("{error:#}").contains("compiled browser UI dispatch required"));
}

#[tokio::test]
#[ignore = "requires populated handler binaries via scripts/populate-bundles.sh"]
async fn ryeos_dimension_separates_session_principal_from_serving_node_identity() {
    let (_tmp, state) = test_state::build_test_state_with_live_bundles();
    let (session_id, ctx, producer, coordinate) =
        test_state::live_presentation_with_surface(&state, "surface:ryeos/ui/lens").await;
    let retained = get_ui_state(&state)
        .unwrap()
        .browser_sessions
        .get_session(&session_id)
        .unwrap();
    let principal = retained.user_principal_id.clone().unwrap();
    let node_principal = state.identity.principal_id();
    let node_fingerprint = state.identity.fingerprint().to_string();
    assert_ne!(
        principal, node_principal,
        "fixture must distinguish actor and serving node"
    );
    let response = (ryeos_ui::handlers::ui_invocations_dispatch::DESCRIPTOR.handler)(
        json!({
            "producer": producer,
            "binding": {
                "binding_attachment_id": coordinate.binding_attachment_id,
                "binding_generation": coordinate.binding_generation,
                "binding_digest": coordinate.binding_digest,
                "coordinate": {"kind":"source", "view_ref":"surface:ryeos/ui/lens", "channel":"dimension"},
                "payload": {"kind":"source_parameters", "params":{}}
            }
        }),
        ctx,
        Arc::new(state),
    ).await.expect("exact admitted dimension source should load");
    assert_eq!(response["status"], "executed");
    let dimension = &response["result"]["result"];
    assert_eq!(dimension["session"]["user_principal_id"], principal);
    assert_eq!(dimension["session"]["root_ref"], "surface:ryeos/ui/lens");
    assert_eq!(
        dimension["local_node"]["identity"]["principal_id"],
        node_principal
    );
    assert_eq!(
        dimension["local_node"]["identity"]["fingerprint"],
        node_fingerprint
    );
}

#[tokio::test]
async fn launch_mint_rejects_invalid_user_principal_as_bad_request() {
    let (_tmp, state) = build_test_state();
    let req = ryeos_ui::handlers::ui_launch_mint::Request {
        ui_binding_contract_revision: ryeos_ui::UI_BINDING_CONTRACT_REVISION.to_string(),
        composition: ryeos_client_base::presentation::UiCompositionSelection::Surface {
            surface_ref: "surface:ryeos/ui/base".into(),
        },
        project_path: None,
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
        ui_binding_contract_revision: ryeos_ui::UI_BINDING_CONTRACT_REVISION.to_string(),
        composition: ryeos_client_base::presentation::UiCompositionSelection::Surface {
            surface_ref: "surface:ryeos/ui/base".into(),
        },
        project_path: None,
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
