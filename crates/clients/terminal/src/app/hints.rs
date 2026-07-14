//! Live-signal subscriptions: session hints and thread-braid tails.
//!
//! Hints are transient "look" signals — lossy by design; bound views
//! declaring `refresh.on_hint` refetch their sources when one arrives.
//! The thread tail follows the route facet's head thread so the
//! conversation timeline refreshes as braid events land.

use std::sync::Arc;

use crate::transport::daemon::DaemonClient;

#[derive(Debug)]
pub enum SessionMessage {
    Hint {
        kind: String,
        payload: serde_json::Value,
    },
    DaemonEvent {
        payload: serde_json::Value,
    },
    /// The session stream came back after a drop. Hints missed during
    /// the gap are gone (lossy by design), so the loop refetches
    /// everything once instead of trusting the stale frame.
    Resubscribed,
}

/// Subscribe to the session event bus and forward hints plus typed UI intents.
pub fn spawn_session_listener(
    client: Arc<DaemonClient>,
    tx: tokio::sync::mpsc::UnboundedSender<SessionMessage>,
) {
    tokio::spawn(async move {
        // The stream is the list's only live signal: if it dies without a
        // reconnect the UI silently freezes at the last flush (classic
        // "updates for a while, then never again" after a daemon restart
        // or an overnight idle drop). Resubscribe forever, with backoff.
        let mut backoff_ms = 500u64;
        let mut first_attach = true;
        loop {
            let Ok(mut stream) = client.open_session_events().await else {
                if tx.is_closed() {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(15_000);
                continue;
            };
            backoff_ms = 500;
            if !first_attach && tx.send(SessionMessage::Resubscribed).is_err() {
                return;
            }
            first_attach = false;
            run_session_stream(&mut stream, &tx).await;
            if tx.is_closed() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
        }
    });
}

async fn run_session_stream(
    stream: &mut crate::transport::daemon::SseStream,
    tx: &tokio::sync::mpsc::UnboundedSender<SessionMessage>,
) {
    while let Some(frame) = stream.next_event().await {
        if frame.event_type == "snapshot_required" {
            if tx.send(SessionMessage::Resubscribed).is_err() {
                break;
            }
            continue;
        }
        if frame.event_type == "ui_intent.applied" {
            let payload = serde_json::from_str::<serde_json::Value>(&frame.data)
                .unwrap_or(serde_json::Value::Null);
            if tx.send(SessionMessage::DaemonEvent { payload }).is_err() {
                break;
            }
            continue;
        }

        if frame.event_type == "message" || frame.event_type.ends_with(".hint") {
            let Some((kind, payload)) = serde_json::from_str::<serde_json::Value>(&frame.data)
                .ok()
                .and_then(|value| {
                    let payload = value.get("payload").cloned().unwrap_or(value);
                    let kind = payload.get("kind")?.as_str()?.to_string();
                    Some((kind, payload))
                })
            else {
                continue;
            };
            if tx.send(SessionMessage::Hint { kind, payload }).is_err() {
                break;
            }
        }
    }
}

/// Tail one chain's event stream, forwarding each frame's
/// `(event_type, raw json data)` to the loop. The loop dispatches a
/// `RyeOsEvent::ThreadTail`, so the shared reducer — not this client —
/// applies ryeos semantics (the same path the web `EventSource` uses).
/// The braid is the truth; this is it arriving now.
pub fn spawn_thread_tail(
    client: Arc<DaemonClient>,
    chain_root_id: String,
    tx: tokio::sync::mpsc::UnboundedSender<(String, String)>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Same reconnect contract as the session stream: a braid tail that
        // dies mid-watch would freeze the timeline. Gap healing rides the
        // hint-driven refetch; this only keeps the live edge flowing.
        let mut backoff_ms = 500u64;
        loop {
            if let Ok(mut stream) = client.open_thread_events(&chain_root_id).await {
                backoff_ms = 500;
                while let Some(frame) = stream.next_event().await {
                    if tx.send((frame.event_type, frame.data)).is_err() {
                        return;
                    }
                }
            }
            if tx.is_closed() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
            backoff_ms = (backoff_ms * 2).min(15_000);
        }
    })
}
