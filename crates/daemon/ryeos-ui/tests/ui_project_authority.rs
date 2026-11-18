use ryeos_ui::test_support as test_state;

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

/// A thread the fixture principal launched outside any live project.
fn create_outside_thread(
    state: &AppState,
    thread_id: &str,
    project_authority: ExecutionProjectAuthority,
) {
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
            project_root: None,
            project_authority,
            base_project_snapshot_hash: None,
            usage_subject: None,
            usage_subject_asserted_by: None,
            captured_history_policy: Some(captured_policy()),
        })
        .expect("create outside fixture thread");
    state
        .state_store
        .mark_thread_running(thread_id, None)
        .expect("mark outside fixture thread running");
}

fn pinned_generation_authority() -> ExecutionProjectAuthority {
    ExecutionProjectAuthority::PinnedGeneration {
        stable_project_identity: "project".to_string(),
        display_path: None,
        base_snapshot_hash: "a".repeat(64),
        snapshot_hash: "b".repeat(64),
        realization: ryeos_state::objects::PinnedProjectRealization::ReadOnly,
        workspace_outputs: None,
        environment: EnvironmentAuthority::None,
        capability_ceiling: Vec::new(),
        child_policy: ryeos_state::objects::ChildProjectAuthorityPolicy::Inherit,
    }
}

fn root_attachment(
    session: &ryeos_ui::browser_session::BrowserSession,
) -> Arc<ryeos_ui::browser_session::AdmittedBindingAttachment> {
    session
        .attachments
        .get(&session.root_attachment_id)
        .expect("surface attachment")
        .clone()
}

async fn expect_not_found(
    handler: ryeos_app::service_registry::RawHandlerFn,
    params: serde_json::Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
    what: &str,
) {
    match handler(params, ctx, state).await {
        Ok(value) => panic!("{what} must not be readable, got {value}"),
        Err(error) => assert!(
            matches!(
                error.downcast_ref::<ryeos_app::handler_error::HandlerError>(),
                Some(ryeos_app::handler_error::HandlerError::NotFound)
            ),
            "{what}: {error:#}"
        ),
    }
}

/// A source a project-authored view declared cannot read the definition of a
/// thread outside the session's live project: not another project's thread,
/// and not a projectless thread the session's own principal launched.
#[tokio::test]
async fn project_authored_field_definition_source_refuses_threads_outside_the_session_project() {
    let (_state_dir, _state_owner, state) = test_state::build_test_state();
    let (_projects, session, ctx) = two_project_session(&state);
    create_outside_thread(
        &state,
        "T-projectless",
        ExecutionProjectAuthority::PROJECTLESS,
    );
    let state = Arc::new(state);
    ryeos_ui::seat_auth::with_project_authored_compiled_ui_source(
        root_attachment(&session),
        async {
            for thread_id in ["T-second", "T-projectless"] {
                expect_not_found(
                    ryeos_ui::handlers::ui_field_definition::DESCRIPTOR.handler,
                    serde_json::json!({ "thread_id": thread_id }),
                    ctx.clone(),
                    state.clone(),
                    thread_id,
                )
                .await;
            }
        },
    )
    .await;
}

/// The same for the execution source, which would otherwise return the
/// thread's events, results and artifacts.
#[tokio::test]
async fn project_authored_field_execution_source_refuses_threads_outside_the_session_project() {
    let (_state_dir, _state_owner, state) = test_state::build_test_state();
    let (_projects, session, ctx) = two_project_session(&state);
    create_outside_thread(
        &state,
        "T-projectless",
        ExecutionProjectAuthority::PROJECTLESS,
    );
    let state = Arc::new(state);
    ryeos_ui::seat_auth::with_project_authored_compiled_ui_source(
        root_attachment(&session),
        async {
            for thread_id in ["T-second", "T-projectless"] {
                expect_not_found(
                    ryeos_ui::handlers::ui_field_execution::DESCRIPTOR.handler,
                    serde_json::json!({ "thread_id": thread_id }),
                    ctx.clone(),
                    state.clone(),
                    thread_id,
                )
                .await;
            }
        },
    )
    .await;
}

/// The runs source lists a projectless thread of the session's principal to
/// a node-authored source, as it always has, and never to a source a
/// project-authored view declared. Another project's thread is in neither.
#[tokio::test]
async fn project_authored_field_runs_source_lists_only_the_session_project() {
    let (_state_dir, _state_owner, state) = test_state::build_test_state();
    let (_projects, session, ctx) = two_project_session(&state);
    create_outside_thread(
        &state,
        "T-projectless",
        ExecutionProjectAuthority::PROJECTLESS,
    );
    let state = Arc::new(state);
    let list = |ctx: HandlerContext, state: Arc<AppState>| async move {
        let runs = (ryeos_ui::handlers::ui_field_runs::DESCRIPTOR.handler)(
            serde_json::json!({ "limit": 1000 }),
            ctx,
            state,
        )
        .await
        .expect("field runs source");
        serde_json::to_string(&runs).expect("encode field runs")
    };

    let node_authored = ryeos_ui::seat_auth::with_compiled_ui_attachment(
        root_attachment(&session),
        list(ctx.clone(), state.clone()),
    )
    .await;
    assert!(node_authored.contains("T-first"));
    assert!(node_authored.contains("T-projectless"));
    assert!(!node_authored.contains("T-second"));

    let project_authored = ryeos_ui::seat_auth::with_project_authored_compiled_ui_source(
        root_attachment(&session),
        list(ctx, state),
    )
    .await;
    assert!(project_authored.contains("T-first"));
    assert!(!project_authored.contains("T-projectless"));
    assert!(!project_authored.contains("T-second"));
}

/// A pinned-generation thread the session's principal launched is outside
/// the live project too. A project-authored source of each thread-reading
/// field service is refused it or does not list it.
#[tokio::test]
async fn project_authored_field_sources_never_read_a_pinned_generation_thread() {
    let (_state_dir, _state_owner, state) = test_state::build_test_state();
    let (_projects, session, ctx) = two_project_session(&state);
    create_outside_thread(&state, "T-pinned", pinned_generation_authority());
    let state = Arc::new(state);
    ryeos_ui::seat_auth::with_project_authored_compiled_ui_source(
        root_attachment(&session),
        async {
            for handler in [
                ryeos_ui::handlers::ui_field_definition::DESCRIPTOR.handler,
                ryeos_ui::handlers::ui_field_execution::DESCRIPTOR.handler,
            ] {
                expect_not_found(
                    handler,
                    serde_json::json!({ "thread_id": "T-pinned" }),
                    ctx.clone(),
                    state.clone(),
                    "a pinned-generation thread",
                )
                .await;
            }
            let runs = (ryeos_ui::handlers::ui_field_runs::DESCRIPTOR.handler)(
                serde_json::json!({ "limit": 1000 }),
                ctx.clone(),
                state.clone(),
            )
            .await
            .expect("field runs source");
            let encoded = serde_json::to_string(&runs).expect("encode field runs");
            assert!(encoded.contains("T-first"));
            assert!(!encoded.contains("T-pinned"));
        },
    )
    .await;
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
    // The work source reads the signed attention config, so the engine needs
    // the installed bundles.
    let (_state_dir, _state_owner, state) = test_state::build_test_state_with_live_bundles();
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

/// Two projects, a thread in each, and a compiled session on the first. The
/// returned directories keep both projects alive for the test's duration.
fn two_project_session(
    state: &AppState,
) -> (
    tempfile::TempDir,
    ryeos_ui::browser_session::BrowserSession,
    HandlerContext,
) {
    let projects = tempfile::tempdir().expect("projects tempdir");
    let first = projects.path().join("first");
    let second = projects.path().join("second");
    std::fs::create_dir(&first).expect("create first project");
    std::fs::create_dir(&second).expect("create second project");
    create_project_thread(state, "T-first", &first);
    create_project_thread(state, "T-second", &second);
    let session = compiled_project_session(state, &first);
    let ctx = HandlerContext::new(
        "fp:compiled-source".to_string(),
        vec!["ui.read".into()],
        true,
    );
    (projects, session, ctx)
}

#[tokio::test]
async fn field_project_refuses_an_authored_project_path() {
    let (_state_dir, _state_owner, state) = test_state::build_test_state();
    let (_projects, session, ctx) = two_project_session(&state);
    let state = Arc::new(state);
    let attachment = session
        .attachments
        .get(&session.root_attachment_id)
        .expect("surface attachment")
        .clone();
    ryeos_ui::seat_auth::with_compiled_ui_attachment(attachment, async {
        // The service takes no project selector; an authored project path
        // is an unknown field and is refused before any project is read.
        let error = (ryeos_ui::handlers::ui_field_project::DESCRIPTOR.handler)(
            serde_json::json!({"project_path": "/"}),
            ctx,
            state,
        )
        .await
        .expect_err("an authored project path must be refused");
        assert!(matches!(
            error.downcast_ref::<ryeos_app::handler_error::HandlerError>(),
            Some(ryeos_app::handler_error::HandlerError::BadRequest(_))
        ));
    })
    .await;
}

#[tokio::test]
async fn field_definition_refuses_a_thread_of_another_project() {
    let (_state_dir, _state_owner, state) = test_state::build_test_state();
    let (_projects, session, ctx) = two_project_session(&state);
    let state = Arc::new(state);
    let attachment = session
        .attachments
        .get(&session.root_attachment_id)
        .expect("surface attachment")
        .clone();
    ryeos_ui::seat_auth::with_compiled_ui_attachment(attachment, async {
        let error = (ryeos_ui::handlers::ui_field_definition::DESCRIPTOR.handler)(
            serde_json::json!({
                "thread_id": "T-second",
                "definition_ref": "directive:test/project-authority",
            }),
            ctx,
            state,
        )
        .await
        .expect_err("a thread of another project must not be readable");
        assert!(matches!(
            error.downcast_ref::<ryeos_app::handler_error::HandlerError>(),
            Some(ryeos_app::handler_error::HandlerError::NotFound)
        ));
    })
    .await;
}

#[tokio::test]
async fn field_execution_refuses_a_thread_of_another_project() {
    let (_state_dir, _state_owner, state) = test_state::build_test_state();
    let (_projects, session, ctx) = two_project_session(&state);
    let state = Arc::new(state);
    let attachment = session
        .attachments
        .get(&session.root_attachment_id)
        .expect("surface attachment")
        .clone();
    ryeos_ui::seat_auth::with_compiled_ui_attachment(attachment, async {
        let error = (ryeos_ui::handlers::ui_field_execution::DESCRIPTOR.handler)(
            serde_json::json!({"thread_id": "T-second", "max_depth": 8, "max_nodes": 64}),
            ctx,
            state,
        )
        .await
        .expect_err("a thread of another project must not be readable");
        assert!(matches!(
            error.downcast_ref::<ryeos_app::handler_error::HandlerError>(),
            Some(ryeos_app::handler_error::HandlerError::NotFound)
        ));
    })
    .await;
}

#[tokio::test]
async fn field_runs_with_an_item_ref_lists_only_the_session_project() {
    let (_state_dir, _state_owner, state) = test_state::build_test_state();
    let (_projects, session, ctx) = two_project_session(&state);
    let state = Arc::new(state);
    let attachment = session
        .attachments
        .get(&session.root_attachment_id)
        .expect("surface attachment")
        .clone();
    ryeos_ui::seat_auth::with_compiled_ui_attachment(attachment, async {
        // Both fixture threads run this item; only the session project's
        // run may come back.
        let runs = (ryeos_ui::handlers::ui_field_runs::DESCRIPTOR.handler)(
            serde_json::json!({
                "item_ref": "directive:test/project-authority",
                "limit": 1000,
            }),
            ctx,
            state,
        )
        .await
        .expect("item-scoped field runs source");
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

    // A by-name walk of the project path now reaches the decoy's shadow.
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
    // The decoy's surface takes a ref the bundle ships, so the walk that
    // reads the decoy is refused as a shadow instead of serving it.
    let by_name = match by_name {
        Ok(_) => panic!("a by-name walk of the swapped path must reach the decoy and be refused"),
        Err(error) => format!("{error:#}"),
    };
    assert!(
        by_name.contains("shadows the item of the same ref"),
        "the decoy must be able to redirect a by-name walk: {by_name}"
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

/// A project file at the ref of a view the node's project surface binds does
/// not replace that view. The surface still opens; the shadowed view is held
/// in the binding as a degraded entry whose reason is the engine's refusal
/// naming both files, it has no sources and no affordances, and the views
/// the project does not shadow keep theirs. The project file here is
/// unsigned: the refusal is decided before any signature is read.
#[tokio::test]
async fn project_file_shadowing_a_bound_view_leaves_that_view_unavailable_with_its_reason() {
    let (_state_dir, _state_owner, state) = test_state::build_test_state_with_live_bundles();
    let projects = tempfile::tempdir().expect("projects tempdir");
    let project = projects.path().join("project");
    std::fs::create_dir(&project).expect("create project");
    write_project_file(
        &project,
        ".ai/views/ryeos/threads/history.yaml",
        "widget: text\n",
    );

    let (session_id, _context, _producer, coordinate) =
        test_state::live_presentation_with_surface_in_project(
            &state,
            &project,
            &project,
            "surface:ryeos/ui/project",
        )
        .await;
    let session = get_ui_state(&state)
        .expect("UI state")
        .browser_sessions
        .get_session(&session_id)
        .expect("active compiled project session");
    let attachment = session
        .attachments
        .get(&coordinate.binding_attachment_id)
        .expect("surface attachment");
    let binding = &attachment.compiled_binding.binding;

    match binding.views.get("view:ryeos/threads/history") {
        Some(ryeos_api::surface_views::EmbeddedViewIdentity::Degraded { reason }) => {
            assert!(
                reason.contains("shadows the item of the same ref"),
                "{reason}"
            );
            assert!(reason.contains("view:ryeos/threads/history"), "{reason}");
            assert!(reason.contains("unavailable"), "{reason}");
        }
        other => panic!("the shadowed view must be degraded, got {other:?}"),
    }
    assert!(!binding.sources.contains_key("view:ryeos/threads/history"));
    assert!(
        !binding
            .affordances
            .contains_key("view:ryeos/threads/history")
    );
    assert!(
        binding
            .sources
            .get("view:ryeos/project/files")
            .is_some_and(|sources| !sources.is_empty()),
        "a view the project does not shadow keeps its sources: {:?}",
        binding.attenuated
    );
}

/// A project file at the ref of the shell's presentation config is never
/// read, even when it is signed by a key the node trusts. The shell config
/// is resolved from installed bundles only, so the session's root is the
/// node's own config, identical to the root of a project that ships no such
/// file.
#[tokio::test]
async fn project_config_at_the_shell_presentation_ref_is_never_read() {
    let (_state_dir, _state_owner, state) = test_state::build_test_state_with_live_bundles();
    let projects = tempfile::tempdir().expect("projects tempdir");
    let plain = projects.path().join("plain");
    let decoy = projects.path().join("decoy");
    std::fs::create_dir(&plain).expect("create plain project");
    std::fs::create_dir(&decoy).expect("create decoy project");
    let key = lillux::crypto::load_signing_key(
        &ryeos_engine::test_support::workspace_root().join(".dev-keys/PUBLISHER_DEV.pem"),
    )
    .expect("established development publisher key");
    let body = "category: test\nversion: '1'\nshared_presentation:\n  schema: 2\n  title: Decoy shell\n  launcher: launch\n  dialogs:\n    - id: launch\n      title: Launch\n      view: 'view:ryeos/ui/launcher'\n  capabilities:\n    sources: ['*']\n    affordances: ['*']\n";
    write_project_file(
        &decoy,
        ".ai/config/ryeos/ui/presentation.yaml",
        &lillux::signature::sign_content(body, &key, "#", None),
    );

    let mut roots = Vec::new();
    for project in [&plain, &decoy] {
        let (session_id, _context, _producer, coordinate) =
            test_state::live_shipped_presentation_in_project(&state, project).await;
        let session = get_ui_state(&state)
            .expect("UI state")
            .browser_sessions
            .get_session(&session_id)
            .expect("active session");
        let attachment = session
            .attachments
            .get(&coordinate.binding_attachment_id)
            .expect("root attachment");
        roots.push(attachment.compiled_binding.binding.surface.clone());
    }

    assert_eq!(roots[0].canonical_ref, "config:ryeos/ui/presentation");
    assert_eq!(
        roots[0].effective_trust_class,
        ryeos_engine::resolution::TrustClass::TrustedBundle
    );
    assert_eq!(
        roots[1], roots[0],
        "the project's config at the shell's ref must not change the session root"
    );
}

/// Opening a session on a shipped Surface fails when the project holds a
/// file at that Surface's ref. The failure is the named conflict, and it
/// names the project file and the bundle that ships the Surface.
#[tokio::test]
async fn project_file_shadowing_a_shipped_surface_fails_the_open_with_the_named_refusal() {
    let (_state_dir, _state_owner, state) = test_state::build_test_state_with_live_bundles();
    let projects = tempfile::tempdir().expect("projects tempdir");
    let project = projects.path().join("project");
    std::fs::create_dir(&project).expect("create project");
    write_project_file(
        &project,
        ".ai/surfaces/ryeos/ui/project.yaml",
        "name: decoy-surface\nversion: \"1.0.0\"\n",
    );
    let principal = test_state::register_fixture_project(&state, &project).await;

    let refused = ryeos_ui::handlers::ui_launch_mint::handle(
        ryeos_ui::handlers::ui_launch_mint::Request {
            ui_binding_contract_revision: ryeos_ui::UI_BINDING_CONTRACT_REVISION.into(),
            root_ref: "surface:ryeos/ui/project".into(),
            project_path: Some(project.display().to_string()),
            user_principal_id: Some(principal.clone()),
            scopes: None,
        },
        HandlerContext::new(principal, vec!["*".into()], true),
        Arc::new(state.clone()),
    )
    .await;
    let error = match refused {
        Ok(_) => panic!("a shadowed Surface must not open"),
        Err(error) => error,
    };
    match ryeos_app::handler_error::extract_handler_error(&error) {
        Some(ryeos_app::handler_error::HandlerError::Structured { code, status, body }) => {
            assert_eq!(code, "project_shadows_bundle_item");
            assert_eq!(status, 409);
            assert_eq!(body["item_ref"], "surface:ryeos/ui/project");
            assert_eq!(body["bundle"], "ryeos-ui");
            assert!(
                body["project_path"]
                    .as_str()
                    .is_some_and(|path| path.ends_with(".ai/surfaces/ryeos/ui/project.yaml")),
                "{body}"
            );
        }
        other => panic!("expected the named shadow conflict, got {other:?}: {error:#}"),
    }
}

/// Revalidation holds a binding to the project-authoring list it was compiled
/// under. A binding that records the digest of the installed list is still
/// valid; one that records the digest of any other list, as a binding minted
/// before the list changed would, is stale and must be minted again.
#[tokio::test]
async fn binding_compiled_under_another_project_authoring_list_is_stale() {
    let (_state_dir, _state_owner, state) = test_state::build_test_state_with_live_bundles();
    let (session_id, _context, _producer, coordinate) =
        test_state::live_shipped_presentation(&state).await;
    let session = get_ui_state(&state)
        .expect("UI state")
        .browser_sessions
        .get_session(&session_id)
        .expect("active session");
    let minted = session
        .attachments
        .get(&coordinate.binding_attachment_id)
        .expect("root attachment")
        .compiled_binding
        .as_ref()
        .clone();
    assert_eq!(
        minted.binding.project_source_ceiling_digest, "",
        "a binding of bundle items records no list"
    );

    state
        .engine
        .with_checked_bundle_generation(|generation| -> anyhow::Result<()> {
            let revalidate = |binding: &ryeos_ui::compiled_binding::SessionCompiledUiBinding| {
                ryeos_ui::handlers::ui_launch_mint::revalidate_binding_authority(
                    generation, binding, None, None,
                )
            };
            revalidate(&minted)?;

            let installed =
                ryeos_ui::handlers::ui_launch_mint::load_project_authoring_config(generation)?;
            assert_eq!(installed.sources.len(), 4, "{:?}", installed.sources);
            let mut current = minted.clone();
            current.binding.project_source_ceiling_digest =
                ryeos_ui::compiled_binding::project_source_ceiling_digest(&installed.sources)?;
            revalidate(&current)?;

            let mut outdated = minted.clone();
            outdated.binding.project_source_ceiling_digest =
                ryeos_ui::compiled_binding::project_source_ceiling_digest(&installed.sources[..1])?;
            let error = revalidate(&outdated).expect_err("a changed list stales the binding");
            match ryeos_app::handler_error::extract_handler_error(&error) {
                Some(ryeos_app::handler_error::HandlerError::Structured { code, body, .. }) => {
                    assert_eq!(code, "ui_binding_stale");
                    assert!(
                        body["error"]
                            .as_str()
                            .is_some_and(|message| message.contains("project-authoring")),
                        "{body}"
                    );
                }
                other => panic!("expected a stale binding, got {other:?}"),
            }
            Ok(())
        })
        .expect("revalidate under the installed list");
}
