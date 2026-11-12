//! Transfer effects on the deterministic platform: what goes on the wire for a
//! destination observation, a contact and a preparation, and which current
//! producer each one insists on. Request size bounds are covered separately in
//! `transfer_bounds.rs`.

mod common;

use std::cell::Cell;
use std::rc::Rc;

use common::sim::{FakePlatform, Reply};
use ryeos_client_base::ui::binding::UiPresentationProducer;
use ryeos_client_base::ui::effect::UiTransferContactRequest;
use ryeos_client_base::ui::transfer_export::UiTransferExport;
use ryeos_client_base::ui::{
    BrowserViewport, RyeOsEffect, RyeOsEffectKind, RyeOsEffectOutcome, RyeOsEffectResult,
    RyeOsEffectResultKind, UiBindingAttachmentCoordinate, UiBindingRequestBounds,
};
use ryeos_client_runtime::api::{SEAT_OPEN_PATH, SEAT_TRANSFER_PATH};
use ryeos_client_runtime::effects::{EffectEnv, execute_effect, unsupported_adapter};
use ryeos_client_runtime::inbox::{Inbox, StreamTag};
use ryeos_client_runtime::replay::CoreConfig;
use ryeos_client_runtime::seat::{SeatShared, random_uuid};
use ryeos_ui_presentation_contract::transfer::{TransferIdentity, TransferSeat};
use serde_json::{Value, json};

const SOURCE_PRESENTATION: &str = "11111111-1111-4111-8111-111111111111";
const DESTINATION_PRESENTATION: &str = "22222222-2222-4222-8222-222222222222";

fn seat(presentation: &str, thread: &str, chain: &str, incarnation: char) -> TransferSeat {
    TransferSeat {
        presentation_id: presentation.to_owned(),
        thread_id: thread.to_owned(),
        chain_root_id: chain.to_owned(),
        producer_incarnation: incarnation.to_string().repeat(64),
        revision: 0,
    }
}

/// The source's original incarnation is `a`, the destination's `b`.
fn transfer() -> TransferIdentity {
    TransferIdentity {
        operation_id: "33333333-3333-4333-8333-333333333333".to_owned(),
        source: seat(SOURCE_PRESENTATION, "source-thread", "source-chain", 'a'),
        destination: seat(
            DESTINATION_PRESENTATION,
            "destination-thread",
            "destination-chain",
            'b',
        ),
        export_digest: "c".repeat(64),
    }
}

fn producer_of(seat: &TransferSeat, incarnation: char) -> UiPresentationProducer {
    UiPresentationProducer {
        presentation_id: seat.presentation_id.clone(),
        thread_id: seat.thread_id.clone(),
        producer_incarnation: incarnation.to_string().repeat(64),
    }
}

fn export() -> UiTransferExport {
    serde_json::from_value(json!({
        "schema": 1,
        "mode": "copy",
        "content": {"kind": "view_sets", "active_set": null, "sets": []},
    }))
    .expect("an empty copy export")
}

fn bounds() -> UiBindingRequestBounds {
    UiBindingRequestBounds {
        max_request_bytes: 1 << 20,
        max_input_bytes: 1,
        max_seat_append_bytes: 1,
    }
}

fn root() -> UiBindingAttachmentCoordinate {
    UiBindingAttachmentCoordinate {
        binding_attachment_id: "root".to_owned(),
        binding_generation: 1,
        binding_digest: "digest".to_owned(),
    }
}

/// An effect environment whose current producer is `current`.
fn effect_env(
    platform: &Rc<FakePlatform>,
    current: Option<UiPresentationProducer>,
) -> EffectEnv<FakePlatform> {
    let shared = SeatShared::new(platform.clone());
    *shared.producer.borrow_mut() = current;
    EffectEnv {
        shared,
        adapter: unsupported_adapter(),
        documents: None,
        viewport: Rc::new(Cell::new(BrowserViewport::default())),
        config: CoreConfig::default(),
        inbox: Rc::new(Inbox::new(Rc::new(|| {}))),
        tag: StreamTag::default(),
    }
}

fn run(
    platform: &Rc<FakePlatform>,
    env: &EffectEnv<FakePlatform>,
    kind: RyeOsEffectKind,
) -> RyeOsEffectResult {
    platform
        .sim()
        .run(execute_effect(env, &RyeOsEffect { id: 1, kind }))
}

/// A refusal made before anything was sent: not ok, a definite platform
/// failure, and no request at all.
fn assert_refused_locally(platform: &FakePlatform, result: &RyeOsEffectResult, what: &str) {
    assert!(!result.ok, "{what}");
    assert!(result.data.is_none(), "{what}");
    let error = result.error.as_ref().expect("a failure carries its error");
    assert_eq!(error.code, "platform_effect_failed", "{what}");
    assert_eq!(error.outcome, RyeOsEffectOutcome::Refused, "{what}");
    assert!(
        platform.transport.calls.borrow().is_empty(),
        "{what}: nothing is sent"
    );
}

fn observe(source_producer: &UiPresentationProducer) -> RyeOsEffectKind {
    let transfer = transfer();
    RyeOsEffectKind::ObserveTransferDestination {
        destination_presentation_id: transfer.destination.presentation_id.clone(),
        destination_thread_id: transfer.destination.thread_id.clone(),
        destination_chain_root_id: transfer.destination.chain_root_id.clone(),
        destination_root_binding: root(),
        destination_initialization_digest: "d".repeat(64),
        source_producer: source_producer.clone(),
        source_chain_root_id: transfer.source.chain_root_id.clone(),
        source_root_binding: root(),
        source_revision: 4,
        source_export: export(),
        request_bounds: bounds(),
    }
}

/// The observation names only the destination seat: no source producer, no
/// export, no bindings. The result carries a fresh operation id and the
/// daemon's observation unchanged.
#[test]
fn a_destination_observation_sends_only_the_seat_locator() {
    let transfer = transfer();
    let source = producer_of(&transfer.source, 'a');
    let platform = FakePlatform::new();
    let observed = json!({"revision": 1, "producer": {"kind": "unclaimed"}});
    platform.transport.script(
        SEAT_OPEN_PATH,
        vec![Reply::Ok(json!({"result": observed}).to_string())],
    );
    let env = effect_env(&platform, Some(source.clone()));

    let result = run(&platform, &env, observe(&source));

    assert!(result.ok, "{result:?}");
    assert_eq!(
        result.kind,
        RyeOsEffectResultKind::TransferDestinationObserved
    );
    let sent = platform.transport.calls_to(SEAT_OPEN_PATH);
    assert_eq!(sent.len(), 1);
    assert_eq!(platform.transport.calls.borrow().len(), 1);
    let body: Value = serde_json::from_str(&sent[0]).unwrap();
    assert_eq!(
        body,
        json!({
            "action": "observe",
            "presentation_id": DESTINATION_PRESENTATION,
            "thread_id": "destination-thread",
            "chain_root_id": "destination-chain",
        })
    );
    for secret in ["a".repeat(64), "c".repeat(64), "d".repeat(64)] {
        assert!(!sent[0].contains(&secret), "{secret}");
    }
    assert_eq!(
        result.data,
        Some(json!({
            "operation_id": random_uuid(&*platform),
            "observation": observed,
        }))
    );
}

/// The observation is read for one captured source: it runs only while that
/// exact producer, including its incarnation, is the current one.
#[test]
fn a_destination_observation_needs_the_exact_source_producer() {
    let transfer = transfer();
    let source = producer_of(&transfer.source, 'a');
    let mut other_thread = source.clone();
    other_thread.thread_id = "another-thread".to_owned();
    let cases = [
        ("no current producer", None),
        ("another incarnation", Some(producer_of(&transfer.source, 'e'))),
        ("another thread", Some(other_thread)),
    ];
    for (what, current) in cases {
        let platform = FakePlatform::new();
        platform.transport.script(
            SEAT_OPEN_PATH,
            vec![Reply::Ok(json!({"revision": 1}).to_string())],
        );
        let env = effect_env(&platform, current);
        let result = run(&platform, &env, observe(&source));
        assert_refused_locally(&platform, &result, what);
    }
}

fn contact(request: UiTransferContactRequest) -> RyeOsEffectKind {
    RyeOsEffectKind::ContactTransfer {
        request,
        request_bounds: bounds(),
    }
}

/// A contact is checked against the seat that acts in its phase (the source
/// for commit, abort and cancel; the destination for accept and activate) by
/// presentation and thread only. The transfer's own incarnation is original
/// provenance and goes on the wire as recorded, not replaced by the current
/// lease's.
#[test]
fn a_contact_checks_the_acting_seat_by_presentation_and_thread_not_incarnation() {
    let transfer = transfer();
    let commit = UiTransferContactRequest::Commit {
        transfer: transfer.clone(),
    };
    let activate = UiTransferContactRequest::Activate {
        transfer: transfer.clone(),
    };
    let rotated_source = producer_of(&transfer.source, 'f');
    let rotated_destination = producer_of(&transfer.destination, 'f');

    for (request, current) in [
        (commit.clone(), rotated_source.clone()),
        (activate.clone(), rotated_destination.clone()),
    ] {
        let platform = FakePlatform::new();
        platform.transport.script(
            SEAT_TRANSFER_PATH,
            vec![Reply::Ok(json!({"result": {"accepted": true}}).to_string())],
        );
        let env = effect_env(&platform, Some(current.clone()));

        let result = run(&platform, &env, contact(request.clone()));

        assert!(result.ok, "{}: {result:?}", request.action_name());
        assert_eq!(result.kind, RyeOsEffectResultKind::BindingInvoked);
        assert_eq!(result.data, Some(json!({"accepted": true})));
        let sent = platform.transport.calls_to(SEAT_TRANSFER_PATH);
        assert_eq!(sent.len(), 1);
        let body: Value = serde_json::from_str(&sent[0]).unwrap();
        assert_eq!(body, serde_json::to_value(&request).unwrap());
        assert!(
            !sent[0].contains(&current.producer_incarnation),
            "{}: the current lease is not substituted into the request",
            request.action_name()
        );
    }

    // The wrong side, the wrong presentation, the wrong thread and no
    // producer at all are all refused before anything is sent.
    let mut other_presentation = rotated_source.clone();
    other_presentation.presentation_id = "99999999-9999-4999-8999-999999999999".to_owned();
    let mut other_thread = rotated_source.clone();
    other_thread.thread_id = "another-thread".to_owned();
    let refusals = [
        ("commit as the destination", commit.clone(), Some(rotated_destination.clone())),
        ("activate as the source", activate, Some(rotated_source.clone())),
        ("another presentation", commit.clone(), Some(other_presentation)),
        ("another thread", commit.clone(), Some(other_thread)),
        ("no producer", commit, None),
    ];
    for (what, request, current) in refusals {
        let platform = FakePlatform::new();
        platform
            .transport
            .script(SEAT_TRANSFER_PATH, vec![Reply::Ok("{}".to_owned())]);
        let env = effect_env(&platform, current);
        let result = run(&platform, &env, contact(request));
        assert_refused_locally(&platform, &result, what);
    }
}

/// A contact is a mutation: when its delivery fails the outcome is unknown,
/// not refused.
#[test]
fn a_contact_that_cannot_be_delivered_reports_an_unknown_outcome() {
    let transfer = transfer();
    let platform = FakePlatform::new();
    platform
        .transport
        .script(SEAT_TRANSFER_PATH, vec![Reply::Network]);
    let env = effect_env(&platform, Some(producer_of(&transfer.source, 'a')));

    let result = run(
        &platform,
        &env,
        contact(UiTransferContactRequest::Abort { transfer }),
    );

    assert!(!result.ok);
    let error = result.error.expect("a failure carries its error");
    assert_eq!(error.outcome, RyeOsEffectOutcome::Unknown);
    assert_eq!(error.code, "invocation_outcome_unknown");
    assert_eq!(platform.transport.calls_to(SEAT_TRANSFER_PATH).len(), 1);
}

fn prepare() -> RyeOsEffectKind {
    RyeOsEffectKind::PrepareTransfer {
        transfer: transfer(),
        export: export(),
        freeze: None,
        request_bounds: bounds(),
    }
}

/// A preparation insists on the source's exact producer: presentation,
/// thread and incarnation. Its answer is observation only: the runtime hands
/// the daemon's body back as data and changes neither the producer nor the
/// delivery state.
#[test]
fn a_preparation_needs_the_exact_source_producer_and_only_observes() {
    let transfer = transfer();
    let source = producer_of(&transfer.source, 'a');
    let platform = FakePlatform::new();
    platform.transport.script(
        SEAT_TRANSFER_PATH,
        vec![Reply::Ok(
            json!({"result": {"prepared": true, "owner": "elsewhere"}}).to_string(),
        )],
    );
    let env = effect_env(&platform, Some(source.clone()));

    let result = run(&platform, &env, prepare());

    assert!(result.ok, "{result:?}");
    assert_eq!(result.kind, RyeOsEffectResultKind::BindingInvoked);
    assert_eq!(
        result.data,
        Some(json!({"prepared": true, "owner": "elsewhere"}))
    );
    let sent = platform.transport.calls_to(SEAT_TRANSFER_PATH);
    assert_eq!(sent.len(), 1);
    assert_eq!(platform.transport.calls.borrow().len(), 1);
    let body: Value = serde_json::from_str(&sent[0]).unwrap();
    assert_eq!(
        body,
        json!({
            "action": "prepare",
            "transfer": serde_json::to_value(&transfer).unwrap(),
            "export": serde_json::to_value(export()).unwrap(),
            "freeze": null,
        })
    );
    assert_eq!(env.shared.current_producer(), Some(source));
    assert!(!env.shared.delivery.borrow().blocked);
    assert!(env.shared.delivery.borrow().pending.is_none());

    let mut other_thread = producer_of(&transfer.source, 'a');
    other_thread.thread_id = "another-thread".to_owned();
    let mut other_presentation = producer_of(&transfer.source, 'a');
    other_presentation.presentation_id = "99999999-9999-4999-8999-999999999999".to_owned();
    let refusals = [
        ("another incarnation", Some(producer_of(&transfer.source, 'e'))),
        ("another thread", Some(other_thread)),
        ("another presentation", Some(other_presentation)),
        ("the destination's producer", Some(producer_of(&transfer.destination, 'a'))),
        ("no producer", None),
    ];
    for (what, current) in refusals {
        let platform = FakePlatform::new();
        platform
            .transport
            .script(SEAT_TRANSFER_PATH, vec![Reply::Ok("{}".to_owned())]);
        let env = effect_env(&platform, current);
        let result = run(&platform, &env, prepare());
        assert_refused_locally(&platform, &result, what);
    }
}
