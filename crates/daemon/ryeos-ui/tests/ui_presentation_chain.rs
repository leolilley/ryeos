//! One presentation session driven end to end through the real handlers, with
//! the client reducers consuming exactly what each step produced: mint,
//! `ui.session.current`, `ui.session.surfaces.open`, a two-seat transfer
//! (prepare, accept, commit, activate), the authenticated seat history stream,
//! `projects/open` and a particular Surface resume.

use ryeos_ui::test_support as test_state;
use test_state::build_test_state_with_live_bundles;

use ryeos_app::handler_context::HandlerContext;
use ryeos_app::state::AppState;
use ryeos_client_base::ids::{PresentationId, RyeOsViewInstanceKey};
use ryeos_client_base::ui::admission_operation::{
    UiPreparedPresentationOperation, UiSeatAppendIdentity,
};
use ryeos_client_base::ui::cold_replay::ColdSeatReplay;
use ryeos_client_base::ui::composition_replay::UiCompositionReplay;
use ryeos_client_base::ui::seat::retained_seat_event_with_evidence;
use ryeos_client_base::ui::transfer_export::{
    UI_TRANSFER_EXPORT_SCHEMA, UiTransferExport, UiTransferExportContent, UiTransferExportMode,
    UiTransferSetExport,
};
use ryeos_client_base::ui::transfer_proof_frame::UiTransferProofFrame;
use ryeos_client_base::ui::{BrowserSession, BrowserViewport, RyeOsCore, seat_payload_digest};
use ryeos_ui::browser_session::BindingAttachmentCoordinate;
use ryeos_ui::state::get_ui_state;
use ryeos_ui_presentation_contract::transfer::{TransferIdentity, TransferSeat};
use serde_json::{Value, json};
use std::sync::Arc;

const SURFACE_OPENED_COMPLETION: &str = "seat.presentation_operation.completed";
const SURFACE_OPEN_PREPARED: &str = "seat.presentation_operation.prepared";

/// One claimed seat and the next engine sequence its producer may append at.
struct Seat {
    producer: ryeos_client_base::ui::UiPresentationProducer,
    next_engine_seq: u64,
}

struct Chain {
    state: Arc<AppState>,
    session_id: String,
    ctx: HandlerContext,
    root: BindingAttachmentCoordinate,
}

fn body_limit(state: &AppState, service_ref: &str) -> u64 {
    state
        .node_config
        .routes
        .iter()
        .find(|route| route.response.source.as_deref() == Some(service_ref))
        .unwrap()
        .limits
        .body_bytes_max
}

async fn append(chain: &Chain, seat: &mut Seat, events: &[(&str, Value)]) -> Value {
    let first = seat.next_engine_seq;
    let last = first + events.len() as u64 - 1;
    let rows = Value::Array(
        events
            .iter()
            .enumerate()
            .map(|(offset, (event_type, payload))| {
                json!({
                    "engine_seq": first + offset as u64,
                    "event_type": event_type,
                    "payload": payload,
                })
            })
            .collect(),
    );
    let payload_digest = seat_payload_digest(&rows).unwrap();
    let operation_id = UiSeatAppendIdentity {
        presentation_id: seat.producer.presentation_id.clone(),
        chain_root_id: seat.producer.thread_id.clone(),
        thread_id: seat.producer.thread_id.clone(),
        producer_incarnation: seat.producer.producer_incarnation.clone(),
        first_engine_seq: first,
        last_engine_seq: last,
        event_count: events.len() as u64,
        payload_digest: payload_digest.clone(),
    }
    .derive_identity()
    .unwrap();
    let receipt = (ryeos_ui::handlers::ui_seat::APPEND_DESCRIPTOR.handler)(
        json!({
            "presentation_id": seat.producer.presentation_id,
            "thread_id": seat.producer.thread_id,
            "producer_incarnation": seat.producer.producer_incarnation,
            "operation_id": operation_id,
            "first_engine_seq": first,
            "last_engine_seq": last,
            "event_count": events.len(),
            "payload_digest": payload_digest,
            "events": rows,
        }),
        chain.ctx.clone(),
        chain.state.clone(),
    )
    .await
    .unwrap_or_else(|error| panic!("append at engine sequence {first}: {error:#}"));
    seat.next_engine_seq = last + 1;
    receipt
}

/// What one presentation admission is: the dialog View it is invoked from,
/// the signed affordance of that View, and the typed purpose it commits to.
struct Admission<'a> {
    dialog: &'a str,
    view_ref: &'a str,
    affordance_id: &'a str,
    purpose: &'a str,
    surface_open_target: Value,
    particular_selection: Value,
    record: Value,
}

/// Prepare the operation on the seat: build it, derive its identity and retain
/// it in the seat's own history.
async fn prepare(
    chain: &Chain,
    seat: &mut Seat,
    admission: &Admission<'_>,
) -> UiPreparedPresentationOperation {
    let state = &chain.state;
    let presentation_id = PresentationId::try_new(seat.producer.presentation_id.clone()).unwrap();
    let origin = RyeOsViewInstanceKey::shared_dialog(&presentation_id, admission.dialog).unwrap();
    let maximum = body_limit(
        state,
        ryeos_ui::handlers::ui_invocations_dispatch::DESCRIPTOR.service_ref,
    );
    let mut prepared: UiPreparedPresentationOperation = serde_json::from_value(json!({
        "schema": 2,
        "prepared_engine_seq": seat.next_engine_seq,
        "operation_id": "00000000-0000-4000-8000-000000000000",
        "producer": seat.producer,
        "root_binding": chain.root,
        "request_bounds": {
            "max_request_bytes": maximum,
            "max_input_bytes": maximum,
            "max_seat_append_bytes": body_limit(
                state, ryeos_ui::handlers::ui_seat::APPEND_DESCRIPTOR.service_ref),
        },
        "body": {
            "kind": "admission",
            "purpose": admission.purpose,
            "surface_open_target": admission.surface_open_target.clone(),
            "particular_selection": admission.particular_selection.clone(),
            "invocation_origin": origin,
            "request": {
                "binding_attachment_id": chain.root.binding_attachment_id,
                "binding_generation": chain.root.binding_generation,
                "binding_digest": chain.root.binding_digest,
                "coordinate": {
                    "kind": "affordance",
                    "view_ref": admission.view_ref,
                    "affordance_id": admission.affordance_id,
                },
                "payload": {"kind": "selection", "record": admission.record.clone()},
            },
            "success_notice": null,
        },
    }))
    .unwrap();
    prepared.assign_identity().unwrap();
    append(
        chain,
        seat,
        &[(
            SURFACE_OPEN_PREPARED,
            serde_json::to_value(&prepared).unwrap(),
        )],
    )
    .await;
    prepared
}

/// Dispatch a prepared operation through the compiled binding.
async fn dispatch_prepared(
    chain: &Chain,
    seat: &Seat,
    prepared: &UiPreparedPresentationOperation,
) -> anyhow::Result<Value> {
    (ryeos_ui::handlers::ui_invocations_dispatch::DESCRIPTOR.handler)(
        json!({
            "presentation_operation_id": prepared.operation_id,
            "producer": seat.producer,
            "binding": prepared.admission().unwrap().request,
        }),
        chain.ctx.clone(),
        chain.state.clone(),
    )
    .await
}

/// Prepare the operation on the seat, dispatch it through the compiled
/// binding, and retain its authenticated result. Returns the dispatch
/// response.
async fn admit(chain: &Chain, seat: &mut Seat, admission: Admission<'_>) -> Value {
    let prepared = prepare(chain, seat, &admission).await;
    let operation_id = prepared.operation_id.clone();
    let dispatched = dispatch_prepared(chain, seat, &prepared)
        .await
        .unwrap_or_else(|error| {
            panic!(
                "dispatch {}: {:?}",
                admission.affordance_id,
                ryeos_app::handler_error::extract_handler_error(&error)
            )
        });
    // The client holds the response as it arrived over the wire, not as the
    // daemon's in-memory value.
    let dispatched: Value = serde_json::from_str(&serde_json::to_string(&dispatched).unwrap())
        .expect("the dispatch response is JSON");
    append(
        chain,
        seat,
        &[(
            SURFACE_OPENED_COMPLETION,
            json!({"operation_id": operation_id, "result": dispatched["result"]}),
        )],
    )
    .await;
    dispatched
}

async fn current_session(chain: &Chain) -> (Value, BrowserSession) {
    let value = (ryeos_ui::handlers::ui_session_current::DESCRIPTOR.handler)(
        json!(null),
        chain.ctx.clone(),
        chain.state.clone(),
    )
    .await
    .expect("ui.session.current");
    let session = serde_json::from_value(value.clone())
        .expect("the session the daemon projects is the session the client decodes");
    (value, session)
}

fn history_route() -> ryeos_app::route_raw::RawRouteSpec {
    serde_yaml::from_str(
        "id: ui/api/session/seat/history\npath: /ui/api/session/seat/history\nmethods: [POST]\nauth: browser_session\nlimits: { body_bytes_max: 16384, timeout_ms: 0, concurrent_max: 8 }\nrequest: { body: json }\nresponse: { mode: event_stream, source: ui_seat_history }\n",
    )
    .unwrap()
}

/// The authenticated seat history of one seat, from its begin frame to its
/// completion fence.
async fn history(
    chain: &Chain,
    seat: &Seat,
) -> Vec<ryeos_app::stream_envelope::RouteStreamEnvelope> {
    use ryeos_api::routes::invocation::{
        CompiledRouteInvocation, RouteInvocationContext, RouteInvocationResult,
    };
    use ryeos_api::routes::response_modes::event_stream_mode::{
        EventStreamStrategy, StreamSourceCompiler,
    };
    use tokio_stream::StreamExt;

    let state = &chain.state;
    let baseline = Arc::strong_count(&state.state_store);
    let raw = history_route();
    let EventStreamStrategy::BodyJson { invoker } =
        ryeos_ui::invokers::seat_history_invocation::SeatHistorySourceFactory
            .compile(&raw)
            .unwrap()
    else {
        panic!("seat history route did not compile to POST JSON");
    };
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "cookie",
        format!("ryeos_session={}", chain.session_id)
            .parse()
            .unwrap(),
    );
    let input = json!({
        "presentation_id": seat.producer.presentation_id,
        "chain_root_id": seat.producer.thread_id,
    });
    let context = |headers, principal| RouteInvocationContext {
        route_id: raw.id.clone().into(),
        method: axum::http::Method::POST,
        uri: raw.path.parse().unwrap(),
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
    };
    let RouteInvocationResult::Principal(principal) =
        ryeos_ui::invokers::browser_session_invocation::CompiledBrowserSessionVerifier {
            ui: get_ui_state(state).unwrap().clone(),
        }
        .invoke(context(headers, None))
        .await
        .unwrap()
    else {
        panic!("browser session route did not authenticate");
    };
    let RouteInvocationResult::Stream(mut stream) = invoker
        .invoke(context(Default::default(), Some(principal)))
        .await
        .unwrap()
    else {
        panic!("seat history route did not return a stream");
    };
    let mut frames = Vec::new();
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(10), stream.events.next())
            .await
            .expect("seat history frame within its timeout")
            .expect("seat history ended before its completion fence")
            .unwrap_or_else(|never| match never {});
        let completed = frame.event_type == "ui_seat.replay.complete.v1";
        frames.push(frame);
        if completed {
            break;
        }
        assert!(frames.len() < 64, "seat history exceeded its fixture bound");
    }
    drop(stream);
    for _ in 0..200 {
        if Arc::strong_count(&state.state_store) <= baseline {
            return frames;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("the seat history stream never released the state it held");
}

fn frame_kinds(frames: &[ryeos_app::stream_envelope::RouteStreamEnvelope]) -> Vec<&str> {
    frames
        .iter()
        .map(|frame| frame.event_type.as_str())
        .collect()
}

/// The client reducer consumes the frames exactly as the terminal does: the
/// paired proofs are staged first, every canonical row is appended, and the
/// completion fence is the authenticated tip.
fn reduce(
    session: BrowserSession,
    presentation_id: &str,
    frames: &[ryeos_app::stream_envelope::RouteStreamEnvelope],
) -> RyeOsCore {
    let mut staged = Some(ColdSeatReplay::new());
    let mut core = RyeOsCore::new(
        session,
        presentation_id,
        BrowserViewport {
            width: 1280,
            height: 800,
            device_pixel_ratio: 1.0,
        },
        1,
    );
    let mut rows = 0u64;
    let mut completed = false;
    for frame in frames {
        match frame.event_type.as_str() {
            "ui_seat.replay.begin.v1" => {}
            "ui_seat.replay.transfer_proof.v1" => {
                let proof: UiTransferProofFrame =
                    serde_json::from_value(frame.payload["proof"].clone()).unwrap();
                staged
                    .as_mut()
                    .unwrap()
                    .stage_transfer_proof(&core, proof)
                    .expect("the reducer stages the paired transfer proof");
            }
            "ui_seat.replay.events.v1" => {
                let events = frame.payload["events"].as_array().unwrap();
                rows += events.len() as u64;
                staged
                    .as_mut()
                    .unwrap()
                    .append_authenticated_records(&mut core, events)
                    .expect("the reducer appends the authenticated rows");
            }
            "ui_seat.replay.complete.v1" => {
                assert_eq!(frame.payload["event_count"], rows);
                staged
                    .take()
                    .expect("one completion fence")
                    .finish_at_authenticated_tip(
                        &mut core,
                        frame.payload["tip_hash"].as_str().unwrap(),
                        frame.payload["thread_seq"].as_u64().unwrap(),
                        rows,
                    )
                    .expect("the reducer completes at the authenticated tip");
                completed = true;
            }
            other => panic!("unexpected seat history frame {other}"),
        }
    }
    assert!(completed, "the history carried its completion fence");
    core
}

/// A seat's canonical composition, folded from its own authenticated history.
fn fold(
    chain: &Chain,
    presentation_id: &str,
    frames: &[ryeos_app::stream_envelope::RouteStreamEnvelope],
) -> UiCompositionReplay {
    let session = get_ui_state(&chain.state)
        .unwrap()
        .browser_sessions
        .get_session(&chain.session_id)
        .unwrap();
    let membership = &session.presentations[presentation_id];
    let events = frames
        .iter()
        .filter(|frame| frame.event_type == "ui_seat.replay.events.v1")
        .flat_map(|frame| frame.payload["events"].as_array().unwrap().iter())
        .filter_map(|row| {
            retained_seat_event_with_evidence(row)
                .expect("a retained seat row decodes")
                .map(|retained| retained.event)
        })
        .collect::<Vec<_>>();
    UiCompositionReplay::from_history(membership.initialization(), &events)
        .expect("the canonical composition folds from the seat's own history")
}

/// A copy export of every View set of the seat's first Surface, in the exact
/// shape its canonical composition holds them.
fn surface_view_sets_export(
    replay: &UiCompositionReplay,
    presentation_id: &str,
) -> (UiTransferExport, ryeos_client_base::ids::SurfaceInstanceId) {
    let surface = replay
        .composition
        .ordered_surfaces()
        .next()
        .expect("the seat holds its authored Surface");
    let surface_id = surface.id().clone();
    let sets = surface
        .view_sets()
        .iter()
        .map(|set| {
            assert!(set.root.is_none(), "the exported Surface mounts no Views");
            let handle = replay.template_ids[&set.id].clone();
            UiTransferSetExport {
                handle: handle.clone(),
                source_view_set_id: set.id,
                source_owner: ryeos_client_base::ids::UiViewMountOwner::Surface {
                    presentation_id: PresentationId::try_new(presentation_id).unwrap(),
                    surface_instance_id: surface_id.clone(),
                },
                binding: replay.insertion_bindings[&set.id].clone(),
                seed: ryeos_client_base::surface::view_sets::ViewSetSeedSpec {
                    schema: 2,
                    id: handle,
                    title: set.title.clone(),
                    root: None,
                },
                focused_view: None,
                mounts: vec![],
            }
        })
        .collect::<Vec<_>>();
    let active_set = surface
        .active_view_set()
        .map(|active| replay.template_ids[&active.id].clone());
    (
        UiTransferExport {
            schema: UI_TRANSFER_EXPORT_SCHEMA,
            mode: UiTransferExportMode::Copy,
            content: UiTransferExportContent::ViewSets { active_set, sets },
        },
        surface_id,
    )
}

async fn transfer(chain: &Chain, request: Value) -> Value {
    (ryeos_ui::handlers::ui_seat::TRANSFER_DESCRIPTOR.handler)(
        request,
        chain.ctx.clone(),
        chain.state.clone(),
    )
    .await
    .unwrap_or_else(|error| panic!("transfer step: {error:#}"))
}

fn transfer_seat(seat: &Seat, revision: u64) -> TransferSeat {
    TransferSeat {
        presentation_id: seat.producer.presentation_id.clone(),
        thread_id: seat.producer.thread_id.clone(),
        chain_root_id: seat.producer.thread_id.clone(),
        producer_incarnation: seat.producer.producer_incarnation.clone(),
        revision,
    }
}

#[tokio::test]
async fn one_session_drives_every_presentation_step_through_the_real_handlers() {
    let (_tmp, _state_owner, state) = build_test_state_with_live_bundles();
    let state = Arc::new(state);

    // Mint: a projectless session under the shipped shared Config, then a
    // second independently owned presentation in the same session.
    let (session_id, ctx, producer_a, root) = test_state::live_shipped_presentation(&state).await;
    let producer_b = test_state::additional_presentation(&state, &session_id).await;
    assert_ne!(producer_a.presentation_id, producer_b.presentation_id);
    let chain = Chain {
        state: state.clone(),
        session_id: session_id.clone(),
        ctx,
        root,
    };
    let mut seat_a = Seat {
        producer: producer_a,
        next_engine_seq: 0,
    };
    let seat_b = Seat {
        producer: producer_b,
        next_engine_seq: 0,
    };

    // ui.session.current reports both memberships, each holding its one
    // authored initial Surface.
    let (_, session) = current_session(&chain).await;
    assert_eq!(session.session_id, session_id);
    assert_eq!(session.presentations.len(), 2);
    for seat in [&seat_a, &seat_b] {
        let membership = session
            .presentations
            .iter()
            .find(|membership| membership.presentation_id == seat.producer.presentation_id)
            .expect("the session reports the presentation");
        assert_eq!(
            membership.thread_id.as_deref(),
            Some(seat.producer.thread_id.as_str())
        );
        assert_eq!(membership.surfaces.len(), 1);
    }

    // ui.session.surfaces.open: an authored Surface joins seat A without
    // replacing the seat or its first Surface.
    let opened = admit(
        &chain,
        &mut seat_a,
        Admission {
            dialog: "surfaces",
            view_ref: "view:ryeos/surfaces/list",
            affordance_id: "open",
            purpose: "open_surface",
            surface_open_target: json!({
                "selection": {"kind": "authored", "surface_ref": "surface:ryeos/ui/base-observe"},
                "placement": {"kind": "new_surface"},
            }),
            particular_selection: Value::Null,
            record: json!({"canonical_ref": "surface:ryeos/ui/base-observe"}),
        },
    )
    .await;
    assert_eq!(opened["target"]["ref"], "service:ui/session/surfaces/open");
    assert_eq!(opened["result"]["ui_transition"]["kind"], "open_surface");
    assert_eq!(
        opened["result"]["ui_transition"]["presentation_id"],
        seat_a.producer.presentation_id
    );
    let (_, session) = current_session(&chain).await;
    let membership_a = session
        .presentations
        .iter()
        .find(|membership| membership.presentation_id == seat_a.producer.presentation_id)
        .unwrap();
    assert_eq!(membership_a.surfaces.len(), 2);

    // Transfer between the two seats: A's first Surface's View sets are
    // exported from A's canonical composition and copied into B.
    let frames_a = history(&chain, &seat_a).await;
    let frames_b = history(&chain, &seat_b).await;
    let replay_a = fold(&chain, &seat_a.producer.presentation_id, &frames_a);
    let replay_b = fold(&chain, &seat_b.producer.presentation_id, &frames_b);
    let (export, _) = surface_view_sets_export(&replay_a, &seat_a.producer.presentation_id);
    let destination_surface = replay_b
        .composition
        .ordered_surfaces()
        .next()
        .expect("the destination holds its authored Surface")
        .id()
        .clone();
    let export_value = serde_json::to_value(&export).unwrap();
    let identity = TransferIdentity {
        operation_id: uuid::Uuid::new_v4().to_string(),
        source: transfer_seat(&seat_a, replay_a.revision),
        destination: transfer_seat(&seat_b, replay_b.revision),
        export_digest: seat_payload_digest(&export_value).unwrap(),
    };
    let prepared = transfer(
        &chain,
        json!({"action": "prepare", "transfer": identity, "export": export_value, "freeze": null}),
    )
    .await;
    assert_eq!(prepared["outcome"], "prepared", "{prepared}");
    let accepted = transfer(
        &chain,
        json!({
            "action": "accept_tabs", "transfer": identity,
            "inbound": prepared["inbound"], "preparation": prepared["preparation"],
            "surface_instance_id": destination_surface,
        }),
    )
    .await;
    assert_eq!(accepted["outcome"], "accepted", "{accepted}");
    let committed = transfer(&chain, json!({"action": "commit", "transfer": identity})).await;
    assert_eq!(committed["outcome"], "recorded", "{committed}");
    let activated = transfer(&chain, json!({"action": "activate", "transfer": identity})).await;
    assert_eq!(activated["outcome"], "activated", "{activated}");

    // Seat history stream: the paired proof frames precede every canonical
    // row on both seats, and the reducer consumes the whole stream.
    let (_, session) = current_session(&chain).await;
    for seat in [&seat_a, &seat_b] {
        let frames = history(&chain, seat).await;
        let kinds = frame_kinds(&frames);
        assert_eq!(kinds.first(), Some(&"ui_seat.replay.begin.v1"));
        assert_eq!(kinds.last(), Some(&"ui_seat.replay.complete.v1"));
        let first_events = kinds
            .iter()
            .position(|kind| *kind == "ui_seat.replay.events.v1")
            .expect("the seat replays its own rows");
        let proofs = kinds
            .iter()
            .enumerate()
            .filter(|(_, kind)| **kind == "ui_seat.replay.transfer_proof.v1")
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        assert_eq!(proofs.len(), 1, "one transfer operation is disclosed");
        assert!(
            proofs[0] < first_events,
            "proofs precede every canonical row"
        );
        let core = reduce(session.clone(), &seat.producer.presentation_id, &frames);
        let composition = core
            .presentation
            .as_ref()
            .expect("the reducer composes the seat");
        assert_eq!(composition.id().as_str(), seat.producer.presentation_id);
        if seat.producer.presentation_id == seat_a.producer.presentation_id {
            assert_eq!(
                composition.surfaces().len(),
                2,
                "the source keeps both Surfaces"
            );
        } else {
            let imported = composition
                .surfaces()
                .get(&destination_surface)
                .expect("the destination Surface")
                .view_sets()
                .len();
            assert_eq!(imported, 2, "the destination Surface gained the copied set");
        }
    }

    // projects/open: a registered project opens its own Surface in seat A.
    let second_project = tempfile::TempDir::new().expect("second project");
    let root_attachment = get_ui_state(&state)
        .unwrap()
        .browser_sessions
        .get_session(&session_id)
        .unwrap()
        .attachments[&chain.root.binding_attachment_id]
        .clone();
    let added = ryeos_ui::seat_auth::with_compiled_ui_attachment(
        root_attachment,
        ryeos_ui::handlers::ui_projects::handle_projects_add(
            json!({"root": second_project.path()}),
            chain.ctx.clone(),
            state.clone(),
        ),
    )
    .await
    .expect("register the second project for the session principal");
    let local_id = added["project"]["local_id"].as_str().unwrap().to_owned();
    let project_opened = admit(
        &chain,
        &mut seat_a,
        Admission {
            dialog: "projects",
            view_ref: "view:ryeos/projects/list",
            affordance_id: "open-project",
            purpose: "open_surface",
            surface_open_target: json!({
                "selection": {"kind": "authored", "surface_ref": "surface:ryeos/ui/project"},
                "placement": {"kind": "new_surface"},
            }),
            particular_selection: Value::Null,
            record: json!({"local_id": local_id}),
        },
    )
    .await;
    assert_eq!(project_opened["target"]["ref"], "service:projects/open");
    let transition = &project_opened["result"]["ui_transition"];
    assert_eq!(transition["kind"], "open_surface");
    assert_eq!(
        transition["presentation_id"],
        seat_a.producer.presentation_id
    );
    current_session(&chain).await;
    let retained = get_ui_state(&state)
        .unwrap()
        .browser_sessions
        .get_session(&session_id)
        .unwrap();
    let successor = &retained.attachments[transition["attachment"]["binding_attachment_id"]
        .as_str()
        .unwrap()];
    assert_eq!(
        successor.registered_project_id.as_deref(),
        Some(local_id.as_str())
    );
    assert_eq!(
        successor.project_query_identity.as_deref(),
        Some(
            second_project
                .path()
                .canonicalize()
                .unwrap()
                .to_str()
                .unwrap()
        )
    );

    // surfaces.resume: a saved particular Surface resumes under fresh
    // admission. It is saved through the principal's own library first.
    let tab = "resumed-tab";
    let saved_origin =
        json!({"surface_ref": "surface:ryeos/ui/lens", "project": {"local_id": local_id}});
    let library_attachment = retained.attachments[&chain.root.binding_attachment_id].clone();
    ryeos_ui::seat_auth::with_compiled_ui_attachment(
        library_attachment,
        ryeos_ui::handlers::ui_projects::handle_config_update(
            json!({
                "particular_view_set_library": {"expected_revision": 0, "particular_view_sets": [{
                    "id": tab, "name": "Resumed tab",
                    "composition": {"schema": 2, "id": tab, "title": "Resumed tab", "root": {
                        "type": "group", "label": "Projects",
                        "views": ["view:ryeos/projects/list"], "active": 0,
                    }},
                    "relationships": [
                        {"mount": {"kind": "tile", "index": 0}, "source": {"mode": "follow_own_set"}},
                    ],
                    "context": {"surface_ref": "surface:ryeos/ui/lens", "project": {"local_id": local_id}},
                }]},
                "particular_surface_library": {"expected_revision": 0, "particular_surfaces": [{
                    "id": "resumed", "title": "Resumed", "origin": saved_origin,
                    "view_set_ids": [tab], "active_view_set_id": tab,
                }]},
            }),
            chain.ctx.clone(),
            state.clone(),
        ),
    )
    .await
    .expect("save the particular Surface");
    let resumed = admit(
        &chain,
        &mut seat_a,
        Admission {
            dialog: "view-sets",
            view_ref: "view:ryeos/view-sets/library",
            affordance_id: "resume-particular-surface",
            purpose: "resume_particular_surface",
            surface_open_target: Value::Null,
            particular_selection: json!({"kind": "surface", "id": "resumed", "expected_revision": 1}),
            record: json!({
                "id": "resumed", "expected_revision": 1, "view_set_expected_revision": 1,
            }),
        },
    )
    .await;
    assert_eq!(
        resumed["target"]["ref"],
        "service:ui/ryeos-ui/surfaces/resume"
    );
    assert_eq!(
        resumed["result"]["ui_transition"]["kind"],
        "resume_particular_surface"
    );
    assert_eq!(
        resumed["result"]["particular_selection"],
        json!({"kind": "surface", "id": "resumed", "expected_revision": 1})
    );

    // The reducer's view of seat A after the whole chain is the session's:
    // the authored Surface, the surfaces.open Surface, the project Surface
    // and the resumed Surface.
    let (_, session) = current_session(&chain).await;
    let frames = history(&chain, &seat_a).await;
    let core = reduce(session.clone(), &seat_a.producer.presentation_id, &frames);
    let composition = core
        .presentation
        .as_ref()
        .expect("the reducer composes seat A");
    let membership_a = session
        .presentations
        .iter()
        .find(|membership| membership.presentation_id == seat_a.producer.presentation_id)
        .unwrap();
    assert_eq!(membership_a.surfaces.len(), 4);
    assert_eq!(composition.surfaces().len(), membership_a.surfaces.len());
    for surface in &membership_a.surfaces {
        assert!(
            composition
                .surfaces()
                .keys()
                .any(|id| id.as_str() == surface.surface_instance_id),
            "the reducer composes every Surface the session reports"
        );
    }
}

/// A seat bound to `project`, with the shipped dialogs and a claimed producer.
async fn project_bound_chain(state: &Arc<AppState>, project: &std::path::Path) -> (Chain, Seat) {
    let (session_id, ctx, producer, root) =
        test_state::live_shipped_presentation_in_project(state, project).await;
    (
        Chain {
            state: state.clone(),
            session_id,
            ctx,
            root,
        },
        Seat {
            producer,
            next_engine_seq: 0,
        },
    )
}

fn open_authored_surface() -> Admission<'static> {
    Admission {
        dialog: "surfaces",
        view_ref: "view:ryeos/surfaces/list",
        affordance_id: "open",
        purpose: "open_surface",
        surface_open_target: json!({
            "selection": {"kind": "authored", "surface_ref": "surface:ryeos/ui/base-observe"},
            "placement": {"kind": "new_surface"},
        }),
        particular_selection: Value::Null,
        record: json!({"canonical_ref": "surface:ryeos/ui/base-observe"}),
    }
}

#[tokio::test]
async fn a_project_bound_session_runs_a_recorded_dispatch_from_its_retained_directory() {
    let (_tmp, _state_owner, state) = build_test_state_with_live_bundles();
    let state = Arc::new(state);
    let project = tempfile::TempDir::new().expect("project");
    std::fs::create_dir(project.path().join(".ai")).expect("project .ai");
    let (chain, mut seat) = project_bound_chain(&state, project.path()).await;
    let canonical = project.path().canonicalize().unwrap();
    let attachment = get_ui_state(&state)
        .unwrap()
        .browser_sessions
        .get_session(&chain.session_id)
        .unwrap()
        .attachments[&chain.root.binding_attachment_id]
        .clone();
    assert_eq!(
        attachment.project_query_identity.as_deref(),
        canonical.to_str(),
        "the session is bound to the project"
    );

    let opened = admit(&chain, &mut seat, open_authored_surface()).await;
    assert_eq!(opened["target"]["ref"], "service:ui/session/surfaces/open");
    assert_eq!(opened["result"]["ui_transition"]["kind"], "open_surface");
    assert_eq!(
        opened["result"]["ui_transition"]["presentation_id"],
        seat.producer.presentation_id
    );
    let (_, session) = current_session(&chain).await;
    let membership = session
        .presentations
        .iter()
        .find(|membership| membership.presentation_id == seat.producer.presentation_id)
        .unwrap();
    assert_eq!(membership.surfaces.len(), 2);
}

#[tokio::test]
async fn a_recorded_dispatch_is_refused_when_the_project_path_no_longer_names_the_retained_directory()
 {
    let (_tmp, _state_owner, state) = build_test_state_with_live_bundles();
    let state = Arc::new(state);
    let parent = tempfile::TempDir::new().expect("projects");
    let project = parent.path().join("project");
    let decoy = parent.path().join("decoy");
    for root in [&project, &decoy] {
        std::fs::create_dir_all(root.join(".ai")).expect("project .ai");
    }
    let (chain, mut seat) = project_bound_chain(&state, &project).await;
    let prepared = prepare(&chain, &mut seat, &open_authored_surface()).await;

    lillux::atomic_exchange_paths(&project, &decoy).expect("swap the project path with a decoy");

    let error = dispatch_prepared(&chain, &seat, &prepared)
        .await
        .expect_err("a dispatch whose retained directory no longer matches is refused");
    assert!(
        matches!(
            ryeos_app::handler_error::extract_handler_error(&error),
            Some(ryeos_app::handler_error::HandlerError::Structured { ref code, .. })
                if code == "ui_binding_stale"
        ),
        "{error:#}"
    );
}
