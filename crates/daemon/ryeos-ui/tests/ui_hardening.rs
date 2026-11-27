use ryeos_ui::test_support as test_state;

use std::sync::Arc;

use ryeos_app::handler_context::HandlerContext;
use ryeos_ui::state::get_ui_state;
use serde_json::json;

fn session_launch_context(
    project_path: Option<String>,
) -> ryeos_ui::browser_session::LaunchContext {
    test_state::launch_context(
        "surface:ryeos/ui/base",
        project_path.as_deref(),
        ryeos_ui::compiled_binding::EffectiveUiPosture::Interactive,
        None,
    )
}

/// A minted, activated browser session: its `session:` context and the
/// launch attachment its UI services are dispatched under.
fn mint_session(
    state: &ryeos_app::state::AppState,
    project_path: Option<String>,
) -> (
    HandlerContext,
    Arc<ryeos_ui::browser_session::AdmittedBindingAttachment>,
) {
    mint_launch_session(state, session_launch_context(project_path))
}

fn mint_launch_session(
    state: &ryeos_app::state::AppState,
    launch: ryeos_ui::browser_session::LaunchContext,
) -> (
    HandlerContext,
    Arc<ryeos_ui::browser_session::AdmittedBindingAttachment>,
) {
    let (session_id, token) = test_state::mint_launch(state, launch);
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
#[ignore = "requires populated handler binaries via scripts/populate-bundles.sh"]
async fn browser_boot_sequence_endpoints_accept_same_session() {
    let (_tmp, _state_owner, state) = test_state::build_test_state_with_live_bundles();
    let (ctx, attachment) = mint_session(&state, None);
    let state = Arc::new(state);

    let session = ryeos_ui::seat_auth::with_compiled_ui_attachment(
        attachment.clone(),
        (ryeos_ui::handlers::ui_session_current::DESCRIPTOR.handler)(
            json!(null),
            ctx.clone(),
            state.clone(),
        ),
    )
    .await
    .expect("session/current should succeed");
    assert!(session.get("surface_ref").is_none());
    let root_attachment_id = session["root_attachment_id"].as_str().unwrap();
    let root = session["binding_attachments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|attachment| attachment["binding_attachment_id"] == root_attachment_id)
        .expect("the session reports its root attachment");
    assert_eq!(root["root_ref"], "surface:ryeos/ui/base");

    let items = ryeos_ui::seat_auth::with_compiled_ui_attachment(
        attachment.clone(),
        (ryeos_ui::handlers::ui_items::ITEMS_LIST_DESCRIPTOR.handler)(
            json!({
                "kind": "surface",
                "query": "ryeos/ui/base",
                "limit": 16
            }),
            ctx.clone(),
            state.clone(),
        ),
    )
    .await
    .expect("items list should succeed");
    assert!(
        items["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["canonical_ref"] == "surface:ryeos/ui/base"),
        "the surface inventory lists the root surface"
    );

    let effective = ryeos_ui::seat_auth::with_compiled_ui_attachment(
        attachment.clone(),
        (ryeos_api::handlers::items_effective::DESCRIPTOR.handler)(
            json!({
                "canonical_ref": root["root_ref"],
                "expected_kind": "surface"
            }),
            ctx.clone(),
            state.clone(),
        ),
    )
    .await
    .expect("items/effective should resolve RyeOS UI surface");
    assert_eq!(effective["canonical_ref"], "surface:ryeos/ui/base");

    let session_id = ctx
        .fingerprint
        .strip_prefix("session:")
        .expect("session context")
        .to_owned();
    let dimension = test_state::with_dispatched_session(
        &state,
        &session_id,
        attachment.clone(),
        (ryeos_ui::handlers::ui_dimension::DESCRIPTOR.handler)(
            json!(null),
            ctx.clone(),
            state.clone(),
        ),
    )
    .await
    .expect("RyeOS UI dimension should succeed");
    assert_eq!(dimension["schema_version"], "ryeos.ui.dimension.v1");

    let inspection = ryeos_ui::seat_auth::with_compiled_ui_attachment(
        attachment.clone(),
        (ryeos_ui::handlers::ui_items::ITEM_INSPECT_DESCRIPTOR.handler)(
            json!({
                "canonical_ref": "surface:ryeos/ui/base",
                "include_raw": true,
                "include_effective": true
            }),
            ctx,
            state,
        ),
    )
    .await
    .expect("item inspect should succeed");
    assert_eq!(inspection["schema_version"], "ryeos-ui.item.inspect.v1");
    assert_eq!(inspection["item"]["canonical_ref"], "surface:ryeos/ui/base");
    assert!(
        inspection["raw"]["content"]
            .as_str()
            .unwrap()
            .contains("ryeos-ui-base")
    );
    assert_eq!(
        inspection["effective"]["composed_value"]["name"],
        "ryeos-ui-base"
    );
}

#[tokio::test]
async fn ryeos_admin_lists_reject_unknown_session_ids() {
    let (_tmp, _state_owner, state) = test_state::build_test_state();
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

    #[cfg(feature = "scheduling")]
    {
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
    }

    let gc = (ryeos_ui::handlers::ui_gc::DESCRIPTOR.handler)(json!({}), ctx, state).await;
    assert!(gc.is_err(), "gc status must reject invalid sessions");
}

#[tokio::test]
async fn files_read_returns_lossy_utf8_for_invalid_bytes() {
    let (_tmp, _state_owner, state) = test_state::build_test_state();
    let project = tempfile::TempDir::new().unwrap();
    std::fs::write(project.path().join("bad.bin"), [b'a', 0xff, b'b']).unwrap();

    let (ctx, attachment) = mint_session(&state, Some(project.path().display().to_string()));
    let result = ryeos_ui::seat_auth::with_compiled_ui_attachment(
        attachment.clone(),
        (ryeos_ui::handlers::ui_files::FILES_READ_DESCRIPTOR.handler)(
            json!({"root": "project", "path": "bad.bin"}),
            ctx,
            Arc::new(state),
        ),
    )
    .await
    .expect("files.read should succeed");

    assert_eq!(result["content"], "a�b");
    assert_eq!(result["size"], 3);
    assert_eq!(result["truncated"], false);
}

#[tokio::test]
async fn files_read_blocks_parent_traversal_outside_project_root() {
    let (_tmp, _state_owner, state) = test_state::build_test_state();
    let root = tempfile::TempDir::new().unwrap();
    let project = root.path().join("project");
    let outside = root.path().join("outside");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), "secret").unwrap();

    let (ctx, attachment) = mint_session(&state, Some(project.display().to_string()));
    let result = ryeos_ui::seat_auth::with_compiled_ui_attachment(
        attachment.clone(),
        (ryeos_ui::handlers::ui_files::FILES_READ_DESCRIPTOR.handler)(
            json!({"root": "project", "path": "../outside/secret.txt"}),
            ctx,
            Arc::new(state),
        ),
    )
    .await;

    assert!(result.is_err(), "files.read must reject path traversal");
}

#[cfg(unix)]
#[tokio::test]
async fn files_read_blocks_symlink_escape_from_project_root() {
    let (_tmp, _state_owner, state) = test_state::build_test_state();
    let root = tempfile::TempDir::new().unwrap();
    let project = root.path().join("project");
    let outside = root.path().join("outside");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), "secret").unwrap();
    std::os::unix::fs::symlink(outside.join("secret.txt"), project.join("secret-link")).unwrap();

    let (ctx, attachment) = mint_session(&state, Some(project.display().to_string()));
    let result = ryeos_ui::seat_auth::with_compiled_ui_attachment(
        attachment.clone(),
        (ryeos_ui::handlers::ui_files::FILES_READ_DESCRIPTOR.handler)(
            json!({"root": "project", "path": "secret-link"}),
            ctx,
            Arc::new(state),
        ),
    )
    .await;

    assert!(result.is_err(), "files.read must reject symlink escapes");
}

#[tokio::test]
async fn files_read_truncates_large_files() {
    let (_tmp, _state_owner, state) = test_state::build_test_state();
    let project = tempfile::TempDir::new().unwrap();
    std::fs::write(project.path().join("large.txt"), vec![b'a'; 256 * 1024 + 7]).unwrap();

    let (ctx, attachment) = mint_session(&state, Some(project.path().display().to_string()));
    let result = ryeos_ui::seat_auth::with_compiled_ui_attachment(
        attachment.clone(),
        (ryeos_ui::handlers::ui_files::FILES_READ_DESCRIPTOR.handler)(
            json!({"root": "project", "path": "large.txt"}),
            ctx,
            Arc::new(state),
        ),
    )
    .await
    .expect("files.read should return truncated content");

    assert_eq!(result["size"], 256 * 1024 + 7);
    assert_eq!(result["truncated"], true);
    assert_eq!(result["content"].as_str().unwrap().len(), 256 * 1024);
}

#[tokio::test]
async fn files_list_truncates_large_directories() {
    let (_tmp, _state_owner, state) = test_state::build_test_state();
    let project = tempfile::TempDir::new().unwrap();
    for i in 0..2005 {
        std::fs::write(project.path().join(format!("file-{i:04}.txt")), "x").unwrap();
    }

    let (ctx, attachment) = mint_session(&state, Some(project.path().display().to_string()));
    let result = ryeos_ui::seat_auth::with_compiled_ui_attachment(
        attachment.clone(),
        (ryeos_ui::handlers::ui_files::FILES_LIST_DESCRIPTOR.handler)(
            json!({"root": "project", "path": ""}),
            ctx,
            Arc::new(state),
        ),
    )
    .await
    .expect("files.list should return truncated listing");

    assert_eq!(result["truncated"], true);
    assert_eq!(result["entries"].as_array().unwrap().len(), 2000);
}

#[tokio::test]
async fn item_inspect_raw_is_bounded_and_lossy() {
    let (_tmp, _state_owner, state) = test_state::build_test_state_with_live_bundles();
    let project = tempfile::TempDir::new().unwrap();
    let tool_dir = project.path().join(".ai/tools/test");
    std::fs::create_dir_all(&tool_dir).unwrap();
    let mut bytes = vec![b'a'; 256 * 1024 + 64];
    bytes[10] = 0xff;
    std::fs::write(tool_dir.join("huge.py"), bytes).unwrap();

    let (ctx, attachment) = mint_session(&state, Some(project.path().display().to_string()));
    let result = ryeos_ui::seat_auth::with_compiled_ui_attachment(
        attachment.clone(),
        (ryeos_ui::handlers::ui_items::ITEM_INSPECT_DESCRIPTOR.handler)(
            json!({
                "canonical_ref": "tool:test/huge",
                "include_raw": true,
                "include_effective": false
            }),
            ctx,
            Arc::new(state),
        ),
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
    let (_tmp, _state_owner, state) = test_state::build_test_state_with_live_bundles();
    let (ctx, attachment) = mint_session(&state, None);
    let result = ryeos_ui::seat_auth::with_compiled_ui_attachment(
        attachment.clone(),
        (ryeos_ui::handlers::ui_items::ITEMS_LIST_DESCRIPTOR.handler)(
            json!({
                "kind": "tool",
                "space": "bundle",
                "query": "core/sign",
                "limit": 1,
                "include_shadowed": true
            }),
            ctx,
            Arc::new(state),
        ),
    )
    .await
    .expect("items.list should succeed");

    let items = result["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "summary: {:?}", &result["counts"]);
    assert_eq!(items[0]["canonical_ref"], "tool:ryeos/core/sign");
    assert_eq!(items[0]["item_kind"], "tool");
    assert_eq!(items[0]["space"], "bundle");
}

#[tokio::test]
async fn usage_observes_only_signed_subjects_owned_by_projectless_caller() {
    use ryeos_state::objects::{
        CapturedItemTrustClass, CapturedNodeHistoryPolicyProvenance, CapturedPolicyProvenance,
        CapturedThreadHistoryPolicy, EnvironmentAuthority, ExecutionProjectAuthority,
        ThreadHistoryRetention,
    };
    let (_tmp, _state_owner, state) = test_state::build_test_state();
    for (thread_id, principal) in [
        (
            "T-visible",
            "fp:7e577e577e577e577e577e577e577e577e577e577e577e577e577e577e577e57",
        ),
        (
            "T-private",
            "fp:f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0",
        ),
    ] {
        state
            .state_store
            .create_thread_for_test(&ryeos_app::state_store::NewThreadRecord {
                thread_id: thread_id.into(),
                chain_root_id: thread_id.into(),
                kind: "tool".into(),
                item_ref: "tool:test/usage".into(),
                executor_ref: "test/executor".into(),
                launch_mode: "wait".into(),
                current_site_id: "site:testhost".into(),
                origin_site_id: "site:testhost".into(),
                upstream_thread_id: None,
                requested_by: Some(principal.into()),
                project_root: None,
                project_authority: ExecutionProjectAuthority::Projectless {
                    environment: EnvironmentAuthority::None,
                },
                base_project_snapshot_hash: None,
                usage_subject: None,
                usage_subject_asserted_by: None,
                captured_history_policy: Some(CapturedThreadHistoryPolicy {
                    retention: ThreadHistoryRetention::Durable,
                    canonical_item_ref: "tool:test/usage".into(),
                    item_content_hash: "a".repeat(64),
                    item_signer_fingerprint: Some("a".repeat(64)),
                    item_trust_class: CapturedItemTrustClass::Trusted,
                    kind_schema_content_hash: "a".repeat(64),
                    resolved_from: CapturedPolicyProvenance::NodeDefault {
                        node_policy: CapturedNodeHistoryPolicyProvenance::test_policy(),
                    },
                }),
                owner: None,
            })
            .unwrap();
    }
    let (context, attachment) = mint_session(&state, None);
    let result = ryeos_ui::seat_auth::with_compiled_ui_attachment(
        attachment,
        (ryeos_ui::handlers::ui_usage::USAGE_DESCRIPTOR.handler)(
            json!({}),
            context,
            Arc::new(state),
        ),
    )
    .await
    .expect("authorized usage observation");
    assert_eq!(result["coverage"]["state"], "observed");
    assert_eq!(result["coverage"]["observed_threads"], 1);
    assert!(result["chains"].as_array().unwrap().is_empty());
    assert_eq!(result["totals"][2]["value"], "0.0000");
    let encoded = serde_json::to_string(&result).unwrap();
    assert!(!encoded.contains("T-private"));
}

#[tokio::test]
async fn activity_returns_only_signed_subjects_owned_by_projectless_caller() {
    use ryeos_state::objects::{
        CapturedItemTrustClass, CapturedNodeHistoryPolicyProvenance, CapturedPolicyProvenance,
        CapturedThreadHistoryPolicy, EnvironmentAuthority, ExecutionProjectAuthority,
        ThreadHistoryRetention,
    };
    let (_tmp, _state_owner, state) = test_state::build_test_state();
    for (thread_id, principal) in [
        (
            "T-visible",
            "fp:7e577e577e577e577e577e577e577e577e577e577e577e577e577e577e577e57",
        ),
        (
            "T-private",
            "fp:f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0",
        ),
    ] {
        state
            .state_store
            .create_thread_for_test(&ryeos_app::state_store::NewThreadRecord {
                thread_id: thread_id.into(),
                chain_root_id: thread_id.into(),
                kind: "tool".into(),
                item_ref: "tool:test/activity".into(),
                executor_ref: "test/executor".into(),
                launch_mode: "wait".into(),
                current_site_id: "site:testhost".into(),
                origin_site_id: "site:testhost".into(),
                upstream_thread_id: None,
                requested_by: Some(principal.into()),
                project_root: None,
                project_authority: ExecutionProjectAuthority::Projectless {
                    environment: EnvironmentAuthority::None,
                },
                base_project_snapshot_hash: None,
                usage_subject: None,
                usage_subject_asserted_by: None,
                captured_history_policy: Some(CapturedThreadHistoryPolicy {
                    retention: ThreadHistoryRetention::Durable,
                    canonical_item_ref: "tool:test/activity".into(),
                    item_content_hash: "a".repeat(64),
                    item_signer_fingerprint: Some("a".repeat(64)),
                    item_trust_class: CapturedItemTrustClass::Trusted,
                    kind_schema_content_hash: "a".repeat(64),
                    resolved_from: CapturedPolicyProvenance::NodeDefault {
                        node_policy: CapturedNodeHistoryPolicyProvenance::test_policy(),
                    },
                }),
                owner: None,
            })
            .unwrap();
    }
    let page = state
        .state_store
        .recent_thread_events("T-visible", "T-visible", None, 64, 128 * 1024)
        .expect("owned fixture has readable event projection");
    assert!(
        !page.events.is_empty(),
        "thread admission must publish signed creation evidence"
    );
    state
        .state_store
        .authenticate_execution_event_candidates("T-visible", &page.events)
        .expect("owned fixture event projection must match signed admission evidence");
    let (context, attachment) = mint_session(&state, None);
    let result = ryeos_ui::seat_auth::with_compiled_ui_attachment(
        attachment,
        (ryeos_ui::handlers::ui_node::ACTIVITY_DESCRIPTOR.handler)(
            json!({}),
            context,
            Arc::new(state),
        ),
    )
    .await
    .expect("authorized activity observation");
    assert_eq!(result["pulse_coverage"]["observed_threads"], 1);
    assert_eq!(result["pulse"][0]["label"], "observed running");
    assert_eq!(result["pulse"][0]["value"], "1");
    let events = result["events"].as_array().unwrap();
    assert!(
        !events.is_empty(),
        "owned signed events must remain inspectable: {result}"
    );
    assert!(events.iter().all(|event| event["thread_id"] == "T-visible"));
    let encoded = serde_json::to_string(&result).unwrap();
    assert!(!encoded.contains("T-private"));
    assert!(
        !encoded.contains("fp:f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0")
    );
    assert_eq!(result["usage_coverage"]["state"], "unavailable");
    assert_eq!(
        result["pulse"].as_array().unwrap().len(),
        4,
        "no invented usage or spend totals"
    );
}

#[tokio::test]
async fn activity_of_a_project_seat_excludes_collaborator_and_other_project_threads() {
    use ryeos_state::objects::{
        CapturedItemTrustClass, CapturedNodeHistoryPolicyProvenance, CapturedPolicyProvenance,
        CapturedThreadHistoryPolicy, EnvironmentAuthority, ExecutionProjectAuthority,
        LiveFilesystemConfinement, LiveProjectAccess, ThreadHistoryRetention,
    };
    let (_tmp, _state_owner, state) = test_state::build_test_state();
    let admitted = tempfile::tempdir().unwrap();
    let foreign = tempfile::tempdir().unwrap();
    let admitted_root = admitted.path().canonicalize().unwrap();
    let foreign_root = foreign.path().canonicalize().unwrap();
    // A seat of a stated project reads that project's threads. A path is no
    // authority: a collaborator's thread on the same checkout is not this
    // key's, and this key's thread in another project is not this project's.
    let admitted_project = test_state::fixture_project_identity(&admitted_root);
    let foreign_project = test_state::fixture_project_identity(&foreign_root);
    for (thread_id, principal, root, sealed) in [
        (
            "T-visible",
            test_state::FIXTURE_KEY,
            Some(admitted_root.clone()),
            Some(admitted_project.clone()),
        ),
        (
            "T-collaborator",
            "fp:c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0",
            Some(admitted_root.clone()),
            None,
        ),
        (
            "T-private",
            test_state::FIXTURE_KEY,
            Some(foreign_root.clone()),
            Some(foreign_project),
        ),
    ] {
        let authority = match &root {
            // These are signed live execution subjects on an unconfined test
            // host, not read-only filesystem realizations. The UI call below
            // remains a read-only observation and gains no execution authority.
            Some(root) => ExecutionProjectAuthority::live(
                root.clone(),
                root.to_string_lossy().into_owned(),
                LiveProjectAccess::ReadWrite,
                LiveFilesystemConfinement::UnconfinedHost,
                EnvironmentAuthority::None,
                vec!["project".to_owned()],
            )
            .unwrap(),
            None => ExecutionProjectAuthority::Projectless {
                environment: EnvironmentAuthority::None,
            },
        };
        let mut authority = authority;
        if let (ExecutionProjectAuthority::LiveProject { signer_project, .. }, Some(project)) =
            (&mut authority, sealed)
        {
            *signer_project = ryeos_state::project_identity::LogicalProject::Stated(project);
        }
        state
            .state_store
            .create_thread_for_test(&ryeos_app::state_store::NewThreadRecord {
                thread_id: thread_id.into(),
                chain_root_id: thread_id.into(),
                kind: "tool".into(),
                item_ref: "tool:test/activity".into(),
                executor_ref: "test/executor".into(),
                launch_mode: "wait".into(),
                current_site_id: "site:testhost".into(),
                origin_site_id: "site:testhost".into(),
                upstream_thread_id: None,
                requested_by: Some(principal.into()),
                project_root: root,
                project_authority: authority,
                base_project_snapshot_hash: None,
                usage_subject: None,
                usage_subject_asserted_by: None,
                captured_history_policy: Some(CapturedThreadHistoryPolicy {
                    retention: ThreadHistoryRetention::Durable,
                    canonical_item_ref: "tool:test/activity".into(),
                    item_content_hash: "a".repeat(64),
                    item_signer_fingerprint: Some("a".repeat(64)),
                    item_trust_class: CapturedItemTrustClass::Trusted,
                    kind_schema_content_hash: "a".repeat(64),
                    resolved_from: CapturedPolicyProvenance::NodeDefault {
                        node_policy: CapturedNodeHistoryPolicyProvenance::test_policy(),
                    },
                }),
                owner: None,
            })
            .unwrap();
    }
    let mut launch = session_launch_context(Some(admitted_root.to_string_lossy().into_owned()));
    Arc::make_mut(&mut launch.compiled_binding)
        .binding
        .signer_project = ryeos_state::project_identity::LogicalProject::Stated(admitted_project);
    let (context, attachment) = mint_launch_session(&state, launch);
    let result = ryeos_ui::seat_auth::with_compiled_ui_attachment(
        attachment,
        (ryeos_ui::handlers::ui_node::ACTIVITY_DESCRIPTOR.handler)(
            json!({}),
            context,
            Arc::new(state),
        ),
    )
    .await
    .expect("authorized activity observation");
    assert_eq!(result["pulse_coverage"]["observed_threads"], 1);
    assert_eq!(result["pulse"][0]["label"], "observed running");
    assert_eq!(result["pulse"][0]["value"], "1");
    let events = result["events"].as_array().unwrap();
    assert!(
        !events.is_empty(),
        "owned signed events must remain inspectable: {result}"
    );
    assert!(events.iter().all(|event| event["thread_id"] == "T-visible"));
    let encoded = serde_json::to_string(&result).unwrap();
    assert!(!encoded.contains("T-private"));
    assert!(!encoded.contains("T-collaborator"));
    assert!(
        !encoded.contains("fp:c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0")
    );
    assert_eq!(result["usage_coverage"]["state"], "unavailable");
    assert_eq!(
        result["pulse"].as_array().unwrap().len(),
        4,
        "no invented usage or spend totals"
    );
}

/// A thread carrying the usage subject a seat launch attributes, with settled
/// usage inside the window, must reach the activity service's windowed usage
/// through the real per-subject summary, and only for the acting principal.
#[tokio::test]
async fn activity_reports_windowed_usage_for_seat_attributed_launches() {
    use ryeos_state::objects::{
        CapturedItemTrustClass, CapturedNodeHistoryPolicyProvenance, CapturedPolicyProvenance,
        CapturedThreadHistoryPolicy, EnvironmentAuthority, ExecutionProjectAuthority,
        ThreadHistoryRetention,
    };
    let (_tmp, _state_owner, state) = test_state::build_test_state();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let settled_at = lillux::time::iso8601_from_unix_secs(now);
    // (thread, owner, attributed subject)
    let attributed = ryeos_ui::handlers::ui_usage::ui_usage_subject(None);
    for (thread_id, principal, subject) in [
        (
            "T-seat",
            "fp:7e577e577e577e577e577e577e577e577e577e577e577e577e577e577e577e57",
            Some(attributed.clone()),
        ),
        (
            "T-unattributed",
            "fp:7e577e577e577e577e577e577e577e577e577e577e577e577e577e577e577e57",
            None,
        ),
        (
            "T-foreign",
            "fp:f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0",
            Some(attributed.clone()),
        ),
    ] {
        state
            .state_store
            .create_thread_for_test(&ryeos_app::state_store::NewThreadRecord {
                thread_id: thread_id.into(),
                chain_root_id: thread_id.into(),
                kind: "tool".into(),
                item_ref: "tool:test/activity".into(),
                executor_ref: "test/executor".into(),
                launch_mode: "wait".into(),
                current_site_id: "site:testhost".into(),
                origin_site_id: "site:testhost".into(),
                upstream_thread_id: None,
                requested_by: Some(principal.into()),
                project_root: None,
                project_authority: ExecutionProjectAuthority::Projectless {
                    environment: EnvironmentAuthority::None,
                },
                base_project_snapshot_hash: None,
                usage_subject: subject.clone(),
                usage_subject_asserted_by: subject.map(|_| principal.to_string()),
                captured_history_policy: Some(CapturedThreadHistoryPolicy {
                    retention: ThreadHistoryRetention::Durable,
                    canonical_item_ref: "tool:test/activity".into(),
                    item_content_hash: "a".repeat(64),
                    item_signer_fingerprint: Some("a".repeat(64)),
                    item_trust_class: CapturedItemTrustClass::Trusted,
                    kind_schema_content_hash: "a".repeat(64),
                    resolved_from: CapturedPolicyProvenance::NodeDefault {
                        node_policy: CapturedNodeHistoryPolicyProvenance::test_policy(),
                    },
                }),
                owner: None,
            })
            .unwrap();
        state
            .state_store
            .append_events(
                thread_id,
                thread_id,
                &[ryeos_app::state_store::NewEventRecord {
                    event_type: "thread_usage".into(),
                    storage_class: "indexed".into(),
                    payload: json!({
                        "completed_turns": 2,
                        "input_tokens": 100,
                        "output_tokens": 10,
                        "spend_usd": ryeos_accounting::UsdNanos::from_nanos(1_500_000_000).unwrap(),
                        "spawns_used": 0,
                        "started_at": settled_at,
                        "settled_at": settled_at,
                        "last_settled_turn_seq": 2,
                        "elapsed_ms": 10,
                    }),
                }],
            )
            .unwrap();
    }
    let (context, attachment) = mint_session(&state, None);
    let result = ryeos_ui::seat_auth::with_compiled_ui_attachment(
        attachment,
        (ryeos_ui::handlers::ui_node::ACTIVITY_DESCRIPTOR.handler)(
            json!({}),
            context,
            Arc::new(state),
        ),
    )
    .await
    .expect("activity with seat-attributed usage");
    assert_eq!(result["usage_coverage"]["state"], "observed");
    let rows: Vec<(String, String)> = result["usage"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["label"].as_str().unwrap().to_string(),
                row["value"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    // Only the acting principal's attributed thread counts.
    assert_eq!(rows[0], ("observed input tokens · 1d".into(), "100".into()));
    assert_eq!(rows[1], ("observed output tokens · 1d".into(), "10".into()));
    assert_eq!(
        rows[2],
        ("observed spend (USD) · 1d".into(), "1.5000".into())
    );
    assert_eq!(
        rows[3],
        ("observed completed turns · 1d".into(), "2".into())
    );
}
