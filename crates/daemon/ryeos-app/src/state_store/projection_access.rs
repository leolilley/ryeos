use std::sync::Arc;

use anyhow::Result;
use ryeos_state::{CommittedWrite, ProjectionStatus};

use crate::projection_health::{ThreadProjectionHealth, ThreadProjectionHealthSnapshot};

use super::StateStore;

pub(super) fn committed_value<T>(write: CommittedWrite<T>) -> T {
    if let ProjectionStatus::RepairRequired(request) = &write.projection {
        tracing::warn!(
            operation = request.operation,
            chain_root_id = %request.chain_root_id,
            committed_head_hash = %request.committed_head_hash,
            error = %request.error,
            "authoritative state committed; projection will be repaired"
        );
    }
    write.value
}

impl StateStore {
    pub fn projection_health(&self) -> Arc<ThreadProjectionHealth> {
        self.projection_health.clone()
    }

    pub fn projection_health_snapshot(&self) -> ThreadProjectionHealthSnapshot {
        self.projection_health.snapshot()
    }

    pub fn repair_thread_projection(&self) -> Result<()> {
        let g = self.lock()?;
        g.state_db.catch_up_projection()?;
        Ok(())
    }

    /// Run a closure with access to the projection database.
    pub fn with_projection<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&ryeos_state::ProjectionDb) -> Result<T>,
    {
        // Preserve the existing lock-before-health-check ordering.
        let g = self.lock()?;
        ensure_projection_current(&self.projection_health)?;
        f(g.state_db.projection())
    }
}

fn ensure_projection_current(health: &ThreadProjectionHealth) -> Result<()> {
    if !health.is_current() {
        anyhow::bail!("thread projection is not current; retry after repair");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_state::{ProjectionRepairRequest, ProjectionRepairSink};

    #[test]
    fn projection_guard_preserves_stale_error_wording() {
        let health = ThreadProjectionHealth::default();
        health.request_repair(ProjectionRepairRequest {
            chain_root_id: "T-root".into(),
            committed_head_hash: "head".into(),
            operation: "append_events",
            error: "projection failed".into(),
        });

        assert_eq!(
            ensure_projection_current(&health).unwrap_err().to_string(),
            "thread projection is not current; retry after repair"
        );
    }

    #[test]
    fn committed_value_returns_value_when_repair_is_required() {
        let write = CommittedWrite {
            value: "committed",
            projection: ProjectionStatus::RepairRequired(ProjectionRepairRequest {
                chain_root_id: "T-root".into(),
                committed_head_hash: "head".into(),
                operation: "append_events",
                error: "projection failed".into(),
            }),
        };

        assert_eq!(committed_value(write), "committed");
    }
}
