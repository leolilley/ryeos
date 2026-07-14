//! Seat-thread lifecycle: the seat is itself a thread — braided, owned,
//! replayable. Open or reattach on start, mirror local seat events into
//! the braid as they append, settle on exit. Fold-from-braid keeps live
//! and replay the same fold.

use ryeos_client_base::ui::{SeatEvent, SeatEventKind};

use crate::transport::daemon::DaemonClient;

/// The transport half of seat startup: which thread carries this seat's
/// braid, and any facet history replayed off it. Pure data — the loop
/// folds it into the core when it arrives, so the daemon round trips
/// never gate the first frame.
pub struct SeatBootstrap {
    pub thread_id: Option<String>,
    pub replayed: Vec<SeatEvent>,
}

/// Reattach to the freshest owned seat for this surface, or open a new
/// one (best effort: an unreachable seat service degrades to a
/// local-only seat, never a crash).
pub async fn bootstrap_seat(client: &DaemonClient, surface_ref: &str) -> SeatBootstrap {
    if let Some((thread_id, replayed)) = reattach_seat_thread(client, surface_ref).await {
        return SeatBootstrap {
            thread_id: Some(thread_id),
            replayed,
        };
    }
    SeatBootstrap {
        thread_id: open_seat_thread(client, surface_ref).await,
        replayed: Vec::new(),
    }
}

/// Open the seat session thread.
pub async fn open_seat_thread(client: &DaemonClient, surface_ref: &str) -> Option<String> {
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

async fn reattach_seat_thread(
    client: &DaemonClient,
    surface_ref: &str,
) -> Option<(String, Vec<SeatEvent>)> {
    // Discovery via `service:seat/list`: the daemon filters by the
    // `seat_session` kind, running status, and surface, and sorts freshest
    // first — the client just takes the most recent. The kind name stays
    // daemon-side rather than leaking into a `threads/list` client filter.
    let body = serde_json::json!({
        "item_ref": "service:seat/list",
        "parameters": { "surface_ref": surface_ref },
    });
    let envelope = client.signed_post("/execute", &body).await.ok()?;
    let seats: Vec<String> = envelope
        .get("result")
        .and_then(|result| result.get("seats"))
        .and_then(serde_json::Value::as_array)?
        .iter()
        .filter_map(|seat| seat.get("thread_id").and_then(serde_json::Value::as_str))
        .map(str::to_string)
        .collect();
    let thread_id = seats.first()?.clone();
    // Discovery itself does not mutate presence. Renew/recreate the selected
    // runtime lease before replay so a freshly reattached seat cannot be reaped
    // during the first heartbeat interval.
    if !touch_seat_thread(client, &thread_id).await {
        return None;
    }

    // Supersede the stragglers: parallel clients share the newest seat by
    // construction (this same freshest-first pick), so a SECOND running
    // seat for the surface is always an orphan — a close that was skipped
    // by a crash, a kill, or a pre-settle exit. Settle them now or they
    // sit "running" in every thread listing forever.
    for orphan in seats.iter().skip(1) {
        close_seat_thread(client, orphan).await;
    }

    let replayed = replay_seat_thread(client, &thread_id).await;
    Some((thread_id, replayed))
}

async fn replay_seat_thread(client: &DaemonClient, thread_id: &str) -> Vec<SeatEvent> {
    let body = serde_json::json!({
        "item_ref": "service:events/chain_replay",
        "parameters": { "chain_root_id": thread_id },
    });
    let Ok(envelope) = client.signed_post("/execute", &body).await else {
        return Vec::new();
    };
    let Some(events) = envelope
        .get("result")
        .and_then(|result| result.get("events"))
        .and_then(serde_json::Value::as_array)
    else {
        return Vec::new();
    };
    events.iter().filter_map(seat_event_from_replay).collect()
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
    let body = serde_json::json!({
        "item_ref": "service:seat/append",
        "parameters": { "thread_id": thread_id, "events": events },
    });
    client.signed_post("/execute", &body).await.is_ok()
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

/// Refresh the runtime-only seat presence lease; best effort and deliberately
/// independent of durable seat events.
pub async fn touch_seat_thread(client: &DaemonClient, thread_id: &str) -> bool {
    client
        .signed_post(
            "/execute",
            &serde_json::json!({
                "item_ref": "service:seat/touch",
                "parameters": { "thread_id": thread_id },
            }),
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
}
