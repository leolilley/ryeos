// Tests for `ui.invocations.dispatch` handler.

mod test_state;
use test_state::{build_test_state, build_test_state_with_live_bundles, launch_context};

use ryeos_app::handler_context::HandlerContext;
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::contracts::{EffectivePrincipal, PlanContext, Principal, ProjectContext};
use ryeos_ui::state::get_ui_state;
use std::sync::Arc;

async fn mint_live_session(
    state: &ryeos_app::state::AppState,
) -> (
    String,
    HandlerContext,
    ryeos_ui::browser_session::BrowserSession,
) {
    let principal = format!("fp:{}", "ab".repeat(32));
    let scopes = vec!["*".to_string()];
    let response = ryeos_ui::handlers::ui_launch_mint::handle(
        ryeos_ui::handlers::ui_launch_mint::Request {
            ui_binding_contract_revision: ryeos_ui::UI_BINDING_CONTRACT_REVISION.to_string(),
            surface_ref: "surface:ryeos/ui/base".to_string(),
            project_path: None,
            user_principal_id: Some(principal.clone()),
        },
        HandlerContext::new(principal, scopes.clone(), true),
        Arc::new(state.clone()),
    )
    .await
    .expect("compile live UI binding");
    let session_id = response["session_id"]
        .as_str()
        .expect("minted session id")
        .to_string();
    let session = get_ui_state(state)
        .expect("UI state")
        .browser_sessions
        .get_session(&session_id)
        .expect("minted session");
    (
        session_id.clone(),
        HandlerContext::new(format!("session:{session_id}"), scopes, false),
        session,
    )
}

fn test_context() -> ryeos_ui::browser_session::LaunchContext {
    launch_context(
        "surface:ryeos/ui/base",
        None,
        ryeos_ui::compiled_binding::EffectiveUiPosture::Interactive,
        None,
    )
}

fn observation_context() -> ryeos_ui::browser_session::LaunchContext {
    let mut context = launch_context(
        "surface:ryeos/ui/base",
        None,
        ryeos_ui::compiled_binding::EffectiveUiPosture::ObservationOnly,
        None,
    );
    context.granted_caps = vec![
        "ui.read".into(),
        "ryeos.execute.service.commands/submit".into(),
    ];
    context
}

#[test]
#[ignore = "requires populated handler binaries via scripts/populate-bundles.sh"]
fn dispatch_transport_is_unrecorded() {
    let (_tmp, state) = build_test_state_with_live_bundles();
    let ctx = PlanContext {
        requested_by: EffectivePrincipal::Local(Principal {
            fingerprint: "fp:test-ui-dispatch".into(),
            scopes: vec![],
        }),
        project_context: ProjectContext::None,
        subject_resolution_authority:
            ryeos_engine::contracts::SubjectResolutionAuthority::Projectless,
        current_site_id: "site:local".into(),
        origin_site_id: "site:local".into(),
        execution_hints: Default::default(),
        scheduled_fire: None,
        validate_only: true,
    };
    let canonical = CanonicalRef::parse("service:ui/invocations/dispatch").unwrap();
    let resolved = state.engine.resolve(&ctx, &canonical).unwrap();
    let verified = state.engine.verify(&ctx, resolved).unwrap();

    assert!(
        !ryeos_app::service_registry::extract_record_thread(&verified.resolved.metadata.extra)
            .unwrap(),
        "the UI dispatch transport must not create a thread that triggers another UI refresh"
    );
}

#[tokio::test]
async fn arbitrary_event_targets_are_rejected() {
    let (_tmp, state) = build_test_state();
    let (session_id, _token) = get_ui_state(&state)
        .unwrap()
        .browser_sessions
        .mint_token(test_context());

    let ctx = HandlerContext::new(
        format!("session:{session_id}"),
        vec!["ui.read".into()],
        false,
    );

    let result = (ryeos_ui::handlers::ui_invocations_dispatch::DESCRIPTOR.handler)(
        serde_json::json!({
            "target": { "kind": "ref", "ref": "custom.event" },
            "params": { "key": "val" }
        }),
        ctx,
        Arc::new(state.clone()),
    )
    .await;

    assert!(result.is_err(), "arbitrary UI event dispatch should fail");
    let msg = format!("{:#}", result.unwrap_err());
    assert!(
        msg.contains("invalid ui.invocations.dispatch request"),
        "{msg}"
    );
}

#[tokio::test]
async fn legacy_client_authority_flags_are_rejected() {
    let (_tmp, state) = build_test_state();
    let (session_id, _token) = get_ui_state(&state)
        .unwrap()
        .browser_sessions
        .mint_token(observation_context());

    let ctx = HandlerContext::new(
        format!("session:{session_id}"),
        vec![
            "ui.read".into(),
            "ryeos.execute.service.commands/submit".into(),
        ],
        false,
    );

    let result = (ryeos_ui::handlers::ui_invocations_dispatch::DESCRIPTOR.handler)(
        serde_json::json!({
            "target": { "kind": "ref", "ref": "service:commands/submit" },
        }),
        ctx,
        Arc::new(state),
    )
    .await;

    assert!(
        result.is_err(),
        "legacy authority-bearing payload should fail"
    );
    let msg = format!("{:#}", result.unwrap_err());
    assert!(
        msg.contains("invalid ui.invocations.dispatch request"),
        "{msg}"
    );
}

#[tokio::test]
async fn session_cookie_required() {
    let (_tmp, state) = build_test_state();

    let ctx = HandlerContext::anonymous();

    let result = (ryeos_ui::handlers::ui_invocations_dispatch::DESCRIPTOR.handler)(
        serde_json::json!({
            "binding_digest": "fixture",
            "coordinate": { "kind": "source", "view_ref": "view:test/x", "channel": "default" },
            "payload": { "kind": "source_parameters", "params": {} }
        }),
        ctx,
        Arc::new(state),
    )
    .await;

    assert!(result.is_err(), "anonymous should be rejected");
    let msg = format!("{:#}", result.unwrap_err());
    assert!(
        msg.contains("session"),
        "expected session mention, got: {msg}"
    );
}

#[tokio::test]
#[ignore = "requires populated handler binaries via scripts/populate-bundles.sh"]
async fn session_local_invocation_publishes_to_session_bus() {
    let (_tmp, state) = build_test_state_with_live_bundles();
    let (session_id, ctx, session) = mint_live_session(&state).await;

    // Subscribe to the session bus before invoking.
    let mut rx = get_ui_state(&state)
        .unwrap()
        .session_bus
        .subscribe(&session_id);

    let result = (ryeos_ui::handlers::ui_invocations_dispatch::DESCRIPTOR.handler)(
        serde_json::json!({
            "binding_digest": session.compiled_binding.binding_digest,
            "coordinate": {
                "kind": "affordance",
                "view_ref": "view:ryeos/projects/list",
                "affordance_id": "register-project"
            },
            "payload": {
                "kind": "selection",
                "record": { "root": state.config.app_root }
            }
        }),
        ctx,
        Arc::new(state),
    )
    .await
    .expect("should succeed");

    let invocation_id = result["invocation_id"].as_str().unwrap();

    // The session bus should have received the event.
    let event = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
        .await
        .expect("timeout")
        .expect("recv error");

    assert_eq!(event.event_type, "invocation.dispatched");
    assert_eq!(event.payload["target"]["kind"], "ref");
    assert_eq!(event.payload["target"]["ref"], "service:projects/add");
    assert_eq!(event.payload["invocation_id"], invocation_id);
}

#[tokio::test]
#[ignore = "requires populated handler binaries via scripts/populate-bundles.sh"]
async fn read_only_thread_sources_replay_without_recording_service_threads() {
    let (_tmp, state) = build_test_state_with_live_bundles();
    let (_session_id, ctx, session) = mint_live_session(&state).await;
    let binding_digest = session.compiled_binding.binding_digest.clone();
    let state = Arc::new(state);

    let opened = (ryeos_ui::handlers::ui_seat::OPEN_DESCRIPTOR.handler)(
        serde_json::json!({ "surface_ref": "surface:ryeos/ui/base" }),
        ctx.clone(),
        state.clone(),
    )
    .await
    .expect("open seat fixture");
    let chain_root_id = opened["chain_root_id"].as_str().unwrap();
    (ryeos_ui::handlers::ui_seat::APPEND_DESCRIPTOR.handler)(
        serde_json::json!({
            "thread_id": opened["thread_id"],
            "events": [{
                "event_type": "seat.facet",
                "payload": { "key": "selection", "value": "T-fixture" }
            }]
        }),
        ctx.clone(),
        state.clone(),
    )
    .await
    .expect("append replay fixture event");

    let rows_before = state
        .state_store
        .list_threads_filtered(100, None)
        .expect("list fixture threads");
    assert_eq!(rows_before.len(), 1, "only the seat is recorded");

    let node_rows = (ryeos_ui::handlers::ui_threads::DESCRIPTOR.handler)(
        serde_json::json!({ "limit": 100, "sort": "watch" }),
        ctx.clone(),
        state.clone(),
    )
    .await
    .expect("fetch node thread projection");
    assert_eq!(
        node_rows["threads"]
            .as_array()
            .expect("node thread collection")
            .len(),
        1
    );

    let project_rows = (ryeos_ui::handlers::ui_threads::DESCRIPTOR.handler)(
        serde_json::json!({
            "project": "current",
            "project_path": state.config.app_root,
            "limit": 100,
            "sort": "watch"
        }),
        ctx.clone(),
        state.clone(),
    )
    .await
    .expect("fetch project thread projection");
    assert_eq!(
        project_rows["threads"]
            .as_array()
            .expect("project thread collection")
            .len(),
        1
    );

    let listed = (ryeos_ui::handlers::ui_invocations_dispatch::DESCRIPTOR.handler)(
        serde_json::json!({
            "binding_digest": binding_digest,
            "coordinate": {
                "kind": "source",
                "view_ref": "view:ryeos/input",
                "channel": "input.line.mentions"
            },
            "payload": { "kind": "source_parameters", "params": {} }
        }),
        ctx.clone(),
        state.clone(),
    )
    .await
    .unwrap_or_else(|error| {
        panic!(
            "fetch thread source: {:?}",
            ryeos_app::handler_error::extract_handler_error(&error)
        )
    });
    assert_eq!(listed["result"]["thread"]["recorded"], false);
    assert_eq!(
        listed["result"]["result"]["threads"]
            .as_array()
            .expect("threads collection")
            .len(),
        1
    );

    let replayed = (ryeos_ui::handlers::ui_invocations_dispatch::DESCRIPTOR.handler)(
        serde_json::json!({
            "binding_digest": binding_digest,
            "coordinate": {
                "kind": "source",
                "view_ref": "view:ryeos/thread/transcript",
                "channel": "default"
            },
            "payload": {
                "kind": "source_parameters",
                "params": { "chain_root_id": chain_root_id }
            }
        }),
        ctx,
        state.clone(),
    )
    .await
    .unwrap_or_else(|error| {
        panic!(
            "fetch transcript source: {:?}",
            ryeos_app::handler_error::extract_handler_error(&error)
        )
    });
    assert_eq!(replayed["result"]["thread"]["recorded"], false);
    assert_eq!(
        replayed["result"]["result"]["events"]
            .as_array()
            .expect("events collection")
            .len(),
        3,
        "thread_created, thread_started, and seat.facet are replayed"
    );

    let rows_after = state
        .state_store
        .list_threads_filtered(100, None)
        .expect("list threads after read-only sources");
    assert_eq!(rows_after.len(), rows_before.len());
}
