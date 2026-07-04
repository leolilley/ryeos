//! Broadcast hub for command settlement, so `commands.wait` can await a command
//! reaching `completed`/`rejected` without polling.
//!
//! Shape mirrors [`crate::event_stream::ThreadEventHub`]: a per-key
//! `tokio::sync::broadcast` lane with lazy allocation. It is process-scoped (a
//! `LazyLock` singleton) rather than an `AppState` field — the same choice made
//! for `author_target_lock`, keeping a new field off AppState's many
//! construction sites. The daemon is a single process, so one hub serves every
//! settlement.
//!
//! Race discipline: a waiter subscribes BEFORE reading the command row, so a
//! settlement that lands between the two is delivered on the lane rather than
//! missed. A command settles exactly once, so `publish` drops the lane after
//! sending.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use tokio::sync::broadcast;

use crate::runtime_db::CommandRecord;

/// Broadcast lane depth. A command settles once and there are seldom more than a
/// handful of concurrent waiters on one command, so a small buffer is ample.
const CAPACITY: usize = 16;

pub struct CommandHub {
    lanes: Mutex<HashMap<i64, broadcast::Sender<CommandRecord>>>,
}

impl CommandHub {
    fn new() -> Self {
        Self {
            lanes: Mutex::new(HashMap::new()),
        }
    }

    /// Subscribe to `command_id`'s settlement. Call this BEFORE reading the row
    /// so a completion racing the subscription is delivered, not lost.
    pub fn subscribe(&self, command_id: i64) -> broadcast::Receiver<CommandRecord> {
        let mut lanes = self.lanes.lock().unwrap_or_else(|e| e.into_inner());
        match lanes.get(&command_id) {
            Some(tx) => tx.subscribe(),
            None => {
                let (tx, rx) = broadcast::channel(CAPACITY);
                lanes.insert(command_id, tx);
                rx
            }
        }
    }

    /// Broadcast a settled command to any waiter and drop its lane (a command
    /// settles once). With no waiter this is a no-op — the lane was never
    /// allocated, and a later `commands.wait` reads the already-terminal row.
    pub fn publish(&self, record: &CommandRecord) {
        let mut lanes = self.lanes.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(tx) = lanes.remove(&record.command_id) {
            // Err means every receiver dropped between subscribe and now — the
            // waiter gave up (timeout/disconnect). Nothing to deliver.
            let _ = tx.send(record.clone());
        }
    }

    /// Drop `command_id`'s lane if no receiver remains. A waiter calls this on
    /// exit so a subscription that never received a `publish` — a settlement
    /// that raced ahead of the subscribe, a timed-out wait, or a command that
    /// never settles — does not leak a `Sender` for the daemon's lifetime. The
    /// caller MUST drop its own receiver before calling, or the count won't
    /// reach zero.
    pub fn reap(&self, command_id: i64) {
        let mut lanes = self.lanes.lock().unwrap_or_else(|e| e.into_inner());
        if lanes
            .get(&command_id)
            .is_some_and(|tx| tx.receiver_count() == 0)
        {
            lanes.remove(&command_id);
        }
    }
}

static GLOBAL: LazyLock<CommandHub> = LazyLock::new(CommandHub::new);

/// The process-wide command settlement hub.
pub fn global() -> &'static CommandHub {
    &GLOBAL
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn settled_record(command_id: i64, status: &str) -> CommandRecord {
        CommandRecord {
            command_id,
            thread_id: "T-1".to_string(),
            command_type: "cancel".to_string(),
            status: status.to_string(),
            requested_by: None,
            params: None,
            result: Some(json!({"ok": true})),
            created_at: "t0".to_string(),
            claimed_at: Some("t1".to_string()),
            completed_at: Some("t2".to_string()),
        }
    }

    #[tokio::test]
    async fn subscribe_then_publish_delivers_the_settled_record() {
        let hub = CommandHub::new();
        let mut rx = hub.subscribe(42);
        hub.publish(&settled_record(42, "completed"));
        let got = rx.recv().await.expect("settlement delivered");
        assert_eq!(got.command_id, 42);
        assert_eq!(got.status, "completed");
    }

    #[tokio::test]
    async fn two_waiters_on_one_command_both_receive() {
        let hub = CommandHub::new();
        let mut a = hub.subscribe(7);
        let mut b = hub.subscribe(7);
        hub.publish(&settled_record(7, "rejected"));
        assert_eq!(a.recv().await.unwrap().status, "rejected");
        assert_eq!(b.recv().await.unwrap().status, "rejected");
    }

    #[test]
    fn publish_with_no_waiter_is_a_noop() {
        let hub = CommandHub::new();
        // No subscriber → no lane → must not panic.
        hub.publish(&settled_record(99, "completed"));
    }

    #[tokio::test]
    async fn reap_drops_lane_only_after_receiver_gone() {
        let hub = CommandHub::new();
        let rx = hub.subscribe(5);
        // A live receiver keeps the lane — reap is a no-op.
        hub.reap(5);
        assert_eq!(hub.lanes.lock().unwrap().len(), 1);
        // Once the waiter drops its receiver, reap collects the orphaned lane so
        // a raced/timed-out subscription does not leak a Sender.
        drop(rx);
        hub.reap(5);
        assert_eq!(hub.lanes.lock().unwrap().len(), 0);
    }

    #[test]
    fn publish_leaves_no_lane_behind() {
        let hub = CommandHub::new();
        let _rx = hub.subscribe(3);
        hub.publish(&settled_record(3, "completed"));
        // publish removes the lane on delivery; nothing to reap.
        assert_eq!(hub.lanes.lock().unwrap().len(), 0);
    }
}
