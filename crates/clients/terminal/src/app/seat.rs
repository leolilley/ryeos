//! Seat-thread lifecycle: the seat is itself a thread — braided, owned,
//! replayable. Open or reattach on start, mirror local seat events into
//! the braid as they append, settle on exit. Fold-from-braid keeps live
//! and replay the same fold.

use ryeos_client_base::studio::{SeatEvent, SeatEventKind, StudioCore};

use crate::transport::daemon::DaemonClient;

/// Open the seat session thread (best effort: an unreachable seat
/// service degrades to a local-only seat, never a crash).
pub async fn open_seat_thread(client: &DaemonClient, core: &StudioCore) -> Option<String> {
    let surface_ref = core
        .data
        .session
        .as_ref()
        .map(|session| session.surface_ref.clone())?;
    let body = serde_json::json!({
        "item_ref": "service:seat/open",
        "parameters": { "surface_ref": surface_ref, "client_ref": "client:ryeos/tui" },
    });
    let envelope = client.signed_post("/execute", &body).await.ok()?;
    envelope
        .get("result")
        .and_then(|result| result.get("thread_id"))
        .and_then(|id| id.as_str())
        .map(str::to_string)
}

pub async fn reattach_seat_thread(client: &DaemonClient, core: &mut StudioCore) -> Option<String> {
    let surface_ref = core
        .data
        .session
        .as_ref()
        .map(|session| session.surface_ref.clone())?;
    // Discovery via `service:seat/list`: the daemon filters by the
    // `seat_session` kind, running status, and surface, and sorts freshest
    // first — the client just takes the most recent. The kind name stays
    // daemon-side rather than leaking into a `threads/list` client filter.
    let body = serde_json::json!({
        "item_ref": "service:seat/list",
        "parameters": { "surface_ref": surface_ref },
    });
    let envelope = client.signed_post("/execute", &body).await.ok()?;
    let thread_id = envelope
        .get("result")
        .and_then(|result| result.get("seats"))
        .and_then(serde_json::Value::as_array)?
        .first()
        .and_then(|seat| seat.get("thread_id"))
        .and_then(serde_json::Value::as_str)?
        .to_string();

    replay_seat_thread(client, core, &thread_id).await;
    Some(thread_id)
}

async fn replay_seat_thread(client: &DaemonClient, core: &mut StudioCore, thread_id: &str) {
    let body = serde_json::json!({
        "item_ref": "service:events/chain_replay",
        "parameters": { "chain_root_id": thread_id },
    });
    let Ok(envelope) = client.signed_post("/execute", &body).await else {
        return;
    };
    let Some(events) = envelope
        .get("result")
        .and_then(|result| result.get("events"))
        .and_then(serde_json::Value::as_array)
    else {
        return;
    };
    for event in events {
        if let Some(seat_event) = seat_event_from_replay(event) {
            core.seat.append_replayed(seat_event);
        }
    }
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

/// Mirror newly-appended seat events into the seat thread's braid. The
/// local log is the write-ahead view; the braid is the durable truth.
pub async fn sync_seat_braid(
    client: &DaemonClient,
    core: &StudioCore,
    seat_thread: &Option<String>,
    synced: &mut usize,
) {
    let Some(thread_id) = seat_thread else {
        return;
    };
    let events = core.seat.events();
    if events.len() <= *synced {
        return;
    }
    let batch: Vec<serde_json::Value> = events[*synced..]
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
        .collect();
    let body = serde_json::json!({
        "item_ref": "service:seat/append",
        "parameters": { "thread_id": thread_id, "events": batch },
    });
    if client.signed_post("/execute", &body).await.is_ok() {
        *synced = events.len();
    }
}

/// Settle the seat thread on clean exit; best effort.
pub async fn close_seat_thread(client: &DaemonClient, thread_id: &str) {
    let _ = client
        .signed_post(
            "/execute",
            &serde_json::json!({
                "item_ref": "service:seat/close",
                "parameters": { "thread_id": thread_id },
            }),
        )
        .await;
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
}
