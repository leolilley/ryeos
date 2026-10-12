mod test_state;

use std::sync::Arc;

use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::{HandlerError, extract_handler_error};
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

fn mint_session(
    state: &ryeos_app::state::AppState,
    project_path: Option<String>,
) -> HandlerContext {
    let (session_id, token) = test_state::mint_launch(state, session_launch_context(project_path));
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
#[ignore = "requires populated handler binaries via scripts/populate-bundles.sh"]
async fn neutral_browser_boot_does_not_admit_domain_sources() {
    let (_tmp, state) = test_state::build_test_state_with_live_bundles();
    let (session_id, ctx, producer, coordinate) =
        test_state::live_presentation_with_surface(&state, "surface:ryeos/ui/base").await;
    let state = Arc::new(state);

    let session = (ryeos_ui::handlers::ui_session_current::DESCRIPTOR.handler)(
        json!(null),
        ctx.clone(),
        state.clone(),
    )
    .await
    .expect("session/current should succeed");
    assert_eq!(session["session_id"], session_id);
    let retained = get_ui_state(&state)
        .unwrap()
        .browser_sessions
        .get_session(&session_id)
        .unwrap();
    let attachment = &retained.attachments[&coordinate.binding_attachment_id];
    assert_eq!(attachment.root_ref, "surface:ryeos/ui/base");

    for (view_ref, channel) in [
        ("surface:ryeos/ui/base", "items"),
        ("surface:ryeos/ui/base", "dimension"),
        ("surface:ryeos/ui/base", "topology"),
        ("surface:ryeos/ui/base", "projects"),
        ("view:ryeos/item/inspector", "default"),
    ] {
        let result = (ryeos_ui::handlers::ui_invocations_dispatch::DESCRIPTOR.handler)(
            json!({
                "producer": producer,
                "binding": {
                    "binding_attachment_id": coordinate.binding_attachment_id,
                    "binding_generation": coordinate.binding_generation,
                    "binding_digest": coordinate.binding_digest,
                    "coordinate": {"kind":"source", "view_ref":view_ref, "channel":channel},
                    "payload": {"kind":"source_parameters", "params":{}}
                }
            }),
            ctx.clone(),
            state.clone(),
        )
        .await;
        assert!(
            result.is_err(),
            "neutral Home must not admit {view_ref}/{channel}"
        );
    }
}

#[tokio::test]
#[ignore = "requires populated handler binaries via scripts/populate-bundles.sh"]
async fn explicit_lens_dispatches_only_its_compiled_purpose_sources() {
    let (_tmp, state) = test_state::build_test_state_with_live_bundles();
    let (session_id, ctx, producer, coordinate) =
        test_state::live_presentation_with_surface(&state, "surface:ryeos/ui/lens").await;
    let principal = get_ui_state(&state)
        .unwrap()
        .browser_sessions
        .get_session(&session_id)
        .unwrap()
        .principal_id;
    let foreign = format!("fp:{}", "77".repeat(32));
    assert_ne!(principal, foreign);
    // Existing live-dispatch fixture; these are owner-scoped observation rows,
    // not executable schedule registration or scheduler lifecycle qualification.
    for (id, owner, enabled) in [
        ("caller-enabled", principal.as_str(), true),
        ("caller-disabled", principal.as_str(), false),
        ("foreign-enabled", foreign.as_str(), true),
    ] {
        state
            .scheduler_db
            .upsert_spec(&ryeos_scheduler::ScheduleSpecRecord {
                schedule_id: id.into(),
                item_ref: "directive:test".into(),
                ref_bindings: Default::default(),
                params: "{}".into(),
                schedule_type: "interval".into(),
                expression: "60".into(),
                timezone: "UTC".into(),
                misfire_policy: "skip".into(),
                overlap_policy: "skip".into(),
                lateness_grace_secs: 60,
                enabled,
                project_root: None,
                signer_fingerprint: "44".repeat(32),
                spec_hash: "22".repeat(32),
                registered_at: 1000,
                execution: ryeos_scheduler::types::ScheduleExecution {
                    authority: ryeos_scheduler::types::ScheduleExecutionAuthority::Node {
                        principal_id: owner.into(),
                        effective_origin_site_id: "site:test".into(),
                    },
                    capabilities: vec!["ryeos.execute.*".into()],
                    policy: ryeos_engine::execution_contract::ExecutionPolicy::projectless(
                        ryeos_engine::execution_contract::ExecutionResponse::Accepted,
                    ),
                },
            })
            .unwrap();
    }
    let state = Arc::new(state);

    let items = dispatch_source(
        state.clone(),
        ctx.clone(),
        &producer,
        &coordinate,
        "surface:ryeos/ui/lens",
        "items",
        json!({"kind":"surface", "query":"ryeos/ui/base"}),
    )
    .await;
    assert!(
        items["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["canonical_ref"] == "surface:ryeos/ui/base")
    );
    assert_eq!(items["coverage"]["counts_scope"], "returned_items");

    let dimension = dispatch_source(
        state.clone(),
        ctx.clone(),
        &producer,
        &coordinate,
        "surface:ryeos/ui/lens",
        "dimension",
        json!({}),
    )
    .await;
    assert_eq!(dimension["schema_version"], "ryeos.ui.dimension.v3");
    assert_eq!(dimension["schedules"]["owner_principal_id"], principal);
    assert_eq!(dimension["schedules"]["identity_filtered"], true);
    assert_eq!(dimension["schedules"]["total"], 2);
    assert_eq!(dimension["schedules"]["enabled"], 1);
    assert_eq!(
        dimension["schedules"]["coverage"],
        "current_caller_owned_schedules_not_node_or_project_totals"
    );
    assert_eq!(dimension["gc"]["activity"], "unknown");
    assert!(dimension["gc"].get("running").is_none());
    assert_eq!(
        dimension["gc"]["coverage"],
        "bounded_log_observations_not_live_or_exhaustive"
    );

    let inspection = dispatch_source(
        state,
        ctx,
        &producer,
        &coordinate,
        "view:ryeos/item/inspector",
        "default",
        json!({"canonical_ref":"surface:ryeos/ui/base"}),
    )
    .await;
    assert_eq!(inspection["schema_version"], "ryeos-ui.item.inspect.v1");
    assert_eq!(inspection["item"]["canonical_ref"], "surface:ryeos/ui/base");
    // The signed inspector requests effective content, not raw source. Browser
    // parameters must not override that authored choice.
    assert_eq!(inspection["raw_coverage"]["state"], "not_requested");
    assert_eq!(inspection["effective_coverage"]["state"], "available");
    assert_eq!(
        inspection["effective"]["composed_value"]["name"],
        "ryeos-ui-base"
    );
}

async fn dispatch_source(
    state: Arc<ryeos_app::state::AppState>,
    context: HandlerContext,
    producer: &ryeos_client_base::ui::UiPresentationProducer,
    coordinate: &ryeos_ui::browser_session::BindingAttachmentCoordinate,
    view_ref: &str,
    channel: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let result = (ryeos_ui::handlers::ui_invocations_dispatch::DESCRIPTOR.handler)(
        json!({
            "producer": producer,
            "binding": {
                "binding_attachment_id": coordinate.binding_attachment_id,
                "binding_generation": coordinate.binding_generation,
                "binding_digest": coordinate.binding_digest,
                "coordinate": {"kind":"source", "view_ref":view_ref, "channel":channel},
                "payload": {"kind":"source_parameters", "params":params}
            }
        }),
        context,
        state,
    )
    .await
    .expect("dispatch exact compiled source under claimed presentation");
    assert_eq!(result["status"], "executed");
    result["result"]["result"].clone()
}

/// Handler-unit boundary: retain the minted attachment's project descriptor
/// rather than treating a raw browser cookie as source authority. Full signed
/// binding/producer dispatch is exercised by the boot integration above.
async fn call_attached_source(
    descriptor: ryeos_api::registry::ServiceDescriptor,
    params: serde_json::Value,
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
        (descriptor.handler)(params, context, state).await
    })
    .await
}

#[tokio::test]
async fn schedule_observation_uses_retained_execution_owner_not_cookie_or_node() {
    let (_tmp, state) = test_state::build_test_state();
    let principal = format!("fp:{}", "11".repeat(32));
    let foreign = format!("fp:{}", "33".repeat(32));
    assert_ne!(state.identity.principal_id(), principal);
    let mut launch = session_launch_context(None);
    Arc::make_mut(&mut launch.compiled_binding)
        .binding
        .principal_id = principal.clone();
    let (session_id, token) = test_state::mint_launch(&state, launch);
    assert_eq!(
        get_ui_state(&state)
            .unwrap()
            .browser_sessions
            .consume_launch_token(&token),
        Some(session_id.clone())
    );
    for (id, owner) in [("owned", principal.as_str()), ("foreign", foreign.as_str())] {
        state
            .scheduler_db
            .upsert_spec(&ryeos_scheduler::ScheduleSpecRecord {
                schedule_id: id.into(),
                item_ref: "directive:test".into(),
                ref_bindings: Default::default(),
                params: "{}".into(),
                schedule_type: "interval".into(),
                expression: "60".into(),
                timezone: "UTC".into(),
                misfire_policy: "skip".into(),
                overlap_policy: "skip".into(),
                lateness_grace_secs: 60,
                enabled: true,
                project_root: None,
                signer_fingerprint: "44".repeat(32),
                spec_hash: "22".repeat(32),
                registered_at: 1000,
                execution: ryeos_scheduler::types::ScheduleExecution {
                    authority: ryeos_scheduler::types::ScheduleExecutionAuthority::Node {
                        principal_id: owner.into(),
                        effective_origin_site_id: "site:test".into(),
                    },
                    capabilities: vec!["ryeos.execute.*".into()],
                    policy: ryeos_engine::execution_contract::ExecutionPolicy::projectless(
                        ryeos_engine::execution_contract::ExecutionResponse::Accepted,
                    ),
                },
            })
            .unwrap();
    }
    let state = Arc::new(state);
    let context = HandlerContext::new(format!("session:{session_id}"), vec!["*".into()], false);
    assert!(
        (ryeos_ui::handlers::ui_schedules::DESCRIPTOR.handler)(
            json!({}),
            context.clone(),
            state.clone()
        )
        .await
        .is_err(),
        "valid cookie alone cannot read schedules"
    );
    let result = call_attached_source(
        ryeos_ui::handlers::ui_schedules::DESCRIPTOR,
        json!({}),
        context,
        state,
    )
    .await
    .unwrap();
    assert_eq!(result["owner_principal_id"], principal);
    assert_eq!(result["identity_filtered"], true);
    assert_eq!(result["schedules"].as_array().unwrap().len(), 1);
    assert_eq!(result["schedules"][0]["schedule_id"], "owned");
    assert_eq!(result["schedules"][0]["total_fires"], 0);
    assert_eq!(
        result["coverage"],
        "current_caller_owned_schedules_not_node_or_project_totals"
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
async fn inspection_sources_refuse_unknown_sessions_and_unbound_live_cookies() {
    let (_tmp, state) = test_state::build_test_state();
    let live_cookie = mint_session(&state, None);
    let state = Arc::new(state);
    for context in [
        HandlerContext::new("session:missing".into(), vec!["ui.read".into()], false),
        live_cookie,
    ] {
        for (descriptor, parameters) in [
            (
                ryeos_ui::handlers::ui_items::ITEMS_LIST_DESCRIPTOR,
                json!({"limit":1}),
            ),
            (
                ryeos_ui::handlers::ui_items::ITEM_INSPECT_DESCRIPTOR,
                json!({"canonical_ref":"surface:ryeos/ui/base"}),
            ),
            (ryeos_ui::handlers::ui_node::ACTIVITY_DESCRIPTOR, json!({})),
            (ryeos_ui::handlers::ui_dimension::DESCRIPTOR, json!({})),
        ] {
            let error = (descriptor.handler)(parameters, context.clone(), state.clone())
                .await
                .expect_err("unbound cookies cannot authorize inspection");
            assert!(
                matches!(
                    extract_handler_error(&error),
                    Some(HandlerError::Forbidden(_))
                ),
                "{} must refuse authority, not return empty observations: {error:#}",
                descriptor.service_ref
            );
        }
    }
}

#[tokio::test]
#[ignore = "requires populated handler binaries via scripts/populate-bundles.sh"]
async fn admitted_dimension_propagates_corrupt_gc_observations() {
    let (_tmp, state) = test_state::build_test_state_with_live_bundles();
    let (_, context, producer, coordinate) =
        test_state::live_presentation_with_surface(&state, "surface:ryeos/ui/lens").await;
    let state = Arc::new(state);
    // Prove this exact source is admitted and usable before introducing the
    // disposable observation failure, so an unrelated denial cannot pass.
    let before = dispatch_source(
        state.clone(),
        context.clone(),
        &producer,
        &coordinate,
        "surface:ryeos/ui/lens",
        "dimension",
        json!({}),
    )
    .await;
    assert_eq!(before["schema_version"], "ryeos.ui.dimension.v3");
    let logs = state.config.runtime_state_dir().join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    let log = logs.join("gc.jsonl");
    std::fs::write(&log, "not-json\n").unwrap();
    let error = (ryeos_ui::handlers::ui_invocations_dispatch::DESCRIPTOR.handler)(
        json!({
            "producer": producer,
            "binding": {
                "binding_attachment_id": coordinate.binding_attachment_id,
                "binding_generation": coordinate.binding_generation,
                "binding_digest": coordinate.binding_digest,
                "coordinate": {"kind":"source", "view_ref":"surface:ryeos/ui/lens", "channel":"dimension"},
                "payload": {"kind":"source_parameters", "params":{}}
            }
        }), context.clone(), state.clone(),
    ).await.expect_err("corrupt observations must not become an empty GC summary");
    assert!(
        matches!(
            extract_handler_error(&error),
            Some(HandlerError::Structured { code, status: 500, .. }) if code == "internal"
        ),
        "the admitted read must fail on its observation, not on caller authority: {error:#}"
    );
    // Public dispatch sanitizes internal causes. Prove the observation caused
    // the failure by restoring only that disposable log and using the same
    // producer/binding again, rather than requiring private causes on the wire.
    std::fs::write(&log, "{}\n").unwrap();
    let restored = dispatch_source(
        state,
        context,
        &producer,
        &coordinate,
        "surface:ryeos/ui/lens",
        "dimension",
        json!({}),
    )
    .await;
    assert_eq!(restored["gc"]["recent_events"], json!([{}]));
}

#[tokio::test]
async fn files_read_returns_lossy_utf8_for_invalid_bytes() {
    let (_tmp, state) = test_state::build_test_state();
    let project = tempfile::TempDir::new().unwrap();
    std::fs::write(project.path().join("bad.bin"), [b'a', 0xff, b'b']).unwrap();

    let ctx = mint_session(&state, Some(project.path().display().to_string()));
    let state = Arc::new(state);
    let denied = (ryeos_ui::handlers::ui_files::FILES_READ_DESCRIPTOR.handler)(
        json!({"root":"project", "path":"bad.bin"}),
        ctx.clone(),
        state.clone(),
    )
    .await
    .expect_err("valid cookie alone cannot invoke a source handler");
    assert!(format!("{denied:#}").contains("compiled binding coordinate"));
    let result = call_attached_source(
        ryeos_ui::handlers::ui_files::FILES_READ_DESCRIPTOR,
        json!({"root": "project", "path": "bad.bin"}),
        ctx,
        state,
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
    let result = call_attached_source(
        ryeos_ui::handlers::ui_files::FILES_READ_DESCRIPTOR,
        json!({"root": "project", "path": "../outside/secret.txt"}),
        ctx,
        Arc::new(state),
    )
    .await;

    assert!(result.is_err(), "files.read must reject path traversal");
}

#[tokio::test]
async fn files_read_blocks_symlink_escape_from_project_root() {
    let (_tmp, state) = test_state::build_test_state();
    let root = tempfile::TempDir::new().unwrap();
    let project = root.path().join("project");
    let outside = root.path().join("outside");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), "secret").unwrap();
    lillux::PinnedDirectory::open(&project)
        .unwrap()
        .unwrap()
        .create_symlink(
            std::ffi::OsStr::new("secret-link"),
            b"../outside/secret.txt",
        )
        .unwrap();

    let ctx = mint_session(&state, Some(project.display().to_string()));
    let result = call_attached_source(
        ryeos_ui::handlers::ui_files::FILES_READ_DESCRIPTOR,
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
    let result = call_attached_source(
        ryeos_ui::handlers::ui_files::FILES_READ_DESCRIPTOR,
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
    let result = call_attached_source(
        ryeos_ui::handlers::ui_files::FILES_LIST_DESCRIPTOR,
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
async fn item_inspect_raw_reports_failure_beyond_stable_read_bound() {
    let (_tmp, state) = test_state::build_test_state_with_live_bundles();
    let project = tempfile::TempDir::new().unwrap();
    let tool_dir = project.path().join(".ai/tools/test");
    std::fs::create_dir_all(&tool_dir).unwrap();
    let mut bytes = vec![b'a'; 256 * 1024 + 64];
    bytes[10] = 0xff;
    std::fs::write(tool_dir.join("huge.py"), bytes).unwrap();

    let ctx = mint_session(&state, Some(project.path().display().to_string()));
    let result = call_attached_source(
        ryeos_ui::handlers::ui_items::ITEM_INSPECT_DESCRIPTOR,
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

    assert!(result.get("raw").is_none());
    assert_eq!(result["raw_coverage"]["state"], "failed");
    assert_eq!(result["effective_coverage"]["state"], "not_requested");
}

#[tokio::test]
async fn items_list_applies_filters_and_accepts_include_shadowed() {
    let (_tmp, state) = test_state::build_test_state_with_live_bundles();
    let ctx = mint_session(&state, None);
    let state = Arc::new(state);
    let result = call_attached_source(
        ryeos_ui::handlers::ui_items::ITEMS_LIST_DESCRIPTOR,
        json!({
            "kind": "tool",
            "space": "bundle",
            "query": "core/sign",
            "limit": 1,
            "include_shadowed": true
        }),
        ctx.clone(),
        state.clone(),
    )
    .await
    .expect("items.list should succeed");

    let items = result["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "summary: {:?}", &result["counts"]);
    assert_eq!(items[0]["canonical_ref"], "tool:ryeos/core/sign");
    assert_eq!(items[0]["item_kind"], "tool");
    assert_eq!(items[0]["space"], "bundle");

    let empty = call_attached_source(
        ryeos_ui::handlers::ui_items::ITEMS_LIST_DESCRIPTOR,
        json!({"kind":"tool", "query":"no-catalogue-item-7b004e13-c74c-4edc-8fb8-792e1be8d1b9"}),
        ctx.clone(),
        state.clone(),
    )
    .await
    .expect("registered kind with no matching items remains a valid observation");
    assert_eq!(empty["items"], json!([]));
    assert_eq!(empty["coverage"]["state"], "observed");
    let unavailable = call_attached_source(
        ryeos_ui::handlers::ui_items::ITEMS_LIST_DESCRIPTOR,
        json!({"kind":"unregistered-catalogue-fixture-kind"}),
        ctx,
        state,
    )
    .await
    .expect("missing kind is explicit coverage, not an invented empty corpus");
    assert_eq!(unavailable["items"], json!([]));
    assert_eq!(unavailable["coverage"]["state"], "unavailable");
    assert_eq!(unavailable["coverage"]["reason"], "kind_not_registered");
}

#[tokio::test]
async fn catalogue_missing_kind_and_invalid_spaces_are_not_successful_empty_reads() {
    let (_tmp, state) = test_state::build_test_state();
    let context = mint_session(&state, None);
    let state = Arc::new(state);
    let unavailable = call_attached_source(
        ryeos_ui::handlers::ui_items::ITEMS_LIST_DESCRIPTOR,
        json!({"kind":"unregistered-catalogue-fixture-kind", "limit":7}),
        context.clone(),
        state.clone(),
    )
    .await
    .expect("unregistered kind carries explicit unavailable coverage");
    assert_eq!(unavailable["items"], json!([]));
    assert_eq!(unavailable["coverage"]["state"], "unavailable");
    assert_eq!(unavailable["coverage"]["reason"], "kind_not_registered");
    assert_eq!(unavailable["coverage"]["limit"], 7);
    assert_eq!(unavailable["coverage"]["traversal_failures"], 0);
    assert_eq!(unavailable["coverage"]["resolution_failures"], 0);
    for space in ["unknown", "user", "system", "node"] {
        let error = call_attached_source(
            ryeos_ui::handlers::ui_items::ITEMS_LIST_DESCRIPTOR,
            json!({"space":space}),
            context.clone(),
            state.clone(),
        )
        .await
        .expect_err("unsupported catalogue spaces must not look like empty success");
        assert!(
            matches!(
                extract_handler_error(&error),
                Some(HandlerError::BadRequest(_))
            ),
            "space {space} must be a typed bad request: {error:#}"
        );
    }
}

#[tokio::test]
async fn activity_returns_only_signed_subjects_owned_by_projectless_caller() {
    use ryeos_state::objects::{
        CapturedItemTrustClass, CapturedNodeHistoryPolicyProvenance, CapturedPolicyProvenance,
        CapturedThreadHistoryPolicy, EnvironmentAuthority, ExecutionProjectAuthority,
        ThreadHistoryRetention,
    };
    let (_tmp, state) = test_state::build_test_state();
    for (thread_id, principal) in [("T-visible", "fp:test"), ("T-private", "fp:foreign")] {
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
    let context = mint_session(&state, None);
    let result = call_attached_source(
        ryeos_ui::handlers::ui_node::ACTIVITY_DESCRIPTOR,
        json!({}),
        context,
        Arc::new(state),
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
    assert!(!encoded.contains("fp:foreign"));
    assert_eq!(result["usage_coverage"]["state"], "unavailable");
    assert_eq!(
        result["pulse"].as_array().unwrap().len(),
        4,
        "no invented usage or spend totals"
    );
}

#[tokio::test]
async fn activity_bound_project_excludes_foreign_project_and_projectless_subjects() {
    use ryeos_state::objects::{
        CapturedItemTrustClass, CapturedNodeHistoryPolicyProvenance, CapturedPolicyProvenance,
        CapturedThreadHistoryPolicy, EnvironmentAuthority, ExecutionProjectAuthority,
        LiveFilesystemConfinement, LiveProjectAccess, ThreadHistoryRetention,
    };
    let (_tmp, state) = test_state::build_test_state();
    let admitted = tempfile::tempdir().unwrap();
    let foreign = tempfile::tempdir().unwrap();
    let admitted_root = admitted.path().canonicalize().unwrap();
    let foreign_root = foreign.path().canonicalize().unwrap();
    // Live-project visibility follows the admitted project, not only creator
    // identity. Own work elsewhere must not leak into this project's activity.
    for (thread_id, principal, root) in [
        ("T-visible", "fp:collaborator", Some(admitted_root.clone())),
        ("T-private", "fp:test", Some(foreign_root)),
        ("T-projectless", "fp:test", None),
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
            })
            .unwrap();
    }
    let context = mint_session(&state, Some(admitted_root.to_string_lossy().into_owned()));
    let result = call_attached_source(
        ryeos_ui::handlers::ui_node::ACTIVITY_DESCRIPTOR,
        json!({}),
        context,
        Arc::new(state),
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
    assert!(!encoded.contains("T-projectless"));
    assert!(!encoded.contains("fp:test"));
    assert_eq!(result["usage_coverage"]["state"], "unavailable");
    assert_eq!(
        result["pulse"].as_array().unwrap().len(),
        4,
        "no invented usage or spend totals"
    );
}

#[tokio::test]
async fn item_sources_refuse_projectless_browser_path_authority_before_resolution() {
    let (_tmp, state) = test_state::build_test_state();
    let project = tempfile::tempdir().unwrap();
    let context = mint_session(&state, None);
    let state = Arc::new(state);
    for (descriptor, mut parameters) in [
        (
            ryeos_ui::handlers::ui_items::ITEMS_LIST_DESCRIPTOR,
            json!({"limit":1}),
        ),
        (
            ryeos_ui::handlers::ui_items::ITEM_INSPECT_DESCRIPTOR,
            json!({"canonical_ref":"tool:test/private"}),
        ),
    ] {
        parameters["project_path"] = json!(project.path());
        let error = call_attached_source(descriptor, parameters, context.clone(), state.clone())
            .await
            .expect_err("source parameters cannot invent project authority");
        assert!(format!("{error:#}").contains("does not match admitted project authority"));
    }
}

#[tokio::test]
async fn file_sources_refuse_project_path_parameter_authority() {
    let (_tmp, state) = test_state::build_test_state();
    let admitted = tempfile::tempdir().unwrap();
    let foreign = tempfile::tempdir().unwrap();
    std::fs::write(foreign.path().join("private.txt"), b"foreign").unwrap();
    let state = Arc::new(state);
    for root in [None, Some(admitted.path().display().to_string())] {
        let context = mint_session(&state, root);
        for (descriptor, parameters) in [
            (
                ryeos_ui::handlers::ui_files::FILES_READ_DESCRIPTOR,
                json!({"root":"project", "path":"private.txt", "project_path":foreign.path()}),
            ),
            (
                ryeos_ui::handlers::ui_files::FILES_LIST_DESCRIPTOR,
                json!({"root":"project", "path":"", "project_path":foreign.path()}),
            ),
            (
                ryeos_ui::handlers::ui_files::FILES_TREE_DESCRIPTOR,
                json!({"root":"project", "path":"", "project_path":foreign.path()}),
            ),
        ] {
            let error =
                call_attached_source(descriptor, parameters, context.clone(), state.clone())
                    .await
                    .expect_err("foreign parameter cannot widen retained attachment authority");
            assert!(
                !format!("{error:#}").contains("private.txt"),
                "refusal must happen before target file traversal"
            );
        }
    }
    let operator = test_state::local_operator_context(&state, vec!["*".into()]);
    let read = (ryeos_ui::handlers::ui_files::FILES_READ_DESCRIPTOR.handler)(
        json!({"root":"project", "path":"private.txt", "project_path":foreign.path()}),
        operator,
        state,
    )
    .await
    .expect("admitted operator preserves explicit local project selection");
    assert_eq!(read["content"], "foreign");
}
