mod test_state;

use std::sync::Arc;

use ryeos_app::handler_context::HandlerContext;
use ryeos_app::state::AppState;
use ryeos_app::state_store::NewThreadRecord;
use ryeos_state::objects::{
    CapturedItemTrustClass, CapturedNodeHistoryPolicyProvenance, CapturedPolicyProvenance,
    CapturedThreadHistoryPolicy, EnvironmentAuthority, ExecutionProjectAuthority,
    LiveFilesystemConfinement, LiveProjectAccess, ThreadHistoryRetention,
};
use ryeos_ui::state::get_ui_state;

fn captured_policy() -> CapturedThreadHistoryPolicy {
    let hash = "a".repeat(64);
    CapturedThreadHistoryPolicy {
        retention: ThreadHistoryRetention::Durable,
        canonical_item_ref: "directive:test/project-authority".to_string(),
        item_content_hash: hash.clone(),
        item_signer_fingerprint: Some(hash.clone()),
        item_trust_class: CapturedItemTrustClass::Trusted,
        kind_schema_content_hash: hash,
        resolved_from: CapturedPolicyProvenance::NodeDefault {
            node_policy: CapturedNodeHistoryPolicyProvenance::test_policy(),
        },
    }
}

fn create_project_thread(state: &AppState, thread_id: &str, project_root: &std::path::Path) {
    let project_root = project_root
        .canonicalize()
        .expect("canonical fixture project");
    let project_authority = ExecutionProjectAuthority::live(
        project_root.clone(),
        format!("local:{}", project_root.display()),
        LiveProjectAccess::ReadOnly,
        LiveFilesystemConfinement::standard_fixed_parents(),
        EnvironmentAuthority::None,
        Vec::new(),
    )
    .expect("live fixture project authority");
    state
        .state_store
        .create_thread_for_test(&NewThreadRecord {
            thread_id: thread_id.to_string(),
            chain_root_id: thread_id.to_string(),
            kind: "directive".to_string(),
            item_ref: "directive:test/project-authority".to_string(),
            executor_ref: "test/executor".to_string(),
            launch_mode: "wait".to_string(),
            current_site_id: "site:test".to_string(),
            origin_site_id: "site:test".to_string(),
            upstream_thread_id: None,
            requested_by: Some("fp:test".to_string()),
            project_root: Some(project_root),
            project_authority,
            base_project_snapshot_hash: None,
            usage_subject: None,
            usage_subject_asserted_by: None,
            captured_history_policy: Some(captured_policy()),
        })
        .expect("create fixture thread");
    state
        .state_store
        .mark_thread_running(thread_id, None)
        .expect("mark fixture thread running");
}

fn compiled_project_session(
    state: &AppState,
    project_root: &std::path::Path,
) -> ryeos_ui::browser_session::BrowserSession {
    let project_root = project_root
        .canonicalize()
        .expect("canonical session project");
    let mut launch = test_state::launch_context(
        "surface:ryeos/ui/base",
        Some(project_root.to_string_lossy().as_ref()),
        ryeos_ui::compiled_binding::EffectiveUiPosture::ObservationOnly,
        None,
    );
    launch.project_authority = Some(Arc::new(
        lillux::PinnedDirectory::open(&project_root)
            .expect("open session project")
            .expect("session project exists"),
    ));
    let (session_id, token) = test_state::mint_launch(state, launch);
    assert_eq!(
        get_ui_state(state)
            .expect("UI state")
            .browser_sessions
            .consume_launch_token(&token),
        Some(session_id.clone())
    );
    get_ui_state(state)
        .expect("UI state")
        .browser_sessions
        .get_session(&session_id)
        .expect("active session")
}

/// A live shared-presentation session whose seat holds one authored project
/// Surface. The launch names the project by `spelling`; the session retains
/// the canonical project identity.
async fn mint_compiled_project_session(
    state: &AppState,
    project_root: &std::path::Path,
    spelling: &std::path::Path,
) -> (
    ryeos_ui::browser_session::BrowserSession,
    HandlerContext,
    ryeos_client_base::ui::UiPresentationProducer,
    ryeos_ui::browser_session::BindingAttachmentCoordinate,
) {
    let (session_id, context, producer, coordinate) =
        test_state::live_presentation_with_surface_in_project(
            state,
            project_root,
            spelling,
            "surface:ryeos/ui/project",
        )
        .await;
    let session = get_ui_state(state)
        .expect("UI state")
        .browser_sessions
        .get_session(&session_id)
        .expect("active compiled project session");
    let attachment = session
        .attachments
        .get(&coordinate.binding_attachment_id)
        .expect("surface attachment");
    let binding = &attachment.compiled_binding.binding;
    assert!(
        binding
            .sources
            .get("view:ryeos/threads/history")
            .is_some_and(|sources| sources.contains_key("default")),
        "the acceptance test must cross a source coordinate compiled from the live surface: {} {:?} {:?} {:?}",
        attachment.root_ref,
        binding.views.keys().collect::<Vec<_>>(),
        binding.sources.keys().collect::<Vec<_>>(),
        binding.attenuated
    );
    (session, context, producer, coordinate)
}

async fn dispatch_project_threads(
    state: Arc<AppState>,
    context: HandlerContext,
    producer: &ryeos_client_base::ui::UiPresentationProducer,
    coordinate: &ryeos_ui::browser_session::BindingAttachmentCoordinate,
    attempted_project_override: &std::path::Path,
) -> serde_json::Value {
    (ryeos_ui::handlers::ui_invocations_dispatch::DESCRIPTOR.handler)(
        serde_json::json!({
            "producer": producer,
            "binding": {
                "binding_attachment_id": coordinate.binding_attachment_id,
                "binding_generation": coordinate.binding_generation,
                "binding_digest": coordinate.binding_digest,
                "coordinate": {
                    "kind": "source",
                    "view_ref": "view:ryeos/threads/history",
                    "channel": "default"
                },
                "payload": {
                    "kind": "source_parameters",
                    // This key is part of the signed source template, but the
                    // browser-supplied value must never replace its retained
                    // @session:project_root authority.
                    "params": {"project_path": attempted_project_override}
                }
            }
        }),
        context,
        state,
    )
    .await
    .expect("dispatch compiled project source")
}

#[tokio::test]
async fn compiled_project_sources_query_only_the_canonical_session_project() {
    let (_state_dir, _state_owner, state) = test_state::build_test_state();
    let projects = tempfile::tempdir().expect("projects tempdir");
    let first = projects.path().join("first");
    let second = projects.path().join("second");
    std::fs::create_dir(&first).expect("create first project");
    std::fs::create_dir(&second).expect("create second project");
    create_project_thread(&state, "T-first", &first);
    create_project_thread(&state, "T-second", &second);
    let session = compiled_project_session(&state, &first);
    let state = Arc::new(state);
    let ctx = HandlerContext::new(
        "fp:compiled-source".to_string(),
        vec!["ui.read".into()],
        true,
    );

    let attachment = session
        .attachments
        .get(&session.root_attachment_id)
        .expect("surface attachment")
        .clone();
    ryeos_ui::seat_auth::with_compiled_ui_attachment(attachment, async {
        let threads = (ryeos_ui::handlers::ui_threads::DESCRIPTOR.handler)(
            serde_json::json!({"project": "current", "limit": 10}),
            ctx.clone(),
            state.clone(),
        )
        .await
        .expect("current-project thread source");
        let thread_ids = threads["threads"]
            .as_array()
            .expect("thread rows")
            .iter()
            .filter_map(|row| row["thread_id"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(thread_ids, vec!["T-first"]);

        let work = (ryeos_ui::handlers::ui_work::DESCRIPTOR.handler)(
            serde_json::json!({"project": "current", "limit": 10}),
            ctx.clone(),
            state.clone(),
        )
        .await
        .expect("current-project work source");
        let work_ids = work["work"]
            .as_array()
            .expect("work rows")
            .iter()
            .filter_map(|row| row["coordinate"]["chain_root_id"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(work_ids, vec!["T-first"]);

        let runs = (ryeos_ui::handlers::ui_field_runs::DESCRIPTOR.handler)(
            serde_json::json!({"limit": 10}),
            ctx,
            state,
        )
        .await
        .expect("current-project field runs source");
        let canonical_first = first.canonicalize().expect("canonical first project");
        assert_eq!(
            runs["subject"]["id"],
            format!(
                "project:{}",
                lillux::sha256_hex(canonical_first.to_string_lossy().as_bytes())
            )
        );
        let encoded = serde_json::to_string(&runs).expect("encode field runs");
        assert!(encoded.contains("T-first"));
        assert!(!encoded.contains("T-second"));
    })
    .await;
}

#[tokio::test]
#[ignore = "requires populated handler binaries via scripts/populate-bundles.sh"]
async fn compiled_browser_dispatch_keeps_equivalent_paths_and_live_projects_isolated() {
    let (_state_dir, _state_owner, state) = test_state::build_test_state_with_live_bundles();
    let projects = tempfile::tempdir().expect("projects tempdir");
    let first = projects.path().join("first");
    let second = projects.path().join("second");
    let first_alias = projects.path().join("first-alias");
    std::fs::create_dir(&first).expect("create first project");
    std::fs::create_dir(&second).expect("create second project");
    std::os::unix::fs::symlink(&first, &first_alias).expect("create equivalent project path");
    create_project_thread(&state, "T-first", &first);
    create_project_thread(&state, "T-second", &second);

    // Compile and activate real browser sessions. The first enters through an
    // equivalent symlink path, proving the session retains the canonical
    // project identity rather than trusting the request spelling.
    let (first_session, first_context, first_producer, first_coordinate) =
        mint_compiled_project_session(&state, &first, &first_alias).await;
    let (second_session, second_context, second_producer, second_coordinate) =
        mint_compiled_project_session(&state, &second, &second).await;
    let canonical_first = first.canonicalize().expect("canonical first project");
    let canonical_second = second.canonicalize().expect("canonical second project");
    assert_eq!(
        first_session
            .attachments
            .get(&first_coordinate.binding_attachment_id)
            .and_then(|attachment| attachment.project_query_identity.as_deref()),
        Some(canonical_first.to_string_lossy().as_ref())
    );
    assert_eq!(
        second_session
            .attachments
            .get(&second_coordinate.binding_attachment_id)
            .and_then(|attachment| attachment.project_query_identity.as_deref()),
        Some(canonical_second.to_string_lossy().as_ref())
    );

    let state = Arc::new(state);
    let first_result = dispatch_project_threads(
        state.clone(),
        first_context,
        &first_producer,
        &first_coordinate,
        &canonical_second,
    )
    .await;
    let second_result = dispatch_project_threads(
        state,
        second_context,
        &second_producer,
        &second_coordinate,
        &canonical_first,
    )
    .await;

    for (result, expected, excluded) in [
        (&first_result, "T-first", "T-second"),
        (&second_result, "T-second", "T-first"),
    ] {
        assert_eq!(result["status"], "executed");
        assert_eq!(result["target"]["ref"], "service:ui/ryeos-ui/threads/list");
        let encoded = serde_json::to_string(&result["result"]["result"]["threads"])
            .expect("encode dispatched thread rows");
        assert!(encoded.contains(expected), "missing {expected}: {encoded}");
        assert!(
            !encoded.contains(excluded),
            "cross-project row {excluded} escaped through compiled dispatch: {encoded}"
        );
    }
}

/// A project directory and a decoy beside it, with the session's retained
/// access opened on the project. Exchanging the two pathnames afterwards
/// leaves the retained descriptor on the original directory while the
/// project's pathname names the decoy.
fn retained_access_then_swap(
    state: &AppState,
    project: &std::path::Path,
    decoy: &std::path::Path,
) -> (
    ryeos_ui::browser_session::BrowserSession,
    ryeos_ui::seat_auth::RetainedProjectAccess,
) {
    let session = compiled_project_session(state, project);
    let attachment = session
        .attachments
        .get(&session.root_attachment_id)
        .expect("surface attachment")
        .clone();
    let access = ryeos_ui::seat_auth::SeatCaller::Attachment(attachment)
        .project_access()
        .expect("project access")
        .expect("bound project");
    lillux::atomic_exchange_paths(project, decoy).expect("swap project path with decoy");
    (session, access)
}

fn write_project_file(root: &std::path::Path, relative: &str, content: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().expect("fixture file parent"))
        .expect("create fixture parent");
    std::fs::write(path, content).expect("write fixture file");
}

fn invalid_remote_names(report: &[serde_json::Value]) -> Vec<String> {
    report
        .iter()
        .filter_map(|entry| entry["name"].as_str().map(str::to_owned))
        .collect()
}

#[test]
fn remotes_list_reads_the_retained_project_after_its_path_is_swapped() {
    let (_state_dir, _state_owner, state) = test_state::build_test_state();
    let projects = tempfile::tempdir().expect("projects tempdir");
    let project = projects.path().join("project");
    let decoy = projects.path().join("decoy");
    let remotes = ".ai/config/remotes/remotes.yaml";
    write_project_file(&project, remotes, "remotes:\n  original-remote: {}\n");
    write_project_file(&decoy, remotes, "remotes:\n  decoy-remote: {}\n");
    let canonical_project = project.canonicalize().expect("canonical project");

    let (_session, access) = retained_access_then_swap(&state, &project, &decoy);

    // A by-name walk of the project path now reaches the decoy's layer.
    let by_name = ryeos_api::remote::config::load_remotes_layered_report(
        &state.config.app_root,
        Some(&project),
    )
    .expect("by-name remotes report");
    assert_eq!(
        by_name
            .invalid
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        vec!["decoy-remote"]
    );

    let listed = ryeos_ui::handlers::ui_remotes::remotes_list_for_access(
        &state.config.app_root,
        Some(&access),
    )
    .expect("remotes list through retained access");
    let invalid = listed["invalid_remotes"]
        .as_array()
        .expect("invalid remote rows");
    assert_eq!(invalid_remote_names(invalid), vec!["original-remote"]);
    assert_eq!(invalid[0]["scope"], "project");
    assert_eq!(
        invalid[0]["config_path"],
        serde_json::json!(canonical_project.join(remotes))
    );
}

#[test]
fn seat_admission_reads_the_retained_project_after_its_path_is_swapped() {
    let (_state_dir, _state_owner, state) = test_state::build_test_state_with_live_bundles();
    let projects = tempfile::tempdir().expect("projects tempdir");
    let project = projects.path().join("project");
    let decoy = projects.path().join("decoy");
    std::fs::create_dir_all(project.join(".ai")).expect("create project");
    // The decoy shadows the seat's bundle surface with a project item.
    write_project_file(
        &decoy,
        ".ai/surfaces/ryeos/ui/base.yaml",
        "name: decoy-surface\nversion: \"1.0.0\"\n",
    );
    let canonical_project = project.canonicalize().expect("canonical project");
    let surface_ref = "surface:ryeos/ui/base";

    let (session, access) = retained_access_then_swap(&state, &project, &decoy);
    let owner = format!("session:{}", session.session_id);

    // A by-name walk of the project path now resolves the decoy's shadow.
    let by_name = ryeos_app::thread_lifecycle::admit_non_execution_root(
        &state.engine,
        state.node_history_policy().expect("node history policy"),
        &project,
        ryeos_app::thread_lifecycle::NonExecutionRootRequest {
            item_ref: surface_ref,
            requested_by: &owner,
            caller_scopes: Vec::new(),
            current_site_id: state.threads.site_id(),
            origin_site_id: state.threads.site_id(),
            thread_profile: "seat".to_string(),
        },
    );
    assert_eq!(
        by_name
            .expect("by-name seat admission")
            .verified_subject()
            .resolved
            .source_space,
        ryeos_engine::contracts::ItemSpace::Project,
        "the decoy must be able to redirect a by-name walk"
    );

    let admission = ryeos_ui::handlers::ui_seat::admit_seat_root(
        &state,
        surface_ref,
        Some(&access),
        &owner,
        Vec::new(),
    )
    .expect("seat admission through retained access");
    assert_eq!(
        admission.verified_subject().resolved.source_space,
        ryeos_engine::contracts::ItemSpace::Bundle
    );
    assert_eq!(admission.project_root(), Some(canonical_project.as_path()));
}
