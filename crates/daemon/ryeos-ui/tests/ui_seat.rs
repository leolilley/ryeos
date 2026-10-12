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

    let route_file = ryeos_engine::test_support::workspace_root()
        .join("bundles/ryeos-ui/.ai/node/routes/ui/session/seat/history.yaml");
    let raw: ryeos_app::route_raw::RawRouteSpec =
        serde_yaml::from_str(&std::fs::read_to_string(route_file)?)?;
    let EventStreamStrategy::BodyJson { invoker } =
        ryeos_ui::invokers::seat_history_invocation::SeatHistorySourceFactory.compile(&raw)?
    else {
        anyhow::bail!("seat history route did not compile to POST JSON");
    };
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("cookie", format!("ryeos_session={session_id}").parse()?);
    let input = json!({"presentation_id":presentation_id,"chain_root_id":chain_root_id});
    let auth = ryeos_ui::invokers::browser_session_invocation::CompiledBrowserSessionVerifier {
        ui: get_ui_state(state).unwrap().clone(),
    }
    .invoke(RouteInvocationContext {
        route_id: raw.id.clone().into(),
        method: axum::http::Method::POST,
        uri: raw.path.parse()?,
        captures: Default::default(),
        headers,
        body_raw: Vec::new(),
        input: input.clone(),
        principal: None,
        workspace_lifeline: None,
        launch_timings: None,
        state: state.as_ref().clone(),
        webhook_dedupe: Arc::new(Default::default()),
    })
    .await?;
    let RouteInvocationResult::Principal(principal) = auth else {
        anyhow::bail!("browser session route did not authenticate");
    };
    let RouteInvocationResult::Stream(stream) = invoker
        .invoke(RouteInvocationContext {
            route_id: raw.id.into(),
            method: axum::http::Method::POST,
            uri: raw.path.parse()?,
            captures: Default::default(),
            headers: Default::default(),
            body_raw: Vec::new(),
            input,
            principal: Some(principal),
            workspace_lifeline: None,
            launch_timings: None,
            state: state.as_ref().clone(),
            webhook_dedupe: Arc::new(Default::default()),
        })
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

async fn history_frames_until_fence_or_eof(
    state: &Arc<ryeos_app::state::AppState>,
    session_id: &str,
    presentation_id: &str,
    chain_root_id: &str,
) -> anyhow::Result<Vec<ryeos_app::stream_envelope::RouteStreamEnvelope>> {
    collect_history_until_fence_or_eof(
        open_history_stream(state, session_id, presentation_id, chain_root_id).await?,
    )
    .await
}

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
    let mut context = launch_context(
        "config:test/presentation",
        None,
        ryeos_ui::compiled_binding::EffectiveUiPosture::ObservationOnly,
        principal,
    );
    let binding = &mut Arc::make_mut(&mut context.compiled_binding).binding;
    let identity = binding.root.identity().clone();
    binding.root =
        ryeos_api::surface_views::UiCompositionRootIdentity::SharedPresentation { identity };
    context.presentation = ryeos_client_base::presentation::UiCompositionPresentation {
        definition: ryeos_client_base::presentation::UiCompositionDefinition::SharedPresentation {
            config: serde_yaml::from_str(body).unwrap(),
        },
        views: Default::default(),
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
    let (_tmp, state) = build_test_state_with_live_bundles();
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
    let (_tmp, state) = build_test_state_with_live_bundles();
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
    let (_tmp, state) = build_test_state_with_live_bundles();
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
#[ignore = "requires populated current signed bundles and the development publisher fixture"]
async fn signed_transfer_preparation_does_not_consume_a_producer_sequence() {
    use ryeos_app::state_store::UiTransferPairSeat;
    use ryeos_state::state_db::ConditionalThreadEventPairAppendResult;
    use ryeos_ui_presentation_contract::transfer::{TransferIdentity, TransferSeat};

    let (_tmp, state) = build_test_state_with_live_bundles();
    let (source_session, _, source_producer, _) =
        test_state::live_presentation_with_surface(&state, "surface:ryeos/ui/assistant-observe")
            .await;
    let (destination_session, _, destination_producer, _) =
        test_state::live_presentation_with_surface(&state, "surface:ryeos/ui/assistant-observe")
            .await;
    let state = Arc::new(state);
    let source_detail = state
        .state_store
        .get_thread(&source_producer.thread_id)
        .unwrap()
        .unwrap();
    let destination_detail = state
        .state_store
        .get_thread(&destination_producer.thread_id)
        .unwrap()
        .unwrap();
    let source_anchor = state
        .state_store
        .capture_thread_event_read_anchor(&source_detail.chain_root_id, &source_detail.thread_id)
        .unwrap()
        .unwrap();
    let destination_anchor = state
        .state_store
        .capture_thread_event_read_anchor(
            &destination_detail.chain_root_id,
            &destination_detail.thread_id,
        )
        .unwrap()
        .unwrap();
    let export = json!({});
    let transfer = TransferIdentity {
        operation_id: uuid::Uuid::new_v4().to_string(),
        source: TransferSeat {
            presentation_id: source_producer.presentation_id.clone(),
            thread_id: source_detail.thread_id.clone(),
            chain_root_id: source_detail.chain_root_id.clone(),
            producer_incarnation: source_producer.producer_incarnation.clone(),
            revision: 0,
        },
        destination: TransferSeat {
            presentation_id: destination_producer.presentation_id.clone(),
            thread_id: destination_detail.thread_id.clone(),
            chain_root_id: destination_detail.chain_root_id.clone(),
            producer_incarnation: destination_producer.producer_incarnation.clone(),
            revision: 0,
        },
        export_digest: ryeos_ui_presentation_contract::payload_digest::seat_payload_digest(&export)
            .unwrap(),
    };
    let source_owner = format!("session:{source_session}");
    let destination_owner = format!("session:{destination_session}");
    let current_append_bound = state
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
    // Both presentations were initialized just above from this exact current
    // route generation; no later route/policy publication occurs in the test.
    let source = UiTransferPairSeat {
        anchor: &source_anchor,
        owner: &source_owner,
        surface: &source_detail.item_ref,
        client_ref: &source_detail.executor_ref,
        presentation_id: &transfer.source.presentation_id,
        producer_incarnation: &transfer.source.producer_incarnation,
        revision: 0,
        next_engine_seq: 0,
        max_append_bytes: current_append_bound,
    };
    let destination = UiTransferPairSeat {
        anchor: &destination_anchor,
        owner: &destination_owner,
        surface: &destination_detail.item_ref,
        client_ref: &destination_detail.executor_ref,
        presentation_id: &transfer.destination.presentation_id,
        producer_incarnation: &transfer.destination.producer_incarnation,
        revision: 0,
        next_engine_seq: 0,
        max_append_bytes: current_append_bound,
    };
    let completed_cut =
        |seat: &ryeos_ui_presentation_contract::transfer::TransferSeat,
         anchor: ryeos_state::AuthoritativeThreadEventReadAnchor| {
            use ryeos_app::thread_lifecycle::presentation_transfer::{
                begin_fixed_seat_read, finish_fixed_seat_read, read_fixed_seat_page,
            };
            let mut read = begin_fixed_seat_read(
                state.state_store.clone(),
                anchor,
                seat,
                std::slice::from_ref(&transfer),
            )
            .unwrap();
            while !read_fixed_seat_page(&mut read, 64, 2 * 1024 * 1024).unwrap() {}
            finish_fixed_seat_read(read).unwrap()
        };
    let source_proof = completed_cut(&transfer.source, source_anchor.clone());
    let destination_proof = completed_cut(&transfer.destination, destination_anchor.clone());
    assert!(matches!(
        state
            .state_store
            .append_ui_transfer_preparation_pair(
                &transfer,
                &export,
                &destination,
                &source,
                &source_proof,
                &destination_proof,
                || { Ok(true) }
            )
            .unwrap(),
        ConditionalThreadEventPairAppendResult::Committed { .. }
    ));
    let source_opened = json!({
        "presentation_id": source_producer.presentation_id,
        "thread_id": source_producer.thread_id,
        "chain_root_id": source_detail.chain_root_id,
        "producer_incarnation": source_producer.producer_incarnation,
    });
    let ack = route(
        &state,
        &source_session,
        &ryeos_ui::handlers::ui_seat::APPEND_DESCRIPTOR,
        append_request(
            &source_opened,
            "ignored",
            0,
            &[json!({"after_transfer": true})],
        ),
    )
    .await
    .unwrap();
    assert_eq!(ack["events"][0]["payload"]["seq"], 0);
    let successor = claim(
        &state,
        &source_session,
        &source_opened,
        "00000000-0000-4000-8000-000000000015",
        json!({"kind":"takeover","expected_producer_incarnation":source_opened["producer_incarnation"]}),
    )
    .await
    .unwrap();
    assert_ne!(
        successor["producer_incarnation"],
        source_opened["producer_incarnation"]
    );
    let observed = observe(&state, &source_session, &source_opened)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(observed).unwrap()["producer"]["producer_incarnation"],
        successor["producer_incarnation"]
    );
}

#[tokio::test]
async fn append_receipts_survive_takeover_and_close_without_granting_old_producer() {
    use ryeos_ui::handlers::ui_seat::*;
    let (_tmp, state) = build_test_state_with_live_bundles();
    let session = activate(&state);
    let foreign = activate(&state);
    let state = Arc::new(state);
    let (opened, original_claim) = fresh_with_request(&state, &session).await;
    let op = "00000000-0000-4000-8000-000000000002";
    let request = append_request(&opened, op, 0, &[json!({"é":"😀","a":"𐀀"})]);
    let op = request["operation_id"].as_str().unwrap().to_owned();
    assert_eq!(
        request["payload_digest"],
        "40442791210ae66e4e67d762a8102f444250935db90a1976d743cb9952eed572"
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
        acknowledged[0]["chain_seq"].as_i64().unwrap() + 1,
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
            append_request(&opened, op, seq, &[json!({"different":true})]),
        )
        .await
        .expect_err("gap/overlap");
    }
    route(
        &state,
        &session,
        &APPEND_DESCRIPTOR,
        append_request(&opened, &op, 0, &[json!({"different":true})]),
    )
    .await
    .expect_err("operation contradiction");
    let mut reused_id = append_request(&opened, "ignored", 1, &[json!({"new_interval":true})]);
    reused_id["operation_id"] = json!(op);
    route(&state, &session, &APPEND_DESCRIPTOR, reused_id)
        .await
        .expect_err("an old operation ID cannot name a new engine interval");
    let mut malformed = append_request(
        &opened,
        "00000000-0000-4000-8000-000000000005",
        1,
        &[json!({})],
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
            &[json!({"fraction":0.000001}), json!({"second":true})],
        ),
    )
    .await
    .unwrap();
    assert_eq!(second_ack["events"].as_array().unwrap().len(), 2);
    assert_eq!(
        second_ack["events"][0]["chain_seq"].as_i64().unwrap() + 1,
        second_ack["events"][1]["chain_seq"].as_i64().unwrap()
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
            &[json!({})],
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
    let (_tmp, state) = build_test_state_with_live_bundles();
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

    let (_tmp, state) = build_test_state_with_live_bundles();
    let session = activate(&state);
    let state = Arc::new(state);
    let opened = fresh(&state, &session).await;
    let request = append_request(
        &opened,
        "00000000-0000-4000-8000-000000000071",
        0,
        &[json!({"selection":"retained"})],
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
    let (_tmp, state) = build_test_state_with_live_bundles();
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
    let (_tmp, state) = build_test_state_with_live_bundles();
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

#[tokio::test]
#[ignore = "requires populated current signed bundles and the development publisher fixture"]
async fn signed_fixed_anchor_history_completes_for_current_seat() {
    let (_tmp, state) = build_test_state_with_live_bundles();
    let (session_id, _, producer, _) =
        test_state::live_presentation_with_surface(&state, "surface:ryeos/ui/assistant-observe")
            .await;
    let state = Arc::new(state);
    let detail = state
        .state_store
        .get_thread(&producer.thread_id)
        .unwrap()
        .unwrap();
    let frames = history_frames_until_fence_or_eof(
        &state,
        &session_id,
        &producer.presentation_id,
        &detail.chain_root_id,
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
#[ignore = "requires populated current signed bundles and the development publisher fixture"]
async fn fixed_anchor_stream_rechecks_attachment_and_tip_before_completion() {
    use ryeos_ui::handlers::ui_seat::APPEND_DESCRIPTOR;
    use tokio_stream::StreamExt;

    for revoke_root in [true, false] {
        let (_tmp, state) = build_test_state_with_live_bundles();
        let (session_id, _, producer, _) = test_state::live_presentation_with_surface(
            &state,
            "surface:ryeos/ui/assistant-observe",
        )
        .await;
        let state = Arc::new(state);
        let detail = state
            .state_store
            .get_thread(&producer.thread_id)
            .unwrap()
            .unwrap();
        let opened = json!({
            "presentation_id": producer.presentation_id,
            "thread_id": producer.thread_id,
            "chain_root_id": detail.chain_root_id,
            "producer_incarnation": producer.producer_incarnation,
        });
        // Four fully receipted intervals cannot fit alongside completion in
        // the route's two-frame channel. After reading only Begin, the source
        // cannot have published Complete, regardless of task scheduling.
        for batch in 0..4_u64 {
            let payloads = (0..32_u64)
                .map(|offset| json!({"key":format!("stream.fixture.{}", batch * 32 + offset), "value":offset}))
                .collect::<Vec<_>>();
            route(
                &state,
                &session_id,
                &APPEND_DESCRIPTOR,
                append_request(&opened, "", batch * 32, &payloads),
            )
            .await
            .unwrap();
        }
        let valid = history_frames_until_fence_or_eof(
            &state,
            &session_id,
            &producer.presentation_id,
            &detail.chain_root_id,
        )
        .await
        .unwrap();
        assert_eq!(
            valid.last().unwrap().event_type,
            "ui_seat.replay.complete.v1",
            "the unmodified multi-frame history must independently complete"
        );

        let mut stream = open_history_stream(
            &state,
            &session_id,
            &producer.presentation_id,
            &detail.chain_root_id,
        )
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
            let session = ui.browser_sessions.get_session(&session_id).unwrap();
            let root = session.attachments[&session.root_attachment_id].coordinate();
            ui.browser_sessions
                .revoke_attachment(&session_id, &root)
                .unwrap();
        } else {
            // The anchor was captured before Begin. A subsequent admitted
            // append must invalidate this provisional cut, not be silently
            // omitted while its old anchor is advertised as current.
            route(
                &state,
                &session_id,
                &APPEND_DESCRIPTOR,
                append_request(
                    &opened,
                    "",
                    128,
                    &[json!({"key":"stream.fixture.late", "value":true})],
                ),
            )
            .await
            .unwrap();
        }
        let remaining = collect_history_until_fence_or_eof(stream).await.unwrap();
        assert!(
            remaining
                .iter()
                .all(|frame| frame.event_type != "ui_seat.replay.complete.v1"),
            "revoked attachment or advanced signed tip must end without Complete (revoke_root={revoke_root})"
        );
    }
}

#[tokio::test]
#[ignore = "requires populated current signed bundles and the development publisher fixture"]
async fn thread_inspection_facets_cross_a_fixed_anchor_page_boundary() {
    let (_tmp, state) = build_test_state_with_live_bundles();
    let (_, ctx, producer, _) =
        test_state::live_presentation_with_surface(&state, "surface:ryeos/ui/assistant-observe")
            .await;
    let detail = state
        .state_store
        .get_thread(&producer.thread_id)
        .unwrap()
        .unwrap();
    let mut events = vec![ryeos_app::state_store::NewEventRecord {
        event_type: ryeos_state::event_types::THREAD_FACET_SET.into(),
        storage_class: "indexed".into(),
        payload: json!({"key": "phase", "value": "old"}),
    }];
    for index in 0..64 {
        events.push(ryeos_app::state_store::NewEventRecord {
            event_type: ryeos_state::event_types::THREAD_FACET_SET.into(),
            storage_class: "indexed".into(),
            payload: json!({"key": format!("filler.{index}"), "value": "present"}),
        });
    }
    events.push(ryeos_app::state_store::NewEventRecord {
        event_type: ryeos_state::event_types::THREAD_FACET_SET.into(),
        storage_class: "indexed".into(),
        payload: json!({"key": "phase", "value": "new"}),
    });
    events.push(ryeos_app::state_store::NewEventRecord {
        event_type: ryeos_state::event_types::THREAD_FACET_SET.into(),
        storage_class: "journal".into(),
        payload: json!({"key": "phase", "value": "journal"}),
    });
    state
        .threads
        .append_thread_events(&detail.chain_root_id, &detail.thread_id, &events)
        .unwrap()
        .expect("signed facet fixture commits");
    let inspected = (ryeos_ui::handlers::ui_threads::INSPECT_DESCRIPTOR.handler)(
        json!({"thread_id": producer.thread_id, "event_limit": 1}),
        ctx,
        Arc::new(state),
    )
    .await
    .unwrap();
    assert_eq!(inspected["facets"]["phase"], "new");
    assert_eq!(inspected["facets"]["filler.0"], "present");
    assert_eq!(inspected["facets"]["filler.63"], "present");
    assert!(
        inspected["evidence_coverage"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| { entry["subject"] == "facets" && entry["state"] == "complete" })
    );
}

#[tokio::test]
#[ignore = "requires populated current signed bundles and the development publisher fixture"]
async fn thread_inspection_does_not_publish_a_partial_facet_map_at_page_budget() {
    let (_tmp, state) = build_test_state_with_live_bundles();
    let (_, ctx, producer, _) =
        test_state::live_presentation_with_surface(&state, "surface:ryeos/ui/assistant-observe")
            .await;
    let detail = state
        .state_store
        .get_thread(&producer.thread_id)
        .unwrap()
        .unwrap();
    state
        .threads
        .append_thread_events(
            &detail.chain_root_id,
            &detail.thread_id,
            &[ryeos_app::state_store::NewEventRecord {
                event_type: ryeos_state::event_types::THREAD_FACET_SET.into(),
                storage_class: "indexed".into(),
                payload: json!({"key": "phase", "value": "old"}),
            }],
        )
        .unwrap()
        .expect("signed facet fixture commits");
    // More than the inspection reader's eight 64-event pages. Journal
    // testimony is signed but not a facet; it must not turn an unfinished
    // walk into an authoritative empty or partial facet collection.
    for batch in 0..9 {
        let events = (0..64)
            .map(|index| ryeos_app::state_store::NewEventRecord {
                event_type: "ui.test.facet-history-padding".into(),
                storage_class: "journal".into(),
                payload: json!({"batch": batch, "index": index}),
            })
            .collect::<Vec<_>>();
        state
            .threads
            .append_thread_events(&detail.chain_root_id, &detail.thread_id, &events)
            .unwrap()
            .expect("signed journal fixture commits");
    }
    let inspected = (ryeos_ui::handlers::ui_threads::INSPECT_DESCRIPTOR.handler)(
        json!({"thread_id": producer.thread_id, "event_limit": 1}),
        ctx,
        Arc::new(state),
    )
    .await
    .unwrap();
    assert_eq!(inspected["facets"], json!({}));
    assert!(
        inspected["evidence_coverage"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| {
                entry["subject"] == "facets"
                    && entry["state"] == "unavailable"
                    && entry["reason"] == "signed_facet_map_not_proven"
            })
    );
}

#[tokio::test]
#[ignore = "requires populated current signed bundles and the development publisher fixture"]
async fn signed_orphan_producer_event_does_not_complete_fixed_anchor_history() {
    let (_tmp, state) = build_test_state_with_live_bundles();
    let (session_id, _, producer, _) =
        test_state::live_presentation_with_surface(&state, "surface:ryeos/ui/assistant-observe")
            .await;
    let state = Arc::new(state);
    let detail = state
        .state_store
        .get_thread(&producer.thread_id)
        .unwrap()
        .unwrap();
    state.threads.append_thread_events(&detail.chain_root_id, &detail.thread_id, &[
        ryeos_app::state_store::NewEventRecord {
            event_type: "seat.facet".into(), storage_class: "indexed".into(),
            payload: json!({"seq":0,"operation_id":"orphan","payload":{"key":"selection","value":"unsettled"}}),
        },
    ]).unwrap().expect("signed orphan fixture commits one event");
    let frames = history_frames_until_fence_or_eof(
        &state,
        &session_id,
        &producer.presentation_id,
        &detail.chain_root_id,
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
#[ignore = "requires populated current signed bundles and the development publisher fixture"]
async fn retained_surface_close_repairs_a_lost_projection_without_revoking_root_or_seat() {
    use ryeos_ui::handlers::ui_seat::{APPEND_DESCRIPTOR, OPEN_DESCRIPTOR, TOUCH_DESCRIPTOR};
    let (_tmp, state) = build_test_state_with_live_bundles();
    let (session_id, context, original, coordinate) =
        test_state::live_presentation_with_surface(&state, "surface:ryeos/ui/assistant-observe")
            .await;
    let state = Arc::new(state);
    let ui = get_ui_state(&state).unwrap();
    let before = ui.browser_sessions.get_session(&session_id).unwrap();
    let membership = &before.presentations[&original.presentation_id];
    let surface = membership.public_descriptor().surfaces[0].clone();
    let original_claim = json!({"presentation_id":original.presentation_id,"chain_root_id":membership.chain_root_id.clone().unwrap(),"thread_id":original.thread_id,
        "producer_incarnation":original.producer_incarnation});
    let rotated = claim(
        &state,
        &session_id,
        &original_claim,
        &uuid::Uuid::new_v4().to_string(),
        json!({"kind":"takeover","expected_producer_incarnation":original.producer_incarnation}),
    )
    .await
    .unwrap();
    let closure = json!({"presentation_id":original.presentation_id,
        "surface_instance_id":surface.surface_instance_id,"binding":surface.binding});
    let mut stale = append_request(
        &original_claim,
        &uuid::Uuid::new_v4().to_string(),
        0,
        &[closure.clone()],
    );
    stale["events"][0]["event_type"] = json!(ryeos_client_base::ui::seat::SURFACE_CLOSED_EVENT);
    identify_append(&original_claim, &mut stale);
    route(&state, &session_id, &APPEND_DESCRIPTOR, stale)
        .await
        .expect_err("replaced writer cannot close a Surface");
    assert_eq!(
        ui.browser_sessions
            .get_session(&session_id)
            .unwrap()
            .presentations[&original.presentation_id]
            .public_descriptor()
            .surfaces
            .len(),
        1
    );

    // Model the precise crash gap: the complete atomic producer batch committed,
    // but the transient membership projection and response were not updated.
    let detail = state
        .state_store
        .get_thread(&original.thread_id)
        .unwrap()
        .unwrap();
    let event_type = ryeos_client_base::ui::seat::SURFACE_CLOSED_EVENT;
    let semantic = json!([{"engine_seq":0,"event_type":event_type,"payload":closure.clone()}]);
    let payload_digest = ryeos_client_base::ui::seat_payload_digest(&semantic).unwrap();
    let operation_id = ryeos_client_base::ui::admission_operation::UiSeatAppendIdentity {
        presentation_id: original.presentation_id.clone(),
        chain_root_id: detail.chain_root_id.clone(),
        thread_id: detail.thread_id.clone(),
        producer_incarnation: rotated["producer_incarnation"].as_str().unwrap().into(),
        first_engine_seq: 0,
        last_engine_seq: 0,
        event_count: 1,
        payload_digest: payload_digest.clone(),
    }
    .derive_identity()
    .unwrap();
    state.threads.append_thread_events(&detail.chain_root_id,&detail.thread_id,&[
        ryeos_app::state_store::NewEventRecord {
            event_type:event_type.into(),
            storage_class:"indexed".into(),payload:json!({"seq":0,"operation_id":surface.surface_instance_id,"payload":closure}),
        },
        ryeos_app::state_store::NewEventRecord {
            event_type:"ui_seat.append_receipt.v2".into(),
            storage_class:"indexed".into(),
            payload:json!({"schema_version":"ryeos.ui.seat.append-receipt.v2",
                "producer_incarnation":rotated["producer_incarnation"],
                "operation_id":operation_id,"first_engine_seq":0,"last_engine_seq":0,
                "event_count":1,"payload_digest":payload_digest}),
        },
    ]).unwrap().expect("running seat persists close evidence");
    assert_eq!(
        ui.browser_sessions
            .get_session(&session_id)
            .unwrap()
            .presentations[&original.presentation_id]
            .public_descriptor()
            .surfaces
            .len(),
        1
    );
    let current =
        ryeos_ui::handlers::ui_session_current::handle(json!({}), context.clone(), state.clone())
            .await
            .unwrap();
    let after = ui.browser_sessions.get_session(&session_id).unwrap();
    assert!(
        after.presentations[&original.presentation_id]
            .public_descriptor()
            .surfaces
            .is_empty()
    );
    assert_eq!(
        after.attachments.len(),
        before.attachments.len(),
        "history remains retained"
    );
    assert!(
        ui.browser_sessions
            .require_presentation_attachment(
                &session_id,
                &original.presentation_id,
                &coordinate,
                state.node_policy.generation_digest()
            )
            .is_err()
    );
    assert!(
        ui.browser_sessions
            .require_presentation_attachment(
                &session_id,
                &original.presentation_id,
                &membership.root_coordinate,
                state.node_policy.generation_digest()
            )
            .is_ok()
    );
    assert_eq!(
        state
            .state_store
            .get_thread(&original.thread_id)
            .unwrap()
            .unwrap()
            .status,
        "running"
    );
    let attachments = current["binding_attachments"].as_array().unwrap();
    assert!(
        !attachments
            .iter()
            .any(|item| item["binding_attachment_id"] == coordinate.binding_attachment_id)
    );
    assert!(
        attachments.iter().any(|item| item["binding_attachment_id"]
            == membership.root_coordinate.binding_attachment_id)
    );
    // Repair is read-only with respect to producer control: the current writer
    // can still touch its presentation, and a repeated bootstrap is stable.
    route(
        &state,
        &session_id,
        &TOUCH_DESCRIPTOR,
        json!({"presentation_id":rotated["presentation_id"],
        "thread_id":rotated["thread_id"],"producer_incarnation":rotated["producer_incarnation"]}),
    )
    .await
    .unwrap();
    let observe = route(&state,&session_id,&OPEN_DESCRIPTOR,json!({"action":"observe",
        "presentation_id":rotated["presentation_id"],"thread_id":rotated["thread_id"],"chain_root_id":rotated["chain_root_id"]})).await.unwrap();
    assert_eq!(
        observe["producer"]["producer_incarnation"],
        rotated["producer_incarnation"]
    );
    assert_eq!(
        ryeos_ui::handlers::ui_session_current::handle(json!({}), context, state)
            .await
            .unwrap()["binding_attachments"],
        current["binding_attachments"]
    );
}
