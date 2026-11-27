// A seat of a key that is not the node's operator and has no project of its
// own cannot name one. `files/list`, `files/tree` and `files/read` answer
// `no_project_bound` for any `project_path`, however real the directory, and
// return nothing from it.

use std::sync::Arc;

use serde_json::json;

use ryeos_app::handler_context::HandlerContext;
use ryeos_ui::state::get_ui_state;
use ryeos_ui::test_support as test_state;

/// A key with no registry record: not the node operator's.
const STRANGER: &str = "fp:b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2";

/// A seat of `principal` bound to no project.
fn projectless_seat(
    state: &ryeos_app::state::AppState,
    principal: &str,
) -> Arc<ryeos_ui::browser_session::AdmittedBindingAttachment> {
    let mut launch = test_state::launch_context(
        "surface:ryeos/ui/base",
        None,
        ryeos_ui::compiled_binding::EffectiveUiPosture::ObservationOnly,
        None,
    );
    Arc::make_mut(&mut launch.compiled_binding)
        .binding
        .principal_id = principal.to_string();
    launch.project_authority = None;
    let (session_id, token) = test_state::mint_launch(state, launch);
    let ui = get_ui_state(state).expect("UI state");
    assert_eq!(
        ui.browser_sessions.consume_launch_token(&token),
        Some(session_id.clone())
    );
    let session = ui
        .browser_sessions
        .get_session(&session_id)
        .expect("active session");
    session.attachments[&session.root_attachment_id].clone()
}

fn seat_context() -> HandlerContext {
    HandlerContext::new(
        "fp:compiled-source".to_string(),
        vec!["ui.read".into()],
        true,
    )
}

#[tokio::test]
async fn a_non_operator_seat_with_no_project_reads_no_directory_it_names() {
    let (_tmp, _owner, state) = test_state::build_test_state();
    let folder = tempfile::TempDir::new().unwrap();
    std::fs::write(folder.path().join("secret.txt"), "the-private-text").unwrap();
    let project_path = folder.path().canonicalize().unwrap().display().to_string();
    let seat = projectless_seat(&state, STRANGER);
    let state = Arc::new(state);

    let list = ryeos_ui::seat_auth::with_compiled_ui_attachment(
        seat.clone(),
        ryeos_ui::handlers::ui_files::handle_files_list(
            json!({ "root": "project", "project_path": project_path }),
            seat_context(),
            state.clone(),
        ),
    )
    .await;
    let tree = ryeos_ui::seat_auth::with_compiled_ui_attachment(
        seat.clone(),
        ryeos_ui::handlers::ui_files::handle_files_tree(
            json!({ "root": "project", "project_path": project_path }),
            seat_context(),
            state.clone(),
        ),
    )
    .await;
    let read = ryeos_ui::seat_auth::with_compiled_ui_attachment(
        seat,
        ryeos_ui::handlers::ui_files::handle_files_read(
            json!({ "root": "project", "path": "secret.txt", "project_path": project_path }),
            seat_context(),
            state,
        ),
    )
    .await;

    for (what, result) in [("list", list), ("tree", tree), ("read", read)] {
        let error = result.expect_err(&format!("files/{what} must refuse a non-operator seat"));
        let text = format!("{error:#}");
        assert!(text.contains("no_project_bound"), "files/{what}: {text}");
        assert!(
            !text.contains("the-private-text") && !text.contains("secret.txt"),
            "files/{what} leaked: {text}"
        );
    }
}
