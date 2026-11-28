// Tests for principal-aware RyeOS UI user-space storage.

use ryeos_ui::test_support as test_state;
use test_state::{build_test_state, build_test_state_with_live_bundles, launch_context};

use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::{HandlerError, extract_handler_error};
use ryeos_ui::state::get_ui_state;
use serde_json::json;
use std::sync::Arc;

/// A launch whose compiled binding is owned by `user_principal_id`: the
/// durable principal its user-space store is keyed by.
fn principal_launch_context(user_principal_id: String) -> ryeos_ui::browser_session::LaunchContext {
    let mut context = launch_context(
        "surface:ryeos/ui/base",
        None,
        ryeos_ui::compiled_binding::EffectiveUiPosture::Interactive,
        Some(user_principal_id.clone()),
    );
    Arc::make_mut(&mut context.compiled_binding)
        .binding
        .principal_id = user_principal_id;
    context
}

/// A minted, activated session for `principal`: its `session:` context and
/// the launch attachment its UI services are dispatched under.
fn session_context(
    state: &ryeos_app::state::AppState,
    principal: &str,
) -> (
    HandlerContext,
    Arc<ryeos_ui::browser_session::AdmittedBindingAttachment>,
) {
    let (session_id, token) =
        test_state::mint_launch(state, principal_launch_context(principal.to_string()));
    assert_eq!(
        get_ui_state(state)
            .unwrap()
            .browser_sessions
            .consume_launch_token(&token),
        Some(session_id.clone())
    );
    let session = get_ui_state(state)
        .unwrap()
        .browser_sessions
        .get_session(&session_id)
        .expect("live browser session");
    let attachment = session
        .attachments
        .get(&session.root_attachment_id)
        .expect("launch attachment")
        .clone();
    (
        HandlerContext::new(
            format!("session:{session_id}"),
            vec!["ui.read".into()],
            false,
        ),
        attachment,
    )
}

#[tokio::test]
async fn ryeos_config_is_isolated_by_durable_user_principal() {
    let (tmp, _state_owner, state) = build_test_state_with_live_bundles();
    let principal_a = format!("fp:{}", "aa".repeat(32));
    let principal_b = format!("fp:{}", "bb".repeat(32));

    let (ctx_a, attachment_a) = session_context(&state, &principal_a);
    let updated = ryeos_ui::seat_auth::with_compiled_ui_attachment(
        attachment_a.clone(),
        ryeos_ui::handlers::ui_projects::handle_config_update(
            json!({ "theme": "theme:ryeos/gruvbox" }),
            ctx_a.clone(),
            Arc::new(state.clone()),
        ),
    )
    .await
    .expect("principal A should update config");
    assert_eq!(updated["theme"], "theme:ryeos/gruvbox");

    let loaded_a = ryeos_ui::seat_auth::with_compiled_ui_attachment(
        attachment_a,
        ryeos_ui::handlers::ui_projects::handle_config_get(
            json!(null),
            ctx_a,
            Arc::new(state.clone()),
        ),
    )
    .await
    .expect("principal A should read updated config");
    assert_eq!(loaded_a["theme"], "theme:ryeos/gruvbox");

    let (ctx_b, attachment_b) = session_context(&state, &principal_b);
    let loaded_b = ryeos_ui::seat_auth::with_compiled_ui_attachment(
        attachment_b,
        ryeos_ui::handlers::ui_projects::handle_config_get(
            json!(null),
            ctx_b,
            Arc::new(state.clone()),
        ),
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
async fn a_stored_light_or_dark_theme_reads_as_system_and_can_be_replaced() {
    let (tmp, _state_owner, state) = build_test_state_with_live_bundles();
    let principal = format!("fp:{}", "dd".repeat(32));
    let (ctx, attachment) = session_context(&state, &principal);
    let call = |params: serde_json::Value, update: bool| {
        let (ctx, attachment, state) = (ctx.clone(), attachment.clone(), Arc::new(state.clone()));
        async move {
            if update {
                ryeos_ui::seat_auth::with_compiled_ui_attachment(
                    attachment,
                    ryeos_ui::handlers::ui_projects::handle_config_update(params, ctx, state),
                )
                .await
            } else {
                ryeos_ui::seat_auth::with_compiled_ui_attachment(
                    attachment,
                    ryeos_ui::handlers::ui_projects::handle_config_get(params, ctx, state),
                )
                .await
            }
        }
    };
    call(json!({ "theme": "theme:ryeos/gruvbox" }), true)
        .await
        .expect("writes the config file");
    let file = tmp
        .path()
        .join(".ai/principals")
        .join("dd".repeat(32))
        .join("space/.ai/config/ryeos-ui.yaml");
    let text = std::fs::read_to_string(&file).expect("config file");
    std::fs::write(
        &file,
        text.replace("theme: theme:ryeos/gruvbox", "theme: dark"),
    )
    .expect("rewrite");

    // The whole config still reads; only the theme is treated as unset.
    let loaded = call(json!(null), false).await.expect("config still reads");
    assert_eq!(loaded["theme"], "system");
    assert_eq!(loaded["landing_view"], "projects");
    // And a config update still works and writes a valid theme back.
    let updated = call(json!({ "theme": "theme:ryeos/paper" }), true)
        .await
        .expect("update works over the stale theme");
    assert_eq!(updated["theme"], "theme:ryeos/paper");
    assert!(
        std::fs::read_to_string(&file)
            .unwrap()
            .contains("theme: theme:ryeos/paper")
    );
}

#[tokio::test]
async fn a_theme_that_is_not_installed_is_refused_and_a_stored_one_reads_as_system() {
    let (tmp, _state_owner, state) = build_test_state_with_live_bundles();
    let principal = format!("fp:{}", "ee".repeat(32));
    let (ctx, attachment) = session_context(&state, &principal);
    let update = |theme: &str| {
        ryeos_ui::seat_auth::with_compiled_ui_attachment(
            attachment.clone(),
            ryeos_ui::handlers::ui_projects::handle_config_update(
                json!({ "theme": theme }),
                ctx.clone(),
                Arc::new(state.clone()),
            ),
        )
    };
    // The earlier palette ids and refs to themes nobody installed are refused.
    for refused in ["gruvbox", "night", "theme:ryeos/neon", "neon"] {
        let error = update(refused).await.expect_err(refused);
        assert!(
            format!("{error:#}").contains("invalid theme value"),
            "{refused}: {error:#}"
        );
    }
    update("theme:ryeos/night").await.expect("installed theme");

    // A well-formed ref to a theme that is later uninstalled reads as system.
    let file = tmp
        .path()
        .join(".ai/principals")
        .join("ee".repeat(32))
        .join("space/.ai/config/ryeos-ui.yaml");
    let text = std::fs::read_to_string(&file).unwrap();
    std::fs::write(
        &file,
        text.replace("theme: theme:ryeos/night", "theme: theme:ryeos/gone"),
    )
    .unwrap();
    let loaded = ryeos_ui::seat_auth::with_compiled_ui_attachment(
        attachment,
        ryeos_ui::handlers::ui_projects::handle_config_get(json!(null), ctx, Arc::new(state)),
    )
    .await
    .expect("config reads");
    assert_eq!(loaded["theme"], "system");
}

#[tokio::test]
async fn config_get_grants_the_session_the_ground_pictures_of_its_active_themes() {
    let (_tmp, _state_owner, state) = build_test_state_with_live_bundles();
    let principal = format!("fp:{}", "ff".repeat(32));
    let (ctx, attachment) = session_context(&state, &principal);
    let session_id = ctx.fingerprint.strip_prefix("session:").unwrap().to_owned();
    let loaded = test_state::with_dispatched_session(
        &state,
        &session_id,
        attachment,
        ryeos_ui::handlers::ui_projects::handle_config_get(
            json!(null),
            ctx,
            Arc::new(state.clone()),
        ),
    )
    .await
    .expect("config reads");

    // Stored `system`: the dark and the light theme may be drawn.
    assert_eq!(loaded["theme"], "system");
    let catalogue = &loaded["theme_catalogue"];
    let themes = catalogue["themes"].as_array().expect("themes");
    let refs: Vec<&str> = themes.iter().map(|t| t["ref"].as_str().unwrap()).collect();
    assert_eq!(
        refs,
        [
            "theme:ryeos/gruvbox",
            "theme:ryeos/night",
            "theme:ryeos/paper"
        ]
    );
    assert_eq!(catalogue["system"]["dark"], "theme:ryeos/night");
    let session = get_ui_state(&state)
        .unwrap()
        .browser_sessions
        .get_session(&session_id)
        .unwrap();
    // The source tree's bundle has no populated picture manifest, so its
    // grounds read as `unavailable` here; a populated bundle grants the hash.
    // Either way an active theme carries an entry for the Surface and the
    // session holds exactly the hashes it was handed.
    let mut handed = 0;
    for theme in themes {
        let ground = &theme["surface_grounds"]["surface:ryeos/ui/lens"];
        if theme["ref"] == "theme:ryeos/gruvbox" {
            assert!(ground.is_null(), "an inactive theme carries no ground");
            continue;
        }
        if let Some(hash) = ground["picture"]["sha256"].as_str() {
            assert_eq!(ground["picture"]["media_type"], "image/webp");
            assert!(session.art_grants.contains(hash));
            handed += 1;
        } else {
            assert_eq!(ground["picture_state"], "unavailable");
            assert!(ground["picture"].is_null());
        }
    }
    assert_eq!(session.art_grants.is_empty(), handed == 0);

    // Another session was granted nothing.
    let (other_ctx, other_attachment) = session_context(&state, &principal);
    let other_id = other_ctx
        .fingerprint
        .strip_prefix("session:")
        .unwrap()
        .to_owned();
    let other = get_ui_state(&state)
        .unwrap()
        .browser_sessions
        .get_session(&other_id)
        .unwrap();
    assert!(other.art_grants.is_empty());
    drop(other_attachment);
}

#[tokio::test]
async fn ryeos_dimension_exposes_session_user_principal() {
    let (_tmp, _state_owner, state) = build_test_state();
    let principal = format!("fp:{}", "cc".repeat(32));
    let (ctx, attachment) = session_context(&state, &principal);

    let session_id = ctx
        .fingerprint
        .strip_prefix("session:")
        .expect("session context")
        .to_owned();
    let dimension = test_state::with_dispatched_session(
        &state,
        &session_id,
        attachment,
        ryeos_ui::handlers::ui_dimension::handle(json!(null), ctx, Arc::new(state.clone())),
    )
    .await
    .expect("ryeos-ui dimension should load");

    assert_eq!(dimension["session"]["user_principal_id"], principal);
    assert_eq!(dimension["session"]["surface_ref"], "surface:ryeos/ui/base");
}

#[tokio::test]
async fn launch_mint_rejects_invalid_user_principal_as_bad_request() {
    let (_tmp, _state_owner, state) = build_test_state();
    let req = ryeos_ui::handlers::ui_launch_mint::Request {
        ui_binding_contract_revision: ryeos_ui::UI_BINDING_CONTRACT_REVISION.to_string(),
        root_ref: "surface:ryeos/ui/base".into(),
        project_path: None,
        user_principal_id: Some("session:not-a-principal".into()),
        scopes: None,
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
async fn launch_mint_refuses_a_remote_node_key_whatever_its_scopes() {
    let (_tmp, _state_owner, state) = build_test_state();
    let principal = format!("fp:{}", "cd".repeat(32));
    let req = ryeos_ui::handlers::ui_launch_mint::Request {
        ui_binding_contract_revision: ryeos_ui::UI_BINDING_CONTRACT_REVISION.to_string(),
        root_ref: "surface:ryeos/ui/base".into(),
        project_path: None,
        user_principal_id: Some(principal.clone()),
        scopes: None,
    };
    let context = HandlerContext::new_with_authority(
        principal,
        vec!["*".into()],
        true,
        Some(ryeos_node_runtime::identity::AuthorizedKeyPrincipalClass::RemoteNode),
        Some("site:peer".into()),
    );

    let err = ryeos_ui::handlers::ui_launch_mint::handle(req, context, Arc::new(state))
        .await
        .expect_err("a peer node's key must not open a browser session");

    let handler_error = extract_handler_error(&err).expect("should preserve typed handler error");
    assert!(
        matches!(&handler_error, HandlerError::Forbidden(message) if message.contains("remote_node")),
        "{handler_error:?}"
    );
}

#[tokio::test]
async fn launch_mint_rejects_mismatched_user_principal() {
    let (_tmp, _state_owner, state) = build_test_state();
    let req = ryeos_ui::handlers::ui_launch_mint::Request {
        ui_binding_contract_revision: ryeos_ui::UI_BINDING_CONTRACT_REVISION.to_string(),
        root_ref: "surface:ryeos/ui/base".into(),
        project_path: None,
        user_principal_id: Some(format!("fp:{}", "aa".repeat(32))),
        scopes: None,
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
