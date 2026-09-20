//! Seat-thread lifecycle: the seat is itself a thread — braided, owned,
//! replayable. Open or reattach on start, mirror local seat events into
//! the braid as they append, settle on exit. Fold-from-braid keeps live
//! and replay the same fold.

use ryeos_client_base::ui::{SeatEvent, SeatEventKind};

use crate::transport::daemon::DaemonClient;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct SeatBindingCoordinate {
    pub binding_attachment_id: String,
    pub binding_generation: u64,
    pub binding_digest: String,
}

impl From<&ryeos_client_base::ui::binding::UiBindingAttachment> for SeatBindingCoordinate {
    fn from(attachment: &ryeos_client_base::ui::binding::UiBindingAttachment) -> Self {
        Self {
            binding_attachment_id: attachment.binding_attachment_id.clone(),
            binding_generation: attachment.binding_generation,
            binding_digest: attachment.binding_digest.clone(),
        }
    }
}

/// The transport half of seat startup: which thread carries this seat's
/// braid, and any facet history replayed off it. Pure data — the loop
/// folds it into the core when it arrives, so the daemon round trips
/// never gate the first frame.
pub struct SeatBootstrap {
    pub thread_id: String,
    pub replayed: Vec<SeatEvent>,
}

/// Reattach to the freshest owned seat for this surface, or open a new
/// one. A daemon-backed UI never degrades to an engine-local seat: doing so
/// would silently discard the durable session authority the user requested.
pub async fn bootstrap_seat(
    client: &DaemonClient,
    binding: &SeatBindingCoordinate,
) -> Result<SeatBootstrap, String> {
    let (thread_id, replayed) = reattach_seat_thread(client, binding).await?;
    Ok(SeatBootstrap {
        thread_id,
        replayed,
    })
}

/// Open the seat session thread.
pub async fn open_seat_thread(
    client: &DaemonClient,
    binding: &SeatBindingCoordinate,
) -> Result<String, String> {
    let body = serde_json::to_value(binding)
        .map_err(|error| format!("encode durable UI seat binding: {error}"))?;
    let envelope = client
        .signed_post("/ui/api/session/seat/open", &body)
        .await
        .map_err(|error| format!("open durable UI seat: {error}"))?;
    envelope
        .get("result")
        .and_then(|result| result.get("thread_id"))
        .and_then(|id| id.as_str())
        .map(str::to_string)
        .ok_or_else(|| "open durable UI seat: response omitted thread_id".to_string())
}

async fn reattach_seat_thread(
    client: &DaemonClient,
    binding: &SeatBindingCoordinate,
) -> Result<(String, Vec<SeatEvent>), String> {
    // The session endpoint atomically reattaches the freshest owned seat or
    // creates one. Clients never enumerate seat-session threads or author the
    // execution policy that owns them.
    let body = serde_json::to_value(binding)
        .map_err(|error| format!("encode durable UI seat binding: {error}"))?;
    let envelope = client
        .signed_post("/ui/api/session/seat/open", &body)
        .await
        .map_err(|error| format!("reattach durable UI seat: {error}"))?;
    let thread_id = envelope
        .get("result")
        .and_then(|result| result.get("thread_id"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "reattach durable UI seat: response omitted thread_id".to_string())?
        .to_string();
    let replayed = replay_seat_thread(client, &thread_id).await?;
    Ok((thread_id, replayed))
}

async fn replay_seat_thread(
    client: &DaemonClient,
    thread_id: &str,
) -> Result<Vec<SeatEvent>, String> {
    let body = serde_json::json!({ "chain_root_id": thread_id });
    let envelope = client
        .signed_post("/ui/api/session/seat/replay", &body)
        .await
        .map_err(|error| format!("replay durable UI seat: {error}"))?;
    let Some(events) = envelope
        .get("result")
        .and_then(|result| result.get("events"))
        .and_then(serde_json::Value::as_array)
    else {
        return Err("replay durable UI seat: response omitted events".to_string());
    };
    Ok(events.iter().filter_map(seat_event_from_replay).collect())
}

fn seat_event_from_replay(event: &serde_json::Value) -> Option<SeatEvent> {
    let event_type = event.get("event_type")?.as_str()?;
    if event_type != "seat.facet" {
        return None;
    }
    let payload = event.get("payload")?;
    let facet = payload.get("payload").unwrap_or(payload);
    let key = facet.get("key")?.as_str()?.to_string();
    let value = facet.get("value")?.clone();
    let seq = payload
        .get("seq")
        .and_then(serde_json::Value::as_u64)
        .or_else(|| event.get("chain_seq").and_then(serde_json::Value::as_u64))
        .unwrap_or(0);
    Some(SeatEvent {
        seq,
        kind: SeatEventKind::Facet { key, value },
    })
}

/// Serialize newly-appended seat events for the braid mirror. The local
/// log is the write-ahead view; the braid is the durable truth.
pub fn braid_batch(events: &[SeatEvent]) -> Vec<serde_json::Value> {
    events
        .iter()
        .filter_map(|event| serde_json::to_value(event).ok())
        .filter_map(|value| {
            let event_type = value.get("event_type")?.as_str()?.to_string();
            Some(serde_json::json!({
                "event_type": event_type,
                "payload": {
                    "seq": value.get("seq"),
                    "payload": value.get("payload"),
                },
            }))
        })
        .collect()
}

/// Append one mirrored batch to the seat thread's braid. A single writer
/// task calls this with at most one batch in flight, so braid order
/// matches local append order without the loop ever waiting on it.
pub async fn append_braid(
    client: &DaemonClient,
    thread_id: &str,
    events: Vec<serde_json::Value>,
) -> bool {
    let body = serde_json::json!({ "thread_id": thread_id, "events": events });
    client
        .signed_post("/ui/api/session/seat/append", &body)
        .await
        .is_ok()
}

/// Settle the seat thread on clean exit; best effort.
pub async fn close_seat_thread(client: &DaemonClient, thread_id: &str) {
    let _ = client
        .signed_post(
            "/ui/api/session/seat/close",
            &serde_json::json!({ "thread_id": thread_id }),
        )
        .await;
}

/// Refresh the runtime-only seat presence lease; best effort and deliberately
/// independent of durable seat events.
pub async fn touch_seat_thread(client: &DaemonClient, thread_id: &str) -> bool {
    client
        .signed_post(
            "/ui/api/session/seat/touch",
            &serde_json::json!({ "thread_id": thread_id }),
        )
        .await
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn replay_parser_accepts_persisted_seat_facet_shape() {
        let event = json!({
            "chain_seq": 4,
            "event_type": "seat.facet",
            "payload": {
                "seq": 2,
                "payload": {
                    "key": "selection",
                    "value": { "item": "thread-1" }
                }
            }
        });

        let seat_event = seat_event_from_replay(&event).expect("seat event");

        assert_eq!(seat_event.seq, 2);
        assert_eq!(
            seat_event.kind,
            SeatEventKind::Facet {
                key: "selection".to_string(),
                value: json!({ "item": "thread-1" }),
            }
        );
    }

    #[test]
    fn replay_parser_ignores_non_seat_events() {
        let event = json!({
            "chain_seq": 4,
            "event_type": "thread.started",
            "payload": {}
        });

        assert!(seat_event_from_replay(&event).is_none());
    }

    #[test]
    fn seat_binding_coordinate_serializes_as_flat_exact_triple() {
        let coordinate = SeatBindingCoordinate {
            binding_attachment_id: "attachment-7".to_string(),
            binding_generation: 4,
            binding_digest: "digest-7".to_string(),
        };

        assert_eq!(
            serde_json::to_value(coordinate).expect("serialize seat coordinate"),
            json!({
                "binding_attachment_id": "attachment-7",
                "binding_generation": 4,
                "binding_digest": "digest-7",
            })
        );
    }
}
