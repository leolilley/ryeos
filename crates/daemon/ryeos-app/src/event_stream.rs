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
use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;

use crate::state_store::PersistedEventRecord;

/// Capacity of live broadcast channels. Per-thread lanes emit roughly
/// one callback per LLM turn boundary plus per tool dispatch; the
/// firehose lane carries all threads and feeds lossy session-hint
/// bridges. 256 absorbs short bursts under typical local load. Lagged
/// per-thread tails recover via event-store replay; firehose consumers
/// are expected to treat hints as "look again" signals.
pub const DEFAULT_EVENT_STREAM_CAPACITY: usize = 256;

pub struct ThreadEventHub {
    inner: Mutex<HashMap<String, broadcast::Sender<PersistedEventRecord>>>,
    /// Firehose lane: every published event, regardless of thread.
    /// Feeds cross-cutting bridges (session hints); per-thread tails
    /// keep using the keyed lanes.
    all: broadcast::Sender<PersistedEventRecord>,
    capacity: usize,
}

impl ThreadEventHub {
    pub fn new(capacity: usize) -> Self {
        let (all, _rx) = broadcast::channel(capacity);
        Self {
            inner: Mutex::new(HashMap::new()),
            all,
            capacity,
        }
    }

    /// Subscribe to every published event (the firehose lane).
    pub fn subscribe_all(&self) -> broadcast::Receiver<PersistedEventRecord> {
        self.all.subscribe()
    }

    /// Recover from poisoning instead of panicking: the map only
    /// holds broadcast senders, and the event store — not this hub —
    /// is the durable source of truth, so a half-updated map after a
    /// panicked publisher is benign.
    fn lock_inner(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<String, broadcast::Sender<PersistedEventRecord>>> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Subscribe to a thread's live event stream. Lazily allocates
    /// the sender so SSE handlers can subscribe before the runtime
    /// has emitted anything.
    pub fn subscribe(&self, thread_id: &str) -> broadcast::Receiver<PersistedEventRecord> {
        let mut guard = self.lock_inner();
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
        let mut guard = self.lock_inner();
        let sender = guard.entry(thread_id.to_owned()).or_insert_with(|| {
            let (tx, _rx) = broadcast::channel(self.capacity);
            tx
        });
        let _ = self.all.send(event.clone());
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
        let mut guard = self.lock_inner();
        let sender = guard.entry(thread_id.to_owned()).or_insert_with(|| {
            let (tx, _rx) = broadcast::channel(self.capacity);
            tx
        });
        for ev in events {
            let _ = self.all.send(ev.clone());
            let _ = sender.send(ev.clone());
        }
        let receiver_count = sender.receiver_count();
        if receiver_count == 0 {
            // Lazy cleanup — see `publish`.
            guard.remove(thread_id);
        }
    }

    /// Publish a slice of persisted records to live subscribers in slice
    /// order, holding the hub lock once for the whole slice. Each record goes
    /// to its OWN thread's lane (records may span threads — a continuation
    /// touches source and successor) plus the firehose. Holding the lock for
    /// the whole slice means no concurrent publisher can interleave a
    /// higher-`chain_seq` event between two records of this slice — which a
    /// per-thread subscriber would otherwise drop as already-seen.
    pub fn publish_ordered(&self, records: &[PersistedEventRecord]) {
        if records.is_empty() {
            return;
        }
        let mut guard = self.lock_inner();
        for ev in records {
            let sender = guard.entry(ev.thread_id.clone()).or_insert_with(|| {
                let (tx, _rx) = broadcast::channel(self.capacity);
                tx
            });
            let _ = self.all.send(ev.clone());
            let _ = sender.send(ev.clone());
        }
        // Lazy cleanup: reclaim any lane we created (or already held) that has
        // no live subscribers. Records can repeat a thread, so dedupe via the
        // map state rather than the slice.
        for ev in records {
            if let Some(sender) = guard.get(&ev.thread_id) {
                if sender.receiver_count() == 0 {
                    guard.remove(&ev.thread_id);
                }
            }
        }
    }

    /// Reclaim a per-thread sender if no receivers remain. Called by
    /// the SSE endpoint when a subscriber disconnects so a long-lived
    /// daemon doesn't accumulate zombie channels for completed
    /// threads.
    pub fn remove_if_idle(&self, thread_id: &str) {
        let mut guard = self.lock_inner();
        if let Some(sender) = guard.get(thread_id) {
            if sender.receiver_count() == 0 {
                guard.remove(thread_id);
            }
        }
    }
}

/// RAII subscription guard for stream endpoints. It OWNS the receiver so
/// cleanup order is guaranteed: reclamation only runs after the receiver is
/// dropped, so `remove_if_idle` sees the true receiver count. A stream that
/// ends (client disconnect, thread completion) therefore leaves no idle
/// broadcast sender behind, even on the common single-subscriber path.
///
/// A chain tail follows successive heads with [`Self::follow`], which drops
/// the previous head's receiver and reclaims it immediately rather than
/// holding idle senders for the life of a long chain; every followed head is
/// also reclaimed as a backstop on drop.
pub struct HubSubscription {
    hub: Arc<ThreadEventHub>,
    followed: Vec<String>,
    current: String,
    rx: Option<broadcast::Receiver<PersistedEventRecord>>,
}

impl HubSubscription {
    /// Subscribe to `thread_id` and take ownership of the receiver.
    pub fn new(hub: Arc<ThreadEventHub>, thread_id: &str) -> Self {
        let rx = hub.subscribe(thread_id);
        Self {
            hub,
            followed: vec![thread_id.to_owned()],
            current: thread_id.to_owned(),
            rx: Some(rx),
        }
    }

    /// Receive the next live event for the current head.
    pub async fn recv(&mut self) -> Result<PersistedEventRecord, broadcast::error::RecvError> {
        self.rx
            .as_mut()
            .expect("receiver present until drop")
            .recv()
            .await
    }

    /// Non-blocking receive for draining buffered events (e.g. after a launch
    /// task joins).
    pub fn try_recv(&mut self) -> Result<PersistedEventRecord, broadcast::error::TryRecvError> {
        self.rx
            .as_mut()
            .expect("receiver present until drop")
            .try_recv()
    }

    /// Follow a new head: drop the previous receiver, reclaim the previous
    /// head's sender if now idle, then subscribe to the new head. No-op when
    /// already on `thread_id`.
    pub fn follow(&mut self, thread_id: &str) {
        if thread_id == self.current {
            return;
        }
        // Subscribe first, then replace — dropping the old receiver — and
        // reclaim the old head before recording the new one.
        let new_rx = self.hub.subscribe(thread_id);
        self.rx = Some(new_rx);
        let previous = std::mem::replace(&mut self.current, thread_id.to_owned());
        self.hub.remove_if_idle(&previous);
        if !self.followed.iter().any(|existing| existing == thread_id) {
            self.followed.push(thread_id.to_owned());
        }
    }
}

impl Drop for HubSubscription {
    fn drop(&mut self) {
        // Drop the receiver FIRST so `remove_if_idle` sees the decremented
        // count; otherwise the still-live receiver would keep the sender.
        self.rx = None;
        for thread_id in &self.followed {
            self.hub.remove_if_idle(thread_id);
        }
    }
}

impl std::fmt::Debug for ThreadEventHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.inner.lock().map(|g| g.len()).unwrap_or(0);
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
            event_hash: None,
            chain_root_id: thread_id.to_owned(),
            chain_seq: seq,
            thread_id: thread_id.to_owned(),
            thread_seq: seq,
            event_type: event_type.to_owned(),
            storage_class: "hot".to_owned(),
            ts: "2026-04-27T00:00:00Z".to_owned(),
            prev_chain_event_hash: None,
            prev_thread_event_hash: None,
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
    async fn publish_ordered_delivers_each_record_to_its_own_thread_lane() {
        // A continuation touches two threads; each subscriber sees only its
        // own record, and the firehose sees both in slice order.
        let hub = ThreadEventHub::new(8);
        let mut rx_src = hub.subscribe("T-src");
        let mut rx_succ = hub.subscribe("T-succ");
        let mut rx_all = hub.subscribe_all();

        let records = vec![
            sample_event("T-src", 5, "thread_continued"),
            sample_event("T-succ", 6, "thread_created"),
        ];
        hub.publish_ordered(&records);

        assert_eq!(rx_src.recv().await.unwrap().event_type, "thread_continued");
        assert_eq!(rx_succ.recv().await.unwrap().event_type, "thread_created");
        // Firehose preserves slice order.
        assert_eq!(rx_all.recv().await.unwrap().chain_seq, 5);
        assert_eq!(rx_all.recv().await.unwrap().chain_seq, 6);
    }

    #[tokio::test]
    async fn publish_ordered_reclaims_lanes_without_subscribers() {
        let hub = ThreadEventHub::new(8);
        // No subscribers on either thread: both lazily-created lanes reclaimed.
        hub.publish_ordered(&[
            sample_event("T-1", 1, "thread_created"),
            sample_event("T-2", 2, "thread_created"),
        ]);
        assert_eq!(hub.inner.lock().unwrap().len(), 0);
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

    #[tokio::test]
    async fn hub_subscription_reclaims_owned_sender_on_drop() {
        let hub = Arc::new(ThreadEventHub::new(8));
        {
            let _sub = HubSubscription::new(hub.clone(), "T-1");
            assert_eq!(hub.inner.lock().unwrap().len(), 1);
            // The guard owns the receiver, so its drop reclaims the sender
            // (no separate live receiver can keep it).
        }
        assert_eq!(hub.inner.lock().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn hub_subscription_follow_reclaims_old_head_immediately() {
        let hub = Arc::new(ThreadEventHub::new(8));
        let mut sub = HubSubscription::new(hub.clone(), "T-head1");
        assert_eq!(hub.inner.lock().unwrap().len(), 1);
        sub.follow("T-head2");
        // Old head reclaimed on switch (not held until stream end); only the
        // new head's sender remains.
        let keys: Vec<String> = hub.inner.lock().unwrap().keys().cloned().collect();
        assert_eq!(keys, vec!["T-head2".to_string()]);
        drop(sub);
        assert_eq!(hub.inner.lock().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn hub_subscription_keeps_sender_for_another_live_subscriber() {
        let hub = Arc::new(ThreadEventHub::new(8));
        let other = hub.subscribe("T-shared");
        {
            let _sub = HubSubscription::new(hub.clone(), "T-shared");
            assert_eq!(hub.inner.lock().unwrap().len(), 1);
        }
        // The guard dropped its own receiver, but another subscriber remains,
        // so the sender is kept.
        assert_eq!(hub.inner.lock().unwrap().len(), 1);
        drop(other);
    }
}
