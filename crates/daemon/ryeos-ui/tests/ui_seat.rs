mod test_state;
use test_state::{build_test_state_with_live_bundles, launch_context};

use ryeos_app::handler_context::HandlerContext;
use ryeos_client_base::ui::{
    UiPresentationProducerObservation, UiSeatControlRequest, UiSeatProducerClaim,
};
use ryeos_ui::state::get_ui_state;
use serde_json::{Value, json};
use std::sync::Arc;

// Exercise the same verifier + canonical service invoker used by the signed
// HTTP routes. Calling the handler alone misses dispatch-class mismatches.
async fn invoke_seat_route(
    descriptor: &ryeos_api::registry::ServiceDescriptor,
    input: serde_json::Value,
    context: HandlerContext,
    state: Arc<ryeos_app::state::AppState>,
) -> anyhow::Result<serde_json::Value> {
    use ryeos_api::routes::invocation::{
        CompiledRouteInvocation, RouteInvocationContext, RouteInvocationResult,
    };
    let mut headers = axum::http::HeaderMap::new();
    let session_id = context.fingerprint.strip_prefix("session:").unwrap();
    headers.insert("cookie", format!("ryeos_session={session_id}").parse()?);
    let mut invocation = RouteInvocationContext {
        route_id: descriptor.service_ref.into(),
        method: axum::http::Method::POST,
        uri: "/ui/api/session/seat/open".parse()?,
        captures: Default::default(),
        headers,
        body_raw: Vec::new(),
        input,
        principal: None,
        workspace_lifeline: None,
        launch_timings: None,
        publication: None,
        state: state.as_ref().clone(),
        webhook_dedupe: Arc::new(Default::default()),
    };
    let verifier = ryeos_ui::invokers::browser_session_invocation::CompiledBrowserSessionVerifier {
        ui: get_ui_state(&state).unwrap().clone(),
    };
    // Auth ignores the input; retain it for the service invocation.
    let input = invocation.input.clone();
    let auth = verifier.invoke(invocation).await?;
    let RouteInvocationResult::Principal(principal) = auth else {
        panic!("auth verifier must return a principal");
    };
    invocation = RouteInvocationContext {
        route_id: descriptor.service_ref.into(),
        method: axum::http::Method::POST,
        uri: "/ui/api/session/seat/open".parse()?,
        captures: Default::default(),
        headers: Default::default(),
        body_raw: Vec::new(),
        input,
        principal: Some(principal),
        workspace_lifeline: None,
        launch_timings: None,
        publication: None,
        state: state.as_ref().clone(),
        webhook_dedupe: Arc::new(Default::default()),
    };
    let invoker = ryeos_api::routes::invokers::service_invocation::CompiledServiceInvocation {
        service_ref: descriptor.service_ref.into(),
        subject_kind: "service".into(),
        endpoint: descriptor.endpoint.into(),
    };
    match invoker.invoke(invocation).await? {
        RouteInvocationResult::Json { value, .. } => Ok(value),
        _ => panic!("seat route must return JSON"),
    }
}

/// Exercise the compiled authenticated POST stream rather than the retired
/// JSON replay pager. Stop at its completion fence or at source EOF.
fn session_context(
    state: &ryeos_app::state::AppState,
    principal: Option<String>,
) -> ryeos_ui::browser_session::LaunchContext {
    // A real, signed Config root; never a Surface masquerading as Config.
    let body = "category: test\nversion: '1'\nshared_presentation:\n  schema: 1\n  title: Test presentation\n  launcher: launch\n  dialogs:\n    - id: launch\n      title: Launch\n      view: 'view:ryeos/home'\n  capabilities:\n    sources: []\n    affordances: []\n";
    let key = lillux::crypto::load_signing_key(
        &ryeos_engine::test_support::workspace_root().join(".dev-keys/PUBLISHER_DEV.pem"),
    )
    .expect("established development publisher key");
    let dir = state.config.app_root.join(".ai/config/test");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("presentation.yaml"),
        lillux::signature::sign_content(body, &key, "#", None),
    )
    .unwrap();
    // The Config lives in the project space of the fixture application root,
    // so its binding is the one the daemon would mint: the engine's own
    // effective identity of the Config and of its View closure.
    let project_root = state.config.app_root.canonicalize().unwrap();
    let mut context = launch_context(
        "config:test/presentation",
        project_root.to_str(),
        ryeos_ui::compiled_binding::EffectiveUiPosture::ObservationOnly,
        principal,
    );
    let authority = context
        .project_authority
        .clone()
        .expect("fixture project authority");
    let embedded = state
        .engine
        .with_checked_bundle_generation(|generation| -> anyhow::Result<_> {
            let root = generation.effective_item_under_project_authority(
                ryeos_engine::engine::EffectiveItemRequest {
                    item_ref: ryeos_engine::canonical_ref::CanonicalRef::parse(
                        "config:test/presentation",
                    )?,
                    expected_kind: Some("config".into()),
                    project_root: Some(project_root.clone()),
                    subject_resolution_authority:
                        ryeos_engine::contracts::SubjectResolutionAuthority::for_live_project_root(
                            Some(&project_root),
                        ),
                },
                authority.as_ref(),
            )?;
            ryeos_api::surface_views::resolve_effective_ui_view_closure_in_generation(
                generation,
                Some(&project_root),
                Some(authority.as_ref()),
                &root,
                vec!["view:ryeos/home".into()],
            )
            .map_err(anyhow::Error::msg)
        })
        .expect("resolve fixture composition");
    let binding = &mut Arc::make_mut(&mut context.compiled_binding).binding;
    binding.surface = embedded.identity.surface;
    binding.views = embedded.identity.views;
    context.presentation = ryeos_client_base::presentation::UiCompositionPresentation {
        definition: ryeos_client_base::presentation::UiCompositionDefinition::SharedPresentation {
            config: serde_yaml::from_str(body).unwrap(),
        },
        views: embedded.values,
    };
    context
}

fn handler_context(session_id: &str) -> HandlerContext {
    HandlerContext::new(
        format!("session:{session_id}"),
        vec!["ui.read".into()],
        false,
    )
}

fn activate(state: &ryeos_app::state::AppState) -> String {
    let (id, token) =
        test_state::mint_launch(state, session_context(state, Some("fp:user-1".into())));
    assert_eq!(
        get_ui_state(state)
            .unwrap()
            .browser_sessions
            .consume_launch_token(&token),
        Some(id.clone())
    );
    id
}

fn prepare_request(
    state: &ryeos_app::state::AppState,
    session_id: &str,
    request_id: &str,
) -> Value {
    let session = get_ui_state(state)
        .unwrap()
        .browser_sessions
        .get_session(session_id)
        .unwrap();
    let root = session.attachments[&session.root_attachment_id].coordinate();
    json!({"action":"prepare","creation_request_id":request_id,
        "binding_attachment_id":root.binding_attachment_id,
        "binding_generation":root.binding_generation,"binding_digest":root.binding_digest})
}

async fn route(
    state: &Arc<ryeos_app::state::AppState>,
    session: &str,
    descriptor: &ryeos_api::registry::ServiceDescriptor,
    input: Value,
) -> anyhow::Result<Value> {
    invoke_seat_route(descriptor, input, handler_context(session), state.clone()).await
}

async fn prepare(state: &Arc<ryeos_app::state::AppState>, session: &str) -> Value {
    route(
        state,
        session,
        &ryeos_ui::handlers::ui_seat::OPEN_DESCRIPTOR,
        prepare_request(state, session, "00000000-0000-4000-8000-000000000010"),
    )
    .await
    .unwrap()
}

async fn observe(
    state: &Arc<ryeos_app::state::AppState>,
    session: &str,
    prepared: &Value,
) -> anyhow::Result<UiPresentationProducerObservation> {
    Ok(serde_json::from_value(
        route(
            state,
            session,
            &ryeos_ui::handlers::ui_seat::OPEN_DESCRIPTOR,
            json!({"action":"observe", "presentation_id":prepared["presentation_id"],
                "thread_id":prepared["thread_id"], "chain_root_id":prepared["chain_root_id"]}),
        )
        .await?,
    )?)
}

fn identify_control(mut request: UiSeatControlRequest) -> anyhow::Result<Value> {
    request.assign_identity().map_err(anyhow::Error::msg)?;
    request.validate().map_err(anyhow::Error::msg)?;
    Ok(serde_json::to_value(request)?)
}

// Only for a new intent. Exact retries must reuse the returned request rather
// than observing a later predecessor or drawing a new nonce.
async fn claim_request(
    state: &Arc<ryeos_app::state::AppState>,
    session: &str,
    prepared: &Value,
    nonce: &str,
    claim: Value,
) -> anyhow::Result<Value> {
    let observed = observe(state, session, prepared).await?;
    identify_control(UiSeatControlRequest::Claim {
        schema: 2,
        operation_id: String::new(),
        presentation_id: observed.presentation_id,
        thread_id: observed.thread_id,
        chain_root_id: observed.chain_root_id,
        predecessor: observed.control_predecessor,
        intent_nonce: nonce.into(),
        claim: serde_json::from_value::<UiSeatProducerClaim>(claim)?,
    })
}

async fn claim(
    state: &Arc<ryeos_app::state::AppState>,
    session: &str,
    prepared: &Value,
    nonce: &str,
    claim: Value,
) -> anyhow::Result<Value> {
    let request = claim_request(state, session, prepared, nonce, claim).await?;
    route(
        state,
        session,
        &ryeos_ui::handlers::ui_seat::OPEN_DESCRIPTOR,
        request,
    )
    .await
}

async fn fresh_with_request(
    state: &Arc<ryeos_app::state::AppState>,
    session: &str,
) -> (Value, Value) {
    let prepared = prepare(state, session).await;
    let request = claim_request(
        state,
        session,
        &prepared,
        "00000000-0000-4000-8000-000000000011",
        json!({"kind":"fresh"}),
    )
    .await
    .unwrap();
    let opened = route(
        state,
        session,
        &ryeos_ui::handlers::ui_seat::OPEN_DESCRIPTOR,
        request.clone(),
    )
    .await
    .unwrap();
    (opened, request)
}

async fn fresh(state: &Arc<ryeos_app::state::AppState>, session: &str) -> Value {
    fresh_with_request(state, session).await.0
}

fn append_request(opened: &Value, _op: &str, first: u64, payloads: &[Value]) -> Value {
    let events:Vec<_> = payloads.iter().enumerate().map(|(offset,payload)|
        json!({"engine_seq":first+offset as u64,"event_type":"seat.facet","payload":payload})).collect();
    let mut request = json!({"presentation_id":opened["presentation_id"],"thread_id":opened["thread_id"],
        "producer_incarnation":opened["producer_incarnation"],"operation_id":"",
        "first_engine_seq":first,"last_engine_seq":first+payloads.len() as u64-1,
        "event_count":payloads.len(),"payload_digest":"","events":events});
    identify_append(opened, &mut request);
    request
}

fn identify_append(opened: &Value, request: &mut Value) {
    let digest = ryeos_client_base::ui::seat_payload_digest(&request["events"]).unwrap();
    let identity = ryeos_client_base::ui::admission_operation::UiSeatAppendIdentity {
        presentation_id: opened["presentation_id"].as_str().unwrap().into(),
        chain_root_id: opened["chain_root_id"].as_str().unwrap().into(),
        thread_id: opened["thread_id"].as_str().unwrap().into(),
        producer_incarnation: opened["producer_incarnation"].as_str().unwrap().into(),
        first_engine_seq: request["first_engine_seq"].as_u64().unwrap(),
        last_engine_seq: request["last_engine_seq"].as_u64().unwrap(),
        event_count: request["event_count"].as_u64().unwrap(),
        payload_digest: digest.clone(),
    }
    .derive_identity()
    .unwrap();
    request["payload_digest"] = json!(digest);
    request["operation_id"] = json!(identity);
}

async fn transition_request(
    state: &Arc<ryeos_app::state::AppState>,
    session: &str,
    opened: &Value,
    nonce: &str,
    action: &str,
) -> Value {
    let observed = observe(state, session, opened).await.unwrap();
    let producer_incarnation = opened["producer_incarnation"].as_str().unwrap().into();
    let request = match action {
        "relinquish" => UiSeatControlRequest::Relinquish {
            schema: 2,
            operation_id: String::new(),
            presentation_id: observed.presentation_id,
            thread_id: observed.thread_id,
            chain_root_id: observed.chain_root_id,
            predecessor: observed.control_predecessor,
            intent_nonce: nonce.into(),
            producer_incarnation,
        },
        "close" => UiSeatControlRequest::Close {
            schema: 2,
            operation_id: String::new(),
            presentation_id: observed.presentation_id,
            thread_id: observed.thread_id,
            chain_root_id: observed.chain_root_id,
            predecessor: observed.control_predecessor,
            intent_nonce: nonce.into(),
            producer_incarnation,
        },
        _ => panic!("unknown test control action"),
    };
    identify_control(request).unwrap()
}

#[tokio::test]
async fn producer_observation_is_exact_read_only_and_does_not_infer_liveness() {
    use ryeos_ui::handlers::ui_seat::{CLOSE_DESCRIPTOR, OPEN_DESCRIPTOR};
    let (_tmp, _state_owner, state) = build_test_state_with_live_bundles();
    let session = activate(&state);
    let foreign = activate(&state);
    let state = Arc::new(state);
    let prepared = prepare(&state, &session).await;
    let request = json!({"action":"observe", "presentation_id":prepared["presentation_id"],
        "thread_id":prepared["thread_id"], "chain_root_id":prepared["chain_root_id"]});
    let observed = route(&state, &session, &OPEN_DESCRIPTOR, request.clone())
        .await
        .unwrap();
    assert_eq!(observed["producer"], json!({"kind":"unclaimed"}));
    assert_eq!(observed["root_binding"], prepared["root_binding"]);
    assert_eq!(observed["presentation_id"], prepared["presentation_id"]);
    assert_eq!(observed["thread_id"], prepared["thread_id"]);
    assert_eq!(observed["chain_root_id"], prepared["chain_root_id"]);
    assert_eq!(observed["control_predecessor"]["kind"], "initialization");
    let initialization_predecessor = observed["control_predecessor"].clone();
    route(&state, &foreign, &OPEN_DESCRIPTOR, request.clone())
        .await
        .expect_err("same principal in a different session cannot observe this presentation");
    for field in ["presentation_id", "thread_id", "chain_root_id"] {
        let mut forged = request.clone();
        forged[field] = json!("00000000-0000-4000-8000-000000000099");
        route(&state, &session, &OPEN_DESCRIPTOR, forged)
            .await
            .expect_err("all retained presentation coordinates must match");
    }
    let opened = fresh(&state, &session).await;
    let thread = opened["thread_id"].as_str().unwrap();
    let detail = state.state_store.get_thread(thread).unwrap().unwrap();
    let before = state
        .state_store
        .capture_thread_event_read_anchor(&detail.chain_root_id, thread)
        .unwrap()
        .unwrap();
    let owner = detail.requested_by.as_deref().unwrap();
    // Absence of a lease is not evidence that a retained active writer died.
    // Observation must neither guess a replacement nor recreate the lease.
    state.state_store.remove_seat_lease(thread).unwrap();
    let observed = route(&state, &session, &OPEN_DESCRIPTOR, request.clone())
        .await
        .unwrap();
    assert_eq!(
        observed["producer"],
        json!({"kind":"active",
        "producer_incarnation":opened["producer_incarnation"]})
    );
    assert_eq!(observed["control_predecessor"]["kind"], "control");
    assert_ne!(observed["control_predecessor"], initialization_predecessor);
    let active_predecessor = observed["control_predecessor"].clone();
    assert!(
        state
            .state_store
            .seat_leases_for_owner_client(owner, &detail.executor_ref)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        state
            .state_store
            .capture_thread_event_read_anchor(&detail.chain_root_id, thread)
            .unwrap()
            .unwrap()
            .tip(),
        before.tip(),
        "observation appends no event and claims no producer"
    );
    let released = route(
        &state,
        &session,
        &CLOSE_DESCRIPTOR,
        transition_request(
            &state,
            &session,
            &opened,
            "00000000-0000-4000-8000-000000000070",
            "relinquish",
        )
        .await,
    )
    .await
    .unwrap();
    let observed = route(&state, &session, &OPEN_DESCRIPTOR, request.clone())
        .await
        .unwrap();
    assert_eq!(
        observed["producer"],
        json!({"kind":"relinquished",
        "producer_incarnation":released["producer_incarnation"]})
    );
    assert_eq!(
        released["producer_incarnation"],
        opened["producer_incarnation"]
    );
    assert_eq!(observed["control_predecessor"]["kind"], "control");
    assert_ne!(
        observed["control_predecessor"], active_predecessor,
        "relinquish advances the CAS predecessor even when incarnation is unchanged"
    );
    let resumed = claim(
        &state,
        &session,
        &prepared,
        "00000000-0000-4000-8000-000000000071",
        json!({"kind":"resume","expected_producer_incarnation":released["producer_incarnation"]}),
    )
    .await
    .unwrap();
    route(
        &state,
        &session,
        &CLOSE_DESCRIPTOR,
        transition_request(
            &state,
            &session,
            &resumed,
            "00000000-0000-4000-8000-000000000072",
            "close",
        )
        .await,
    )
    .await
    .unwrap();
    let observed = route(&state, &session, &OPEN_DESCRIPTOR, request.clone())
        .await
        .unwrap();
    assert_eq!(
        observed["producer"],
        json!({"kind":"closed",
        "producer_incarnation":resumed["producer_incarnation"]})
    );
    route(
        &state,
        &session,
        &OPEN_DESCRIPTOR,
        claim_request(
            &state,
            &session,
            &prepared,
            "00000000-0000-4000-8000-000000000073",
            json!({"kind":"resume","expected_producer_incarnation":resumed["producer_incarnation"]}),
        )
        .await
        .unwrap(),
    )
    .await
    .expect_err("an exact Closed predecessor is not a resumable native presentation");
    let root: ryeos_ui::browser_session::BindingAttachmentCoordinate =
        serde_json::from_value(prepared["root_binding"].clone()).unwrap();
    get_ui_state(&state)
        .unwrap()
        .browser_sessions
        .revoke_attachment(&session, &root)
        .unwrap();
    route(&state, &session, &OPEN_DESCRIPTOR, request)
        .await
        .expect_err("revoked root attachment fences even read-only producer observation");
}

#[tokio::test]
async fn prepare_is_idempotent_without_claiming_or_stealing_a_producer() {
    use ryeos_ui::handlers::ui_seat::OPEN_DESCRIPTOR;
    let (_tmp, _state_owner, state) = build_test_state_with_live_bundles();
    let session = activate(&state);
    let foreign = activate(&state);
    let state = Arc::new(state);
    let prepared = prepare(&state, &session).await;
    assert!(prepared.get("producer_incarnation").is_none());
    assert_eq!(prepare(&state, &session).await, prepared);
    let unclaimed_session = activate(&state);
    let unclaimed = prepare(&state, &unclaimed_session).await;
    let initialized_thread = unclaimed["thread_id"].as_str().unwrap();
    let initialized_detail = state
        .state_store
        .get_thread(initialized_thread)
        .unwrap()
        .unwrap();
    state
        .state_store
        .remove_seat_lease(initialized_thread)
        .unwrap();
    assert_eq!(prepare(&state, &unclaimed_session).await, unclaimed);
    assert!(
        state
            .state_store
            .seat_leases_for_owner_client(
                initialized_detail.requested_by.as_deref().unwrap(),
                &initialized_detail.executor_ref
            )
            .unwrap()
            .is_empty()
    );
    let (opened, original_claim) = fresh_with_request(&state, &session).await;
    let thread = opened["thread_id"].as_str().unwrap();
    let detail = state.state_store.get_thread(thread).unwrap().unwrap();
    let control_before = observe(&state, &session, &prepared).await.unwrap();
    state.state_store.remove_seat_lease(thread).unwrap();
    assert_eq!(prepare(&state, &session).await, prepared);
    assert_eq!(
        observe(&state, &session, &prepared).await.unwrap(),
        control_before
    );
    assert!(
        state
            .state_store
            .seat_leases_for_owner_client(
                detail.requested_by.as_deref().unwrap(),
                &detail.executor_ref
            )
            .unwrap()
            .is_empty()
    );
    let retry = route(&state, &session, &OPEN_DESCRIPTOR, original_claim.clone())
        .await
        .unwrap();
    assert_eq!(retry, opened);
    let mut competing = original_claim.clone();
    competing["intent_nonce"] = json!("00000000-0000-4000-8000-000000000012");
    let competing = identify_control(serde_json::from_value(competing).unwrap()).unwrap();
    assert_ne!(competing["operation_id"], original_claim["operation_id"]);
    route(&state, &session, &OPEN_DESCRIPTOR, competing)
        .await
        .expect_err("a new fresh intent cannot reuse the consumed initialization predecessor");
    route(&state, &foreign, &OPEN_DESCRIPTOR, original_claim)
        .await
        .expect_err("same principal is not same session");
    let sibling = route(
        &state,
        &session,
        &ryeos_ui::handlers::ui_seat::OPEN_DESCRIPTOR,
        prepare_request(&state, &session, "00000000-0000-4000-8000-000000000013"),
    )
    .await
    .unwrap();
    assert_ne!(prepared["thread_id"], sibling["thread_id"]);
    let mut wrong = prepare_request(&state, &session, "00000000-0000-4000-8000-000000000010");
    wrong["binding_digest"] = json!("aa".repeat(32));
    route(
        &state,
        &session,
        &ryeos_ui::handlers::ui_seat::OPEN_DESCRIPTOR,
        wrong,
    )
    .await
    .expect_err("same creation request cannot substitute its exact root");
}

#[tokio::test]
async fn legacy_and_forged_control_intents_do_not_claim_a_producer() {
    use ryeos_ui::handlers::ui_seat::OPEN_DESCRIPTOR;
    let (_tmp, _state_owner, state) = build_test_state_with_live_bundles();
    let session = activate(&state);
    let state = Arc::new(state);
    let prepared = prepare(&state, &session).await;
    let before = observe(&state, &session, &prepared).await.unwrap();
    let request = claim_request(
        &state,
        &session,
        &prepared,
        "00000000-0000-4000-8000-000000000031",
        json!({"kind":"fresh"}),
    )
    .await
    .unwrap();
    assert_eq!(request["schema"], 2);
    assert_eq!(
        request["predecessor"],
        serde_json::to_value(&before.control_predecessor).unwrap()
    );
    assert_eq!(
        request["operation_id"].as_str().unwrap().as_bytes()[14],
        b'8'
    );
    let mut rejected = vec![json!({"action":"claim",
        "presentation_id":prepared["presentation_id"],
        "operation_id":"00000000-0000-4000-8000-000000000031", "claim":{"kind":"fresh"}})];
    for field in [
        "schema",
        "thread_id",
        "chain_root_id",
        "predecessor",
        "intent_nonce",
    ] {
        let mut missing = request.clone();
        missing.as_object_mut().unwrap().remove(field);
        rejected.push(missing);
    }
    for (field, value) in [
        ("schema", json!(1)),
        (
            "operation_id",
            json!("00000000-0000-4000-8000-000000000031"),
        ),
        (
            "intent_nonce",
            json!("00000000-0000-4000-8000-000000000032"),
        ),
        (
            "predecessor",
            json!({"kind":"initialization", "event_hash":"ab".repeat(32)}),
        ),
    ] {
        let mut contradicted = request.clone();
        contradicted[field] = value;
        rejected.push(contradicted);
    }
    let mut invented_predecessor = request.clone();
    invented_predecessor["predecessor"]["event_hash"] = json!("ab".repeat(32));
    rejected.push(identify_control(serde_json::from_value(invented_predecessor).unwrap()).unwrap());
    for invalid in rejected {
        route(&state, &session, &OPEN_DESCRIPTOR, invalid)
            .await
            .expect_err("old shape, false identity or unauthenticated predecessor must refuse");
        assert_eq!(observe(&state, &session, &prepared).await.unwrap(), before);
    }
    let opened = route(&state, &session, &OPEN_DESCRIPTOR, request.clone())
        .await
        .unwrap();
    assert_eq!(
        route(&state, &session, &OPEN_DESCRIPTOR, request)
            .await
            .unwrap(),
        opened,
        "retry retains its original predecessor, nonce and derived ID"
    );
}

#[tokio::test]
async fn append_receipts_survive_takeover_and_close_without_granting_old_producer() {
    use ryeos_ui::handlers::ui_seat::*;
    let (_tmp, _state_owner, state) = build_test_state_with_live_bundles();
    let session = activate(&state);
    let foreign = activate(&state);
    let state = Arc::new(state);
    let (opened, original_claim) = fresh_with_request(&state, &session).await;
    let op = "00000000-0000-4000-8000-000000000002";
    let request = append_request(&opened, op, 0, &[json!({"key":"é😀","value":{"a":"𐀀"}})]);
    let op = request["operation_id"].as_str().unwrap().to_owned();
    assert_eq!(
        request["payload_digest"],
        "a7b97b79ab2c07cfaf889d314a1dcf70276b5f8b59a37c2b75f81f8148b6307b"
    );
    let ack = route(&state, &session, &APPEND_DESCRIPTOR, request.clone())
        .await
        .unwrap();
    let acknowledged = ack["events"].as_array().unwrap();
    assert_eq!(ack["events_authority"], "authoritative");
    assert_eq!(acknowledged.len(), 1);
    assert_eq!(acknowledged[0]["event_type"], "seat.facet");
    assert_eq!(acknowledged[0]["durability"], "durable");
    assert!(acknowledged[0].get("event_id").is_none());
    assert!(acknowledged[0].get("storage_class").is_none());
    assert_eq!(acknowledged[0]["payload"]["seq"], 0);
    assert_eq!(
        acknowledged[0]["payload"]["payload"],
        request["events"][0]["payload"]
    );
    assert_eq!(acknowledged[0]["event_hash"].as_str().unwrap().len(), 64);
    assert_eq!(
        acknowledged[0]["chain_seq"]
            .as_str()
            .unwrap()
            .parse::<i64>()
            .unwrap()
            + 1,
        ack["chain_seq"].as_str().unwrap().parse::<i64>().unwrap()
    );
    assert_eq!(
        acknowledged[0]["thread_seq"]
            .as_str()
            .unwrap()
            .parse::<i64>()
            .unwrap()
            + 1,
        ack["receipt_thread_seq"]
            .as_str()
            .unwrap()
            .parse::<i64>()
            .unwrap()
    );
    let receipt_hash = ack["receipt_event_hash"].as_str().unwrap();
    assert_eq!(receipt_hash.len(), 64);
    assert!(receipt_hash.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_eq!(
        route(&state, &session, &APPEND_DESCRIPTOR, request.clone())
            .await
            .unwrap(),
        ack
    );
    for (op, seq) in [
        ("00000000-0000-4000-8000-000000000003", 2),
        ("00000000-0000-4000-8000-000000000004", 0),
    ] {
        route(
            &state,
            &session,
            &APPEND_DESCRIPTOR,
            append_request(&opened, op, seq, &[json!({"key":"different","value":true})]),
        )
        .await
        .expect_err("gap/overlap");
    }
    route(
        &state,
        &session,
        &APPEND_DESCRIPTOR,
        append_request(&opened, &op, 0, &[json!({"key":"different","value":true})]),
    )
    .await
    .expect_err("operation contradiction");
    let mut reused_id = append_request(
        &opened,
        "ignored",
        1,
        &[json!({"key":"new_interval","value":true})],
    );
    reused_id["operation_id"] = json!(op);
    route(&state, &session, &APPEND_DESCRIPTOR, reused_id)
        .await
        .expect_err("an old operation ID cannot name a new engine interval");
    let mut malformed = append_request(
        &opened,
        "00000000-0000-4000-8000-000000000005",
        1,
        &[json!({"key":"empty","value":null})],
    );
    malformed["payload_digest"] = json!("bad");
    route(&state, &session, &APPEND_DESCRIPTOR, malformed)
        .await
        .expect_err("malformed digest");
    let second_ack = route(
        &state,
        &session,
        &APPEND_DESCRIPTOR,
        append_request(
            &opened,
            "00000000-0000-4000-8000-000000000006",
            1,
            &[
                json!({"key":"fraction","value":0.000001}),
                json!({"key":"second","value":true}),
            ],
        ),
    )
    .await
    .unwrap();
    assert_eq!(second_ack["events"].as_array().unwrap().len(), 2);
    assert_eq!(
        second_ack["events"][0]["chain_seq"]
            .as_str()
            .unwrap()
            .parse::<i64>()
            .unwrap()
            + 1,
        second_ack["events"][1]["chain_seq"]
            .as_str()
            .unwrap()
            .parse::<i64>()
            .unwrap()
    );
    assert_ne!(
        second_ack["events"][0]["event_hash"],
        ack["events"][0]["event_hash"]
    );
    let rotated = claim(
        &state,
        &session,
        &opened,
        "00000000-0000-4000-8000-000000000014",
        json!({"kind":"takeover","expected_producer_incarnation":opened["producer_incarnation"]}),
    )
    .await
    .unwrap();
    assert_ne!(
        rotated["producer_incarnation"],
        opened["producer_incarnation"]
    );
    assert_eq!(
        route(&state, &session, &APPEND_DESCRIPTOR, request.clone())
            .await
            .unwrap(),
        ack
    );
    route(
        &state,
        &session,
        &APPEND_DESCRIPTOR,
        append_request(
            &opened,
            "00000000-0000-4000-8000-000000000015",
            2,
            &[json!({"key":"empty","value":null})],
        ),
    )
    .await
    .expect_err("stale producer");
    route(&state, &foreign, &APPEND_DESCRIPTOR, request.clone())
        .await
        .expect_err("foreign session");
    let lookup = json!({"presentation_id":opened["presentation_id"],"chain_root_id":opened["chain_root_id"],
        "receipt":{"thread_id":opened["thread_id"],"producer_incarnation":opened["producer_incarnation"],
            "operation_id":op,"first_engine_seq":0,"last_engine_seq":0,"event_count":1,"payload_digest":request["payload_digest"]}});
    let found = route(&state, &session, &REPLAY_DESCRIPTOR, lookup.clone())
        .await
        .unwrap();
    assert_eq!(found["receipt_status"], "found");
    assert_eq!(found["receipt"], ack);
    let mut missing = lookup.clone();
    missing["receipt"]["operation_id"] = json!("00000000-0000-4000-8000-000000000099");
    let absent = route(&state, &session, &REPLAY_DESCRIPTOR, missing)
        .await
        .unwrap();
    assert_eq!(absent["receipt_status"], "not_found");
    let mut mixed = lookup.clone();
    mixed["limit"] = json!(5);
    route(&state, &session, &REPLAY_DESCRIPTOR, mixed)
        .await
        .expect_err("receipt lookup cannot silently become cursor replay");
    let close = transition_request(
        &state,
        &session,
        &rotated,
        "00000000-0000-4000-8000-000000000016",
        "close",
    )
    .await;
    route(&state, &session, &OPEN_DESCRIPTOR, original_claim)
        .await
        .expect_err("historical claim receipt must not re-advertise displaced producer as active");
    let closed = route(&state, &session, &CLOSE_DESCRIPTOR, close.clone())
        .await
        .unwrap();
    assert_eq!(closed["state"], "closed");
    assert_eq!(
        route(&state, &session, &CLOSE_DESCRIPTOR, close)
            .await
            .unwrap(),
        closed
    );
    assert_eq!(
        route(&state, &session, &APPEND_DESCRIPTOR, request)
            .await
            .unwrap(),
        ack
    );
    assert_eq!(
        route(&state, &session, &REPLAY_DESCRIPTOR, lookup)
            .await
            .unwrap()["receipt"],
        ack
    );
    route(
        &state,
        &session,
        &REPLAY_DESCRIPTOR,
        json!({"presentation_id":opened["presentation_id"],"chain_root_id":opened["chain_root_id"]}),
    )
    .await
    .expect_err("the receipt route cannot serve unauthenticated cursor pages");
}

#[tokio::test]
async fn relinquish_resume_and_touch_are_exact_producer_fenced() {
    use ryeos_ui::handlers::ui_seat::*;
    let (_tmp, _state_owner, state) = build_test_state_with_live_bundles();
    let session = activate(&state);
    let state = Arc::new(state);
    let opened = fresh(&state, &session).await;
    let touch = json!({"presentation_id":opened["presentation_id"],"thread_id":opened["thread_id"],"producer_incarnation":opened["producer_incarnation"]});
    assert_eq!(
        route(&state, &session, &TOUCH_DESCRIPTOR, touch.clone())
            .await
            .unwrap()["touched"],
        true
    );
    let stale_resume = claim_request(
        &state,
        &session,
        &opened,
        "00000000-0000-4000-8000-000000000021",
        json!({"kind":"resume","expected_producer_incarnation":opened["producer_incarnation"]}),
    )
    .await
    .unwrap();
    route(&state, &session, &OPEN_DESCRIPTOR, stale_resume.clone())
        .await
        .expect_err("active producer cannot be resumed implicitly");
    let relinquish = transition_request(
        &state,
        &session,
        &opened,
        "00000000-0000-4000-8000-000000000022",
        "relinquish",
    )
    .await;
    let released = route(&state, &session, &CLOSE_DESCRIPTOR, relinquish.clone())
        .await
        .unwrap();
    assert_eq!(released["state"], "relinquished");
    assert_eq!(
        route(&state, &session, &CLOSE_DESCRIPTOR, relinquish.clone())
            .await
            .unwrap(),
        released
    );
    route(&state, &session, &OPEN_DESCRIPTOR, stale_resume)
        .await
        .expect_err("resume observed before relinquish cannot reuse the same incarnation with an old CAS predecessor");
    route(&state, &session, &TOUCH_DESCRIPTOR, touch)
        .await
        .expect_err("relinquished writer");
    let resumed = claim(
        &state,
        &session,
        &opened,
        "00000000-0000-4000-8000-000000000023",
        json!({"kind":"resume","expected_producer_incarnation":opened["producer_incarnation"]}),
    )
    .await
    .unwrap();
    assert_ne!(
        resumed["producer_incarnation"],
        opened["producer_incarnation"]
    );
    assert_eq!(
        route(&state, &session, &CLOSE_DESCRIPTOR, relinquish)
            .await
            .unwrap(),
        released,
        "historical exact relinquish retry returns its retained result without replacing the current writer"
    );
    assert_eq!(
        serde_json::to_value(observe(&state, &session, &opened).await.unwrap()).unwrap()["producer"],
        json!({"kind":"active", "producer_incarnation":resumed["producer_incarnation"]})
    );
    route(
        &state,
        &session,
        &CLOSE_DESCRIPTOR,
        transition_request(
            &state,
            &session,
            &opened,
            "00000000-0000-4000-8000-000000000024",
            "close",
        )
        .await,
    )
    .await
    .expect_err("stale close cannot terminate seat");
    let action = ryeos_ui::handlers::ALL
        .iter()
        .find(|d| d.service_ref == "service:projects/open")
        .unwrap();
    route(&state, &session, action, json!({}))
        .await
        .expect_err("cookie is not compiled action authority");
}

#[tokio::test]
async fn exact_append_retry_survives_a_later_unreceipted_signed_tip() {
    use ryeos_ui::handlers::ui_seat::APPEND_DESCRIPTOR;

    let (_tmp, _state_owner, state) = build_test_state_with_live_bundles();
    let session = activate(&state);
    let state = Arc::new(state);
    let opened = fresh(&state, &session).await;
    let request = append_request(
        &opened,
        "00000000-0000-4000-8000-000000000071",
        0,
        &[json!({"key":"selection","value":"retained"})],
    );
    let acknowledged = route(&state, &session, &APPEND_DESCRIPTOR, request.clone())
        .await
        .unwrap();
    let detail = state
        .state_store
        .get_thread(opened["thread_id"].as_str().unwrap())
        .unwrap()
        .unwrap();
    state
        .threads
        .append_thread_events(
            &detail.chain_root_id,
            &detail.thread_id,
            &[ryeos_app::state_store::NewEventRecord {
                event_type: "seat.facet".into(),
                storage_class: "indexed".into(),
                payload: json!({
                    "seq": 1,
                    "operation_id": "unreceipted-crash-fixture",
                    "payload": {"key":"selection","value":"unsettled"}
                }),
            }],
        )
        .unwrap()
        .expect("signed orphan fixture commits one event");

    assert_eq!(
        route(&state, &session, &APPEND_DESCRIPTOR, request)
            .await
            .unwrap(),
        acknowledged,
        "an exact authenticated receipt is replayable without folding a later incomplete tip"
    );
}

#[tokio::test]
async fn prepare_rejects_stale_attachment_policy() {
    let (_tmp, _state_owner, state) = build_test_state_with_live_bundles();
    let mut context = session_context(&state, None);
    let stale = "99".repeat(32);
    Arc::make_mut(&mut context.compiled_binding)
        .binding
        .node_policy_generation_digest = stale.clone();
    let sessions = &get_ui_state(&state).unwrap().browser_sessions;
    let (id, token, _) = sessions.mint_token(context, 16, &stale).unwrap();
    assert_eq!(sessions.consume_launch_token(&token), Some(id.clone()));
    let request = prepare_request(&state, &id, "00000000-0000-4000-8000-000000000010");
    route(
        &Arc::new(state),
        &id,
        &ryeos_ui::handlers::ui_seat::OPEN_DESCRIPTOR,
        request,
    )
    .await
    .expect_err("stale policy");
}

#[tokio::test]
async fn forged_presentation_operation_records_do_not_change_the_seat() {
    use ryeos_ui::handlers::ui_seat::APPEND_DESCRIPTOR;
    let (_tmp, _state_owner, state) = build_test_state_with_live_bundles();
    let session = activate(&state);
    let state = Arc::new(state);
    let opened = fresh(&state, &session).await;
    let thread = opened["thread_id"].as_str().unwrap();
    let root = opened["chain_root_id"].as_str().unwrap();
    let before = state
        .state_store
        .capture_thread_event_read_anchor(root, thread)
        .unwrap()
        .unwrap();
    for (index, event_type, payload) in [
        (
            0,
            "seat.presentation_operation.prepared",
            json!({"operation_id":"00000000-0000-4000-8000-000000000081"}),
        ),
        (
            1,
            "seat.presentation_operation.completed",
            json!({"operation_id":"00000000-0000-4000-8000-000000000082","result":{}}),
        ),
    ] {
        let mut request = append_request(
            &opened,
            &format!("00000000-0000-4000-8000-{:012}", 90 + index),
            0,
            &[payload],
        );
        request["events"][0]["event_type"] = json!(event_type);
        identify_append(&opened, &mut request);
        route(&state, &session, &APPEND_DESCRIPTOR, request)
            .await
            .expect_err("unproven preparation/completion must refuse");
        assert_eq!(
            state
                .state_store
                .capture_thread_event_read_anchor(root, thread)
                .unwrap()
                .unwrap()
                .tip(),
            before.tip(),
            "refused forged operations must not advance the signed seat"
        );
    }
}

fn install_transfer_composition() {
    ryeos_app::object_contracts::install_with_event_contracts(
        ryeos_ui::transfer_store::retention::event_contracts(),
    )
    .unwrap();
    ryeos_app::object_contracts::install_retained_evidence_validator(
        ryeos_ui::transfer_store::retention::validate_retained_evidence,
    )
    .unwrap();
}

/// A second presentation in the same session: its own creation request and
/// claim intent, otherwise the same Prepare and fresh Claim.
async fn fresh_distinct(
    state: &Arc<ryeos_app::state::AppState>,
    session: &str,
    request_id: &str,
    nonce: &str,
) -> Value {
    let prepared = route(
        state,
        session,
        &ryeos_ui::handlers::ui_seat::OPEN_DESCRIPTOR,
        prepare_request(state, session, request_id),
    )
    .await
    .unwrap();
    let request = claim_request(state, session, &prepared, nonce, json!({"kind":"fresh"}))
        .await
        .unwrap();
    route(
        state,
        session,
        &ryeos_ui::handlers::ui_seat::OPEN_DESCRIPTOR,
        request,
    )
    .await
    .unwrap()
}

fn transfer_seat(
    opened: &Value,
    revision: u64,
) -> ryeos_ui_presentation_contract::transfer::TransferSeat {
    ryeos_ui_presentation_contract::transfer::TransferSeat {
        presentation_id: opened["presentation_id"].as_str().unwrap().into(),
        thread_id: opened["thread_id"].as_str().unwrap().into(),
        chain_root_id: opened["chain_root_id"].as_str().unwrap().into(),
        producer_incarnation: opened["producer_incarnation"].as_str().unwrap().into(),
        revision,
    }
}

/// The destination's inbound commitment, written where a seat's own browser
/// transfer testimony lands: after its authenticated producer frontier.
fn append_inbound(
    state: &ryeos_app::state::AppState,
    destination: &Value,
    seq: u64,
    transfer: &ryeos_ui_presentation_contract::transfer::TransferIdentity,
) {
    let record = ryeos_ui_presentation_contract::transfer::TransferRecord {
        schema: 1,
        transfer: transfer.clone(),
        phase: ryeos_ui_presentation_contract::transfer::TransferPhase::InboundCommitment,
    };
    state
        .state_store
        .append_events(
            destination["chain_root_id"].as_str().unwrap(),
            destination["thread_id"].as_str().unwrap(),
            &[ryeos_app::state_store::NewEventRecord {
                event_type: ryeos_ui_presentation_contract::transfer::TRANSFER_EVENT.into(),
                storage_class: "indexed".into(),
                payload: serde_json::to_value(
                    ryeos_ui_presentation_contract::transfer::TransferEventPayload {
                        seq,
                        operation_id: transfer.operation_id.clone(),
                        payload: record,
                    },
                )
                .unwrap(),
            }],
        )
        .unwrap();
}

fn pair_transfer(
    source: ryeos_ui_presentation_contract::transfer::TransferSeat,
    destination: ryeos_ui_presentation_contract::transfer::TransferSeat,
) -> ryeos_ui_presentation_contract::transfer::TransferIdentity {
    ryeos_ui_presentation_contract::transfer::TransferIdentity {
        operation_id: "33333333-3333-4333-8333-333333333333".into(),
        source,
        destination,
        export_digest: ryeos_ui_presentation_contract::payload_digest::seat_payload_digest(
            &json!({"mode":"copy"}),
        )
        .unwrap(),
    }
}

#[tokio::test]
async fn session_current_folds_a_transfer_bearing_seat_through_its_completed_paired_cut() {
    let (_tmp, _state_owner, state) = build_test_state_with_live_bundles();
    install_transfer_composition();
    let session = activate(&state);
    let state = Arc::new(state);
    let source = fresh_distinct(
        &state,
        &session,
        "00000000-0000-4000-8000-000000000020",
        "00000000-0000-4000-8000-000000000021",
    )
    .await;
    let destination = fresh_distinct(
        &state,
        &session,
        "00000000-0000-4000-8000-000000000030",
        "00000000-0000-4000-8000-000000000031",
    )
    .await;
    let ordinary = ryeos_ui::handlers::ui_session_current::handle(
        json!({}),
        handler_context(&session),
        state.clone(),
    )
    .await
    .unwrap();
    assert_eq!(ordinary["presentations"].as_array().unwrap().len(), 2);
    let reported = ordinary["presentations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["presentation_id"] == source["presentation_id"])
        .unwrap();
    assert_eq!(reported["thread_id"], source["thread_id"]);
    assert_eq!(reported["chain_root_id"], source["chain_root_id"]);

    let source_revision = observe(&state, &session, &source).await.unwrap().revision;
    let destination_revision = observe(&state, &session, &destination)
        .await
        .unwrap()
        .revision;
    let transfer = pair_transfer(
        transfer_seat(&source, source_revision),
        transfer_seat(&destination, destination_revision),
    );
    let next_engine_seq: u64 = destination["next_engine_seq"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    append_inbound(&state, &destination, next_engine_seq, &transfer);

    // One seat's fold cannot judge transfer testimony; only the completed
    // paired cut reads it, and observation now reads through that cut.
    let observed = observe(&state, &session, &destination).await.unwrap();
    assert_eq!(
        observed.revision, destination_revision,
        "an inbound commitment witnesses the frontier without changing composition"
    );
    let current = ryeos_ui::handlers::ui_session_current::handle(
        json!({}),
        handler_context(&session),
        state.clone(),
    )
    .await
    .unwrap();
    assert_eq!(current["presentations"], ordinary["presentations"]);
}

#[tokio::test]
async fn session_current_refuses_a_transfer_naming_a_seat_the_session_does_not_hold() {
    let (_tmp, _state_owner, state) = build_test_state_with_live_bundles();
    install_transfer_composition();
    let session = activate(&state);
    let state = Arc::new(state);
    let destination = fresh_distinct(
        &state,
        &session,
        "00000000-0000-4000-8000-000000000030",
        "00000000-0000-4000-8000-000000000031",
    )
    .await;
    let destination_revision = observe(&state, &session, &destination)
        .await
        .unwrap()
        .revision;
    let mut ghost = transfer_seat(&destination, 0);
    ghost.presentation_id = "55555555-5555-4555-8555-555555555555".into();
    ghost.thread_id = "T-ghost".into();
    ghost.chain_root_id = "T-ghost".into();
    let transfer = pair_transfer(ghost, transfer_seat(&destination, destination_revision));
    let next_engine_seq: u64 = destination["next_engine_seq"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    append_inbound(&state, &destination, next_engine_seq, &transfer);
    ryeos_ui::handlers::ui_session_current::handle(
        json!({}),
        handler_context(&session),
        state.clone(),
    )
    .await
    .expect_err("a counterpart with neither a signed head nor a fence has no authenticated cut");
}

#[tokio::test]
async fn session_current_refuses_a_transfer_whose_witness_is_stale() {
    let (_tmp, _state_owner, state) = build_test_state_with_live_bundles();
    install_transfer_composition();
    let session = activate(&state);
    let state = Arc::new(state);
    let source = fresh_distinct(
        &state,
        &session,
        "00000000-0000-4000-8000-000000000020",
        "00000000-0000-4000-8000-000000000021",
    )
    .await;
    let destination = fresh_distinct(
        &state,
        &session,
        "00000000-0000-4000-8000-000000000030",
        "00000000-0000-4000-8000-000000000031",
    )
    .await;
    let transfer = pair_transfer(transfer_seat(&source, 0), transfer_seat(&destination, 0));
    // The witness names a producer sequence the seat has not reached.
    append_inbound(&state, &destination, 7, &transfer);
    ryeos_ui::handlers::ui_session_current::handle(
        json!({}),
        handler_context(&session),
        state.clone(),
    )
    .await
    .expect_err("a transfer witness must equal the authenticated producer frontier");
}

#[tokio::test]
async fn a_transfer_bearing_seat_appends_and_relinquishes_through_its_paired_cut() {
    use ryeos_ui::handlers::ui_seat::{APPEND_DESCRIPTOR, CLOSE_DESCRIPTOR};
    let (_tmp, _state_owner, state) = build_test_state_with_live_bundles();
    install_transfer_composition();
    let session = activate(&state);
    let state = Arc::new(state);
    let source = fresh_distinct(
        &state,
        &session,
        "00000000-0000-4000-8000-000000000020",
        "00000000-0000-4000-8000-000000000021",
    )
    .await;
    let destination = fresh_distinct(
        &state,
        &session,
        "00000000-0000-4000-8000-000000000030",
        "00000000-0000-4000-8000-000000000031",
    )
    .await;
    let source_revision = observe(&state, &session, &source).await.unwrap().revision;
    let destination_revision = observe(&state, &session, &destination)
        .await
        .unwrap()
        .revision;
    let transfer = pair_transfer(
        transfer_seat(&source, source_revision),
        transfer_seat(&destination, destination_revision),
    );
    let next_engine_seq: u64 = destination["next_engine_seq"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    append_inbound(&state, &destination, next_engine_seq, &transfer);

    // The inbound commitment witnesses the frontier without consuming a
    // producer sequence, so the destination's own next edit still lands at
    // the sequence it was claimed at. Before the paired cut owned admission
    // this append was refused outright.
    let ack = route(
        &state,
        &session,
        &APPEND_DESCRIPTOR,
        append_request(
            &destination,
            "",
            next_engine_seq,
            &[json!({"key": "after.inbound", "value": 1})],
        ),
    )
    .await
    .unwrap();
    assert_eq!(ack["appended"], 1);

    // The control writer reads through the same cut.
    let relinquish = transition_request(
        &state,
        &session,
        &destination,
        "00000000-0000-4000-8000-000000000032",
        "relinquish",
    )
    .await;
    let released = route(&state, &session, &CLOSE_DESCRIPTOR, relinquish)
        .await
        .unwrap();
    assert_eq!(released["state"], "relinquished");
    assert_eq!(
        serde_json::to_value(observe(&state, &session, &destination).await.unwrap()).unwrap()["producer"],
        json!({"kind": "relinquished", "producer_incarnation": destination["producer_incarnation"]})
    );
}

/// Publish the destination InboundCommitment and the source Prepared event
/// through the typed pair writer, from completed reads of both seats. The
/// export is the writer's own closed input; the browser ingress would first
/// bind it to a mounted source composition.
fn prepare_pair_directly(
    state: &Arc<ryeos_app::state::AppState>,
    source: (&str, &Value),
    destination: (&str, &Value),
    transfer: &ryeos_ui_presentation_contract::transfer::TransferIdentity,
    export: &Value,
) -> anyhow::Result<ryeos_app::state_store::CutPairAppendResult> {
    use ryeos_ui::transfer_store::appends::{
        PreparationInputs, TransferCutDependencies, TransferSeatContext, TransferSeatPair,
        append_transfer_preparation_pair,
    };
    use ryeos_ui::transfer_store::reader::{
        begin_fixed_seat_read, finish_fixed_seat_read, read_fixed_seat_page,
    };

    let policy = state
        .node_policy
        .require::<ryeos_node_runtime::node_policy::sections::ui_browser_sessions::UiBrowserSessionPolicy>()?;
    let max_append_bytes = state
        .node_config
        .routes
        .iter()
        .find(|route| {
            route.response.source.as_deref()
                == Some(ryeos_ui::handlers::ui_seat::APPEND_DESCRIPTOR.service_ref)
        })
        .unwrap()
        .limits
        .body_bytes_max;
    let anchor_of = |opened: &Value| {
        state
            .state_store
            .capture_thread_event_read_anchor(
                opened["chain_root_id"].as_str().unwrap(),
                opened["thread_id"].as_str().unwrap(),
            )
            .unwrap()
            .unwrap()
    };
    let detail_of = |opened: &Value| {
        state
            .state_store
            .get_thread(opened["thread_id"].as_str().unwrap())
            .unwrap()
            .unwrap()
    };
    let (source_anchor, destination_anchor) = (anchor_of(source.1), anchor_of(destination.1));
    let (source_detail, destination_detail) = (detail_of(source.1), detail_of(destination.1));
    let (source_owner, destination_owner) = (
        format!("session:{}", source.0),
        format!("session:{}", destination.0),
    );
    fn context<'a>(
        anchor: &'a ryeos_state::AuthoritativeThreadEventReadAnchor,
        owner: &'a str,
        detail: &'a ryeos_app::state_store::ThreadDetail,
        opened: &'a Value,
        max_append_bytes: u64,
    ) -> TransferSeatContext<'a> {
        TransferSeatContext {
            anchor,
            owner,
            surface: &detail.item_ref,
            client_ref: &detail.executor_ref,
            presentation_id: opened["presentation_id"].as_str().unwrap(),
            producer_incarnation: opened["producer_incarnation"].as_str().unwrap(),
            revision: 0,
            next_engine_seq: 0,
            max_append_bytes,
        }
    }
    let source_context = context(
        &source_anchor,
        &source_owner,
        &source_detail,
        source.1,
        max_append_bytes,
    );
    let destination_context = context(
        &destination_anchor,
        &destination_owner,
        &destination_detail,
        destination.1,
        max_append_bytes,
    );
    let completed = |seat: &ryeos_ui_presentation_contract::transfer::TransferSeat, anchor| {
        let mut read = begin_fixed_seat_read(
            state.state_store.clone(),
            anchor,
            seat,
            std::slice::from_ref(transfer),
        )
        .unwrap();
        while !read_fixed_seat_page(&mut read, 64, 2 * 1024 * 1024).unwrap() {}
        finish_fixed_seat_read(read).unwrap()
    };
    let source_proof = completed(&transfer.source, source_anchor.clone());
    let destination_proof = completed(&transfer.destination, destination_anchor.clone());
    append_transfer_preparation_pair(
        &state.state_store,
        &policy.seat.kind,
        &PreparationInputs {
            transfer,
            export,
            source_proof: &source_proof,
            destination_proof: &destination_proof,
        },
        TransferSeatPair {
            target: &destination_context,
            counterpart: &source_context,
        },
        TransferCutDependencies {
            current_anchors: &[],
            retired: &[],
        },
        || Ok(true),
    )
}

#[tokio::test]
async fn signed_transfer_preparation_does_not_consume_a_producer_sequence() {
    use ryeos_ui::handlers::ui_seat::APPEND_DESCRIPTOR;

    let (_tmp, _state_owner, state) = build_test_state_with_live_bundles();
    install_transfer_composition();
    let session = activate(&state);
    let state = Arc::new(state);
    let (source, _, destination, _) = two_seats(&state, &session).await;
    let export = set_export(
        &state,
        &session,
        &source,
        ryeos_client_base::ui::transfer_export::UiTransferExportMode::Copy,
    );
    let transfer = export_transfer(&source, 0, &destination, 0, &export);
    let outcome = prepare_pair_directly(
        &state,
        (&session, &source),
        (&session, &destination),
        &transfer,
        &export,
    )
    .unwrap();
    assert!(
        matches!(
            outcome,
            ryeos_app::state_store::CutPairAppendResult::Committed { .. }
        ),
        "{outcome:?}"
    );

    // The source's first producer sequence is still free after Prepared.
    let ack = route(
        &state,
        &session,
        &APPEND_DESCRIPTOR,
        append_request(
            &source,
            "",
            0,
            &[json!({"key": "after.transfer", "value": true})],
        ),
    )
    .await
    .unwrap();
    assert_eq!(ack["first_engine_seq"], "0");
    assert_eq!(ack["appended"], 1);

    // A second preparation of the same operation is never a second pair: the
    // completed reads now hold the first one's phases.
    prepare_pair_directly(
        &state,
        (&session, &source),
        (&session, &destination),
        &transfer,
        &export,
    )
    .expect_err("a preparation repeats an existing source phase");
}

/// The compiled route shape the stream source accepts, without the signed
/// bundle document that deploys it.
fn history_route() -> ryeos_app::route_raw::RawRouteSpec {
    serde_yaml::from_str(
        "id: ui/api/session/seat/history\npath: /ui/api/session/seat/history\nmethods: [POST]\nauth: browser_session\nlimits: { body_bytes_max: 16384, timeout_ms: 0, concurrent_max: 8 }\nrequest: { body: json }\nresponse: { mode: event_stream, source: ui_seat_history }\n",
    )
    .unwrap()
}

async fn open_history_stream(
    state: &Arc<ryeos_app::state::AppState>,
    session_id: &str,
    presentation_id: &str,
    chain_root_id: &str,
) -> anyhow::Result<ryeos_api::routes::invocation::RouteEventStream> {
    use ryeos_api::routes::invocation::{
        CompiledRouteInvocation, RouteInvocationContext, RouteInvocationResult,
    };
    use ryeos_api::routes::response_modes::event_stream_mode::{
        EventStreamStrategy, StreamSourceCompiler,
    };

    let raw = history_route();
    let EventStreamStrategy::BodyJson { invoker } =
        ryeos_ui::invokers::seat_history_invocation::SeatHistorySourceFactory.compile(&raw)?
    else {
        anyhow::bail!("seat history route did not compile to POST JSON");
    };
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("cookie", format!("ryeos_session={session_id}").parse()?);
    let input = json!({"presentation_id": presentation_id, "chain_root_id": chain_root_id});
    let context = |headers, principal| -> anyhow::Result<RouteInvocationContext> {
        Ok(RouteInvocationContext {
            route_id: raw.id.clone().into(),
            method: axum::http::Method::POST,
            uri: raw.path.parse()?,
            captures: Default::default(),
            headers,
            body_raw: Vec::new(),
            input: input.clone(),
            principal,
            workspace_lifeline: None,
            launch_timings: None,
            publication: None,
            state: state.as_ref().clone(),
            webhook_dedupe: Arc::new(Default::default()),
        })
    };
    let auth = ryeos_ui::invokers::browser_session_invocation::CompiledBrowserSessionVerifier {
        ui: get_ui_state(state).unwrap().clone(),
    }
    .invoke(context(headers, None)?)
    .await?;
    let RouteInvocationResult::Principal(principal) = auth else {
        anyhow::bail!("browser session route did not authenticate");
    };
    let RouteInvocationResult::Stream(stream) = invoker
        .invoke(context(Default::default(), Some(principal))?)
        .await?
    else {
        anyhow::bail!("seat history route did not return a stream");
    };
    Ok(stream)
}

async fn collect_history_until_fence_or_eof(
    mut stream: ryeos_api::routes::invocation::RouteEventStream,
) -> anyhow::Result<Vec<ryeos_app::stream_envelope::RouteStreamEnvelope>> {
    use tokio_stream::StreamExt;
    let mut frames = Vec::new();
    loop {
        let Some(frame) =
            tokio::time::timeout(std::time::Duration::from_secs(5), stream.events.next()).await?
        else {
            break;
        };
        let frame = frame.map_err(|never| match never {})?;
        let completed = frame.event_type == "ui_seat.replay.complete.v1";
        frames.push(frame);
        if completed {
            break;
        }
        if frames.len() >= 64 {
            anyhow::bail!("seat history exceeded fixture frame bound before completion or EOF");
        }
    }
    Ok(frames)
}

/// The stream's tasks hold clones of the state; a test must not end while one
/// still does, or the host would see a live store at teardown.
async fn wait_for_stream_release(state: &Arc<ryeos_app::state::AppState>, baseline: usize) {
    for _ in 0..200 {
        if Arc::strong_count(&state.state_store) <= baseline {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("the seat history stream never released the state it held");
}

async fn history_frames_until_fence_or_eof(
    state: &Arc<ryeos_app::state::AppState>,
    session_id: &str,
    presentation_id: &str,
    chain_root_id: &str,
) -> anyhow::Result<Vec<ryeos_app::stream_envelope::RouteStreamEnvelope>> {
    let baseline = Arc::strong_count(&state.state_store);
    let frames = collect_history_until_fence_or_eof(
        open_history_stream(state, session_id, presentation_id, chain_root_id).await?,
    )
    .await;
    wait_for_stream_release(state, baseline).await;
    frames
}

#[tokio::test]
async fn signed_fixed_anchor_history_completes_for_current_seat() {
    let (_tmp, _state_owner, state) = build_test_state_with_live_bundles();
    let session = activate(&state);
    let state = Arc::new(state);
    let opened = fresh(&state, &session).await;
    let frames = history_frames_until_fence_or_eof(
        &state,
        &session,
        opened["presentation_id"].as_str().unwrap(),
        opened["chain_root_id"].as_str().unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        frames.first().unwrap().event_type,
        "ui_seat.replay.begin.v1"
    );
    assert_eq!(
        frames.last().unwrap().event_type,
        "ui_seat.replay.complete.v1",
        "a valid signed seat must reach the fixed-anchor completion fence"
    );
}

#[tokio::test]
async fn fixed_anchor_stream_rechecks_attachment_and_tip_before_completion() {
    use ryeos_ui::handlers::ui_seat::APPEND_DESCRIPTOR;
    use tokio_stream::StreamExt;

    for revoke_root in [true, false] {
        let (_tmp, _state_owner, state) = build_test_state_with_live_bundles();
        let session = activate(&state);
        let state = Arc::new(state);
        let opened = fresh(&state, &session).await;
        let presentation_id = opened["presentation_id"].as_str().unwrap().to_owned();
        let chain_root_id = opened["chain_root_id"].as_str().unwrap().to_owned();
        // Four fully receipted intervals cannot fit alongside completion in
        // the route's two-frame channel. After reading only Begin, the source
        // cannot have published Complete, regardless of task scheduling.
        for batch in 0..4_u64 {
            let payloads = (0..32_u64)
                .map(|offset| json!({"key": format!("stream.fixture.{}", batch * 32 + offset), "value": offset}))
                .collect::<Vec<_>>();
            route(
                &state,
                &session,
                &APPEND_DESCRIPTOR,
                append_request(&opened, "", batch * 32, &payloads),
            )
            .await
            .unwrap();
        }
        let valid =
            history_frames_until_fence_or_eof(&state, &session, &presentation_id, &chain_root_id)
                .await
                .unwrap();
        assert_eq!(
            valid.last().unwrap().event_type,
            "ui_seat.replay.complete.v1",
            "the unmodified multi-frame history must independently complete"
        );

        let baseline = Arc::strong_count(&state.state_store);
        let mut stream = open_history_stream(&state, &session, &presentation_id, &chain_root_id)
            .await
            .unwrap();
        let begin = tokio::time::timeout(std::time::Duration::from_secs(5), stream.events.next())
            .await
            .unwrap()
            .expect("Begin before mutation")
            .unwrap();
        assert_eq!(begin.event_type, "ui_seat.replay.begin.v1");
        if revoke_root {
            let ui = get_ui_state(&state).unwrap();
            let held = ui.browser_sessions.get_session(&session).unwrap();
            let root = held.attachments[&held.root_attachment_id].coordinate();
            ui.browser_sessions
                .revoke_attachment(&session, &root)
                .unwrap();
        } else {
            // The anchor was captured before Begin. A subsequent admitted
            // append must invalidate this provisional cut, not be silently
            // omitted while its old anchor is advertised as current.
            route(
                &state,
                &session,
                &APPEND_DESCRIPTOR,
                append_request(
                    &opened,
                    "",
                    128,
                    &[json!({"key": "stream.fixture.late", "value": true})],
                ),
            )
            .await
            .unwrap();
        }
        let remaining = collect_history_until_fence_or_eof(stream).await.unwrap();
        wait_for_stream_release(&state, baseline).await;
        assert!(
            remaining
                .iter()
                .all(|frame| frame.event_type != "ui_seat.replay.complete.v1"),
            "revoked attachment or advanced signed tip must end without Complete (revoke_root={revoke_root})"
        );
    }
}

#[tokio::test]
async fn signed_orphan_producer_event_does_not_complete_fixed_anchor_history() {
    let (_tmp, _state_owner, state) = build_test_state_with_live_bundles();
    let session = activate(&state);
    let state = Arc::new(state);
    let opened = fresh(&state, &session).await;
    state
        .state_store
        .append_events(
            opened["chain_root_id"].as_str().unwrap(),
            opened["thread_id"].as_str().unwrap(),
            &[ryeos_app::state_store::NewEventRecord {
                event_type: "seat.facet".into(),
                storage_class: "indexed".into(),
                payload: json!({"seq": 0, "operation_id": "orphan", "payload": {"key": "selection", "value": "unsettled"}}),
            }],
        )
        .expect("signed orphan fixture commits one event");
    let frames = history_frames_until_fence_or_eof(
        &state,
        &session,
        opened["presentation_id"].as_str().unwrap(),
        opened["chain_root_id"].as_str().unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        frames.first().unwrap().event_type,
        "ui_seat.replay.begin.v1"
    );
    assert!(
        frames
            .iter()
            .all(|frame| frame.event_type != "ui_seat.replay.complete.v1"),
        "orphan producer testimony must end without a completion fence"
    );
}

#[tokio::test]
async fn a_transfer_bearing_seat_streams_its_proof_frames_before_completing() {
    let (_tmp, _state_owner, state) = build_test_state_with_live_bundles();
    install_transfer_composition();
    let session = activate(&state);
    let state = Arc::new(state);
    let source = fresh_distinct(
        &state,
        &session,
        "00000000-0000-4000-8000-000000000020",
        "00000000-0000-4000-8000-000000000021",
    )
    .await;
    let destination = fresh_distinct(
        &state,
        &session,
        "00000000-0000-4000-8000-000000000030",
        "00000000-0000-4000-8000-000000000031",
    )
    .await;
    let source_revision = observe(&state, &session, &source).await.unwrap().revision;
    let destination_revision = observe(&state, &session, &destination)
        .await
        .unwrap()
        .revision;
    let transfer = pair_transfer(
        transfer_seat(&source, source_revision),
        transfer_seat(&destination, destination_revision),
    );
    let next_engine_seq: u64 = destination["next_engine_seq"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    append_inbound(&state, &destination, next_engine_seq, &transfer);
    let frames = history_frames_until_fence_or_eof(
        &state,
        &session,
        destination["presentation_id"].as_str().unwrap(),
        destination["chain_root_id"].as_str().unwrap(),
    )
    .await
    .unwrap();
    let kinds = frames
        .iter()
        .map(|frame| frame.event_type.as_str())
        .collect::<Vec<_>>();
    assert_eq!(kinds.first(), Some(&"ui_seat.replay.begin.v1"));
    assert_eq!(kinds.last(), Some(&"ui_seat.replay.complete.v1"));
    let proof = kinds
        .iter()
        .position(|kind| *kind == "ui_seat.replay.transfer_proof.v1")
        .expect("the destination's inbound commitment is disclosed as a proof frame");
    let first_events = kinds
        .iter()
        .position(|kind| *kind == "ui_seat.replay.events.v1")
        .expect("the seat's own history is replayed");
    assert!(proof < first_events, "proofs precede every canonical row");
}

/// The transfer service is deployed by a signed service document that is not
/// part of this build's bundle yet, so the handler runs directly under the
/// session's context rather than through the compiled service route.
async fn transfer_route(
    state: &Arc<ryeos_app::state::AppState>,
    session: &str,
    input: Value,
) -> anyhow::Result<Value> {
    (ryeos_ui::handlers::ui_seat::TRANSFER_DESCRIPTOR.handler)(
        input,
        handler_context(session),
        state.clone(),
    )
    .await
}

/// A closed view-set export holding one empty set owned by a Surface the
/// source never mounted. It decodes and validates as an export, which is all
/// the typed pair writer and the paired readers ask of it; binding it to a
/// mounted source composition is the browser ingress's own, later check.
fn set_export(
    state: &Arc<ryeos_app::state::AppState>,
    session: &str,
    source: &Value,
    mode: ryeos_client_base::ui::transfer_export::UiTransferExportMode,
) -> Value {
    use ryeos_client_base::ids::{PresentationId, SurfaceInstanceId, UiViewMountOwner, ViewSetId};
    use ryeos_client_base::ui::binding::UiBindingAttachmentCoordinate;
    use ryeos_client_base::ui::transfer_export::{
        UI_TRANSFER_EXPORT_SCHEMA, UiTransferExport, UiTransferExportContent, UiTransferSetExport,
    };
    let held = get_ui_state(state)
        .unwrap()
        .browser_sessions
        .get_session(session)
        .unwrap();
    let root = held.attachments[&held.root_attachment_id].coordinate();
    let export = UiTransferExport {
        schema: UI_TRANSFER_EXPORT_SCHEMA,
        mode,
        content: UiTransferExportContent::ViewSets {
            active_set: Some("empty".into()),
            sets: vec![UiTransferSetExport {
                handle: "empty".into(),
                source_view_set_id: ViewSetId::new(1),
                source_owner: UiViewMountOwner::Surface {
                    presentation_id: PresentationId::try_new(
                        source["presentation_id"].as_str().unwrap(),
                    )
                    .unwrap(),
                    surface_instance_id: SurfaceInstanceId::try_new(
                        "99999999-9999-4999-8999-999999999999",
                    )
                    .unwrap(),
                },
                binding: UiBindingAttachmentCoordinate {
                    binding_attachment_id: root.binding_attachment_id,
                    binding_generation: root.binding_generation,
                    binding_digest: root.binding_digest,
                },
                seed: ryeos_client_base::surface::view_sets::ViewSetSeedSpec {
                    schema: 2,
                    id: "empty".into(),
                    title: "Empty".into(),
                    root: None,
                },
                focused_view: None,
                mounts: vec![],
            }],
        },
    };
    serde_json::to_value(export).unwrap()
}

fn empty_export(mode: &str) -> Value {
    json!({"schema": 1, "mode": mode,
        "content": {"kind": "view_sets", "active_set": null, "sets": []}})
}

fn export_transfer(
    source: &Value,
    source_revision: u64,
    destination: &Value,
    destination_revision: u64,
    export: &Value,
) -> ryeos_ui_presentation_contract::transfer::TransferIdentity {
    ryeos_ui_presentation_contract::transfer::TransferIdentity {
        operation_id: "33333333-3333-4333-8333-333333333333".into(),
        source: transfer_seat(source, source_revision),
        destination: transfer_seat(destination, destination_revision),
        export_digest: ryeos_ui_presentation_contract::payload_digest::seat_payload_digest(export)
            .unwrap(),
    }
}

async fn two_seats(
    state: &Arc<ryeos_app::state::AppState>,
    session: &str,
) -> (Value, u64, Value, u64) {
    let source = fresh_distinct(
        state,
        session,
        "00000000-0000-4000-8000-000000000020",
        "00000000-0000-4000-8000-000000000021",
    )
    .await;
    let destination = fresh_distinct(
        state,
        session,
        "00000000-0000-4000-8000-000000000030",
        "00000000-0000-4000-8000-000000000031",
    )
    .await;
    let source_revision = observe(state, session, &source).await.unwrap().revision;
    let destination_revision = observe(state, session, &destination)
        .await
        .unwrap()
        .revision;
    (source, source_revision, destination, destination_revision)
}

#[tokio::test]
async fn transfer_requests_refuse_unheld_seats_old_selectors_and_a_single_seat() {
    let (_tmp, _state_owner, state) = build_test_state_with_live_bundles();
    install_transfer_composition();
    let session = activate(&state);
    let foreign = activate(&state);
    let state = Arc::new(state);
    let (source, source_revision, destination, destination_revision) =
        two_seats(&state, &session).await;
    let anchor_of = |opened: &Value| {
        state
            .state_store
            .capture_thread_event_read_anchor(
                opened["chain_root_id"].as_str().unwrap(),
                opened["thread_id"].as_str().unwrap(),
            )
            .unwrap()
            .unwrap()
    };
    let (before_source, before_destination) = (anchor_of(&source), anchor_of(&destination));
    let export = empty_export("copy");
    let transfer = export_transfer(
        &source,
        source_revision,
        &destination,
        destination_revision,
        &export,
    );

    // A session that holds neither seat has no standing to move anything.
    let error = transfer_route(
        &state,
        &foreign,
        json!({"action": "prepare", "transfer": transfer, "export": export, "freeze": null}),
    )
    .await
    .expect_err("a foreign session holds neither transfer seat");
    assert!(
        format!("{error:#}").to_lowercase().contains("presentation")
            || format!("{error:#}").to_lowercase().contains("forbidden"),
        "{error:#}"
    );

    // Predecessor lifecycle selectors and unknown actions are not requests.
    for action in ["settle", "reap", "transfer_lifecycle_v2"] {
        transfer_route(
            &state,
            &session,
            json!({"action": action, "transfer": transfer}),
        )
        .await
        .expect_err("an old selector is not a transfer action");
    }

    // One seat cannot be both ends.
    let mut single = transfer.clone();
    single.destination = single.source.clone();
    transfer_route(
        &state,
        &session,
        json!({"action": "prepare", "transfer": single, "export": export, "freeze": null}),
    )
    .await
    .expect_err("a transfer needs two distinct seats");

    // Neither end advanced: nothing was written by any refusal.
    for (opened, before) in [
        (&source, &before_source),
        (&destination, &before_destination),
    ] {
        let anchor = state
            .state_store
            .capture_thread_event_read_anchor(
                opened["chain_root_id"].as_str().unwrap(),
                opened["thread_id"].as_str().unwrap(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(&anchor, before);
    }
}

#[tokio::test]
async fn a_move_without_its_receipted_freeze_and_a_copy_with_one_are_both_refused() {
    let (_tmp, _state_owner, state) = build_test_state_with_live_bundles();
    install_transfer_composition();
    let session = activate(&state);
    let state = Arc::new(state);
    let (source, source_revision, destination, destination_revision) =
        two_seats(&state, &session).await;
    let before = state
        .state_store
        .capture_thread_event_read_anchor(
            source["chain_root_id"].as_str().unwrap(),
            source["thread_id"].as_str().unwrap(),
        )
        .unwrap()
        .unwrap();

    let move_export = empty_export("move");
    let moved = export_transfer(
        &source,
        source_revision,
        &destination,
        destination_revision,
        &move_export,
    );
    let error = transfer_route(
        &state,
        &session,
        json!({"action": "prepare", "transfer": moved, "export": move_export, "freeze": null}),
    )
    .await
    .expect_err("a MOVE is admitted only through the receipted typed Core freeze");
    assert!(format!("{error:#}").contains("freeze"), "{error:#}");

    let copy_export = empty_export("copy");
    let copied = export_transfer(
        &source,
        source_revision,
        &destination,
        destination_revision,
        &copy_export,
    );
    let links = json!({
        "preparation": {"event_hash": "a".repeat(64), "chain_seq": 4},
        "completion": {"event_hash": "b".repeat(64), "chain_seq": 5},
    });
    transfer_route(
        &state,
        &session,
        json!({"action": "prepare", "transfer": copied, "export": copy_export, "freeze": links}),
    )
    .await
    .expect_err("a COPY cannot carry MOVE freeze links");

    assert_eq!(
        state
            .state_store
            .capture_thread_event_read_anchor(
                source["chain_root_id"].as_str().unwrap(),
                source["thread_id"].as_str().unwrap(),
            )
            .unwrap()
            .unwrap(),
        before,
        "a refused preparation writes nothing"
    );
}

#[tokio::test]
async fn an_export_that_snapshots_nothing_is_refused_before_any_write() {
    let (_tmp, _state_owner, state) = build_test_state_with_live_bundles();
    install_transfer_composition();
    let session = activate(&state);
    let state = Arc::new(state);
    let (source, source_revision, destination, destination_revision) =
        two_seats(&state, &session).await;
    let export = empty_export("copy");
    let transfer = export_transfer(
        &source,
        source_revision,
        &destination,
        destination_revision,
        &export,
    );
    let anchor_of = |opened: &Value| {
        state
            .state_store
            .capture_thread_event_read_anchor(
                opened["chain_root_id"].as_str().unwrap(),
                opened["thread_id"].as_str().unwrap(),
            )
            .unwrap()
            .unwrap()
    };
    let (before_source, before_destination) = (anchor_of(&source), anchor_of(&destination));
    let error = transfer_route(
        &state,
        &session,
        json!({"action": "prepare", "transfer": transfer, "export": export, "freeze": null}),
    )
    .await
    .expect_err("an empty export is not a snapshot of the source composition");
    assert!(
        format!("{error:#}").contains("inventory is empty"),
        "{error:#}"
    );
    assert_eq!(anchor_of(&source), before_source);
    assert_eq!(anchor_of(&destination), before_destination);
}

#[tokio::test]
async fn a_prepared_pair_is_read_through_its_cut_by_every_seat_service() {
    let (_tmp, _state_owner, state) = build_test_state_with_live_bundles();
    install_transfer_composition();
    let session = activate(&state);
    let state = Arc::new(state);
    let (source, _, destination, _) = two_seats(&state, &session).await;
    let export = set_export(
        &state,
        &session,
        &source,
        ryeos_client_base::ui::transfer_export::UiTransferExportMode::Copy,
    );
    let transfer = export_transfer(&source, 0, &destination, 0, &export);
    let outcome = prepare_pair_directly(
        &state,
        (&session, &source),
        (&session, &destination),
        &transfer,
        &export,
    )
    .unwrap();
    assert!(
        matches!(
            outcome,
            ryeos_app::state_store::CutPairAppendResult::Committed { .. }
        ),
        "{outcome:?}"
    );

    // The session bootstraps through the completed pair, the destination
    // observes and appends, and the source streams its proof before completing.
    let current = ryeos_ui::handlers::ui_session_current::handle(
        json!({}),
        handler_context(&session),
        state.clone(),
    )
    .await
    .unwrap();
    assert_eq!(current["presentations"].as_array().unwrap().len(), 2);
    observe(&state, &session, &destination).await.unwrap();
    let destination_seq: u64 = destination["next_engine_seq"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    route(
        &state,
        &session,
        &ryeos_ui::handlers::ui_seat::APPEND_DESCRIPTOR,
        append_request(
            &destination,
            "",
            destination_seq,
            &[json!({"key": "after.prepared", "value": true})],
        ),
    )
    .await
    .unwrap();
    let frames = history_frames_until_fence_or_eof(
        &state,
        &session,
        source["presentation_id"].as_str().unwrap(),
        source["chain_root_id"].as_str().unwrap(),
    )
    .await
    .unwrap();
    let kinds = frames
        .iter()
        .map(|frame| frame.event_type.as_str())
        .collect::<Vec<_>>();
    assert!(
        kinds.contains(&"ui_seat.replay.transfer_proof.v1"),
        "{kinds:?}"
    );
    assert_eq!(kinds.last(), Some(&"ui_seat.replay.complete.v1"));
}
