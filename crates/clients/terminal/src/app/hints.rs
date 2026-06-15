//! Live-signal subscriptions: session hints and thread-braid tails.
//!
//! Hints are transient "look" signals — lossy by design; bound views
//! declaring `refresh.on_hint` refetch their sources when one arrives.
//! The thread tail follows the route facet's head thread so the
//! conversation timeline refreshes as braid events land.

use std::sync::Arc;

use crate::transport::daemon::DaemonClient;
use crate::transport::sse::SseEvent;

/// Subscribe to the session event bus and forward hint kinds.
pub fn spawn_hint_listener(
    client: Arc<DaemonClient>,
    tx: tokio::sync::mpsc::UnboundedSender<String>,
) {
    tokio::spawn(async move {
        let Ok(mut stream) = client.open_session_events().await else {
            return;
        };
        while let Some(event) = stream.next_event().await {
            if let SseEvent::Unknown { event_type, data } = &event {
                if event_type == "message" || event_type.ends_with(".hint") {
                    let kind = serde_json::from_str::<serde_json::Value>(data)
                        .ok()
                        .and_then(|value| {
                            value
                                .get("payload")
                                .and_then(|p| p.get("kind"))
                                .or_else(|| value.get("kind"))
                                .and_then(|k| k.as_str())
                                .map(str::to_string)
                        });
                    if let Some(kind) = kind {
                        if tx.send(kind).is_err() {
                            break;
                        }
                    }
                }
            }
        }
    });
}

/// Tail one thread's event stream, forwarding each frame's
/// `(event_type, raw json data)` to the loop. The loop dispatches a
/// `StudioEvent::ThreadTail`, so the shared reducer — not this client —
/// applies ryeos semantics (the same path the web `EventSource` uses).
/// The braid is the truth; this is it arriving now.
pub fn spawn_thread_tail(
    client: Arc<DaemonClient>,
    thread_id: String,
    tx: tokio::sync::mpsc::UnboundedSender<(String, String)>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let Ok(mut stream) = client.open_thread_events(&thread_id).await else {
            return;
        };
        while let Some(event) = stream.next_event().await {
            let frame = match event {
                SseEvent::Unknown { event_type, data } => (event_type, data),
                SseEvent::TextDelta { text } => (
                    "token_delta".to_string(),
                    serde_json::json!({ "text": text }).to_string(),
                ),
                // The chain tail frames events by runtime type name (→
                // Unknown); other parsed variants don't occur here.
                _ => continue,
            };
            if tx.send(frame).is_err() {
                break;
            }
        }
    })
}
