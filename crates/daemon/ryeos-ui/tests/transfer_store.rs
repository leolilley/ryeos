//! The typed transfer layer against real signed seat histories: settlement of
//! claimed seats, qualification and retirement through retained evidence, and
//! the refusal of every predecessor lifecycle version.

mod test_state;
use test_state::{build_test_state, launch_context, mint_launch};

use std::sync::Arc;

use ryeos_app::retention_exclusion::ExclusionMode;
use ryeos_app::state::AppState;
use ryeos_app::state_store::{NewEventRecord, NewThreadRecord, PersistedEventRecord, StateStore};
use ryeos_state::objects::thread_event::ThreadEvent;
use ryeos_ui::transfer_store::historical::{
    HistoryEndpoint, TransferClosure, read_historical_transfer_cut, read_transfer_cut,
};
use ryeos_ui::transfer_store::reader::read_complete_fixed_history;
use ryeos_ui::transfer_store::retained::capture_retired_seat;
use ryeos_ui::transfer_store::retention::{
    event_contracts, qualify_seat_retirement, settle_claimed_seat_transfers, settle_claimed_seats,
    validate_retained_evidence,
};
use ryeos_ui_presentation_contract::{
    payload_digest::seat_payload_digest,
    seat_control::{
        SEAT_PRODUCER_EVENT, SEAT_PRODUCER_SCHEMA, UiSeatControlPredecessor, UiSeatControlRequest,
        UiSeatProducer, UiSeatProducerClaim, UiSeatProducerState,
    },
    transfer::{
        TRANSFER_EVENT, TransferEventPayload, TransferEvidence, TransferIdentity, TransferPhase,
        TransferRecord, TransferSeat,
    },
};
use serde_json::json;

const KIND: &str = "seat_session";
const OWNER: &str = "fp:transfer-owner";
const SURFACE: &str = "surface:transfer-test";
const CLIENT: &str = "client:transfer-test";
const FAR_FUTURE: &str = "2099-01-01T00:00:00Z";

fn install_composition() {
    ryeos_app::object_contracts::install_with_event_contracts(event_contracts()).unwrap();
    ryeos_app::object_contracts::install_retained_evidence_validator(validate_retained_evidence)
        .unwrap();
}

fn history_policy() -> ryeos_state::objects::CapturedThreadHistoryPolicy {
    let hash = "a".repeat(64);
    ryeos_state::objects::CapturedThreadHistoryPolicy {
        retention: ryeos_state::objects::ThreadHistoryRetention::TerminalFor { seconds: 60 },
        canonical_item_ref: SURFACE.to_string(),
        item_content_hash: hash.clone(),
        item_signer_fingerprint: Some(hash.clone()),
        item_trust_class: ryeos_state::objects::CapturedItemTrustClass::Trusted,
        kind_schema_content_hash: hash,
        resolved_from: ryeos_state::objects::CapturedPolicyProvenance::NodeDefault {
            node_policy: ryeos_state::objects::CapturedNodeHistoryPolicyProvenance::test_policy(),
        },
    }
}

fn thread_record(id: &str) -> NewThreadRecord {
    NewThreadRecord {
        thread_id: id.to_string(),
        chain_root_id: id.to_string(),
        kind: KIND.to_string(),
        item_ref: SURFACE.to_string(),
        executor_ref: CLIENT.to_string(),
        launch_mode: "wait".to_string(),
        current_site_id: "site:test".to_string(),
        origin_site_id: "site:test".to_string(),
        upstream_thread_id: None,
        requested_by: Some(OWNER.to_string()),
        project_root: None,
        project_authority: ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS,
        base_project_snapshot_hash: None,
        usage_subject: None,
        usage_subject_asserted_by: None,
        captured_history_policy: Some(history_policy()),
    }
}

/// One fixture seat: a running seat root with its initialization and an
/// active producer control already signed.
struct Seat {
    id: String,
    presentation_id: String,
    incarnation: String,
    claim_hash: String,
}

impl Seat {
    fn identity(&self) -> TransferSeat {
        TransferSeat {
            presentation_id: self.presentation_id.clone(),
            thread_id: self.id.clone(),
            chain_root_id: self.id.clone(),
            producer_incarnation: self.incarnation.clone(),
            revision: 0,
        }
    }
}

fn append(
    store: &StateStore,
    id: &str,
    event_type: &str,
    payload: serde_json::Value,
) -> PersistedEventRecord {
    store
        .append_events(
            id,
            id,
            &[NewEventRecord {
                event_type: event_type.to_string(),
                storage_class: "indexed".to_string(),
                payload,
            }],
        )
        .unwrap()
        .remove(0)
}

fn evidence_of(record: &PersistedEventRecord) -> TransferEvidence {
    TransferEvidence {
        event_hash: record.event_hash.clone().expect("durable event hash"),
        chain_seq: u64::try_from(record.chain_seq).unwrap(),
    }
}

fn new_seat(store: &StateStore, id: &str, presentation_id: &str, incarnation: char) -> Seat {
    seat_with_initialization(
        store,
        id,
        presentation_id,
        incarnation,
        json!({
            "schema": 2,
            "selection": {"kind": "configured_defaults"},
            "creation_request_id": "66666666-6666-4666-8666-666666666666",
            "presentation_id": presentation_id,
            "root_binding": {
                "binding_attachment_id": "root",
                "binding_generation": 1,
                "binding_digest": "a".repeat(64),
            },
            "binding_attachments": [{
                "binding_attachment_id": "root",
                "binding_generation": 1,
                "binding_digest": "a".repeat(64),
                "binding_request_bounds": {
                    "max_request_bytes": 4096,
                    "max_input_bytes": 1024,
                    "max_seat_append_bytes": 65536,
                },
            }],
        }),
    )
}

/// A seat whose signed initialization is exactly the one its session
/// membership holds, so a live read of it can authenticate the replay.
fn live_seat(
    state: &AppState,
    fixture: &SessionFixture,
    index: usize,
    id: &str,
    incarnation: char,
) -> Seat {
    let presentation_id = &fixture.presentations[index];
    let membership = ryeos_ui::state::get_ui_state(state)
        .unwrap()
        .browser_sessions
        .get_session(&fixture.session_id)
        .unwrap()
        .presentations[presentation_id]
        .clone();
    seat_with_initialization(
        &state.state_store,
        id,
        presentation_id,
        incarnation,
        serde_json::to_value(membership.initialization()).unwrap(),
    )
}

fn seat_with_initialization(
    store: &StateStore,
    id: &str,
    presentation_id: &str,
    incarnation: char,
    initialization: serde_json::Value,
) -> Seat {
    store.create_thread_for_test(&thread_record(id)).unwrap();
    store
        .claim_thread_launch(id, &format!("claim-{id}"), "daemon:test")
        .unwrap();
    store.mark_thread_running(id, None).unwrap();
    let incarnation = incarnation.to_string().repeat(64);
    let creation_request_id = initialization["creation_request_id"].clone();
    let initialized = append(
        store,
        id,
        "ui_seat.presentation_initialized.v1",
        json!({
            "operation_id": creation_request_id,
            "initialization": initialization,
        }),
    );
    let mut claim = UiSeatControlRequest::Claim {
        schema: 2,
        operation_id: String::new(),
        presentation_id: presentation_id.to_string(),
        thread_id: id.to_string(),
        chain_root_id: id.to_string(),
        predecessor: UiSeatControlPredecessor::Initialization {
            event_hash: initialized.event_hash.clone().unwrap(),
        },
        intent_nonce: "44444444-4444-4444-8444-444444444444".to_string(),
        claim: UiSeatProducerClaim::Fresh,
    };
    claim.assign_identity().unwrap();
    let producer = UiSeatProducer {
        schema_version: SEAT_PRODUCER_SCHEMA.to_string(),
        operation_id: claim.operation_id().to_string(),
        request: claim,
        claim: Some(UiSeatProducerClaim::Fresh),
        predecessor: None,
        producer_incarnation: incarnation.clone(),
        state: UiSeatProducerState::Active,
        next_engine_seq: 0,
    };
    let control = append(
        store,
        id,
        SEAT_PRODUCER_EVENT,
        serde_json::to_value(producer).unwrap(),
    );
    store.touch_seat_lease(id, OWNER, SURFACE, CLIENT).unwrap();
    Seat {
        id: id.to_string(),
        presentation_id: presentation_id.to_string(),
        incarnation,
        claim_hash: control.event_hash.unwrap(),
    }
}

/// Sign the seat's browser Close, after which only the node may write on it.
fn close_seat(store: &StateStore, seat: &Seat) {
    let mut close = UiSeatControlRequest::Close {
        schema: 2,
        operation_id: String::new(),
        presentation_id: seat.presentation_id.clone(),
        thread_id: seat.id.clone(),
        chain_root_id: seat.id.clone(),
        predecessor: UiSeatControlPredecessor::Control {
            event_hash: seat.claim_hash.clone(),
        },
        intent_nonce: "55555555-5555-4555-8555-555555555555".to_string(),
        producer_incarnation: seat.incarnation.clone(),
    };
    close.assign_identity().unwrap();
    let producer = UiSeatProducer {
        schema_version: SEAT_PRODUCER_SCHEMA.to_string(),
        operation_id: close.operation_id().to_string(),
        request: close,
        claim: None,
        predecessor: Some(seat.incarnation.clone()),
        producer_incarnation: seat.incarnation.clone(),
        state: UiSeatProducerState::Closed,
        next_engine_seq: 0,
    };
    append(
        store,
        &seat.id,
        SEAT_PRODUCER_EVENT,
        serde_json::to_value(producer).unwrap(),
    );
}

/// Claim the seat's lease for reaping, as an expiry pass does.
fn claim_for_reaping(store: &StateStore, seat: &Seat) {
    assert!(store.claim_expired_seat_lease(&seat.id, i64::MAX).unwrap());
}

fn phase_event(seat: &TransferSeat, record: TransferRecord, engine_seq: u64) -> serde_json::Value {
    let _ = seat;
    serde_json::to_value(TransferEventPayload {
        seq: engine_seq,
        operation_id: record.transfer.operation_id.clone(),
        payload: record,
    })
    .unwrap()
}

struct Pair {
    transfer: TransferIdentity,
    inbound: TransferEvidence,
}

/// The destination's inbound commitment followed by the source's Prepared
/// phase linked to it: a copy transfer awaiting the destination's decision.
fn prepared_pair(store: &StateStore, source: &Seat, destination: &Seat) -> Pair {
    prepared_pair_with_export(
        store,
        source,
        destination,
        json!({"mode": "copy", "view_ref": "view:test/one"}),
    )
}

fn prepared_pair_with_export(
    store: &StateStore,
    source: &Seat,
    destination: &Seat,
    export: serde_json::Value,
) -> Pair {
    let transfer = TransferIdentity {
        operation_id: "33333333-3333-4333-8333-333333333333".to_string(),
        source: source.identity(),
        destination: destination.identity(),
        export_digest: seat_payload_digest(&export).unwrap(),
    };
    let inbound_record = TransferRecord {
        schema: 1,
        transfer: transfer.clone(),
        phase: TransferPhase::InboundCommitment,
    };
    let inbound = evidence_of(&append(
        store,
        &destination.id,
        TRANSFER_EVENT,
        phase_event(&transfer.destination, inbound_record, 0),
    ));
    let prepared = TransferRecord {
        schema: 1,
        transfer: transfer.clone(),
        phase: TransferPhase::Prepared {
            inbound: inbound.clone(),
            freeze: None,
            export,
        },
    };
    append(
        store,
        &source.id,
        TRANSFER_EVENT,
        phase_event(&transfer.source, prepared, 0),
    );
    Pair { transfer, inbound }
}

async fn exclusion(
    state: &AppState,
) -> ryeos_app::retention_exclusion::RetentionExclusion<'static> {
    state.seal_thread_pin_sources().unwrap();
    state
        .acquire_retention_gate(ExclusionMode::Exclusive)
        .await
        .unwrap()
        .acquire_sealed_pins()
        .await
        .unwrap()
}

fn retire_due(
    state: &AppState,
    exclusion: &ryeos_app::retention_exclusion::RetentionExclusion<'_>,
) -> usize {
    let store = Arc::clone(&state.state_store);
    state
        .threads
        .retire_due_fenced_chains(exclusion, FAR_FUTURE, false, |root| {
            qualify_seat_retirement(&store, root)
        })
        .unwrap()
        .retired_chains
}

/// Let go of the launch claim a fixture seat took, as a live seat's owner
/// does when it stops, so only the retirement obligations remain.
fn release_launch_claim(store: &StateStore, seat: &Seat) {
    store
        .release_thread_launch_claim(&seat.id, &format!("claim-{}", seat.id))
        .unwrap();
}

fn thread_status(store: &StateStore, id: &str) -> String {
    store.get_thread(id).unwrap().unwrap().status
}

#[test]
fn only_the_current_lifecycle_version_has_an_event_contract() {
    let selectors: Vec<&str> = event_contracts().iter().map(|c| c.event_type).collect();
    assert_eq!(
        selectors,
        [
            "contract.ryeos_ui.move_freeze_cancelled.v1",
            "contract.ryeos_ui.presentation_transfer.v1",
            "contract.ryeos_ui.presentation_transfer_lifecycle.v3",
        ]
    );
}

#[test]
fn earlier_lifecycle_versions_have_no_decoder_and_are_not_retainable() {
    let (_dir, _owner, state) = build_test_state();
    install_composition();
    let store = &state.state_store;
    let seat = new_seat(
        store,
        "S-versions",
        "11111111-1111-4111-8111-111111111111",
        'b',
    );
    for earlier in [
        "contract.ryeos_ui.presentation_transfer_lifecycle.v1",
        "contract.ryeos_ui.presentation_transfer_lifecycle.v2",
    ] {
        assert!(
            !ryeos_state::object_closure::current_event_payload_selectors().contains(&earlier),
            "{earlier} must have no installed decoder"
        );
        let forged = ryeos_state::objects::thread_event::NewEvent::new(&seat.id, &seat.id, earlier)
            .thread_seq(1)
            .chain_seq(1)
            .payload(json!({}))
            .build();
        assert!(
            validate_retained_evidence(&forged).is_err(),
            "{earlier} must not be retainable evidence"
        );
    }
    let ordinary: ThreadEvent =
        ryeos_state::objects::thread_event::NewEvent::new(&seat.id, &seat.id, "seat.facet")
            .thread_seq(1)
            .chain_seq(1)
            .payload(json!({}))
            .build();
    assert!(validate_retained_evidence(&ordinary).is_err());
}

#[tokio::test]
async fn a_closed_seat_without_transfers_settles_finalizes_and_retires_with_no_evidence() {
    let (_dir, _owner, state) = build_test_state();
    install_composition();
    let store = &state.state_store;
    let seat = new_seat(
        store,
        "S-plain",
        "11111111-1111-4111-8111-111111111111",
        'b',
    );
    close_seat(store, &seat);
    claim_for_reaping(store, &seat);
    let held = exclusion(&state).await;
    let claimed = vec![seat.id.clone()];
    assert_eq!(settle_claimed_seats(&state, &held, &claimed).unwrap(), 1);
    assert_eq!(thread_status(store, &seat.id), "completed");
    assert!(
        store
            .claimed_seat_leases_slice(None, "S-plain", 64)
            .unwrap()
            .is_empty()
    );
    let plan = qualify_seat_retirement(store, &seat.id).unwrap().unwrap();
    assert!(plan.evidence.is_empty() && plan.counterpart_roots.is_empty());
    release_launch_claim(store, &seat);
    assert_eq!(retire_due(&state, &held), 1);
    let retired = capture_retired_seat(store, &seat.id).unwrap().unwrap();
    assert!(retired.owner_events().is_empty());
}

#[tokio::test]
async fn an_unaccepted_transfer_settles_by_source_abort_and_both_seats_retire_through_evidence() {
    let (_dir, _owner, state) = build_test_state();
    install_composition();
    let store = &state.state_store;
    let source = new_seat(
        store,
        "S-source",
        "11111111-1111-4111-8111-111111111111",
        'b',
    );
    let destination = new_seat(store, "S-dest", "22222222-2222-4222-8222-222222222222", 'c');
    let pair = prepared_pair(store, &source, &destination);
    close_seat(store, &source);
    close_seat(store, &destination);

    // Before settlement the transfer is open on both seats.
    let anchor = store
        .capture_thread_event_read_anchor(&source.id, &source.id)
        .unwrap()
        .unwrap();
    let cut = read_transfer_cut(
        store,
        &HistoryEndpoint::Current(anchor),
        std::slice::from_ref(&pair.transfer),
    )
    .unwrap();
    assert!(matches!(
        cut.closure(&pair.transfer).unwrap(),
        TransferClosure::Open(_)
    ));
    // A qualifier asked now refuses: the seat is not even terminal, and its
    // transfer is open.
    assert!(
        qualify_seat_retirement(store, &source.id)
            .unwrap()
            .is_none()
    );

    claim_for_reaping(store, &source);
    claim_for_reaping(store, &destination);
    let held = exclusion(&state).await;
    let claimed = vec![source.id.clone(), destination.id.clone()];
    assert_eq!(settle_claimed_seats(&state, &held, &claimed).unwrap(), 2);
    assert_eq!(thread_status(store, &source.id), "completed");
    assert_eq!(thread_status(store, &destination.id), "completed");

    // The source's abort is signed testimony both seats now agree on.
    let proof = read_complete_fixed_history(Arc::clone(store), &pair.transfer).unwrap();
    assert!(proof.lifecycle().aborted().is_some());
    assert!(proof.lifecycle().accepted().is_none());

    release_launch_claim(store, &source);
    release_launch_claim(store, &destination);

    // The source retires first; its transfer events stay as retained evidence.
    let source_plan = qualify_seat_retirement(store, &source.id).unwrap().unwrap();
    assert_eq!(
        source_plan.counterpart_roots.iter().collect::<Vec<_>>(),
        [&destination.id]
    );
    assert_eq!(
        source_plan.evidence.len(),
        2,
        "Prepared and the lifecycle Aborted"
    );
    store
        .retire_fenced_chain(&source.id, FAR_FUTURE, &source_plan, |_| Ok(0))
        .unwrap();
    let retired = capture_retired_seat(store, &source.id).unwrap().unwrap();
    assert_eq!(retired.owner_events().len(), 2);

    // The retired source is still a readable endpoint of the destination's
    // history, through its fence.
    let destination_anchor = store
        .capture_thread_event_read_anchor(&destination.id, &destination.id)
        .unwrap()
        .unwrap();
    let historical = read_historical_transfer_cut(
        store,
        &HistoryEndpoint::Current(destination_anchor.clone()),
        std::slice::from_ref(&pair.transfer),
    )
    .unwrap();
    assert!(
        historical
            .lifecycle(&pair.transfer.operation_id)
            .unwrap()
            .aborted()
            .is_some()
    );
    assert_eq!(
        historical.retired_dependencies(),
        [retired.dependency().clone()]
    );
    assert_eq!(
        historical.current_anchors(),
        std::slice::from_ref(&destination_anchor)
    );
    // A cut over the live destination and the retired source holds exactly
    // while the retired fence is unchanged.
    let held_cut = store
        .with_thread_event_read_mixed_cut(
            &historical.current_anchors(),
            &historical.retired_dependencies(),
            || Ok(()),
        )
        .unwrap();
    assert!(held_cut.is_some());

    // The destination retires second: its counterpart is retired, so there is
    // nothing to retain toward it, and the transfer still reads as closed.
    let destination_plan = qualify_seat_retirement(store, &destination.id)
        .unwrap()
        .unwrap();
    assert!(destination_plan.evidence.is_empty());
    store
        .retire_fenced_chain(&destination.id, FAR_FUTURE, &destination_plan, |_| Ok(0))
        .unwrap();
    assert!(
        capture_retired_seat(store, &destination.id)
            .unwrap()
            .is_some()
    );
    let _ = pair.inbound;
}

#[tokio::test]
async fn an_orphan_inbound_waits_for_its_source_to_be_claimed_then_cancels() {
    let (_dir, _owner, state) = build_test_state();
    install_composition();
    let store = &state.state_store;
    let source = new_seat(
        store,
        "S-orphan-source",
        "11111111-1111-4111-8111-111111111111",
        'b',
    );
    let destination = new_seat(
        store,
        "S-orphan-dest",
        "22222222-2222-4222-8222-222222222222",
        'c',
    );
    let export = json!({"mode": "copy", "view_ref": "view:test/one"});
    let transfer = TransferIdentity {
        operation_id: "33333333-3333-4333-8333-333333333333".to_string(),
        source: source.identity(),
        destination: destination.identity(),
        export_digest: seat_payload_digest(&export).unwrap(),
    };
    append(
        store,
        &destination.id,
        TRANSFER_EVENT,
        phase_event(
            &transfer.destination,
            TransferRecord {
                schema: 1,
                transfer: transfer.clone(),
                phase: TransferPhase::InboundCommitment,
            },
            0,
        ),
    );
    close_seat(store, &source);
    close_seat(store, &destination);
    claim_for_reaping(store, &destination);

    // The source is live and unclaimed: it could still prepare, so the orphan
    // cannot be cancelled and the destination stays claimed.
    assert!(!settle_claimed_seat_transfers(store, KIND, &destination.id).unwrap());
    let orphan = read_complete_fixed_history(Arc::clone(store), &transfer).unwrap();
    assert!(orphan.lifecycle().prepared().is_none());
    assert!(orphan.lifecycle().orphan_cancelled().is_none());

    claim_for_reaping(store, &source);
    assert!(settle_claimed_seat_transfers(store, KIND, &destination.id).unwrap());
    let anchor = store
        .capture_thread_event_read_anchor(&destination.id, &destination.id)
        .unwrap()
        .unwrap();
    let cut = read_transfer_cut(
        store,
        &HistoryEndpoint::Current(anchor),
        std::slice::from_ref(&transfer),
    )
    .unwrap();
    assert_eq!(cut.closure(&transfer).unwrap(), TransferClosure::Closed);
}

/// A launch whose root is a shared-presentation Config: the only root kind a
/// presentation can be admitted under.
fn shared_launch_context() -> ryeos_ui::browser_session::LaunchContext {
    let mut context = launch_context(
        "surface:ryeos/ui/base",
        None,
        ryeos_ui::compiled_binding::EffectiveUiPosture::ObservationOnly,
        None,
    );
    Arc::make_mut(&mut context.compiled_binding)
        .binding
        .surface
        .canonical_ref = "config:ryeos/test/presentation".to_owned();
    let config = serde_json::from_value(json!({
        "category": "ryeos/test",
        "shared_presentation": {
            "schema": 1, "title": "Test presentation", "launcher": "launch",
            "dialogs": [{"id": "launch", "title": "Launch", "view": "view:ryeos/test/launcher"}],
            "capabilities": {"sources": [], "affordances": []}
        }
    }))
    .expect("typed shared Config fixture");
    context.presentation = ryeos_client_base::presentation::UiCompositionPresentation {
        definition: ryeos_client_base::presentation::UiCompositionDefinition::SharedPresentation {
            config,
        },
        views: std::collections::BTreeMap::from([(
            "view:ryeos/test/launcher".into(),
            json!({"widget": "rows"}),
        )]),
    };
    context
}

/// A session holding one unbound presentation per requested seat.
struct SessionFixture {
    session_id: String,
    root: ryeos_ui::browser_session::BindingAttachmentCoordinate,
    digest: String,
    presentations: Vec<String>,
}

fn session_with_presentations(state: &AppState, count: usize) -> SessionFixture {
    let (session_id, token) = mint_launch(state, shared_launch_context());
    let ui = ryeos_ui::state::get_ui_state(state).unwrap();
    ui.browser_sessions.consume_launch_token(&token).unwrap();
    let session = ui.browser_sessions.get_session(&session_id).unwrap();
    let root = session.attachments[&session.root_attachment_id].coordinate();
    let policy = state
        .node_policy
        .require::<ryeos_node_runtime::node_policy::sections::ui_browser_sessions::UiBrowserSessionPolicy>()
        .expect("signed UI browser-session policy");
    let limits =
        ryeos_ui::browser_session::PresentationLimits::from_policy(policy).expect("policy limits");
    let digest = state.node_policy.generation_digest().to_owned();
    let presentations = (0..count)
        .map(|_| {
            ui.browser_sessions
                .prepare_presentation(
                    &session_id,
                    &uuid::Uuid::new_v4().to_string(),
                    &root,
                    &limits,
                    &digest,
                    ryeos_client_base::ui::UiBindingRequestBounds {
                        max_request_bytes: 65536,
                        max_input_bytes: 65536,
                        max_seat_append_bytes: 65536,
                    },
                )
                .unwrap()
                .presentation_id
        })
        .collect();
    SessionFixture {
        session_id,
        root,
        digest,
        presentations,
    }
}

fn bind(state: &AppState, fixture: &SessionFixture, presentation: &str, seat: &Seat) {
    ryeos_ui::state::get_ui_state(state)
        .unwrap()
        .browser_sessions
        .bind_presentation_seat(
            &fixture.session_id,
            presentation,
            &fixture.root,
            &seat.id,
            &seat.id,
            &fixture.digest,
        )
        .unwrap();
}

async fn current(
    state: &Arc<AppState>,
    fixture: &SessionFixture,
) -> anyhow::Result<serde_json::Value> {
    ryeos_ui::handlers::ui_session_current::handle(
        json!(null),
        ryeos_app::handler_context::HandlerContext::new(
            format!("session:{}", fixture.session_id),
            vec![],
            false,
        ),
        state.clone(),
    )
    .await
}

/// Close the seats, let their leases expire and settle them; then retire the
/// `retired` ones one after another in the given order. A seat retired while
/// its counterpart is still on the chain keeps its transfer testimony as
/// retained evidence.
async fn settle_and_retire(state: &Arc<AppState>, settled: &[&Seat], retired: &[&Seat]) {
    let store = &state.state_store;
    for seat in settled {
        close_seat(store, seat);
        claim_for_reaping(store, seat);
    }
    let held = exclusion(state).await;
    let claimed = settled
        .iter()
        .map(|seat| seat.id.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        settle_claimed_seats(state, &held, &claimed).unwrap(),
        settled.len()
    );
    for seat in settled {
        release_launch_claim(store, seat);
    }
    for seat in retired {
        let plan = qualify_seat_retirement(store, &seat.id).unwrap().unwrap();
        store
            .retire_fenced_chain(&seat.id, FAR_FUTURE, &plan, |_| Ok(0))
            .unwrap();
    }
}

#[tokio::test]
async fn session_current_reads_a_retired_seat_with_no_transfers_through_its_fence() {
    let (_dir, _owner, state) = build_test_state();
    install_composition();
    let fixture = session_with_presentations(&state, 1);
    let seat = new_seat(
        &state.state_store,
        "S-plain",
        &fixture.presentations[0],
        'b',
    );
    bind(&state, &fixture, &fixture.presentations[0], &seat);
    let state = Arc::new(state);
    settle_and_retire(&state, &[&seat], &[&seat]).await;
    let retired = capture_retired_seat(&state.state_store, &seat.id)
        .unwrap()
        .unwrap();
    assert!(retired.owner_events().is_empty());

    let reported = current(&state, &fixture).await.unwrap();
    let presentations = reported["presentations"].as_array().unwrap();
    assert_eq!(presentations.len(), 1);
    assert_eq!(presentations[0]["thread_id"], "S-plain");
    assert_eq!(presentations[0]["chain_root_id"], "S-plain");
}

#[tokio::test]
async fn session_current_reads_a_settled_transfer_between_two_retired_seats() {
    let (_dir, _owner, state) = build_test_state();
    install_composition();
    let fixture = session_with_presentations(&state, 2);
    let store = Arc::clone(&state.state_store);
    let source = new_seat(&store, "S-source", &fixture.presentations[0], 'b');
    let destination = new_seat(&store, "S-dest", &fixture.presentations[1], 'c');
    prepared_pair(&store, &source, &destination);
    bind(&state, &fixture, &fixture.presentations[0], &source);
    bind(&state, &fixture, &fixture.presentations[1], &destination);
    let state = Arc::new(state);
    settle_and_retire(&state, &[&source, &destination], &[&source, &destination]).await;
    let retired = capture_retired_seat(&store, &source.id).unwrap().unwrap();
    assert_eq!(
        retired.owner_events().len(),
        2,
        "the retired source keeps its Prepared and Aborted testimony"
    );

    let reported = current(&state, &fixture).await.unwrap();
    assert_eq!(reported["presentations"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn session_current_refuses_retired_transfer_evidence_naming_a_presentation_the_session_lacks()
{
    let (_dir, _owner, state) = build_test_state();
    install_composition();
    let fixture = session_with_presentations(&state, 1);
    let store = Arc::clone(&state.state_store);
    let source = new_seat(&store, "S-source", &fixture.presentations[0], 'b');
    let stranger = new_seat(
        &store,
        "S-stranger",
        "22222222-2222-4222-8222-222222222222",
        'c',
    );
    prepared_pair(&store, &source, &stranger);
    bind(&state, &fixture, &fixture.presentations[0], &source);
    let state = Arc::new(state);
    settle_and_retire(&state, &[&source, &stranger], &[&source]).await;
    let error = current(&state, &fixture)
        .await
        .expect_err("the cut names a seat this session does not hold");
    assert!(error.to_string().contains("does not hold"), "{error}");
}

/// A closed view-set export: one empty set owned by a Surface the source never
/// mounted. It decodes and validates as an export, which is what a live
/// reader of the pair asks of Prepared.
fn closed_export(fixture: &SessionFixture, source: &Seat) -> serde_json::Value {
    use ryeos_client_base::ids::{PresentationId, SurfaceInstanceId, UiViewMountOwner, ViewSetId};
    use ryeos_client_base::ui::binding::UiBindingAttachmentCoordinate;
    use ryeos_client_base::ui::transfer_export::{
        UI_TRANSFER_EXPORT_SCHEMA, UiTransferExport, UiTransferExportContent, UiTransferExportMode,
        UiTransferSetExport,
    };
    serde_json::to_value(UiTransferExport {
        schema: UI_TRANSFER_EXPORT_SCHEMA,
        mode: UiTransferExportMode::Copy,
        content: UiTransferExportContent::ViewSets {
            active_set: Some("empty".into()),
            sets: vec![UiTransferSetExport {
                handle: "empty".into(),
                source_view_set_id: ViewSetId::new(1),
                source_owner: UiViewMountOwner::Surface {
                    presentation_id: PresentationId::try_new(&source.presentation_id).unwrap(),
                    surface_instance_id: SurfaceInstanceId::try_new(
                        "99999999-9999-4999-8999-999999999999",
                    )
                    .unwrap(),
                },
                binding: UiBindingAttachmentCoordinate {
                    binding_attachment_id: fixture.root.binding_attachment_id.clone(),
                    binding_generation: fixture.root.binding_generation,
                    binding_digest: fixture.root.binding_digest.clone(),
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
    })
    .unwrap()
}

#[tokio::test]
async fn session_current_reads_a_retired_seat_beside_its_live_counterpart() {
    let (_dir, _owner, state) = build_test_state();
    install_composition();
    let fixture = session_with_presentations(&state, 2);
    let store = Arc::clone(&state.state_store);
    let source = live_seat(&state, &fixture, 0, "S-source", 'b');
    let destination = live_seat(&state, &fixture, 1, "S-dest", 'c');
    let export = closed_export(&fixture, &source);
    prepared_pair_with_export(&store, &source, &destination, export);
    bind(&state, &fixture, &fixture.presentations[0], &source);
    bind(&state, &fixture, &fixture.presentations[1], &destination);
    let state = Arc::new(state);
    // Both seats settle; only the source retires, so its Prepared and Aborted
    // testimony stays as retained evidence toward the destination still on
    // the chain.
    settle_and_retire(&state, &[&source, &destination], &[&source]).await;
    let retired = capture_retired_seat(&store, &source.id).unwrap().unwrap();
    assert_eq!(retired.owner_events().len(), 2);
    assert!(
        store
            .capture_thread_event_read_anchor(&destination.id, &destination.id)
            .unwrap()
            .is_some(),
        "the counterpart is still a live signed chain"
    );

    let reported = current(&state, &fixture).await.unwrap();
    let presentations = reported["presentations"].as_array().unwrap();
    assert_eq!(presentations.len(), 2);
    for seat in [&source, &destination] {
        assert!(
            presentations.iter().any(|reported| {
                reported["thread_id"] == seat.id
                    && reported["presentation_id"] == seat.presentation_id
            }),
            "{} is reported through the cut that names both seats",
            seat.id
        );
    }
}
