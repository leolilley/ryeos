//! Data-only browser seat append transport sizing, shared by browser producers
//! and the node's lifecycle settlement preparation. This is not admission.

use serde::Serialize;

/// Conservative complete single-event transport envelope. Fixed UUID/digest
/// widths and maximum-u64 sequence strings match the browser/native writer.
pub fn single_seat_append_fits(
    presentation_id: &str,
    thread_id: &str,
    producer_incarnation: &str,
    event_type: &str,
    payload: &impl Serialize,
    max_append_bytes: u64,
) -> bool {
    if max_append_bytes == 0 {
        return false;
    }
    let body = serde_json::json!({
        "presentation_id":presentation_id,"thread_id":thread_id,
        "producer_incarnation":producer_incarnation,
        "operation_id":"00000000-0000-4000-8000-000000000000",
        "first_engine_seq":u64::MAX.to_string(),"last_engine_seq":u64::MAX.to_string(),
        "event_count":1,"payload_digest":"f".repeat(64),
        "events":[{"engine_seq":u64::MAX.to_string(),"event_type":event_type,"payload":payload}]
    });
    serde_json::to_vec(&body)
        .ok()
        .is_some_and(|bytes| bytes.len() as u64 <= max_append_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charges_complete_transport_not_only_payload() {
        let payload = serde_json::json!({"value":true});
        let fit = |max| {
            single_seat_append_fits("presentation", "thread", "f", "seat.test", &payload, max)
        };
        assert!(!fit(serde_json::to_vec(&payload).unwrap().len() as u64));
        assert!(!fit(0));
        assert!(fit(4096));
    }

    #[test]
    fn complete_envelope_accepts_exact_bound_and_rejects_one_byte_less() {
        let payload = serde_json::json!({"quoted":"a\"b\\c","nested":[true, null]});
        let presentation_id = "presentation";
        let thread_id = "T-seat";
        let producer = "f".repeat(64);
        let event_type = "ui_seat.transfer_lifecycle.v2";
        let body = serde_json::json!({
            "presentation_id":presentation_id,
            "thread_id":thread_id,
            "producer_incarnation":producer,
            "operation_id":"00000000-0000-4000-8000-000000000000",
            "first_engine_seq":u64::MAX.to_string(),
            "last_engine_seq":u64::MAX.to_string(),
            "event_count":1,
            "payload_digest":"f".repeat(64),
            "events":[{
                "engine_seq":u64::MAX.to_string(),
                "event_type":event_type,
                "payload":payload,
            }],
        });
        let exact = serde_json::to_vec(&body).unwrap().len() as u64;
        assert!(single_seat_append_fits(
            presentation_id,
            thread_id,
            &producer,
            event_type,
            &payload,
            exact,
        ));
        assert!(!single_seat_append_fits(
            presentation_id,
            thread_id,
            &producer,
            event_type,
            &payload,
            exact - 1,
        ));
    }
}
