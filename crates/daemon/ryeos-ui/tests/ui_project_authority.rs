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

async fn mint_compiled_project_session(
    state: &AppState,
    project_root: &std::path::Path,
) -> (ryeos_ui::browser_session::BrowserSession, HandlerContext) {
    let operator = test_state::local_operator_context(state, vec!["*".into()]);
    let minted = ryeos_ui::handlers::ui_launch_mint::handle(
        ryeos_ui::handlers::ui_launch_mint::Request {
            ui_binding_contract_revision: ryeos_ui::UI_BINDING_CONTRACT_REVISION.to_string(),
            surface_ref: "surface:ryeos/ui/base".to_string(),
            project_path: Some(project_root.to_string_lossy().into_owned()),
            user_principal_id: None,
        },
        operator,
        Arc::new(state.clone()),
    )
    .await
    .expect("compile project browser session");
    let session_id = minted["session_id"]
        .as_str()
        .expect("minted session id")
        .to_string();
    let activated = ryeos_ui::handlers::ui_launch::handle(
        serde_json::json!({"token": minted["token"]}),
        HandlerContext::anonymous(),
        Arc::new(state.clone()),
    )
    .await
    .expect("activate project browser session");
    assert_eq!(activated["session_id"], session_id);
    let session = get_ui_state(state)
        .expect("UI state")
        .browser_sessions
        .get_session(&session_id)
        .expect("active compiled project session");
    let attachment = session
        .attachments
        .get(&session.surface_attachment_id)
        .expect("surface attachment");
    assert!(
        attachment
            .compiled_binding
            .binding
            .sources
            .get("view:ryeos/threads/history")
            .is_some_and(|sources| sources.contains_key("default")),
        "the acceptance test must cross a source coordinate compiled from the live surface"
    );
    let context = HandlerContext::new(format!("session:{session_id}"), vec!["*".into()], false);
    (session, context)
}

async fn dispatch_project_threads(
    state: Arc<AppState>,
    session: &ryeos_ui::browser_session::BrowserSession,
    context: HandlerContext,
    attempted_project_override: &std::path::Path,
) -> serde_json::Value {
    let coordinate = session
        .attachments
        .get(&session.surface_attachment_id)
        .expect("surface attachment")
        .coordinate();
    (ryeos_ui::handlers::ui_invocations_dispatch::DESCRIPTOR.handler)(
        serde_json::json!({
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
        }),
        context,
        state,
    )
    .await
    .expect("dispatch compiled project source")
}

#[tokio::test]
async fn compiled_project_sources_query_only_the_canonical_session_project() {
    let (_state_dir, state) = test_state::build_test_state();
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
        .get(&session.surface_attachment_id)
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
    let (_state_dir, state) = test_state::build_test_state_with_live_bundles();
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
    let (first_session, first_context) = mint_compiled_project_session(&state, &first_alias).await;
    let (second_session, second_context) = mint_compiled_project_session(&state, &second).await;
    let canonical_first = first.canonicalize().expect("canonical first project");
    let canonical_second = second.canonicalize().expect("canonical second project");
    assert_eq!(
        first_session
            .attachments
            .get(&first_session.surface_attachment_id)
            .and_then(|attachment| attachment.project_query_identity.as_deref()),
        Some(canonical_first.to_string_lossy().as_ref())
    );
    assert_eq!(
        second_session
            .attachments
            .get(&second_session.surface_attachment_id)
            .and_then(|attachment| attachment.project_query_identity.as_deref()),
        Some(canonical_second.to_string_lossy().as_ref())
    );

    let state = Arc::new(state);
    let first_result = dispatch_project_threads(
        state.clone(),
        &first_session,
        first_context,
        &canonical_second,
    )
    .await;
    let second_result =
        dispatch_project_threads(state, &second_session, second_context, &canonical_first).await;

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
