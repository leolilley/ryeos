//! Per-session event bus for browser clients.
//!
//! `SessionBus` is a `tokio::broadcast` channel per session ID. The daemon
//! publishes bundle events (thread changes, capability updates, health) and
//! the `/ui/events/session/{session_id}` SSE endpoint subscribes and frames
//! them as `RouteStreamEnvelope` items.
//!
//! ## Replay
//!
//! A bounded replay ring stores the last N events per session. When a client
//! reconnects with `Last-Event-ID`, the bus replays from the ring. If the
//! gap exceeds ring capacity, a `snapshot_required` event is emitted instead,
//! forcing the client to re-bootstrap.

use std::collections::HashMap;
use std::sync::Mutex;

#[cfg(test)]
use serde_json::json;
use serde_json::Value;
use tokio::sync::broadcast;

use ryeos_app::stream_envelope::RouteStreamEnvelope;

/// Default broadcast channel capacity per session.
const DEFAULT_CHANNEL_CAPACITY: usize = 256;

/// Default replay ring size per session.
const DEFAULT_RING_SIZE: usize = 128;

/// Per-session event bus.
pub struct SessionBus {
    inner: Mutex<SessionBusInner>,
    capacity: usize,
    ring_size: usize,
}

struct SessionBusInner {
    channels: HashMap<String, broadcast::Sender<RouteStreamEnvelope>>,
    rings: HashMap<String, Vec<RingEntry>>,
}

#[derive(Debug, Clone)]
struct RingEntry {
    id: String,
    envelope: RouteStreamEnvelope,
}

impl Default for SessionBus {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionBus {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(SessionBusInner {
                channels: HashMap::new(),
                rings: HashMap::new(),
            }),
            capacity: DEFAULT_CHANNEL_CAPACITY,
            ring_size: DEFAULT_RING_SIZE,
        }
    }

    /// Subscribe to a session's event stream. Creates the channel if needed.
    pub fn subscribe(&self, session_id: &str) -> broadcast::Receiver<RouteStreamEnvelope> {
        let mut inner = self.inner.lock().unwrap();
        let sender = inner
            .channels
            .entry(session_id.to_string())
            .or_insert_with(|| broadcast::channel(self.capacity).0);
        sender.subscribe()
    }

    /// Publish an event to a session's subscribers.
    pub fn publish(&self, session_id: &str, event_type: &str, payload: Value) {
        let id = uuid::Uuid::new_v4().to_string();
        let envelope = RouteStreamEnvelope::with_id(&id, event_type, payload);
        let mut inner = self.inner.lock().unwrap();

        // Store in replay ring.
        let ring = inner.rings.entry(session_id.to_string()).or_default();
        ring.push(RingEntry {
            id: id.clone(),
            envelope: envelope.clone(),
        });
        if ring.len() > self.ring_size {
            ring.remove(0);
        }

        // Broadcast (ignore send error — no subscribers is fine).
        if let Some(sender) = inner.channels.get(session_id) {
            let _ = sender.send(envelope);
        }
    }

    /// Replay events after the given last event ID. Returns `None` if the
    /// gap exceeds ring capacity (caller should emit `snapshot_required`).
    pub fn replay_after(
        &self,
        session_id: &str,
        last_id: &str,
    ) -> Option<Vec<RouteStreamEnvelope>> {
        let inner = self.inner.lock().unwrap();
        let ring = inner.rings.get(session_id)?;

        // Find the position of last_id.
        let pos = ring.iter().position(|e| e.id == last_id)?;

        // Return everything after that position.
        let events: Vec<_> = ring[(pos + 1)..]
            .iter()
            .map(|e| e.envelope.clone())
            .collect();
        Some(events)
    }

    /// Create a `snapshot_required` envelope.
    pub fn snapshot_required_envelope() -> RouteStreamEnvelope {
        RouteStreamEnvelope::new(
            "snapshot_required",
            serde_json::json!({"reason": "event gap exceeds replay ring; re-bootstrap required"}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn subscribe_and_publish() {
        let bus = SessionBus::new();
        let mut rx = bus.subscribe("session-1");

        bus.publish("session-1", "thread.upsert", json!({"id": "t1"}));

        let event = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .expect("timeout")
            .expect("recv error");

        assert_eq!(event.event_type, "thread.upsert");
        assert_eq!(event.payload["id"], "t1");
    }

    #[tokio::test]
    async fn publish_without_subscribers_is_ok() {
        let bus = SessionBus::new();
        // Should not panic.
        bus.publish("session-no-sub", "test.event", json!({}));
    }

    #[test]
    fn replay_after_returns_missed_events() {
        let bus = SessionBus::new();
        let mut rx = bus.subscribe("session-replay");

        bus.publish("session-replay", "event.1", json!({"n": 1}));
        bus.publish("session-replay", "event.2", json!({"n": 2}));
        bus.publish("session-replay", "event.3", json!({"n": 3}));

        // Drain the channel.
        let e1 = rx.try_recv().unwrap();
        let _e2 = rx.try_recv().unwrap();
        let _e3 = rx.try_recv().unwrap();

        // Replay after e1 should return e2, e3.
        let replayed = bus.replay_after("session-replay", &e1.id.unwrap()).unwrap();
        assert_eq!(replayed.len(), 2);
        assert_eq!(replayed[0].payload["n"], 2);
        assert_eq!(replayed[1].payload["n"], 3);
    }

    #[test]
    fn replay_after_unknown_id_returns_none() {
        let bus = SessionBus::new();
        bus.subscribe("session-x");
        bus.publish("session-x", "event.1", json!({}));

        assert!(bus.replay_after("session-x", "nonexistent-id").is_none());
    }

    #[test]
    fn snapshot_required_envelope_has_correct_type() {
        let env = SessionBus::snapshot_required_envelope();
        assert_eq!(env.event_type, "snapshot_required");
    }
}
