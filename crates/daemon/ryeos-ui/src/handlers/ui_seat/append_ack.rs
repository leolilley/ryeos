//! Exact append-result consistency checks, not history authentication.
//!
//! Live callers supply the records returned by the authoritative append owner.
//! Historical callers must establish membership separately; passing records to
//! these checks does not promote a projection row into retained evidence.

use ryeos_app::handler_error::HandlerError;
use ryeos_app::state_store::PersistedEventRecord;
use ryeos_ui_presentation_contract::seat_control::SeatAppendSemanticEvent;
use serde::Serialize;
use serde_json::Value;

use super::{
    MAX_SEAT_APPEND_EVENTS, SEAT_APPEND_RECEIPT_EVENT, SeatAppendReceipt, canonical_digest,
};

fn invalid(message: &str) -> HandlerError {
    HandlerError::Internal(message.into())
}

fn owned_record(
    record: &PersistedEventRecord,
    root: &str,
    thread: &str,
    storage_class: &str,
) -> bool {
    record.chain_root_id == root
        && record.thread_id == thread
        && record.chain_seq >= 0
        && record.thread_seq >= 0
        && record.storage_class == storage_class
        && record.event_hash.as_deref().is_some_and(canonical_digest)
}

fn directly_follows(previous: &PersistedEventRecord, next: &PersistedEventRecord) -> bool {
    previous.chain_seq.checked_add(1) == Some(next.chain_seq)
        && previous.thread_seq.checked_add(1) == Some(next.thread_seq)
        && next.prev_chain_event_hash == previous.event_hash
        && next.prev_thread_event_hash == previous.event_hash
}

/// Only stable event coordinates/content cross the ACK boundary. A projection
/// row ID can change after rebuilding, and append storage selection is not
/// event durability. Neither may make an exact retry differ from its live ACK.
#[derive(Serialize)]
pub(super) struct AcknowledgedEvent<'a> {
    event_hash: &'a Option<String>,
    chain_root_id: &'a str,
    chain_seq: String,
    thread_id: &'a str,
    thread_seq: String,
    event_type: &'a str,
    durability: ryeos_state::objects::EventDurability,
    ts: &'a str,
    prev_chain_event_hash: &'a Option<String>,
    prev_thread_event_hash: &'a Option<String>,
    payload: &'a Value,
}

pub(super) fn wire_events(events: &[PersistedEventRecord]) -> Vec<AcknowledgedEvent<'_>> {
    events
        .iter()
        .map(|event| AcknowledgedEvent {
            event_hash: &event.event_hash,
            chain_root_id: &event.chain_root_id,
            chain_seq: event.chain_seq.to_string(),
            thread_id: &event.thread_id,
            thread_seq: event.thread_seq.to_string(),
            event_type: &event.event_type,
            durability: ryeos_state::objects::EventDurability::Durable,
            ts: &event.ts,
            prev_chain_event_hash: &event.prev_chain_event_hash,
            prev_thread_event_hash: &event.prev_thread_event_hash,
            payload: &event.payload,
        })
        .collect()
}

/// Projection serialization exposes event durability, not the append input's
/// storage-selection vocabulary. Neither representation authenticates history.
pub(super) fn validate_projected_interval(
    root: &str,
    thread: &str,
    receipt: &SeatAppendReceipt,
    receipt_chain_seq: i64,
    events: &[PersistedEventRecord],
) -> Result<(), HandlerError> {
    validate_interval(root, thread, receipt, receipt_chain_seq, events, "durable")
}

fn validate_interval(
    root: &str,
    thread: &str,
    receipt: &SeatAppendReceipt,
    receipt_chain_seq: i64,
    events: &[PersistedEventRecord],
    storage_class: &str,
) -> Result<(), HandlerError> {
    if receipt.event_count == 0
        || receipt.event_count > MAX_SEAT_APPEND_EVENTS
        || receipt.event_count != events.len()
    {
        return Err(invalid("seat receipt has an invalid event interval"));
    }
    let first_chain_seq = receipt_chain_seq
        .checked_sub(receipt.event_count as i64)
        .filter(|seq| *seq >= 0)
        .ok_or_else(|| invalid("seat receipt interval underflows"))?;
    if events.iter().enumerate().any(|(offset, event)| {
        !owned_record(event, root, thread, storage_class)
            || event.chain_seq != first_chain_seq + offset as i64
            || event.payload.get("seq").and_then(Value::as_u64).is_none()
            || event.payload.get("payload").is_none()
    }) || events
        .windows(2)
        .any(|pair| !directly_follows(&pair[0], &pair[1]))
    {
        return Err(invalid(
            "seat receipt does not cover one exact persisted event interval",
        ));
    }
    let semantic = events
        .iter()
        .map(|event| SeatAppendSemanticEvent {
            engine_seq: event.payload["seq"].as_u64().expect("interval checked"),
            event_type: &event.event_type,
            payload: &event.payload["payload"],
        })
        .collect::<Vec<_>>();
    receipt
        .validate_semantic_interval(&semantic, MAX_SEAT_APPEND_EVENTS)
        .map_err(invalid)?;
    Ok(())
}

/// Borrows only the just-committed append result. No state/projection lookup is
/// required to construct its interval. The final receipt is checked as part of
/// the same result rather than borrowing its coordinates from the request.
pub(super) fn from_committed_append<'a>(
    root: &str,
    thread: &str,
    receipt: &SeatAppendReceipt,
    records: &'a [PersistedEventRecord],
) -> Result<(i64, &'a [PersistedEventRecord]), HandlerError> {
    let (committed_receipt, events) = records
        .split_last()
        .ok_or_else(|| invalid("seat append returned no records"))?;
    let expected_receipt =
        serde_json::to_value(receipt).map_err(|error| HandlerError::Internal(error.to_string()))?;
    if !owned_record(committed_receipt, root, thread, "indexed")
        || committed_receipt.event_type != SEAT_APPEND_RECEIPT_EVENT
        || committed_receipt.payload != expected_receipt
    {
        return Err(invalid("seat append returned a different receipt"));
    }
    validate_interval(
        root,
        thread,
        receipt,
        committed_receipt.chain_seq,
        events,
        "indexed",
    )?;
    if !directly_follows(
        events.last().expect("nonempty interval checked"),
        committed_receipt,
    ) {
        return Err(invalid(
            "seat append receipt is not linked to its event interval",
        ));
    }
    Ok((committed_receipt.chain_seq, events))
}

#[cfg(test)]
mod tests {
    use super::super::{AppendEvent, seat_payload_digest};
    use super::*;
    use serde_json::json;

    fn fixture() -> (SeatAppendReceipt, Vec<PersistedEventRecord>) {
        let events = vec![
            AppendEvent {
                engine_seq: 7,
                event_type: "seat.facet".into(),
                payload: json!({"key":"one","value":null}),
            },
            AppendEvent {
                engine_seq: 8,
                event_type: "seat.facet".into(),
                payload: json!({"key":"two","value":"é"}),
            },
        ];
        let receipt = SeatAppendReceipt {
            schema_version: "ryeos.ui.seat.append-receipt.v2".into(),
            producer_incarnation: "a".repeat(64),
            operation_id: "00000000-0000-4000-8000-000000000002".into(),
            first_engine_seq: 7,
            last_engine_seq: 8,
            event_count: events.len(),
            payload_digest: seat_payload_digest(&serde_json::to_value(&events).unwrap()).unwrap(),
        };
        let mut payloads = events
            .into_iter()
            .map(|event| {
                (
                    event.event_type,
                    json!({"seq":event.engine_seq,"payload":event.payload}),
                )
            })
            .collect::<Vec<_>>();
        payloads.push((
            SEAT_APPEND_RECEIPT_EVENT.into(),
            serde_json::to_value(&receipt).unwrap(),
        ));
        let records = payloads
            .into_iter()
            .enumerate()
            .map(|(index, (event_type, payload))| {
                let previous = format!("{:064x}", index);
                PersistedEventRecord {
                    event_id: 20 + index as i64,
                    event_hash: Some(format!("{:064x}", index + 1)),
                    chain_root_id: "root".into(),
                    chain_seq: 20 + index as i64,
                    thread_id: "thread".into(),
                    thread_seq: 10 + index as i64,
                    event_type,
                    storage_class: "indexed".into(),
                    ts: "2026-10-06T00:00:00Z".into(),
                    prev_chain_event_hash: Some(previous.clone()),
                    prev_thread_event_hash: Some(previous),
                    payload,
                }
            })
            .collect();
        (receipt, records)
    }

    #[test]
    fn live_ack_uses_the_exact_committed_slice_without_a_store() {
        let (receipt, records) = fixture();
        let (seq, events) = from_committed_append("root", "thread", &receipt, &records).unwrap();
        assert_eq!(seq, 22);
        assert_eq!(events.len(), 2);
        assert!(std::ptr::eq(events.as_ptr(), records.as_ptr()));
        assert_eq!(events[0].payload["seq"], 7);
        assert_eq!(events[0].chain_seq, 20);
        assert_eq!(events[0].thread_seq, 10);
    }

    #[test]
    fn live_ack_rejects_incomplete_extra_or_reordered_records() {
        let (receipt, records) = fixture();
        for removed in 0..records.len() {
            let mut altered = records.clone();
            altered.remove(removed);
            assert!(from_committed_append("root", "thread", &receipt, &altered).is_err());
        }
        let mut extra = records.clone();
        extra.insert(1, records[0].clone());
        assert!(from_committed_append("root", "thread", &receipt, &extra).is_err());
        let mut reordered = records.clone();
        reordered.swap(0, 1);
        assert!(from_committed_append("root", "thread", &receipt, &reordered).is_err());
        assert!(from_committed_append("root", "thread", &receipt, &[]).is_err());
    }

    #[test]
    fn live_ack_checks_receipt_identity_payload_and_coordinates() {
        let (receipt, records) = fixture();
        let changes: &[fn(&mut PersistedEventRecord)] = &[
            |event| event.chain_root_id = "other".into(),
            |event| event.thread_id = "other".into(),
            |event| event.chain_seq += 1,
            |event| event.thread_seq += 1,
            |event| event.event_hash = None,
            |event| event.event_type = "wrong".into(),
            |event| event.storage_class = "journal".into(),
            |event| event.prev_chain_event_hash = Some("b".repeat(64)),
            |event| event.prev_thread_event_hash = Some("b".repeat(64)),
            |event| event.payload["producer_incarnation"] = json!("b".repeat(64)),
            |event| event.payload["operation_id"] = json!("different"),
            |event| event.payload["payload_digest"] = json!("b".repeat(64)),
        ];
        for change in changes {
            let mut altered = records.clone();
            change(altered.last_mut().unwrap());
            assert!(from_committed_append("root", "thread", &receipt, &altered).is_err());
        }
    }

    #[test]
    fn live_ack_checks_each_event_and_complete_semantic_digest() {
        let (receipt, records) = fixture();
        let changes: &[fn(&mut PersistedEventRecord)] = &[
            |event| event.chain_root_id = "other".into(),
            |event| event.thread_id = "other".into(),
            |event| event.thread_seq += 1,
            |event| event.event_hash = Some("z".repeat(64)),
            |event| event.storage_class = "journal".into(),
            |event| event.prev_chain_event_hash = None,
            |event| event.prev_thread_event_hash = None,
            |event| event.payload["seq"] = json!(9),
            |event| {
                event.payload.as_object_mut().unwrap().remove("payload");
            },
            |event| event.payload["payload"]["value"] = json!("changed"),
        ];
        for change in changes {
            let mut altered = records.clone();
            change(&mut altered[1]);
            assert!(from_committed_append("root", "thread", &receipt, &altered).is_err());
        }
        let mut invalid_receipt = receipt;
        invalid_receipt.last_engine_seq = 9;
        assert!(
            validate_interval(
                "root",
                "thread",
                &invalid_receipt,
                22,
                &records[..2],
                "indexed"
            )
            .is_err()
        );
    }

    #[test]
    fn projected_interval_uses_durability_not_append_storage_selection() {
        let (receipt, records) = fixture();
        let mut projected = records.clone();
        for record in &mut projected {
            record.storage_class = "durable".into();
        }
        validate_projected_interval("root", "thread", &receipt, 22, &projected[..2]).unwrap();
        // Callers must choose the actual owner representation, not accept a
        // union of unrelated storage labels or silently change live provenance.
        assert!(
            validate_projected_interval("root", "thread", &receipt, 22, &records[..2]).is_err()
        );
        assert!(from_committed_append("root", "thread", &receipt, &projected).is_err());
        projected[1].storage_class = "ephemeral".into();
        assert!(
            validate_projected_interval("root", "thread", &receipt, 22, &projected[..2]).is_err()
        );
    }

    #[test]
    fn exact_retry_ack_omits_projection_ids_and_normalizes_event_metadata() {
        let (receipt, records) = fixture();
        let (seq, live) = from_committed_append("root", "thread", &receipt, &records).unwrap();
        let mut rebuilt = live.to_vec();
        for (index, event) in rebuilt.iter_mut().enumerate() {
            event.event_id = 900 + index as i64;
            event.storage_class = "durable".into();
        }
        validate_projected_interval("root", "thread", &receipt, seq, &rebuilt).unwrap();
        let receipt_row = records.last().unwrap();
        let receipt_hash = receipt_row.event_hash.as_deref().unwrap();
        let actual = super::super::append_acknowledgement(
            &receipt,
            seq,
            receipt_row.thread_seq,
            receipt_hash,
            live,
            "authoritative",
        );
        assert_eq!(
            actual,
            super::super::append_acknowledgement(
                &receipt,
                seq,
                receipt_row.thread_seq,
                receipt_hash,
                &rebuilt,
                "authoritative",
            )
        );
        for event in actual["events"].as_array().unwrap() {
            assert!(event.get("event_id").is_none());
            assert!(event.get("storage_class").is_none());
            assert_eq!(event["durability"], "durable");
        }
    }

    #[test]
    fn acknowledgement_retains_original_canonical_interval_evidence_not_engine_positions() {
        let (receipt, records) = fixture();
        let (seq, interval) = from_committed_append("root", "thread", &receipt, &records).unwrap();
        let receipt_row = records.last().unwrap();
        let response = super::super::append_acknowledgement(
            &receipt,
            seq,
            receipt_row.thread_seq,
            receipt_row.event_hash.as_deref().unwrap(),
            interval,
            "authoritative",
        );
        assert_eq!(response["events_authority"], "authoritative");
        assert_eq!(
            response["receipt_event_hash"],
            receipt_row.event_hash.as_ref().unwrap().as_str()
        );
        let rows = response["events"].as_array().unwrap();
        assert_eq!(rows.len(), receipt.event_count);
        for (row, original) in rows.iter().zip(interval) {
            // Wire coordinates are precision-preserving decimal strings.
            // Platform adapters normalize them before Core's retained decoder.
            let mut normalized = row.clone();
            for field in ["chain_seq", "thread_seq"] {
                let position = row[field].as_str().unwrap().parse::<u64>().unwrap();
                assert_eq!(position.to_string(), row[field].as_str().unwrap());
                normalized[field] = json!(position);
            }
            let retained =
                ryeos_client_base::ui::seat::retained_seat_event_with_evidence(&normalized)
                    .unwrap()
                    .unwrap();
            assert_eq!(
                retained.evidence.event_hash,
                original.event_hash.as_ref().unwrap().as_str()
            );
            assert_eq!(retained.evidence.chain_seq, original.chain_seq as u64);
            assert_eq!(retained.event.seq, original.chain_seq as u64);
            assert_eq!(
                retained.event.producer_engine_seq,
                original.payload["seq"].as_u64()
            );
            assert_ne!(
                retained.evidence.chain_seq,
                retained.event.producer_engine_seq.unwrap()
            );
            assert_eq!(retained.thread_id, original.thread_id);
            assert_eq!(retained.chain_root_id, original.chain_root_id);
            assert_eq!(row["payload"], original.payload);
        }
    }

    #[test]
    fn acknowledged_event_coordinates_keep_full_i64_precision_for_browser_clients() {
        let (_, mut records) = fixture();
        let beyond_js_safe_integer = 9_007_199_254_740_993i64;
        records[0].chain_seq = beyond_js_safe_integer;
        records[0].thread_seq = beyond_js_safe_integer;
        let wire = serde_json::to_value(wire_events(&records[..1])).unwrap();
        assert_eq!(
            wire[0]["chain_seq"],
            serde_json::json!(beyond_js_safe_integer.to_string())
        );
        assert_eq!(
            wire[0]["thread_seq"],
            serde_json::json!(beyond_js_safe_integer.to_string())
        );
    }
}
