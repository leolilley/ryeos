// The event streams a seat opens pass the same signed `ui_seat_reads` policy
// as every read: opening is judged by an entry for the route, the chain
// stream additionally needs the seat's authority over the chain's signed root
// subject, and each event is shaped by the entry's keep list and bounds.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio_stream::StreamExt;

use ryeos_api::route_error::RouteDispatchError;
use ryeos_api::routes::invocation::{
    CompiledRouteInvocation, RouteEventStream, RouteInvocationContext, RouteInvocationResult,
};
use ryeos_api::routes::response_modes::event_stream_mode::{
    EventStreamStrategy, StreamSourceCompiler,
};
use ryeos_app::state::AppState;
use ryeos_app::state_store::{NewEventRecord, NewThreadRecord};
use ryeos_ui::state::get_ui_state;
use ryeos_ui::test_support::{build_test_state, launch_context, mint_launch, set_seat_reads};

const CHAIN_ROUTE: &str = "ui/events/chain";
const SESSION_ROUTE: &str = "ui/events/session";

fn chain_entry(extra: Value) -> Value {
    let mut base = json!({
        "target": "route:ui/events/chain",
        "classes": ["local_operator", "hosted_principal"],
        "keep": [
            "/event_type",
            "/payload/chain_root_id",
            "/payload/thread_id",
            "/payload/event_type",
            "/payload/payload/delta",
            "/payload/payload/tool_use_partial/delta",
            "/payload/code"
        ],
        "max_result_bytes": 4096,
        "max_items": 8
    });
    for (key, value) in extra.as_object().expect("entry fields") {
        base[key] = value.clone();
    }
    base
}

fn session_entry(extra: Value) -> Value {
    let mut base = json!({
        "target": "route:ui/events/session",
        "classes": ["local_operator", "hosted_principal"],
        "keep": [
            "/event_type",
            "/payload/kind",
            "/payload/thread_id",
            "/payload/status"
        ],
        "max_result_bytes": 1024,
        "max_items": 4
    });
    for (key, value) in extra.as_object().expect("entry fields") {
        base[key] = value.clone();
    }
    base
}

fn policy(entries: Vec<Value>) -> Value {
    json!({"schema": 1, "reads": entries})
}

fn route(file: &str) -> ryeos_app::route_raw::RawRouteSpec {
    let path = ryeos_engine::test_support::workspace_root()
        .join("bundles/ryeos-ui/.ai/node/routes/ui/events")
        .join(file);
    serde_yaml::from_str(&std::fs::read_to_string(&path).expect("read shipped route"))
        .expect("parse shipped route")
}

/// A state whose policy is `body`, with one live browser session.
fn seated(
    body: &Value,
    user_principal_id: Option<&str>,
) -> (
    tempfile::TempDir,
    ryeos_app::test_support::TestStateStore,
    AppState,
    String,
    String,
) {
    let (tmp, owner, mut state) = build_test_state();
    set_seat_reads(&mut state, body);
    let (session_id, token) = mint_launch(
        &state,
        launch_context(
            "surface:ryeos/ui/base",
            None,
            ryeos_ui::compiled_binding::EffectiveUiPosture::Interactive,
            user_principal_id.map(str::to_owned),
        ),
    );
    let ui = get_ui_state(&state).unwrap();
    assert_eq!(
        ui.browser_sessions.consume_launch_token(&token),
        Some(session_id.clone())
    );
    let principal = ui
        .browser_sessions
        .get_session(&session_id)
        .unwrap()
        .principal_id;
    (tmp, owner, state, session_id, principal)
}

fn captured_policy() -> ryeos_state::objects::CapturedThreadHistoryPolicy {
    let hash = "a".repeat(64);
    ryeos_state::objects::CapturedThreadHistoryPolicy {
        retention: ryeos_state::objects::ThreadHistoryRetention::Durable,
        canonical_item_ref: "directive:test/stream".to_string(),
        item_content_hash: hash.clone(),
        item_signer_fingerprint: Some(hash.clone()),
        item_trust_class: ryeos_state::objects::CapturedItemTrustClass::Trusted,
        kind_schema_content_hash: hash,
        resolved_from: ryeos_state::objects::CapturedPolicyProvenance::NodeDefault {
            node_policy: ryeos_state::objects::CapturedNodeHistoryPolicyProvenance::test_policy(),
        },
    }
}

fn create_thread(
    state: &AppState,
    thread_id: &str,
    chain_root_id: &str,
    upstream: Option<&str>,
    requested_by: &str,
) {
    state
        .state_store
        .create_thread_for_test(&NewThreadRecord {
            thread_id: thread_id.to_string(),
            chain_root_id: chain_root_id.to_string(),
            kind: "directive".to_string(),
            item_ref: "directive:test/stream".to_string(),
            executor_ref: "test/executor".to_string(),
            launch_mode: "wait".to_string(),
            current_site_id: "site:test".to_string(),
            origin_site_id: "site:test".to_string(),
            upstream_thread_id: upstream.map(str::to_owned),
            requested_by: Some(requested_by.to_string()),
            project_root: None,
            project_authority: ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS,
            base_project_snapshot_hash: None,
            usage_subject: None,
            usage_subject_asserted_by: None,
            captured_history_policy: upstream.is_none().then(captured_policy),
        })
        .unwrap();
    state
        .state_store
        .mark_thread_running(thread_id, None)
        .unwrap();
}

fn append(state: &AppState, thread_id: &str, event_type: &str, payload: Value) {
    state
        .threads
        .append_thread_events(
            thread_id,
            thread_id,
            &[NewEventRecord {
                event_type: event_type.to_string(),
                storage_class: "indexed".to_string(),
                payload,
            }],
        )
        .unwrap()
        .expect("fixture thread is running");
}

async fn open(
    state: &AppState,
    session_id: &str,
    file: &str,
    value: &str,
) -> Result<RouteEventStream, RouteDispatchError> {
    let raw = route(file);
    let EventStreamStrategy::PathCaptureInput {
        invoker,
        input_field,
        ..
    } = (match file {
        "chain.yaml" => {
            ryeos_ui::invokers::browser_chain_tail_invocation::BrowserChainTailSourceFactory {
                ui: get_ui_state(state).unwrap().clone(),
            }
            .compile(&raw)
        }
        _ => ryeos_ui::invokers::session_events_invocation::SessionEventsSourceFactory {
            ui: get_ui_state(state).unwrap().clone(),
        }
        .compile(&raw),
    })
    .expect("shipped route compiles")
    else {
        panic!("a stream route captures its path");
    };
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "cookie",
        format!("ryeos_session={session_id}").parse().unwrap(),
    );
    let context = |headers, principal, input: Value| RouteInvocationContext {
        route_id: raw.id.clone().into(),
        method: axum::http::Method::GET,
        uri: raw.path.parse().unwrap(),
        captures: Default::default(),
        headers,
        body_raw: Vec::new(),
        input,
        principal,
        workspace_lifeline: None,
        launch_timings: None,
        publication: None,
        state: state.clone(),
        webhook_dedupe: Arc::new(Default::default()),
    };
    let auth = ryeos_ui::invokers::browser_session_invocation::CompiledBrowserSessionVerifier {
        ui: get_ui_state(state).unwrap().clone(),
    }
    .invoke(context(headers, None, Value::Null))
    .await
    .expect("live session authenticates");
    let RouteInvocationResult::Principal(principal) = auth else {
        panic!("browser session route did not authenticate");
    };
    match invoker
        .invoke(context(
            Default::default(),
            Some(principal),
            json!({ input_field: value }),
        ))
        .await?
    {
        RouteInvocationResult::Stream(stream) => Ok(stream),
        _ => panic!("a stream route returns a stream"),
    }
}

/// Every frame the stream yields until it has been quiet for a moment, and
/// whether it ended.
async fn drain(mut stream: RouteEventStream) -> (Vec<Value>, bool) {
    let mut frames = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_millis(400), stream.events.next()).await {
            Ok(Some(Ok(envelope))) => frames.push(json!({
                "id": envelope.id,
                "name": envelope.event_type,
                "data": envelope.payload,
            })),
            Ok(Some(Err(never))) => match never {},
            Ok(None) => return (frames, true),
            Err(_quiet) => return (frames, false),
        }
    }
}

fn refusal_text(error: &RouteDispatchError) -> String {
    match error {
        RouteDispatchError::Forbidden(text) => text.clone(),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn the_chain_stream_is_refused_without_an_entry_for_its_route() {
    for body in [
        policy(vec![]),
        // An entry for another route or a service is not an entry for this one.
        policy(vec![session_entry(json!({}))]),
        policy(vec![chain_entry(
            json!({"target": "service:ui/events/chain"}),
        )]),
        // An entry for another class is not an entry for this seat.
        policy(vec![chain_entry(json!({"classes": ["hosted_principal"]}))]),
    ] {
        let (_tmp, _owner, state, session_id, principal) = seated(&body, None);
        create_thread(&state, "T-own", "T-own", None, &principal);
        let error = open(&state, &session_id, "chain.yaml", "T-own")
            .await
            .err()
            .expect("no admitting entry refuses the open");
        assert!(
            refusal_text(&error).contains("seat_read_no_entry"),
            "{error:?}"
        );
    }
}

#[tokio::test]
async fn a_hosted_seat_needs_an_entry_that_admits_its_class() {
    let body = policy(vec![chain_entry(json!({"classes": ["local_operator"]}))]);
    let (_tmp, _owner, state, session_id, principal) = seated(&body, Some("fp:hosted"));
    create_thread(&state, "T-own", "T-own", None, &principal);
    let error = open(&state, &session_id, "chain.yaml", "T-own")
        .await
        .err()
        .expect("a hosted principal is not a local operator");
    assert!(refusal_text(&error).contains("seat_read_no_entry"));
}

#[tokio::test]
async fn an_unauthorized_chain_is_not_found_exactly_like_an_absent_one() {
    let (_tmp, _owner, state, session_id, principal) =
        seated(&policy(vec![chain_entry(json!({}))]), None);
    create_thread(&state, "T-other", "T-other", None, "fp:somebody-else");
    create_thread(&state, "T-own", "T-own", None, &principal);
    create_thread(&state, "T-child", "T-own", Some("T-own"), &principal);
    for id in ["T-other", "T-absent", "T-child"] {
        let error = open(&state, &session_id, "chain.yaml", id)
            .await
            .err()
            .unwrap_or_else(|| panic!("`{id}` must not open"));
        assert!(
            matches!(error, RouteDispatchError::NotFound),
            "`{id}` must read as not found, got {error:?}"
        );
    }
    open(&state, &session_id, "chain.yaml", "T-own")
        .await
        .expect("the seat's own chain root opens");
}

#[tokio::test]
async fn open_refuses_a_chain_the_entry_condition_does_not_admit() {
    // A condition the chain does not meet.
    let (_tmp, _owner, state, session_id, principal) = seated(
        &policy(vec![chain_entry(
            json!({"when": {"chain_root_id": ["T-elsewhere"]}}),
        )]),
        None,
    );
    create_thread(&state, "T-own", "T-own", None, &principal);
    let error = open(&state, &session_id, "chain.yaml", "T-own")
        .await
        .err()
        .expect("an unmet condition refuses the open");
    assert!(refusal_text(&error).contains("seat_read_no_matching_entry"));
}

#[tokio::test]
async fn open_refuses_a_denied_chain_parameter() {
    // A parameter the seat may not send.
    let (_tmp, _owner, state, session_id, principal) = seated(
        &policy(vec![chain_entry(json!({"deny_params": ["chain_root_id"]}))]),
        None,
    );
    create_thread(&state, "T-own", "T-own", None, &principal);
    let error = open(&state, &session_id, "chain.yaml", "T-own")
        .await
        .err()
        .expect("a denied parameter refuses the open");
    assert!(refusal_text(&error).contains("seat_read_parameter_denied"));
}

#[tokio::test]
async fn open_pins_replace_the_requested_chain() {
    // A pin replaces what the client asked for, and the pinned chain is
    // authorized like any other.
    let (_tmp, _owner, state, session_id, principal) = seated(
        &policy(vec![chain_entry(
            json!({"pin": {"chain_root_id": "T-pinned"}}),
        )]),
        None,
    );
    create_thread(&state, "T-own", "T-own", None, &principal);
    create_thread(&state, "T-pinned", "T-pinned", None, &principal);
    append(
        &state,
        "T-pinned",
        "cognition_out",
        json!({"delta": "pinned"}),
    );
    let (frames, _) = drain(
        open(&state, &session_id, "chain.yaml", "T-own")
            .await
            .expect("the pinned chain opens"),
    )
    .await;
    assert!(!frames.is_empty());
    assert!(
        frames
            .iter()
            .all(|frame| !frame.to_string().contains("T-own")),
        "{frames:?}"
    );
    assert!(
        frames
            .iter()
            .any(|frame| frame.to_string().contains("pinned"))
    );
}

#[tokio::test]
async fn open_pin_cannot_widen_the_seats_chains() {
    // A pin onto a chain the seat does not own is refused as not found.
    let (_tmp, _owner, state, session_id, principal) = seated(
        &policy(vec![chain_entry(
            json!({"pin": {"chain_root_id": "T-other"}}),
        )]),
        None,
    );
    create_thread(&state, "T-own", "T-own", None, &principal);
    create_thread(&state, "T-other", "T-other", None, "fp:somebody-else");
    let error = open(&state, &session_id, "chain.yaml", "T-own")
        .await
        .err()
        .expect("a pin cannot widen the seat's chains");
    assert!(matches!(error, RouteDispatchError::NotFound), "{error:?}");
}

#[tokio::test]
async fn each_chain_event_keeps_only_what_the_entry_names() {
    let (_tmp, _owner, state, session_id, principal) =
        seated(&policy(vec![chain_entry(json!({}))]), None);
    create_thread(&state, "T-own", "T-own", None, &principal);
    append(
        &state,
        "T-own",
        "cognition_in",
        json!({"origin": "operator", "parts": [{"type": "text", "text": "private prompt"}]}),
    );
    append(
        &state,
        "T-own",
        "cognition_out",
        json!({"delta": "visible", "model": "internal-model", "usage": {"tokens": 9}}),
    );
    append(
        &state,
        "T-own",
        "tool_call_start",
        json!({"tool_use_partial": {"delta": "{\"a\"", "tool": "secret-tool"}}),
    );
    let (frames, ended) = drain(
        open(&state, &session_id, "chain.yaml", "T-own")
            .await
            .expect("opens"),
    )
    .await;
    assert!(!ended);
    assert_eq!(
        frames[0]["data"]["payload"],
        json!({"chain_root_id": "T-own"})
    );
    assert_eq!(frames[0]["data"]["event_type"], "stream_started");
    let text = serde_json::to_string(&frames).unwrap();
    for leaked in [
        "private prompt",
        "internal-model",
        "tokens",
        "secret-tool",
        "event_hash",
        "chain_seq",
        "storage_class",
        "prev_chain_event_hash",
    ] {
        assert!(
            !text.contains(leaked),
            "`{leaked}` reached the seat: {text}"
        );
    }
    let record = |event_type: &str| {
        frames
            .iter()
            .find(|frame| frame["data"]["payload"]["event_type"] == event_type)
            .unwrap_or_else(|| panic!("no `{event_type}` frame in {frames:?}"))
    };
    assert_eq!(
        record("cognition_out")["data"],
        json!({
            "event_type": "cognition_out",
            "payload": {
                "chain_root_id": "T-own",
                "thread_id": "T-own",
                "event_type": "cognition_out",
                "payload": {"delta": "visible"}
            }
        })
    );
    assert_eq!(
        record("tool_call_start")["data"]["payload"]["payload"],
        json!({"tool_use_partial": {"delta": "{\"a\""}})
    );
    // The prompt event still arrives, as identifiers only.
    assert_eq!(
        record("cognition_in")["data"]["payload"]["payload"],
        json!({})
    );
    // Resume ids ride the frame, not the payload.
    assert!(frames.iter().any(|frame| frame["id"].is_string()));
    // Every frame is an unnamed message, as before.
    assert!(frames.iter().all(|frame| frame["name"] == "message"));
}

#[tokio::test]
async fn an_oversize_chain_event_ends_the_stream_with_a_refusal() {
    let (_tmp, _owner, state, session_id, principal) = seated(
        &policy(vec![chain_entry(json!({"max_result_bytes": 400}))]),
        None,
    );
    create_thread(&state, "T-own", "T-own", None, &principal);
    append(&state, "T-own", "cognition_out", json!({"delta": "small"}));
    append(
        &state,
        "T-own",
        "cognition_out",
        json!({"delta": "x".repeat(2000)}),
    );
    append(&state, "T-own", "cognition_out", json!({"delta": "after"}));
    let (frames, ended) = drain(
        open(&state, &session_id, "chain.yaml", "T-own")
            .await
            .expect("opens"),
    )
    .await;
    assert!(ended, "a refusal ends the stream");
    let last = frames.last().unwrap();
    assert_eq!(last["data"]["event_type"], "stream_error");
    assert_eq!(
        last["data"]["payload"]["code"],
        "seat_read_result_too_large"
    );
    let text = serde_json::to_string(&frames).unwrap();
    assert!(text.contains("small"));
    assert!(!text.contains("xxxx"), "the oversize event must not leak");
    assert!(!text.contains("after"), "nothing follows the refusal");
}

#[tokio::test]
async fn the_session_stream_is_refused_without_an_entry() {
    // No entry.
    let (_tmp, _owner, state, session_id, _principal) =
        seated(&policy(vec![chain_entry(json!({}))]), None);
    let error = open(&state, &session_id, "session.yaml", &session_id)
        .await
        .err()
        .expect("no entry for the session stream");
    assert!(refusal_text(&error).contains("seat_read_no_entry"));
}

#[tokio::test]
async fn a_session_pin_cannot_send_a_seat_to_another_sessions_bus() {
    // A pin cannot send the seat to another session's bus.
    let (_tmp, _owner, state, session_id, _principal) = seated(
        &policy(vec![session_entry(json!({"pin": {"session_id": "other"}}))]),
        None,
    );
    let error = open(&state, &session_id, "session.yaml", &session_id)
        .await
        .err()
        .expect("a pin onto another session is refused");
    assert!(
        matches!(error, RouteDispatchError::Forbidden(_)),
        "{error:?}"
    );
}

#[tokio::test]
async fn session_hints_keep_only_what_the_entry_names() {
    // Admitted: hints keep only what the entry names.
    let (_tmp, _owner, state, session_id, _principal) =
        seated(&policy(vec![session_entry(json!({}))]), None);
    let bus = get_ui_state(&state).unwrap().session_bus.clone();
    bus.publish(
        &session_id,
        "thread.hint",
        json!({"kind": "thread", "thread_id": "T-1", "status": "running", "chain_root_id": "T-root", "updated_at": "now"}),
    );
    let stream = open(&state, &session_id, "session.yaml", &session_id)
        .await
        .expect("the session stream opens");
    // Subscribed after the first publish, so replay is not used; publish more.
    let drain_task = tokio::spawn(drain(stream));
    tokio::time::sleep(Duration::from_millis(100)).await;
    bus.publish(
        &session_id,
        "thread.hint",
        json!({"kind": "thread", "thread_id": "T-2", "status": "running", "chain_root_id": "T-root", "updated_at": "now"}),
    );
    bus.publish(
        &session_id,
        "thread.hint",
        json!({"kind": "activity", "thread_ids": ["T-1", "T-9"], "event_count": 3}),
    );
    let (frames, _) = drain_task.await.unwrap();
    assert_eq!(frames.len(), 2, "{frames:?}");
    assert_eq!(frames[0]["name"], "thread.hint");
    assert_eq!(
        frames[0]["data"],
        json!({"kind": "thread", "thread_id": "T-2", "status": "running"})
    );
    assert_eq!(frames[1]["data"], json!({"kind": "activity"}));
    let text = serde_json::to_string(&frames).unwrap();
    for leaked in ["T-root", "T-9", "event_count", "updated_at"] {
        assert!(
            !text.contains(leaked),
            "`{leaked}` reached the seat: {text}"
        );
    }
}

#[tokio::test]
async fn an_item_hint_reaches_the_seat_with_the_fields_the_shipped_entry_names() {
    let shipped = ryeos_ui::test_support::shipped_seat_reads_record();
    let shipped = shipped
        .as_any()
        .downcast_ref::<ryeos_node_runtime::node_policy::sections::ui_seat_reads::UiSeatReadsPolicy>()
        .unwrap();
    let entries = shipped.entries_for(&format!("route:{SESSION_ROUTE}"));
    assert_eq!(entries.len(), 1);
    let keep: Vec<&str> = entries[0].keep.iter().map(|path| path.text()).collect();
    for field in ["item_ref", "digest", "revision", "change_set"] {
        assert!(
            keep.contains(&format!("/payload/{field}").as_str()),
            "the shipped session entry does not keep `{field}`: {keep:?}"
        );
    }

    let (_tmp, _owner, state, session_id, _principal) =
        seated(&policy(vec![session_entry(json!({"keep": keep}))]), None);
    let bus = get_ui_state(&state).unwrap().session_bus.clone();
    let stream = open(&state, &session_id, "session.yaml", &session_id)
        .await
        .expect("the session stream opens");
    let drain_task = tokio::spawn(drain(stream));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let hint = json!({
        "item_ref": "view:acme/a",
        "digest": "ab".repeat(32),
        "revision": 4,
        "change_set": "set-1",
    });
    bus.publish(&session_id, "item.hint", hint.clone());
    bus.publish(
        &session_id,
        "thread.hint",
        json!({"kind": "activity", "thread_ids": ["T-1"], "event_count": 3}),
    );
    let (frames, _) = drain_task.await.unwrap();
    assert_eq!(frames.len(), 2, "{frames:?}");
    assert_eq!(frames[0]["name"], "item.hint");
    assert_eq!(frames[0]["data"], hint);
}

#[tokio::test]
async fn an_oversize_session_event_ends_the_session_stream_with_a_refusal() {
    let (_tmp, _owner, state, session_id, _principal) = seated(
        &policy(vec![session_entry(json!({"max_result_bytes": 128}))]),
        None,
    );
    let bus = get_ui_state(&state).unwrap().session_bus.clone();
    let stream = open(&state, &session_id, "session.yaml", &session_id)
        .await
        .expect("opens");
    let drain_task = tokio::spawn(drain(stream));
    tokio::time::sleep(Duration::from_millis(100)).await;
    bus.publish(
        &session_id,
        "thread.hint",
        json!({"kind": "thread", "thread_id": "x".repeat(500)}),
    );
    bus.publish(
        &session_id,
        "thread.hint",
        json!({"kind": "thread", "thread_id": "T-after"}),
    );
    let (frames, ended) = drain_task.await.unwrap();
    assert!(ended);
    assert_eq!(frames.len(), 1, "{frames:?}");
    assert_eq!(frames[0]["name"], "stream_error");
    assert_eq!(frames[0]["data"]["code"], "seat_read_result_too_large");
}

#[test]
fn the_shipped_profiles_admit_the_streams_the_clients_open() {
    let policy = ryeos_ui::test_support::shipped_seat_reads_record();
    let policy = policy
        .as_any()
        .downcast_ref::<ryeos_node_runtime::node_policy::sections::ui_seat_reads::UiSeatReadsPolicy>()
        .unwrap();
    for route in [CHAIN_ROUTE, SESSION_ROUTE] {
        let entries = policy.entries_for(&format!("route:{route}"));
        assert_eq!(entries.len(), 1, "{route}");
        let keep: Vec<&str> = entries[0].keep.iter().map(|path| path.text()).collect();
        assert!(!keep.iter().any(|path| path.contains("event_hash")));
        assert!(!keep.iter().any(|path| path.contains("prev_")));
    }
}
