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
        self.snapshot().status == ThreadProjectionState::Current
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
                state.status = ThreadProjectionState::Current;
                state.chain_root_id = None;
                state.operation = None;
                state.error = None;
            }
            Err(error) => {
                state.status = ThreadProjectionState::Failed;
                state.error = Some(format!("{error:#}"));
            }
        }
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
                chain_root_id: None,
                operation: None,
                error: None,
            }
        );
    }
}
