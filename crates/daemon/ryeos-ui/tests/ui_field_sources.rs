mod test_state;

use std::sync::Arc;

use ryeos_app::handler_context::HandlerContext;
use ryeos_ui::browser_session::LaunchContext;
use ryeos_ui::state::get_ui_state;

use test_state::build_test_state;

fn workspace_root() -> String {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|path| path.join("bundles").is_dir())
        .expect("workspace root with bundles")
        .to_string_lossy()
        .to_string()
}

#[tokio::test]
async fn field_sources_use_the_authenticated_ui_read_lane() {
    let (_tmp, state) = build_test_state();
    let project_path = workspace_root();
    let launch_context = LaunchContext {
        surface_ref: "surface:ryeos/ui/atlas".to_string(),
        project_path: Some(project_path.clone()),
        read_only: true,
        granted_caps: vec!["ui.read".to_string()],
        user_principal_id: None,
    };
    let (session_id, _token) = get_ui_state(&state)
        .expect("ui state registered")
        .browser_sessions
        .mint_token(launch_context);
    let ctx = HandlerContext::new(
        format!("session:{session_id}"),
        vec!["ui.read".to_string()],
        false,
    );
    let state = Arc::new(state);

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
    .await
    .expect("field project source");
    assert_eq!(project["schema_version"], "ryeos.ui.field.facts.v2");
    assert_eq!(project["source"], "project");
    assert!(project["entities"].is_array());
    assert!(
        !serde_json::to_string(&project)
            .unwrap()
            .contains(&project_path)
    );

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
}
