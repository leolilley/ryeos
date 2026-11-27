// Tests for `ui.invocations.dispatch` handler.

use ryeos_ui::test_support as test_state;
use test_state::{
    build_test_state, build_test_state_with_live_bundles, launch_context, mint_launch,
};

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
    ryeos_client_base::ui::UiPresentationProducer,
    ryeos_ui::browser_session::BindingAttachmentCoordinate,
) {
    test_state::live_presentation_with_surface(state, "surface:ryeos/ui/assistant").await
}

/// A durable chain owned by the browser session's principal, the only kind of
/// chain a verified service may replay for it. A seat is owned by its browser
/// session itself, so its history is read through the seat stream instead.
///
/// The seat is bound to the fixture checkout, so the chain lives in that
/// checkout: a project seat lists only its own project's threads.
fn create_principal_owned_chain(
    state: &ryeos_app::state::AppState,
    thread_id: &str,
    owner: &str,
    project_root: &std::path::Path,
) {
    use ryeos_state::objects::{
        CapturedItemTrustClass, CapturedNodeHistoryPolicyProvenance, CapturedPolicyProvenance,
        CapturedThreadHistoryPolicy, EnvironmentAuthority, ExecutionProjectAuthority,
        LiveFilesystemConfinement, LiveProjectAccess, ThreadHistoryRetention,
    };
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
    let hash = "a".repeat(64);
    state
        .state_store
        .create_thread_for_test(&ryeos_app::state_store::NewThreadRecord {
            thread_id: thread_id.to_string(),
            chain_root_id: thread_id.to_string(),
            kind: "directive".to_string(),
            item_ref: "directive:test/owned-chain".to_string(),
            executor_ref: "test/executor".to_string(),
            launch_mode: "wait".to_string(),
            current_site_id: "site:test".to_string(),
            origin_site_id: "site:test".to_string(),
            upstream_thread_id: None,
            requested_by: Some(owner.to_string()),
            project_root: Some(project_root),
            project_authority,
            base_project_snapshot_hash: None,
            usage_subject: None,
            usage_subject_asserted_by: None,
            captured_history_policy: Some(CapturedThreadHistoryPolicy {
                retention: ThreadHistoryRetention::Durable,
                canonical_item_ref: "directive:test/owned-chain".to_string(),
                item_content_hash: hash.clone(),
                item_signer_fingerprint: Some(hash.clone()),
                item_trust_class: CapturedItemTrustClass::Trusted,
                kind_schema_content_hash: hash,
                resolved_from: CapturedPolicyProvenance::NodeDefault {
                    node_policy: CapturedNodeHistoryPolicyProvenance::test_policy(),
                },
            }),
        })
        .expect("create principal-owned chain");
    state
        .state_store
        .mark_thread_running(thread_id, None)
        .expect("mark chain running");
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
    let (_tmp, _state_owner, state) = build_test_state_with_live_bundles();
    let ctx = PlanContext {
        logical_project: ryeos_engine::contracts::LogicalProject::Absent(
            ryeos_engine::contracts::LogicalProjectAbsence::Projectless,
        ),
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
    let (_tmp, _state_owner, state) = build_test_state();
    let (session_id, token) = mint_launch(&state, test_context());
    assert_eq!(
        get_ui_state(&state)
            .unwrap()
            .browser_sessions
            .consume_launch_token(&token),
        Some(session_id.clone())
    );

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
    let (_tmp, _state_owner, state) = build_test_state();
    let (session_id, token) = mint_launch(&state, observation_context());
    assert_eq!(
        get_ui_state(&state)
            .unwrap()
            .browser_sessions
            .consume_launch_token(&token),
        Some(session_id.clone())
    );

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
    let (_tmp, _state_owner, mut state) = build_test_state();
    // Dispatch validates both transport bounds before authenticating. Supply
    // the ordinary append route in this empty-engine handler fixture so an
    // absent route cannot masquerade as anonymous-session rejection.
    let mut append_route = state
        .node_config
        .routes
        .iter()
        .find(|route| {
            route.response.source.as_deref()
                == Some(ryeos_ui::handlers::ui_invocations_dispatch::DESCRIPTOR.service_ref)
        })
        .unwrap()
        .clone();
    append_route.id = "ui/seat/append".into();
    append_route.path = "/ui/api/seat/append".into();
    append_route.response.source = Some(
        ryeos_ui::handlers::ui_seat::APPEND_DESCRIPTOR
            .service_ref
            .into(),
    );
    Arc::make_mut(&mut state.node_config)
        .routes
        .push(append_route);

    let ctx = HandlerContext::anonymous();

    let result = (ryeos_ui::handlers::ui_invocations_dispatch::DESCRIPTOR.handler)(
        serde_json::json!({
            "binding": {
                "binding_attachment_id": "attachment-fixture",
                "binding_generation": 1,
                "binding_digest": "11".repeat(32),
                "coordinate": { "kind": "source", "view_ref": "view:test/x", "channel": "default" },
                "payload": { "kind": "source_parameters", "params": {} }
            },
            "producer": {
                "presentation_id": "00000000-0000-4000-8000-000000000001",
                "thread_id": "T-fixture", "producer_incarnation": "22".repeat(32)
            }
        }),
        ctx,
        Arc::new(state),
    )
    .await;

    assert!(result.is_err(), "anonymous should be rejected");
    let error = result.unwrap_err();
    assert!(matches!(
        error.downcast_ref::<ryeos_app::handler_error::HandlerError>(),
        Some(ryeos_app::handler_error::HandlerError::Forbidden(_))
    ));
    let msg = format!("{error:#}");
    assert!(
        msg.contains("session"),
        "expected session mention, got: {msg}"
    );
}

#[tokio::test]
#[ignore = "requires populated handler binaries via scripts/populate-bundles.sh"]
async fn session_local_invocation_publishes_to_session_bus() {
    let (_tmp, _state_owner, state) = build_test_state_with_live_bundles();
    let (session_id, ctx, producer, coordinate) = test_state::live_shipped_presentation_in_project(
        &state,
        &test_state::fixture_project_root(&state),
    )
    .await;
    let state = Arc::new(state);
    let folder = tempfile::TempDir::new().expect("folder to register");

    // Subscribe to the session bus before invoking.
    let mut rx = get_ui_state(&state)
        .unwrap()
        .session_bus
        .subscribe(&session_id);

    let result = (ryeos_ui::handlers::ui_invocations_dispatch::DESCRIPTOR.handler)(
        serde_json::json!({
            "producer": producer,
            "binding": {
            "binding_attachment_id": coordinate.binding_attachment_id,
            "binding_generation": coordinate.binding_generation,
            "binding_digest": coordinate.binding_digest,
            "coordinate": {
                "kind": "affordance",
                "view_ref": "view:ryeos/projects/list",
                "affordance_id": "register-project"
            },
            "payload": {
                "kind": "selection",
                "record": { "root": folder.path() }
            }
            }
        }),
        ctx,
        state.clone(),
    )
    .await
    .unwrap_or_else(|error| {
        panic!(
            "dispatch the registration affordance: {:?}",
            ryeos_app::handler_error::extract_handler_error(&error)
        )
    });

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

    // Registering a host folder is the node operator's act: a seat minted for
    // another principal dispatches the same affordance and is refused.
    let (_other_session, other_ctx, other_producer, other_coordinate) =
        test_state::live_shipped_presentation(&state).await;
    let refused = (ryeos_ui::handlers::ui_invocations_dispatch::DESCRIPTOR.handler)(
        serde_json::json!({
            "producer": other_producer,
            "binding": {
            "binding_attachment_id": other_coordinate.binding_attachment_id,
            "binding_generation": other_coordinate.binding_generation,
            "binding_digest": other_coordinate.binding_digest,
            "coordinate": {
                "kind": "affordance",
                "view_ref": "view:ryeos/projects/list",
                "affordance_id": "register-project"
            },
            "payload": {
                "kind": "selection",
                "record": { "root": folder.path() }
            }
            }
        }),
        other_ctx,
        state.clone(),
    )
    .await
    .expect_err("a non-operator seat may not register a project");
    assert!(
        matches!(
            ryeos_app::handler_error::extract_handler_error(&refused),
            Some(ryeos_app::handler_error::HandlerError::Structured { code, status: 403, .. })
                if code == "local_operator_required"
        ),
        "expected local_operator_required, got: {refused:#}"
    );
    // The refusal comes before any effect: nothing was registered.
    let registered = test_state::non_operator_registered_roots(&state).await;
    assert!(
        !registered
            .iter()
            .any(|root| std::path::Path::new(root) == folder.path()),
        "the refused seat registered {registered:?}"
    );
}

#[tokio::test]
#[ignore = "requires populated handler binaries via scripts/populate-bundles.sh"]
async fn read_only_thread_sources_replay_without_recording_service_threads() {
    let (_tmp, _state_owner, state) = build_test_state_with_live_bundles();
    let (session_id, ctx, producer, coordinate) = mint_live_session(&state).await;
    let state = Arc::new(state);

    let chain_root_id = producer.thread_id.as_str();
    let events = serde_json::json!([{
        "engine_seq": 0, "event_type": "seat.facet",
        "payload": { "key": "selection", "value": "T-fixture" }
    }]);
    let payload_digest =
        ryeos_ui_presentation_contract::payload_digest::seat_payload_digest(&events).unwrap();
    let operation_id = ryeos_client_base::ui::admission_operation::UiSeatAppendIdentity {
        presentation_id: producer.presentation_id.clone(),
        chain_root_id: chain_root_id.into(),
        thread_id: producer.thread_id.clone(),
        producer_incarnation: producer.producer_incarnation.clone(),
        first_engine_seq: 0,
        last_engine_seq: 0,
        event_count: 1,
        payload_digest: payload_digest.clone(),
    }
    .derive_identity()
    .unwrap();
    (ryeos_ui::handlers::ui_seat::APPEND_DESCRIPTOR.handler)(
        serde_json::json!({
            "presentation_id": producer.presentation_id,
            "thread_id": producer.thread_id,
            "producer_incarnation": producer.producer_incarnation,
            "operation_id": operation_id,
            "first_engine_seq": 0, "last_engine_seq": 0, "event_count": 1,
            "payload_digest": payload_digest, "events": events
        }),
        ctx.clone(),
        state.clone(),
    )
    .await
    .expect("append replay fixture event");

    let session_owner = get_ui_state(&state)
        .unwrap()
        .browser_sessions
        .get_session(&session_id)
        .unwrap()
        .principal_id
        .clone();
    let owned_chain = "T-owned-chain";
    create_principal_owned_chain(
        &state,
        owned_chain,
        &session_owner,
        &test_state::fixture_project_root(&state),
    );
    let rows_before = state
        .state_store
        .list_threads_filtered(100, None)
        .expect("list fixture threads");
    assert_eq!(
        rows_before.len(),
        2,
        "only the seat and the owned chain are recorded"
    );

    let node_rows = (ryeos_ui::handlers::ui_threads::DESCRIPTOR.handler)(
        serde_json::json!({ "limit": 100, "sort": "watch" }),
        ctx.clone(),
        state.clone(),
    )
    .await
    .expect_err("cookie cannot bypass compiled node source binding");
    assert!(format!("{node_rows:#}").contains("compiled binding coordinate"));

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
    .expect_err("browser project_path is not admitted project source authority");
    assert!(format!("{project_rows:#}").contains("compiled binding coordinate"));

    let listed = (ryeos_ui::handlers::ui_invocations_dispatch::DESCRIPTOR.handler)(
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
            "payload": { "kind": "source_parameters", "params": {} }
            }
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
            "producer": producer,
            "binding": {
            "binding_attachment_id": coordinate.binding_attachment_id,
            "binding_generation": coordinate.binding_generation,
            "binding_digest": coordinate.binding_digest,
            "coordinate": {
                "kind": "source",
                "view_ref": "view:ryeos/thread/transcript",
                "channel": "raw"
            },
            "payload": {
                "kind": "source_parameters",
                // The transcript's raw channel returns the chain's durable events
                // for a range, exactly as the daemon retains them; the folded
                // default channel returns timeline items instead.
                "params": { "chain_root_id": owned_chain, "from_chain_seq": "1", "to_chain_seq": "200" }
            }
            }
        }),
        ctx.clone(),
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

    // The default channel is the folded timeline of the same chain: items,
    // not raw events.
    let folded = (ryeos_ui::handlers::ui_invocations_dispatch::DESCRIPTOR.handler)(
        serde_json::json!({
            "producer": producer,
            "binding": {
            "binding_attachment_id": coordinate.binding_attachment_id,
            "binding_generation": coordinate.binding_generation,
            "binding_digest": coordinate.binding_digest,
            "coordinate": {
                "kind": "source",
                "view_ref": "view:ryeos/thread/transcript",
                "channel": "default"
            },
            "payload": {
                "kind": "source_parameters",
                // The client resolves the signed `limit` default (`|200`)
                // before it dispatches.
                "params": { "chain_root_id": owned_chain, "limit": 200 }
            }
            }
        }),
        ctx.clone(),
        state.clone(),
    )
    .await
    .unwrap_or_else(|error| {
        panic!(
            "fetch folded timeline source: {:?}",
            ryeos_app::handler_error::extract_handler_error(&error)
        )
    });
    assert_eq!(folded["result"]["thread"]["recorded"], false);
    assert!(
        folded["result"]["result"]["items"].is_array(),
        "the default channel returns timeline items: {folded}"
    );
    let retained = state
        .events
        .replay(&ryeos_app::event_store_service::EventReplayParams {
            thread_id: None,
            chain_root_id: Some(owned_chain.to_string()),
            after_chain_seq: None,
            limit: 100,
            event_types: None,
        })
        .expect("replay the owned chain directly")
        .events;
    assert!(!retained.is_empty(), "the owned chain retains its creation");
    assert_eq!(
        replayed["result"]["result"]["events"]
            .as_array()
            .expect("events collection")
            .len(),
        retained.len(),
        "the compiled source replays exactly the chain's retained events"
    );

    let rows_after = state
        .state_store
        .list_threads_filtered(100, None)
        .expect("list threads after read-only sources");
    assert_eq!(rows_after.len(), rows_before.len());
}

/// The Surface ceiling holds at dispatch on every call, not only when the
/// binding compiles: a call a browser crafts for a source or an act the
/// Surface's views do not bind is refused as Forbidden before any effect.
/// `base-observe` keeps an empty act lane, so its registration affordance is
/// not in the compiled binding either.
#[tokio::test]
async fn a_crafted_call_outside_the_surface_ceiling_is_refused_at_dispatch() {
    let (_tmp, _state_owner, state) = build_test_state_with_live_bundles();
    let (_session_id, ctx, producer, coordinate) =
        test_state::live_presentation_with_surface(&state, "surface:ryeos/ui/base-observe").await;
    let state = Arc::new(state);
    let folder = tempfile::TempDir::new().expect("folder to register");

    let crafted = [
        serde_json::json!({
            "kind": "affordance",
            "view_ref": "view:ryeos/projects/list",
            "affordance_id": "register-project"
        }),
        serde_json::json!({
            "kind": "source",
            "view_ref": "view:ryeos/projects/list",
            "channel": "a-channel-the-view-does-not-declare"
        }),
    ];
    for coordinate_value in crafted {
        let payload = if coordinate_value["kind"] == "affordance" {
            serde_json::json!({ "kind": "selection", "record": { "root": folder.path() } })
        } else {
            serde_json::json!({ "kind": "source_parameters", "params": {} })
        };
        let refused = (ryeos_ui::handlers::ui_invocations_dispatch::DESCRIPTOR.handler)(
            serde_json::json!({
                "producer": producer,
                "binding": {
                    "binding_attachment_id": coordinate.binding_attachment_id,
                    "binding_generation": coordinate.binding_generation,
                    "binding_digest": coordinate.binding_digest,
                    "coordinate": coordinate_value,
                    "payload": payload
                }
            }),
            ctx.clone(),
            state.clone(),
        )
        .await
        .expect_err("a call outside the ceiling is refused");
        assert!(
            matches!(
                ryeos_app::handler_error::extract_handler_error(&refused),
                Some(ryeos_app::handler_error::HandlerError::Forbidden(_))
            ),
            "expected Forbidden for {coordinate_value}, got: {refused:#}"
        );
    }
}
