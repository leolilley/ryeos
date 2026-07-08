mod test_state;

use std::sync::Arc;

use ryeos_app::handler_context::HandlerContext;
use ryeos_ui::browser_session::LaunchContext;
use ryeos_ui::state::get_ui_state;
use serde_json::json;

fn launch_context(project_path: Option<String>) -> LaunchContext {
    LaunchContext {
        surface_ref: "surface:ryeos/ui/base".into(),
        project_path,
        read_only: false,
        granted_caps: vec!["ui.read".into()],
        user_principal_id: None,
    }
}

fn mint_session(
    state: &ryeos_app::state::AppState,
    project_path: Option<String>,
) -> HandlerContext {
    let (session_id, _token) = get_ui_state(state)
        .unwrap()
        .browser_sessions
        .mint_token(launch_context(project_path));
    HandlerContext::new(
        format!("session:{session_id}"),
        vec!["ui.read".into()],
        false,
    )
}

#[tokio::test]
#[ignore = "requires populated handler binaries via scripts/populate-bundles.sh"]
async fn browser_boot_sequence_endpoints_accept_same_session() {
    let (_tmp, state) = test_state::build_test_state_with_live_bundles();
    let ctx = mint_session(&state, None);
    let state = Arc::new(state);

    let session = (ryeos_ui::handlers::ui_session_current::DESCRIPTOR.handler)(
        json!(null),
        ctx.clone(),
        state.clone(),
    )
    .await
    .expect("session/current should succeed");
    assert_eq!(session["surface_ref"], "surface:ryeos/ui/base");

    let items = (ryeos_ui::handlers::ui_items::ITEMS_LIST_DESCRIPTOR.handler)(
        json!({
            "kind": "surface",
            "query": "ryeos/ryeos-ui/base",
            "limit": 1
        }),
        ctx.clone(),
        state.clone(),
    )
    .await
    .expect("items list should succeed");
    assert_eq!(items["items"][0]["canonical_ref"], "surface:ryeos/ui/base");

    let effective = (ryeos_api::handlers::items_effective::DESCRIPTOR.handler)(
        json!({
            "canonical_ref": session["surface_ref"],
            "expected_kind": "surface"
        }),
        ctx.clone(),
        state.clone(),
    )
    .await
    .expect("items/effective should resolve RyeOS UI surface");
    assert_eq!(effective["canonical_ref"], "surface:ryeos/ui/base");

    let dimension = (ryeos_ui::handlers::ui_dimension::DESCRIPTOR.handler)(
        json!(null),
        ctx.clone(),
        state.clone(),
    )
    .await
    .expect("RyeOS UI dimension should succeed");
    assert_eq!(dimension["schema_version"], "ryeos.ui.dimension.v0");

    let inspection = (ryeos_ui::handlers::ui_items::ITEM_INSPECT_DESCRIPTOR.handler)(
        json!({
            "canonical_ref": "surface:ryeos/ui/base",
            "include_raw": true,
            "include_effective": true
        }),
        ctx,
        state,
    )
    .await
    .expect("item inspect should succeed");
    assert_eq!(inspection["schema_version"], "ryeos.ui.item.inspect.v1");
    assert_eq!(inspection["item"]["canonical_ref"], "surface:ryeos/ui/base");
    assert!(inspection["raw"]["content"]
        .as_str()
        .unwrap()
        .contains("ryeos-ui-base"));
    assert_eq!(
        inspection["effective"]["composed_value"]["name"],
        "ryeos-ui-base"
    );
}

#[tokio::test]
async fn ryeos_admin_lists_reject_unknown_session_ids() {
    let (_tmp, state) = test_state::build_test_state();
    let state = Arc::new(state);
    let ctx = HandlerContext::new("session:missing".into(), vec!["ui.read".into()], false);

    let threads = (ryeos_ui::handlers::ui_threads::DESCRIPTOR.handler)(
        json!({"limit": 1}),
        ctx.clone(),
        state.clone(),
    )
    .await;
    assert!(
        threads.is_err(),
        "threads list must reject invalid sessions"
    );

    let schedules = (ryeos_ui::handlers::ui_schedules::DESCRIPTOR.handler)(
        json!({}),
        ctx.clone(),
        state.clone(),
    )
    .await;
    assert!(
        schedules.is_err(),
        "schedules list must reject invalid sessions"
    );

    let gc = (ryeos_ui::handlers::ui_gc::DESCRIPTOR.handler)(json!({}), ctx, state).await;
    assert!(gc.is_err(), "gc status must reject invalid sessions");
}

#[tokio::test]
async fn files_read_returns_lossy_utf8_for_invalid_bytes() {
    let (_tmp, state) = test_state::build_test_state();
    let project = tempfile::TempDir::new().unwrap();
    std::fs::write(project.path().join("bad.bin"), [b'a', 0xff, b'b']).unwrap();

    let ctx = mint_session(&state, Some(project.path().display().to_string()));
    let result = (ryeos_ui::handlers::ui_files::FILES_READ_DESCRIPTOR.handler)(
        json!({"root": "project", "path": "bad.bin"}),
        ctx,
        Arc::new(state),
    )
    .await
    .expect("files.read should succeed");

    assert_eq!(result["content"], "a�b");
    assert_eq!(result["size"], 3);
    assert_eq!(result["truncated"], false);
}

#[tokio::test]
async fn files_read_blocks_parent_traversal_outside_project_root() {
    let (_tmp, state) = test_state::build_test_state();
    let root = tempfile::TempDir::new().unwrap();
    let project = root.path().join("project");
    let outside = root.path().join("outside");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), "secret").unwrap();

    let ctx = mint_session(&state, Some(project.display().to_string()));
    let result = (ryeos_ui::handlers::ui_files::FILES_READ_DESCRIPTOR.handler)(
        json!({"root": "project", "path": "../outside/secret.txt"}),
        ctx,
        Arc::new(state),
    )
    .await;

    assert!(result.is_err(), "files.read must reject path traversal");
}

#[cfg(unix)]
#[tokio::test]
async fn files_read_blocks_symlink_escape_from_project_root() {
    let (_tmp, state) = test_state::build_test_state();
    let root = tempfile::TempDir::new().unwrap();
    let project = root.path().join("project");
    let outside = root.path().join("outside");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), "secret").unwrap();
    std::os::unix::fs::symlink(outside.join("secret.txt"), project.join("secret-link")).unwrap();

    let ctx = mint_session(&state, Some(project.display().to_string()));
    let result = (ryeos_ui::handlers::ui_files::FILES_READ_DESCRIPTOR.handler)(
        json!({"root": "project", "path": "secret-link"}),
        ctx,
        Arc::new(state),
    )
    .await;

    assert!(result.is_err(), "files.read must reject symlink escapes");
}

#[tokio::test]
async fn files_read_truncates_large_files() {
    let (_tmp, state) = test_state::build_test_state();
    let project = tempfile::TempDir::new().unwrap();
    std::fs::write(project.path().join("large.txt"), vec![b'a'; 256 * 1024 + 7]).unwrap();

    let ctx = mint_session(&state, Some(project.path().display().to_string()));
    let result = (ryeos_ui::handlers::ui_files::FILES_READ_DESCRIPTOR.handler)(
        json!({"root": "project", "path": "large.txt"}),
        ctx,
        Arc::new(state),
    )
    .await
    .expect("files.read should return truncated content");

    assert_eq!(result["size"], 256 * 1024 + 7);
    assert_eq!(result["truncated"], true);
    assert_eq!(result["content"].as_str().unwrap().len(), 256 * 1024);
}

#[tokio::test]
async fn files_list_truncates_large_directories() {
    let (_tmp, state) = test_state::build_test_state();
    let project = tempfile::TempDir::new().unwrap();
    for i in 0..2005 {
        std::fs::write(project.path().join(format!("file-{i:04}.txt")), "x").unwrap();
    }

    let ctx = mint_session(&state, Some(project.path().display().to_string()));
    let result = (ryeos_ui::handlers::ui_files::FILES_LIST_DESCRIPTOR.handler)(
        json!({"root": "project", "path": ""}),
        ctx,
        Arc::new(state),
    )
    .await
    .expect("files.list should return truncated listing");

    assert_eq!(result["truncated"], true);
    assert_eq!(result["entries"].as_array().unwrap().len(), 2000);
}

#[tokio::test]
async fn item_inspect_raw_is_bounded_and_lossy() {
    let (_tmp, state) = test_state::build_test_state_with_live_bundles();
    let project = tempfile::TempDir::new().unwrap();
    let tool_dir = project.path().join(".ai/tools/test");
    std::fs::create_dir_all(&tool_dir).unwrap();
    let mut bytes = vec![b'a'; 256 * 1024 + 64];
    bytes[10] = 0xff;
    std::fs::write(tool_dir.join("huge.py"), bytes).unwrap();

    let ctx = mint_session(&state, Some(project.path().display().to_string()));
    let result = (ryeos_ui::handlers::ui_items::ITEM_INSPECT_DESCRIPTOR.handler)(
        json!({
            "canonical_ref": "tool:test/huge",
            "include_raw": true,
            "include_effective": false
        }),
        ctx,
        Arc::new(state),
    )
    .await
    .expect("item.inspect should succeed");

    let raw = &result["raw"];
    assert_eq!(raw["bytes"], 256 * 1024 + 64);
    assert_eq!(raw["truncated"], true);
    assert!(
        raw["content"].as_str().unwrap().len() <= 256 * 1024 + 2,
        "lossy UTF-8 may expand the single replacement character but must stay bounded"
    );
    assert!(raw["content"].as_str().unwrap().contains('�'));
}

#[tokio::test]
async fn items_list_applies_filters_and_accepts_include_shadowed() {
    let (_tmp, state) = test_state::build_test_state_with_live_bundles();
    let ctx = mint_session(&state, None);
    let result = (ryeos_ui::handlers::ui_items::ITEMS_LIST_DESCRIPTOR.handler)(
        json!({
            "kind": "tool",
            "space": "bundle",
            "query": "core/sign",
            "limit": 1,
            "include_shadowed": true
        }),
        ctx,
        Arc::new(state),
    )
    .await
    .expect("items.list should succeed");

    let items = result["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "summary: {:?}", &result["counts"]);
    assert_eq!(items[0]["canonical_ref"], "tool:ryeos/core/sign");
    assert_eq!(items[0]["item_kind"], "tool");
    assert_eq!(items[0]["space"], "bundle");
}
