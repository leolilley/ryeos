//! `LiveInputQueue` — per-thread queue of operator inputs awaiting delivery
//! to a *running* directive thread.
//!
//! The queue is a daemon-side staging area, **not** the source of truth. An
//! enqueued [`LiveInput`] only becomes a durable braid event when the
//! running runtime polls (`runtime.poll_input`) and the daemon appends it
//! through the running-guarded path (`append_events_if_thread_running`). The
//! queue just lets the operator's submit return immediately while the live
//! loop folds the input at its own next turn boundary.
//!
//! ## Terminal safety (the enqueue / finalize race)
//!
//! Each entry carries a `closed` tombstone. Thread finalization (every terminal
//! status) calls [`LiveInputQueue::close`] *after* the terminal state write,
//! clearing any queued inputs and rejecting future enqueues for that thread.
//! Combined with the running-guard on the persist path, this gives two
//! invariants from any ordering of a concurrent enqueue and finalize:
//!
//! - **No `cognition_in` is appended after terminal.** Even if an enqueue lands
//!   in the tiny window between the terminal state write and `close`, the poll's
//!   `append_events_if_thread_running` re-checks `running` under the state lock
//!   and discards it.
//! - **No queued input survives terminal.** `close` clears the queue under
//!   the queue lock; a steer that didn't get polled before the thread
//!   finalized is honestly dropped (the operator's recourse is the settled
//!   chained-resume path — a finalized execution is done).
//!
//! An entry is created lazily on first enqueue, so only threads an operator has
//! actually intervened on hold an entry.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use ryeos_state::objects::LiveInput;

/// Default cap on inputs queued for a single thread before backpressure. A
/// human operator steering one execution will never approach this; the bound
/// exists only to refuse a runaway/automated submitter rather than grow
/// unboundedly.
pub const DEFAULT_MAX_PENDING_PER_THREAD: usize = 32;

#[derive(Debug, Default)]
struct ThreadEntry {
    queue: VecDeque<LiveInput>,
    /// Items drained for an in-flight poll but not yet committed or restored.
    /// Counted toward the bound so concurrent enqueues during the persist window
    /// can't overfill: a restore of the drained items then never exceeds the cap.
    in_flight: usize,
    /// Set once the thread reaches any terminal status. A closed entry rejects
    /// enqueues and never accepts restored items.
    closed: bool,
}

impl ThreadEntry {
    fn load(&self) -> usize {
        self.queue.len() + self.in_flight
    }
}

/// Result of attempting to enqueue an input for a thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueOutcome {
    /// Accepted; `pending` is the queue depth after the push.
    Accepted { pending: usize },
    /// The thread has finalized — the entry is closed. The caller should refuse
    /// (the live window is over; fall back to a chained-resume).
    Closed,
    /// The per-thread bound is reached; the caller should refuse without
    /// enqueueing. `pending` is the (unchanged) queue depth.
    Full { pending: usize },
}

/// Per-thread staging queue for operator live input.
#[derive(Debug)]
pub struct LiveInputQueue {
    entries: Mutex<HashMap<String, ThreadEntry>>,
    max_pending_per_thread: usize,
}

impl Default for LiveInputQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveInputQueue {
    pub fn new() -> Self {
        Self::with_bound(DEFAULT_MAX_PENDING_PER_THREAD)
    }

    pub fn with_bound(max_pending_per_thread: usize) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            max_pending_per_thread: max_pending_per_thread.max(1),
        }
    }

    /// Queue an input for `thread_id`. Lazily creates the entry. Rejects if
    /// the entry is closed (thread terminal) or full (backpressure).
    ///
    /// The caller is expected to have already gated on the thread being a
    /// running, follow-up-eligible directive; the `closed` check here is the
    /// race backstop for a finalize that landed after that gate.
    pub fn enqueue(&self, thread_id: &str, input: LiveInput) -> EnqueueOutcome {
        let mut entries = self.entries.lock().unwrap();
        let entry = entries.entry(thread_id.to_string()).or_default();
        if entry.closed {
            return EnqueueOutcome::Closed;
        }
        // Bound the queued PLUS in-flight (drained-not-yet-committed) items, so a
        // restore of the in-flight items after concurrent enqueues can't exceed
        // the cap.
        if entry.load() >= self.max_pending_per_thread {
            return EnqueueOutcome::Full {
                pending: entry.load(),
            };
        }
        entry.queue.push_back(input);
        EnqueueOutcome::Accepted {
            pending: entry.load(),
        }
    }

    /// Take all pending inputs for `thread_id` in FIFO order, leaving the entry
    /// present (so a `closed` tombstone persists). Returns empty if there is no
    /// entry or nothing pending.
    ///
    /// The drained items are *not yet durable* — the caller MUST resolve every
    /// drain with exactly one of [`Self::ack_drained`] (persisted or discarded)
    /// or [`Self::restore_front`] (transient failure), which clears the in-flight
    /// reservation. Until then the drained items stay counted toward the bound.
    pub fn drain(&self, thread_id: &str) -> Vec<LiveInput> {
        let mut entries = self.entries.lock().unwrap();
        match entries.get_mut(thread_id) {
            Some(entry) if !entry.queue.is_empty() => {
                let items: Vec<LiveInput> = entry.queue.drain(..).collect();
                entry.in_flight += items.len();
                items
            }
            _ => Vec::new(),
        }
    }

    /// Release the in-flight reservation for `n` drained items that are now
    /// resolved WITHOUT re-queuing — either persisted durably or discarded
    /// (thread terminal). Pairs with a [`Self::drain`] of `n` items.
    pub fn ack_drained(&self, thread_id: &str, n: usize) {
        if n == 0 {
            return;
        }
        let mut entries = self.entries.lock().unwrap();
        if let Some(entry) = entries.get_mut(thread_id) {
            entry.in_flight = entry.in_flight.saturating_sub(n);
        }
    }

    /// Return previously-drained items to the FRONT of the queue (FIFO
    /// preserved) after a transient persist failure. No-op if the entry has
    /// since been closed (the thread finalized) — those items are honestly
    /// dropped.
    pub fn restore_front(&self, thread_id: &str, items: Vec<LiveInput>) {
        if items.is_empty() {
            return;
        }
        let mut entries = self.entries.lock().unwrap();
        if let Some(entry) = entries.get_mut(thread_id) {
            // Release the in-flight reservation regardless — these items are no
            // longer in flight after this call.
            entry.in_flight = entry.in_flight.saturating_sub(items.len());
            if entry.closed {
                // Thread finalized while the poll was in flight: honestly drop.
                return;
            }
            for item in items.into_iter().rev() {
                entry.queue.push_front(item);
            }
        }
    }

    /// Mark a thread terminal: clear its queue and reject future enqueues.
    /// Idempotent. Call from the common lifecycle finalize *after* the terminal
    /// state write.
    pub fn close(&self, thread_id: &str) {
        let mut entries = self.entries.lock().unwrap();
        let entry = entries.entry(thread_id.to_string()).or_default();
        entry.queue.clear();
        // In-flight items are already persisted-or-dropped by their poll's
        // ack/restore; a terminal thread keeps no reservation.
        entry.in_flight = 0;
        entry.closed = true;
    }

    /// Queue depth for a thread (0 if no entry). Test/inspection helper.
    pub fn pending_len(&self, thread_id: &str) -> usize {
        self.entries
            .lock()
            .unwrap()
            .get(thread_id)
            .map(|i| i.queue.len())
            .unwrap_or(0)
    }

    /// Whether a thread's entry has been closed (finalized). Test helper.
    pub fn is_closed(&self, thread_id: &str) -> bool {
        self.entries
            .lock()
            .unwrap()
            .get(thread_id)
            .map(|i| i.closed)
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_state::objects::LiveInputIntent;

    fn steer(content: &str) -> LiveInput {
        LiveInput::new(content, LiveInputIntent::Steer)
    }

    #[test]
    fn enqueue_then_drain_is_fifo() {
        let q = LiveInputQueue::new();
        assert_eq!(q.enqueue("T-1", steer("a")), EnqueueOutcome::Accepted { pending: 1 });
        assert_eq!(q.enqueue("T-1", steer("b")), EnqueueOutcome::Accepted { pending: 2 });
        let drained = q.drain("T-1");
        assert_eq!(
            drained.iter().map(|s| s.content.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        // Drained — nothing left, entry still present.
        assert_eq!(q.pending_len("T-1"), 0);
    }

    #[test]
    fn drain_missing_thread_is_empty() {
        let q = LiveInputQueue::new();
        assert!(q.drain("nope").is_empty());
    }

    #[test]
    fn backpressure_refuses_without_enqueue() {
        let q = LiveInputQueue::with_bound(2);
        assert!(matches!(q.enqueue("T", steer("1")), EnqueueOutcome::Accepted { .. }));
        assert!(matches!(q.enqueue("T", steer("2")), EnqueueOutcome::Accepted { .. }));
        assert_eq!(q.enqueue("T", steer("3")), EnqueueOutcome::Full { pending: 2 });
        assert_eq!(q.pending_len("T"), 2);
    }

    #[test]
    fn close_clears_queue_and_rejects_enqueue() {
        let q = LiveInputQueue::new();
        q.enqueue("T", steer("queued"));
        q.close("T");
        assert_eq!(q.pending_len("T"), 0);
        assert!(q.is_closed("T"));
        // No item survives terminal; enqueue after close is refused.
        assert_eq!(q.enqueue("T", steer("late")), EnqueueOutcome::Closed);
        assert_eq!(q.pending_len("T"), 0);
    }

    #[test]
    fn close_is_idempotent_and_works_before_any_enqueue() {
        let q = LiveInputQueue::new();
        q.close("T");
        q.close("T");
        assert!(q.is_closed("T"));
        assert_eq!(q.enqueue("T", steer("x")), EnqueueOutcome::Closed);
    }

    #[test]
    fn restore_front_preserves_fifo_and_prepends() {
        let q = LiveInputQueue::new();
        q.enqueue("T", steer("c")); // already pending
        // Simulate a failed persist of an earlier drain of [a, b].
        q.restore_front("T", vec![steer("a"), steer("b")]);
        let drained = q.drain("T");
        assert_eq!(
            drained.iter().map(|s| s.content.as_str()).collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
    }

    #[test]
    fn restore_front_drops_into_closed_entry() {
        let q = LiveInputQueue::new();
        q.close("T");
        q.restore_front("T", vec![steer("a")]);
        assert_eq!(q.pending_len("T"), 0);
    }

    #[test]
    fn in_flight_counts_toward_bound_so_restore_never_exceeds() {
        // The drain/restore race: drain empties the visible queue, concurrent
        // enqueues fill it, restore prepends the drained items → would overflow if
        // in-flight weren't counted.
        let q = LiveInputQueue::with_bound(2);
        q.enqueue("T", steer("a"));
        let drained = q.drain("T"); // 1 in-flight, queue empty
        assert_eq!(q.pending_len("T"), 0);
        // A concurrent enqueue fits (1 queued + 1 in-flight = 2)...
        assert_eq!(
            q.enqueue("T", steer("b")),
            EnqueueOutcome::Accepted { pending: 2 }
        );
        // ...but the next is refused: the in-flight item still counts.
        assert_eq!(q.enqueue("T", steer("c")), EnqueueOutcome::Full { pending: 2 });
        // Restoring the drained item returns to exactly the bound, never above.
        q.restore_front("T", drained);
        assert_eq!(q.pending_len("T"), 2);
        assert_eq!(q.enqueue("T", steer("d")), EnqueueOutcome::Full { pending: 2 });
    }

    #[test]
    fn ack_drained_releases_the_reservation() {
        let q = LiveInputQueue::with_bound(2);
        q.enqueue("T", steer("1"));
        q.enqueue("T", steer("2"));
        let drained = q.drain("T");
        // In-flight still counts → at cap.
        assert_eq!(q.enqueue("T", steer("3")), EnqueueOutcome::Full { pending: 2 });
        // Persisted (or discarded): release the reservation.
        q.ack_drained("T", drained.len());
        assert!(matches!(
            q.enqueue("T", steer("4")),
            EnqueueOutcome::Accepted { pending: 1 }
        ));
    }
}
