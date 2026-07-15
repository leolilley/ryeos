use std::sync::Mutex;

use ryeos_state::{ProjectionRepairRequest, ProjectionRepairSink};
use serde::Serialize;
use tokio::sync::Notify;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThreadProjectionState {
    Current,
    Stale,
    Repairing,
    Failed,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ThreadProjectionHealthSnapshot {
    pub status: ThreadProjectionState,
    pub generation: u64,
    pub pending_transitions: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chain_root_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug)]
pub struct ThreadProjectionHealth {
    state: Mutex<ThreadProjectionHealthSnapshot>,
    notify: Notify,
}

impl Default for ThreadProjectionHealth {
    fn default() -> Self {
        Self {
            state: Mutex::new(ThreadProjectionHealthSnapshot {
                status: ThreadProjectionState::Current,
                generation: 0,
                pending_transitions: 0,
                chain_root_id: None,
                operation: None,
                error: None,
            }),
            notify: Notify::new(),
        }
    }
}

impl ThreadProjectionHealth {
    pub fn snapshot(&self) -> ThreadProjectionHealthSnapshot {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn is_current(&self) -> bool {
        let snapshot = self.snapshot();
        snapshot.status == ThreadProjectionState::Current && snapshot.pending_transitions == 0
    }

    pub async fn notified(&self) {
        self.notify.notified().await;
    }

    pub fn begin_repair(&self) -> Option<u64> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.status == ThreadProjectionState::Current {
            return None;
        }
        state.status = ThreadProjectionState::Repairing;
        Some(state.generation)
    }

    pub fn finish_repair(&self, generation: u64, result: &anyhow::Result<()>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.generation != generation {
            self.notify.notify_one();
            return;
        }
        match result {
            Ok(()) => {
                if state.pending_transitions == 0 {
                    state.status = ThreadProjectionState::Current;
                    state.chain_root_id = None;
                    state.operation = None;
                    state.error = None;
                } else {
                    state.status = ThreadProjectionState::Stale;
                    state.chain_root_id = None;
                    state.operation = Some("pending_head_transition".to_string());
                    state.error = Some(format!(
                        "{} durable head transition(s) remain unresolved",
                        state.pending_transitions
                    ));
                }
            }
            Err(error) => {
                state.status = ThreadProjectionState::Failed;
                state.error = Some(format!("{error:#}"));
            }
        }
    }

    pub fn observe_pending_transitions(&self, pending_transitions: usize) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = state.clone();
        state.pending_transitions = pending_transitions;
        if pending_transitions != 0 {
            let recovered_scan_failure = state.status == ThreadProjectionState::Failed
                && state.operation.as_deref() == Some("pending_head_transition_scan");
            if state.status == ThreadProjectionState::Current || recovered_scan_failure {
                if state.status == ThreadProjectionState::Current {
                    state.generation = state.generation.saturating_add(1);
                }
                state.status = ThreadProjectionState::Stale;
                state.chain_root_id = None;
                state.operation = Some("pending_head_transition".to_string());
                state.error = Some(format!(
                    "{pending_transitions} durable head transition(s) remain unresolved"
                ));
            }
        } else {
            let pending_only_staleness = state.status == ThreadProjectionState::Stale
                && state.operation.as_deref() == Some("pending_head_transition");
            let recovered_scan_failure = state.status == ThreadProjectionState::Failed
                && state.operation.as_deref() == Some("pending_head_transition_scan");
            if pending_only_staleness || recovered_scan_failure {
                state.status = ThreadProjectionState::Current;
                state.chain_root_id = None;
                state.operation = None;
                state.error = None;
            }
        }
        let changed = *state != previous;
        drop(state);
        if changed {
            self.notify.notify_one();
        }
    }

    pub fn observe_pending_transition_error(&self, error: &anyhow::Error) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.generation = state.generation.saturating_add(1);
        state.status = ThreadProjectionState::Failed;
        state.chain_root_id = None;
        state.operation = Some("pending_head_transition_scan".to_string());
        state.error = Some(format!("read pending head transitions: {error:#}"));
        drop(state);
        self.notify.notify_one();
    }
}

impl ProjectionRepairSink for ThreadProjectionHealth {
    fn request_repair(&self, request: ProjectionRepairRequest) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.generation = state.generation.saturating_add(1);
        state.status = ThreadProjectionState::Stale;
        state.chain_root_id = Some(request.chain_root_id);
        state.operation = Some(request.operation.to_string());
        state.error = Some(request.error);
        drop(state);
        self.notify.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_failure_cannot_be_cleared_by_older_repair() {
        let health = ThreadProjectionHealth::default();
        health.request_repair(ProjectionRepairRequest {
            chain_root_id: "T-a".into(),
            committed_head_hash: "a".into(),
            operation: "append_events",
            error: "first".into(),
        });
        let generation = health.begin_repair().unwrap();
        health.request_repair(ProjectionRepairRequest {
            chain_root_id: "T-b".into(),
            committed_head_hash: "b".into(),
            operation: "append_events",
            error: "second".into(),
        });
        health.finish_repair(generation, &Ok(()));
        let snapshot = health.snapshot();
        assert_eq!(snapshot.status, ThreadProjectionState::Stale);
        assert_eq!(snapshot.chain_root_id.as_deref(), Some("T-b"));
    }

    #[test]
    fn successful_current_generation_repair_clears_degradation() {
        let health = ThreadProjectionHealth::default();
        health.request_repair(ProjectionRepairRequest {
            chain_root_id: "T-root".into(),
            committed_head_hash: "head".into(),
            operation: "create_chain",
            error: "projection failed".into(),
        });
        let generation = health.begin_repair().unwrap();
        health.finish_repair(generation, &Ok(()));
        assert_eq!(
            health.snapshot(),
            ThreadProjectionHealthSnapshot {
                status: ThreadProjectionState::Current,
                generation,
                pending_transitions: 0,
                chain_root_id: None,
                operation: None,
                error: None,
            }
        );
    }

    #[test]
    fn pending_transition_count_prevents_current_health() {
        let health = ThreadProjectionHealth::default();
        health.observe_pending_transitions(1);
        assert_eq!(health.snapshot().status, ThreadProjectionState::Stale);
        assert!(!health.is_current());

        let generation = health.begin_repair().expect("repair generation");
        health.finish_repair(generation, &Ok(()));
        assert_eq!(health.snapshot().status, ThreadProjectionState::Stale);
        assert!(!health.is_current());

        health.observe_pending_transitions(0);
        assert!(health.is_current());
    }

    #[test]
    fn successful_empty_scan_clears_prior_scan_failure() {
        let health = ThreadProjectionHealth::default();
        health.observe_pending_transition_error(&anyhow::anyhow!("unreadable journal"));
        assert_eq!(health.snapshot().status, ThreadProjectionState::Failed);

        // The count was already zero before the scan failed. A later successful
        // scan must still reconcile the failure instead of returning early.
        health.observe_pending_transitions(0);
        assert!(health.is_current());
    }
}
