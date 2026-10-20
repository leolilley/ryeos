//! Bounded reads of one signed seat-history endpoint. Projection rows may
//! suggest coordinates, but never establish membership or absence.

use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_app::state_store::PersistedEventRecord;
use ryeos_client_base::ui::UiSeatControlPredecessor;
use ryeos_client_base::ui::presentation_initialization::{
    PRESENTATION_INITIALIZED_EVENT, UiPresentationInitializationRecord,
};
use ryeos_state::objects::thread_event::ThreadEvent;

use super::{
    MAX_SEAT_APPEND_EVENTS, SEAT_APPEND_RECEIPT_EVENT, SEAT_PRODUCER_EVENT, SeatAppendReceipt,
    SeatProducer, canonical_digest, canonical_operation_id,
};

const PAGE_EVENTS: u64 = 256;
const PAGE_BYTES: u64 = 32 * 1024 * 1024;

fn invalid(message: impl Into<String>) -> HandlerError {
    HandlerError::Internal(message.into())
}

pub(super) fn persisted(
    hash: String,
    event: ThreadEvent,
) -> Result<PersistedEventRecord, HandlerError> {
    Ok(PersistedEventRecord {
        event_id: 0, // Rebuildable projection identity is not part of this proof.
        event_hash: Some(hash),
        chain_root_id: event.chain_root_id,
        chain_seq: i64::try_from(event.chain_seq)
            .map_err(|_| invalid("authenticated seat chain sequence overflows"))?,
        thread_id: event.thread_id,
        thread_seq: i64::try_from(event.thread_seq)
            .map_err(|_| invalid("authenticated seat thread sequence overflows"))?,
        event_type: event.event_type,
        storage_class: event.durability.to_string(),
        ts: event.ts,
        prev_chain_event_hash: event.prev_chain_event_hash,
        prev_thread_event_hash: event.prev_thread_event_hash,
        payload: event.payload,
    })
}

/// Reverse-CAS initialization fold. Owned activity must not be interpreted as
/// initialization absence just because it uses the separate transfer lane.
#[derive(Default)]
struct ReverseInitialization {
    found: Option<UiPresentationInitializationRecord>,
    activity_without_initialization: bool,
}

impl ReverseInitialization {
    fn push(&mut self, event_type: &str, payload: serde_json::Value) -> Result<(), HandlerError> {
        if event_type != PRESENTATION_INITIALIZED_EVENT {
            let seat_activity = event_type.starts_with("seat.")
                || event_type.starts_with("ui_seat.")
                || ryeos_ui_presentation_contract::transfer::is_transfer_testimony(event_type);
            if seat_activity && self.found.is_some() {
                return Err(invalid("seat activity precedes signed initialization"));
            }
            self.activity_without_initialization |= seat_activity;
            return Ok(());
        }
        let record: UiPresentationInitializationRecord = serde_json::from_value(payload)
            .map_err(|error| invalid(format!("invalid retained initialization: {error}")))?;
        record.validate().map_err(invalid)?;
        if self.found.replace(record).is_some() {
            return Err(invalid("duplicate signed presentation initialization"));
        }
        Ok(())
    }

    fn finish(self) -> Result<Option<UiPresentationInitializationRecord>, HandlerError> {
        if self.found.is_none() && self.activity_without_initialization {
            return Err(invalid("seat activity has no signed initialization"));
        }
        Ok(self.found)
    }
}

fn validate_owned_control_version(event_type: &str) -> Result<(), HandlerError> {
    if event_type.starts_with("ui_seat.producer.") && event_type != SEAT_PRODUCER_EVENT {
        return Err(invalid("predecessor producer control is unsupported"));
    }
    Ok(())
}

fn validate_owned_receipt_version(event_type: &str) -> Result<(), HandlerError> {
    if event_type.starts_with("ui_seat.append_receipt.") && event_type != SEAT_APPEND_RECEIPT_EVENT
    {
        return Err(invalid("seat append receipt version is unsupported"));
    }
    Ok(())
}

/// Resolve initialization from one complete signed history, including exact
/// absence after exhausting the fixed anchor. A projection miss or row payload
/// is never initialization authority.
pub(super) fn initialization_record(
    state: &AppState,
    chain_root_id: &str,
    thread_id: &str,
) -> Result<Option<UiPresentationInitializationRecord>, HandlerError> {
    Ok(initialization_record_with_anchor(state, chain_root_id, thread_id)?.0)
}

/// The absence proof and its append anchor must come from the same signed
/// head. A missing head is not permission to create initialization testimony.
pub(super) fn initialization_record_with_anchor(
    state: &AppState,
    chain_root_id: &str,
    thread_id: &str,
) -> Result<
    (
        Option<UiPresentationInitializationRecord>,
        Option<ryeos_state::AuthoritativeThreadEventReadAnchor>,
    ),
    HandlerError,
> {
    let Some(anchor) = state
        .state_store
        .capture_thread_event_read_anchor(chain_root_id, thread_id)
        .map_err(|error| invalid(error.to_string()))?
    else {
        return Ok((None, None));
    };
    let mut progress = None;
    let mut initialization = ReverseInitialization::default();
    loop {
        let page = state
            .state_store
            .read_thread_event_page(&anchor, progress.as_ref(), PAGE_EVENTS, PAGE_BYTES)
            .map_err(|error| invalid(error.to_string()))?;
        if page.events.is_empty() && page.progress.is_some() {
            return Err(invalid(
                "authenticated initialization history made no progress",
            ));
        }
        for (_, event) in page.events {
            initialization.push(&event.event_type, event.payload)?;
        }
        match page.progress {
            Some(next) => progress = Some(next),
            None => return Ok((initialization.finish()?, Some(anchor))),
        }
    }
}

/// The producer-control predecessor is an exact CAS event, not an incarnation
/// or a projected latest-row claim. The same anchor must fence its successor
/// append, including intervening non-control events.
pub(super) fn control_predecessor(
    state: &AppState,
    chain_root_id: &str,
    thread_id: &str,
) -> Result<
    (
        UiSeatControlPredecessor,
        Option<SeatProducer>,
        ryeos_state::AuthoritativeThreadEventReadAnchor,
    ),
    HandlerError,
> {
    let anchor = state
        .state_store
        .capture_thread_event_read_anchor(chain_root_id, thread_id)
        .map_err(|error| invalid(error.to_string()))?
        .ok_or_else(|| invalid("control history has no signed thread anchor"))?;
    let mut progress = None;
    loop {
        let page = state
            .state_store
            .read_thread_event_page(&anchor, progress.as_ref(), PAGE_EVENTS, PAGE_BYTES)
            .map_err(|error| invalid(error.to_string()))?;
        if page.events.is_empty() && page.progress.is_some() {
            return Err(invalid("authenticated control history made no progress"));
        }
        for (hash, event) in page.events {
            validate_owned_control_version(&event.event_type)?;
            if event.event_type == SEAT_PRODUCER_EVENT {
                let control: SeatProducer = serde_json::from_value(event.payload)
                    .map_err(|error| invalid(format!("invalid authenticated control: {error}")))?;
                control.validate().map_err(invalid)?;
                control
                    .validate_subject(chain_root_id, thread_id)
                    .map_err(invalid)?;
                return Ok((
                    UiSeatControlPredecessor::Control { event_hash: hash },
                    Some(control),
                    anchor,
                ));
            }
            if event.event_type
                == ryeos_client_base::ui::presentation_initialization::PRESENTATION_INITIALIZED_EVENT
            {
                return Ok((UiSeatControlPredecessor::Initialization { event_hash: hash }, None, anchor));
            }
        }
        match page.progress {
            Some(next) => progress = Some(next),
            None => {
                return Err(invalid(
                    "authenticated control history has no initialization",
                ));
            }
        }
    }
}

pub(super) fn retained_control(
    state: &AppState,
    chain_root_id: &str,
    thread_id: &str,
    operation_id: &str,
) -> Result<Option<SeatProducer>, HandlerError> {
    let anchor = state
        .state_store
        .capture_thread_event_read_anchor(chain_root_id, thread_id)
        .map_err(|error| invalid(error.to_string()))?
        .ok_or_else(|| invalid("control history has no signed thread anchor"))?;
    let mut progress = None;
    let mut found = None;
    loop {
        let page = state
            .state_store
            .read_thread_event_page(&anchor, progress.as_ref(), PAGE_EVENTS, PAGE_BYTES)
            .map_err(|error| invalid(error.to_string()))?;
        if page.events.is_empty() && page.progress.is_some() {
            return Err(invalid("authenticated control lookup made no progress"));
        }
        for (_, event) in page.events {
            validate_owned_control_version(&event.event_type)?;
            if event.event_type == SEAT_PRODUCER_EVENT {
                let control: SeatProducer = serde_json::from_value(event.payload)
                    .map_err(|error| invalid(format!("invalid authenticated control: {error}")))?;
                control.validate().map_err(invalid)?;
                control
                    .validate_subject(chain_root_id, thread_id)
                    .map_err(invalid)?;
                if control.operation_id == operation_id && found.replace(control).is_some() {
                    return Err(invalid("duplicate authenticated control operation"));
                }
            }
        }
        match page.progress {
            Some(next) => progress = Some(next),
            None => return Ok(found),
        }
    }
}

/// An exhausted fixed anchor proves absence at that endpoint. A partial page
/// never does. The interval is retained newest-first until the receipt's
/// entire predecessor span has been authenticated, then returned oldest-first.
pub(super) fn receipt_and_interval(
    state: &AppState,
    presentation_id: &str,
    chain_root_id: &str,
    thread_id: &str,
    operation_id: &str,
) -> Result<
    Option<(
        SeatAppendReceipt,
        i64,
        i64,
        String,
        Vec<PersistedEventRecord>,
    )>,
    HandlerError,
> {
    if !canonical_operation_id(operation_id) {
        return Err(HandlerError::BadRequest(
            "invalid receipt operation ID".into(),
        ));
    }
    let anchor = state
        .state_store
        .capture_thread_event_read_anchor(chain_root_id, thread_id)
        .map_err(|error| invalid(error.to_string()))?
        .ok_or_else(|| invalid("authenticated seat history has no thread anchor"))?;
    receipt_and_interval_at_anchor(
        state,
        presentation_id,
        chain_root_id,
        thread_id,
        operation_id,
        &anchor,
    )
}

pub(super) fn receipt_and_interval_at_anchor(
    state: &AppState,
    presentation_id: &str,
    chain_root_id: &str,
    thread_id: &str,
    operation_id: &str,
    anchor: &ryeos_state::AuthoritativeThreadEventReadAnchor,
) -> Result<
    Option<(
        SeatAppendReceipt,
        i64,
        i64,
        String,
        Vec<PersistedEventRecord>,
    )>,
    HandlerError,
> {
    if !canonical_operation_id(operation_id)
        || anchor.chain_root_id() != chain_root_id
        || anchor.thread_id() != thread_id
    {
        return Err(HandlerError::BadRequest(
            "receipt lookup anchor or operation is invalid".into(),
        ));
    }
    let mut progress = None;
    let mut found: Option<(
        SeatAppendReceipt,
        i64,
        i64,
        String,
        String,
        Vec<PersistedEventRecord>,
    )> = None;
    loop {
        let page = state
            .state_store
            .read_thread_event_page(anchor, progress.as_ref(), PAGE_EVENTS, PAGE_BYTES)
            .map_err(|error| invalid(error.to_string()))?;
        if page.events.is_empty() && page.progress.is_some() {
            return Err(invalid("authenticated seat history made no progress"));
        }
        for (hash, event) in page.events {
            let record = persisted(hash, event)?;
            validate_owned_receipt_version(&record.event_type)?;
            if let Some((receipt, _, _, _, _, interval)) = found.as_mut()
                && interval.len() < receipt.event_count
            {
                interval.push(record);
                continue;
            }
            if record.event_type != SEAT_APPEND_RECEIPT_EVENT
                || record
                    .payload
                    .get("operation_id")
                    .and_then(serde_json::Value::as_str)
                    != Some(operation_id)
            {
                continue;
            }
            if found.is_some() {
                return Err(invalid("duplicate authenticated seat append receipts"));
            }
            let receipt_hash = record
                .event_hash
                .as_deref()
                .filter(|hash| canonical_digest(hash))
                .ok_or_else(|| invalid("authenticated seat receipt has no canonical hash"))?
                .to_owned();
            let receipt_thread_seq = record.thread_seq;
            let receipt_chain_seq = record.chain_seq;
            let receipt_predecessor_hash = record
                .prev_thread_event_hash
                .as_deref()
                .filter(|hash| canonical_digest(hash))
                .filter(|hash| record.prev_chain_event_hash.as_deref() == Some(*hash))
                .ok_or_else(|| invalid("authenticated seat receipt has no exact predecessor"))?
                .to_owned();
            let receipt: SeatAppendReceipt = serde_json::from_value(record.payload)
                .map_err(|error| invalid(format!("invalid authenticated seat receipt: {error}")))?;
            let expected_id = ryeos_client_base::ui::admission_operation::UiSeatAppendIdentity {
                presentation_id: presentation_id.to_owned(),
                chain_root_id: chain_root_id.to_owned(),
                thread_id: thread_id.to_owned(),
                producer_incarnation: receipt.producer_incarnation.clone(),
                first_engine_seq: receipt.first_engine_seq,
                last_engine_seq: receipt.last_engine_seq,
                event_count: receipt.event_count as u64,
                payload_digest: receipt.payload_digest.clone(),
            }
            .derive_identity()
            .map_err(invalid)?;
            if receipt.schema_version
                != ryeos_ui_presentation_contract::seat_control::SEAT_APPEND_RECEIPT_SCHEMA
                || receipt.event_count == 0
                || receipt.event_count > MAX_SEAT_APPEND_EVENTS
                || receipt.operation_id != expected_id
            {
                return Err(invalid(
                    "authenticated seat receipt contract is not current",
                ));
            }
            found = Some((
                receipt,
                receipt_chain_seq,
                receipt_thread_seq,
                receipt_hash,
                receipt_predecessor_hash,
                Vec::new(),
            ));
        }
        match page.progress {
            Some(next) => progress = Some(next),
            None => break,
        }
    }
    let Some((
        receipt,
        chain_seq,
        receipt_thread_seq,
        receipt_hash,
        receipt_predecessor_hash,
        mut interval,
    )) = found
    else {
        return Ok(None);
    };
    if interval.len() != receipt.event_count {
        return Err(invalid(
            "authenticated seat receipt predecessor interval is incomplete",
        ));
    }
    interval.reverse();
    if interval
        .last()
        .and_then(|event| event.event_hash.as_deref())
        != Some(receipt_predecessor_hash.as_str())
    {
        return Err(invalid(
            "authenticated seat receipt is not linked to its interval",
        ));
    }
    super::append_ack::validate_projected_interval(
        chain_root_id,
        thread_id,
        &receipt,
        chain_seq,
        &interval,
    )?;
    Ok(Some((
        receipt,
        chain_seq,
        receipt_thread_seq,
        receipt_hash,
        interval,
    )))
}

#[cfg(test)]
mod replay_initialization_tests {
    use super::*;

    fn valid_initialization_event() -> PersistedEventRecord {
        let attachment = serde_json::json!({
            "binding_attachment_id":"root",
            "binding_generation":1,
            "binding_digest":"a".repeat(64),
            "root_ref":"config:test/shared",
            "root_generation":"generation",
            "project_path":null,
            "subject":{"kind":"projectless"},
            "posture":"observation_only",
            "binding_request_bounds":{
                "max_request_bytes":4096,
                "max_input_bytes":1024,
                "max_seat_append_bytes":65536
            },
            "presentation":{
                "definition":{"kind":"shared_presentation","config":{
                    "category":"ui",
                    "shared_presentation":{
                        "schema":1,"title":"RyeOS","launcher":"launch",
                        "dialogs":[{"id":"launch","title":"Launch","view":"view:test/work"}],
                        "capabilities":{"sources":[],"affordances":[]}
                    }
                }},
                "views":{"view:test/work":{}}
            }
        });
        let record: UiPresentationInitializationRecord =
            serde_json::from_value(serde_json::json!({
                "operation_id":"22222222-2222-4222-8222-222222222222",
                "initialization":{
                    "schema":2,
                    "selection":{"kind":"configured_defaults"},
                    "presentation_id":"11111111-1111-4111-8111-111111111111",
                    "creation_request_id":"22222222-2222-4222-8222-222222222222",
                    "root_binding":{
                        "binding_attachment_id":"root",
                        "binding_generation":1,
                        "binding_digest":"a".repeat(64)
                    },
                    "next_mount_id":1,
                    "surfaces":[],
                    "binding_attachments":[attachment]
                }
            }))
            .unwrap();
        record.validate().unwrap();
        PersistedEventRecord {
            event_id: 0,
            event_hash: Some("a".repeat(64)),
            chain_root_id: "T-seat".into(),
            chain_seq: 2,
            thread_id: "T-seat".into(),
            thread_seq: 2,
            event_type: PRESENTATION_INITIALIZED_EVENT.into(),
            storage_class: "indexed".into(),
            ts: "2026-10-06T00:00:00Z".into(),
            prev_chain_event_hash: None,
            prev_thread_event_hash: None,
            payload: serde_json::to_value(record).unwrap(),
        }
    }

    fn owned_activity_kinds() -> [&'static str; 7] {
        [
            "seat.facet",
            SEAT_PRODUCER_EVENT,
            SEAT_APPEND_RECEIPT_EVENT,
            ryeos_ui_presentation_contract::transfer::TRANSFER_EVENT,
            ryeos_ui_presentation_contract::transfer_lifecycle::TRANSFER_LIFECYCLE_EVENT,
            "contract.ryeos_ui.presentation_transfer_lifecycle.v2",
            "contract.ryeos_ui.presentation_transfer.future",
        ]
    }

    #[test]
    fn owned_transfer_family_without_initialization_is_never_absence() {
        for kind in owned_activity_kinds() {
            let mut reverse = ReverseInitialization::default();
            reverse.push(kind, serde_json::json!({})).unwrap();
            reverse
                .push("thread_created", serde_json::json!({}))
                .unwrap();
            let error = reverse.finish().unwrap_err();
            assert!(
                format!("{error}").contains("activity has no signed initialization"),
                "{kind}: {error}"
            );
        }
    }

    #[test]
    fn owned_activity_older_than_signed_initialization_refuses_reverse_fold() {
        for kind in owned_activity_kinds() {
            let mut reverse = ReverseInitialization::default();
            reverse
                .push(
                    PRESENTATION_INITIALIZED_EVENT,
                    valid_initialization_event().payload,
                )
                .unwrap();
            let error = reverse.push(kind, serde_json::json!({})).unwrap_err();
            assert!(
                format!("{error}").contains("activity precedes signed initialization"),
                "{kind}: {error}"
            );
        }
    }

    #[test]
    fn newer_owned_activity_preserves_retained_initialization_not_transfer_admission() {
        for kind in owned_activity_kinds() {
            let initial = valid_initialization_event();
            let expected: UiPresentationInitializationRecord =
                serde_json::from_value(initial.payload.clone()).unwrap();
            let mut reverse = ReverseInitialization::default();
            // This lookup classifies activity only. It does not decode or
            // authenticate transfers; even unknown family members prevent an
            // absence result. Their phase reader still refuses unknown kinds.
            reverse.push(kind, serde_json::json!({})).unwrap();
            reverse
                .push(PRESENTATION_INITIALIZED_EVENT, initial.payload)
                .unwrap();
            reverse
                .push("thread_created", serde_json::json!({}))
                .unwrap();
            let retained = reverse.finish().unwrap().unwrap();
            assert_eq!(
                serde_json::to_value(retained).unwrap(),
                serde_json::to_value(expected).unwrap(),
                "{kind}"
            );
        }
    }

    #[test]
    fn valid_newer_transfer_retains_the_original_signed_initialization() {
        let transfer: ryeos_ui_presentation_contract::transfer::TransferRecord =
            serde_json::from_value(serde_json::json!({
                "schema":1,"transfer":{
                    "operation_id":"44444444-4444-4444-8444-444444444444",
                    "source":{"presentation_id":"33333333-3333-4333-8333-333333333333",
                        "thread_id":"T-other","chain_root_id":"T-other","producer_incarnation":"b".repeat(64),"revision":0},
                    "destination":{"presentation_id":"11111111-1111-4111-8111-111111111111",
                        "thread_id":"T-seat","chain_root_id":"T-seat","producer_incarnation":"c".repeat(64),"revision":0},
                    "export_digest":"d".repeat(64)},"phase":{"kind":"inbound_commitment"}
            })).unwrap();
        transfer.validate_shape().unwrap();
        let payload = serde_json::json!({"seq":0,"operation_id":transfer.transfer.operation_id,"payload":transfer});
        let mut reverse = ReverseInitialization::default();
        reverse
            .push(
                ryeos_ui_presentation_contract::transfer::TRANSFER_EVENT,
                payload,
            )
            .unwrap();
        reverse
            .push(
                PRESENTATION_INITIALIZED_EVENT,
                valid_initialization_event().payload,
            )
            .unwrap();
        assert_eq!(
            reverse
                .finish()
                .unwrap()
                .unwrap()
                .initialization
                .presentation_id,
            "11111111-1111-4111-8111-111111111111"
        );
    }

    #[test]
    fn foreign_activity_alone_is_absent_and_does_not_disturb_initialization() {
        let mut absent = ReverseInitialization::default();
        absent
            .push("thread_created", serde_json::json!({}))
            .unwrap();
        absent.push("cognition_in", serde_json::json!({})).unwrap();
        assert!(absent.finish().unwrap().is_none());
        let mut initialized = ReverseInitialization::default();
        initialized
            .push("cognition_in", serde_json::json!({}))
            .unwrap();
        initialized
            .push(
                PRESENTATION_INITIALIZED_EVENT,
                valid_initialization_event().payload,
            )
            .unwrap();
        initialized
            .push("thread_created", serde_json::json!({}))
            .unwrap();
        assert!(initialized.finish().unwrap().is_some());
    }

    #[test]
    fn receipt_lookup_refuses_every_noncurrent_owned_version() {
        for kind in [
            "ui_seat.append_receipt.v1",
            "ui_seat.append_receipt.v3",
            "ui_seat.append_receipt.v999",
            "ui_seat.append_receipt.v2.future",
            "ui_seat.append_receipt.",
        ] {
            assert!(validate_owned_receipt_version(kind).is_err(), "{kind}");
        }
        for kind in [
            SEAT_APPEND_RECEIPT_EVENT,
            SEAT_PRODUCER_EVENT,
            "thread_created",
        ] {
            assert!(validate_owned_receipt_version(kind).is_ok(), "{kind}");
        }
    }

    #[test]
    fn control_lookup_refuses_every_noncurrent_owned_version_not_just_v2() {
        for kind in [
            "ui_seat.producer.v1",
            "ui_seat.producer.v2",
            "ui_seat.producer.v4",
            "ui_seat.producer.v99",
            "ui_seat.producer.v3.future",
            "ui_seat.producer.unknown",
            "ui_seat.producer.",
        ] {
            let error = validate_owned_control_version(kind).unwrap_err();
            assert!(
                format!("{error}").contains("producer control is unsupported"),
                "{kind}: {error}"
            );
        }
        for kind in [
            SEAT_PRODUCER_EVENT,
            PRESENTATION_INITIALIZED_EVENT,
            "thread_created",
            "seat.facet",
        ] {
            validate_owned_control_version(kind).unwrap();
        }
    }
}
