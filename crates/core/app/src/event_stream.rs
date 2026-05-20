//! Per-thread live event broadcaster used by the SSE bridge.
//!
//! The runtime appends events through the token-gated UDS callback
//! `runtime.append_event(s)`. Those events land in the persistent
//! event store via `EventStoreService::append`. After persistence
//! succeeds, the UDS handler also publishes the same
//! `PersistedEventRecord` into a per-thread `tokio::sync::broadcast`
//! channel maintained by `ThreadEventHub` so HTTP `/execute/stream`
//! subscribers can tail events in real time.
//!
//! Persistence-first ordering is the contract: a published event has
//! already been recorded in the event store with its monotonic
//! `chain_seq`. SSE consumers tolerate broadcast lag by replaying
//! from the event store at `after_chain_seq = last_seen_seq` (handled
//! at the SSE endpoint, not here).
//!
//! ## Lifecycle
//!
//! - `subscribe(thread_id)` lazily creates a sender and returns a
//!   fresh receiver. Subscribers may attach before any publish.
//! - `publish(thread_id, event)` lazily creates a sender if needed
//!   and broadcasts. If no subscribers are attached, the send drops
//!   silently — the event store is the durable source of truth.
//! - `remove_if_idle(thread_id)` reclaims the sender when no
//!   receivers remain, called by the SSE endpoint on disconnect.

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::broadcast;

use crate::state_store::PersistedEventRecord;

/// Capacity of each per-thread broadcast channel. The runtime emits
/// roughly one callback per LLM turn boundary plus per tool dispatch;
/// 256 absorbs short bursts without dropping under typical load.
/// Lagged subscribers recover via event-store replay so this is a
/// latency knob, not a correctness boundary.
pub const DEFAULT_EVENT_STREAM_CAPACITY: usize = 256;

pub struct ThreadEventHub {
    inner: Mutex<HashMap<String, broadcast::Sender<PersistedEventRecord>>>,
    capacity: usize,
}

impl ThreadEventHub {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            capacity,
        }
    }

    /// Subscribe to a thread's live event stream. Lazily allocates
    /// the sender so SSE handlers can subscribe before the runtime
    /// has emitted anything.
    pub fn subscribe(&self, thread_id: &str) -> broadcast::Receiver<PersistedEventRecord> {
        let mut guard = self.inner.lock().expect("ThreadEventHub mutex poisoned");
        guard
            .entry(thread_id.to_owned())
            .or_insert_with(|| {
                let (tx, _rx) = broadcast::channel(self.capacity);
                tx
            })
            .subscribe()
    }

    /// Publish a persisted event to all live subscribers for the
    /// thread. Best-effort: if no receivers are attached the send
    /// returns `Err(SendError)` and we drop — the event has already
    /// been persisted by the caller.
    pub fn publish(&self, thread_id: &str, event: PersistedEventRecord) {
        let mut guard = self.inner.lock().expect("ThreadEventHub mutex poisoned");
        let sender = guard
            .entry(thread_id.to_owned())
            .or_insert_with(|| {
                let (tx, _rx) = broadcast::channel(self.capacity);
                tx
            });
        let _ = sender.send(event);
        let receiver_count = sender.receiver_count();
        if receiver_count == 0 {
            // Lazy cleanup: no live subscribers means this thread's
            // sender is dead weight. Reclaim the entry; the next
            // subscriber recreates lazily.
            guard.remove(thread_id);
        }
    }

    /// Bulk publish in order. Used by `runtime.append_events` so a
    /// batched callback delivers contiguous `chain_seq` values to
    /// subscribers atomically (one lock acquire instead of N).
    pub fn publish_batch(&self, thread_id: &str, events: &[PersistedEventRecord]) {
        if events.is_empty() {
            return;
        }
        let mut guard = self.inner.lock().expect("ThreadEventHub mutex poisoned");
        let sender = guard
            .entry(thread_id.to_owned())
            .or_insert_with(|| {
                let (tx, _rx) = broadcast::channel(self.capacity);
                tx
            });
        for ev in events {
            let _ = sender.send(ev.clone());
        }
        let receiver_count = sender.receiver_count();
        if receiver_count == 0 {
            // Lazy cleanup — see `publish`.
            guard.remove(thread_id);
        }
    }

    /// Reclaim a per-thread sender if no receivers remain. Called by
    /// the SSE endpoint when a subscriber disconnects so a long-lived
    /// daemon doesn't accumulate zombie channels for completed
    /// threads.
    pub fn remove_if_idle(&self, thread_id: &str) {
        let mut guard = self.inner.lock().expect("ThreadEventHub mutex poisoned");
        if let Some(sender) = guard.get(thread_id) {
            if sender.receiver_count() == 0 {
                guard.remove(thread_id);
            }
        }
    }
}

impl std::fmt::Debug for ThreadEventHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self
            .inner
            .lock()
            .map(|g| g.len())
            .unwrap_or(0);
        f.debug_struct("ThreadEventHub")
            .field("active_threads", &count)
            .field("capacity", &self.capacity)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_event(thread_id: &str, seq: i64, event_type: &str) -> PersistedEventRecord {
        PersistedEventRecord {
            event_id: seq,
            chain_root_id: thread_id.to_owned(),
            chain_seq: seq,
            thread_id: thread_id.to_owned(),
            thread_seq: seq,
            event_type: event_type.to_owned(),
            storage_class: "hot".to_owned(),
            ts: "2026-04-27T00:00:00Z".to_owned(),
            payload: json!({"i": seq}),
        }
    }

    #[tokio::test]
    async fn subscribe_then_publish_delivers() {
        let hub = ThreadEventHub::new(8);
        let mut rx = hub.subscribe("T-x");
        hub.publish("T-x", sample_event("T-x", 1, "stream_opened"));
        let got = rx.recv().await.expect("receive");
        assert_eq!(got.event_type, "stream_opened");
        assert_eq!(got.chain_seq, 1);
    }

    #[tokio::test]
    async fn publish_with_no_subscribers_drops_silently() {
        let hub = ThreadEventHub::new(8);
        // Must not panic; broadcast::Sender::send returns Err with no
        // receivers and the hub deliberately ignores it because
        // persistence already happened upstream.
        hub.publish("T-y", sample_event("T-y", 1, "turn_start"));
    }

    #[tokio::test]
    async fn publish_with_no_subscribers_lazily_cleans_up() {
        let hub = ThreadEventHub::new(8);
        hub.publish("T-cleanup", sample_event("T-cleanup", 1, "turn_start"));
        // No subscriber was ever attached — the lazily-created sender
        // is reclaimed so the hub does not accumulate zombie channels.
        let count = hub.inner.lock().unwrap().len();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn publish_keeps_entry_when_subscriber_attached() {
        let hub = ThreadEventHub::new(8);
        let _rx = hub.subscribe("T-keep");
        hub.publish("T-keep", sample_event("T-keep", 1, "turn_start"));
        // A live subscriber means the entry is still useful.
        let count = hub.inner.lock().unwrap().len();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn batch_publish_preserves_order() {
        let hub = ThreadEventHub::new(8);
        let mut rx = hub.subscribe("T-z");
        let events: Vec<_> = (1..=3)
            .map(|i| sample_event("T-z", i, "tool_dispatch"))
            .collect();
        hub.publish_batch("T-z", &events);
        for expected_seq in 1..=3 {
            let got = rx.recv().await.expect("receive");
            assert_eq!(got.chain_seq, expected_seq);
        }
    }

    #[tokio::test]
    async fn remove_if_idle_reclaims_when_no_receivers() {
        let hub = ThreadEventHub::new(8);
        let rx = hub.subscribe("T-a");
        drop(rx);
        hub.remove_if_idle("T-a");
        let count = hub.inner.lock().unwrap().len();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn remove_if_idle_keeps_when_receiver_attached() {
        let hub = ThreadEventHub::new(8);
        let _rx = hub.subscribe("T-b");
        hub.remove_if_idle("T-b");
        let count = hub.inner.lock().unwrap().len();
        assert_eq!(count, 1);
    }
}
