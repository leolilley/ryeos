use ryeos_ui::test_support as test_state;

use std::sync::Arc;

use ryeos_app::handler_context::HandlerContext;
use ryeos_ui::state::get_ui_state;

use test_state::{
    build_test_state, build_test_state_with_live_bundles, launch_context, mint_launch,
};

fn workspace_root() -> String {
    ryeos_engine::test_support::workspace_root()
        .to_string_lossy()
        .to_string()
}

/// The session's launch attachment: the compiled authority a dispatched UI
/// service runs under. A bare `session:` context reaches no data on its own.
fn surface_attachment(
    state: &ryeos_app::state::AppState,
    session_id: &str,
) -> Arc<ryeos_ui::browser_session::AdmittedBindingAttachment> {
    let session = get_ui_state(state)
        .unwrap()
        .browser_sessions
        .get_session(session_id)
        .expect("live browser session");
    session
        .attachments
        .get(&session.root_attachment_id)
        .expect("launch attachment")
        .clone()
}

#[tokio::test]
async fn field_sources_use_the_authenticated_ui_read_lane() {
    let (_tmp, _state_owner, state) = build_test_state_with_live_bundles();
    exercise_field_sources(state, true).await;
}

#[tokio::test]
async fn field_sources_authenticate_and_report_missing_projection_contract() {
    let (_tmp, _state_owner, state) = build_test_state();
    exercise_field_sources(state, false).await;
}

async fn exercise_field_sources(state: ryeos_app::state::AppState, project_available: bool) {
    let project_path = workspace_root();
    let launch_context = launch_context(
        "surface:ryeos/ui/atlas",
        Some(&project_path),
        ryeos_ui::compiled_binding::EffectiveUiPosture::ObservationOnly,
        None,
    );
    let (session_id, token) = mint_launch(&state, launch_context);
    assert_eq!(
        get_ui_state(&state)
            .unwrap()
            .browser_sessions
            .consume_launch_token(&token),
        Some(session_id.clone())
    );
    let ctx = HandlerContext::new(
        format!("session:{session_id}"),
        vec!["ui.read".to_string()],
        false,
    );
    let attachment = surface_attachment(&state, &session_id);
    let state = Arc::new(state);
    ryeos_ui::seat_auth::with_compiled_ui_attachment(attachment, async move {
        let arbitrary_project = (ryeos_ui::handlers::ui_field_project::DESCRIPTOR.handler)(
            serde_json::json!({"project_path": "/must/not/be/accepted"}),
            ctx.clone(),
            state.clone(),
        )
        .await;
        assert!(arbitrary_project.is_err());
        let project = (ryeos_ui::handlers::ui_field_project::DESCRIPTOR.handler)(
            serde_json::json!({}),
            ctx.clone(),
            state.clone(),
        )
        .await;
        if project_available {
            let project = project.expect("field project source");
            assert_eq!(project["schema_version"], "ryeos.ui.field.facts.v2");
            assert_eq!(project["source"], "project");
            assert!(project["entities"].is_array());
            assert!(
                !serde_json::to_string(&project)
                    .unwrap()
                    .contains(&project_path)
            );
        } else {
            let error = project.expect_err("empty Engine cannot fabricate project topology");
            assert!(format!("{error:#}").contains("parser kind schema not registered"));
        }

        let runs = (ryeos_ui::handlers::ui_field_runs::DESCRIPTOR.handler)(
            serde_json::json!({
                "limit": 10,
                "facets": {"opaque.test_key": "not-present"}
            }),
            ctx.clone(),
            state.clone(),
        )
        .await
        .expect("field runs source");
        assert_eq!(runs["schema_version"], "ryeos.ui.field.facts.v2");
        assert_eq!(runs["source"], "runs");
        assert!(runs["entities"].is_array());

        let item_scoped_runs = (ryeos_ui::handlers::ui_field_runs::DESCRIPTOR.handler)(
            serde_json::json!({"item_ref": "graph:test/item", "limit": 1}),
            ctx.clone(),
            state.clone(),
        )
        .await
        .expect("item-scoped field runs source");
        assert_eq!(item_scoped_runs["subject"]["kind"], "item");
        assert_eq!(item_scoped_runs["subject"]["id"], "graph:test/item");
        assert!(item_scoped_runs["subject"].get("definition_ref").is_none());

        let combined_scoped_runs = (ryeos_ui::handlers::ui_field_runs::DESCRIPTOR.handler)(
            serde_json::json!({
                "item_ref": "graph:test/item",
                "definition_ref": "graph:test/definition",
                "limit": 1
            }),
            ctx.clone(),
            state.clone(),
        )
        .await
        .expect("combined-scoped field runs source");
        assert_eq!(combined_scoped_runs["subject"]["id"], "graph:test/item");
        assert_eq!(
            combined_scoped_runs["subject"]["definition_ref"],
            "graph:test/definition"
        );
        let digest_scoped_runs = (ryeos_ui::handlers::ui_field_runs::DESCRIPTOR.handler)(
            serde_json::json!({"effective_definition_digest": "a".repeat(64), "limit": 1}),
            ctx.clone(),
            state.clone(),
        )
        .await
        .expect("digest-scoped field runs source");
        assert_eq!(digest_scoped_runs["subject"]["kind"], "project");
        assert_eq!(
            digest_scoped_runs["subject"]["effective_definition_digest"],
            "a".repeat(64)
        );

        let execution = (ryeos_ui::handlers::ui_field_execution::DESCRIPTOR.handler)(
            serde_json::json!({}),
            ctx,
            state,
        )
        .await
        .expect("unselected field execution source");
        assert_eq!(execution["schema_version"], "ryeos.ui.field.facts.v2");
        assert_eq!(execution["source"], "execution");
        assert_eq!(execution["subject"]["kind"], "none");
    })
    .await;
}
