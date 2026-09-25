//! Narrow durable cuts for daemon-owned qualification children.
//!
//! Callers must never retain a StateStore guard while waiting for Lillux or a
//! process. Each method commits one short journal transition; process and
//! scope ownership remains in the separate scoped-producer service.

use anyhow::{Result, bail};
use std::sync::atomic::Ordering;

use crate::process::ExecutionProcessIdentity;
use crate::runtime_db::scoped_child_attempt::{
    NewScopedChildAttempt, ScopedChildAttemptRecord, ScopedChildInputOperation,
    ScopedChildInputReservation, ScopedChildNaturalEmptyReceipt,
};

use super::StateStore;

impl StateStore {
    fn ensure_scoped_child_root_running_locked(
        &self,
        guard: &super::StateStoreGuard<'_>,
        thread_id: &str,
    ) -> Result<()> {
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!("scoped child authority is closed for daemon shutdown");
        }
        let thread = guard
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow::anyhow!("scoped child root thread is absent"))?;
        if thread.status != super::ThreadStatus::Running.as_str() {
            bail!("scoped child root is not running");
        }
        let runtime = guard
            .runtime_db
            .get_runtime_info(thread_id)?
            .ok_or_else(|| anyhow::anyhow!("scoped child root runtime is absent"))?;
        if runtime.stop_intent.is_some() {
            bail!("scoped child root has a durable stop intent");
        }
        Ok(())
    }

    pub fn reserve_scoped_child_attempt(&self, initial: &NewScopedChildAttempt) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        let guard = self.lock()?;
        self.ensure_scoped_child_root_running_locked(&guard, &initial.owner.thread_id)?;
        guard.runtime_db.reserve_scoped_child_attempt(initial)
    }

    pub fn reserve_scoped_child_input_operation(
        &self,
        attempt_id: &str,
        owner: &crate::runtime_db::LaunchOwner,
        operation: &ScopedChildInputOperation,
        source: &ryeos_state::external_content::products::producer_recipe::ProducerStdinSource,
    ) -> Result<ScopedChildInputReservation> {
        let _permit = self.acquire_write_permit()?;
        let guard = self.lock()?;
        self.ensure_scoped_child_root_running_locked(&guard, &owner.thread_id)?;
        guard
            .runtime_db
            .reserve_scoped_child_input_operation(attempt_id, owner, operation, source)
    }

    pub fn delivered_scoped_child_input_operation(
        &self,
        attempt_id: &str,
        owner: &crate::runtime_db::LaunchOwner,
        operation: &ScopedChildInputOperation,
    ) -> Result<bool> {
        self.lock()?
            .runtime_db
            .delivered_scoped_child_input_operation(attempt_id, owner, operation)
    }

    pub fn complete_scoped_child_input_operation(
        &self,
        attempt_id: &str,
        owner: &crate::runtime_db::LaunchOwner,
        operation: &ScopedChildInputOperation,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .complete_scoped_child_input_operation(attempt_id, owner, operation)
    }

    pub fn bind_scoped_child_scope(
        &self,
        attempt_id: &str,
        recovery: &lillux::ProcessScopeRecovery,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .bind_scoped_child_scope(attempt_id, recovery)
    }

    pub fn attach_scoped_child_process(
        &self,
        attempt_id: &str,
        identity: &ExecutionProcessIdentity,
        mount_evidence: &crate::runtime_db::scoped_child_attempt::ScopedChildMountPreparationEvidence,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .attach_scoped_child_process(attempt_id, identity, mount_evidence)
    }

    pub fn permit_scoped_child_release(
        &self,
        attempt_id: &str,
        identity: &ExecutionProcessIdentity,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        let guard = self.lock()?;
        let record = guard
            .runtime_db
            .get_scoped_child_attempt(attempt_id)?
            .ok_or_else(|| anyhow::anyhow!("scoped child attempt is absent before release"))?;
        self.ensure_scoped_child_root_running_locked(&guard, &record.initial.owner.thread_id)?;
        guard
            .runtime_db
            .permit_scoped_child_release(attempt_id, identity)
    }

    pub fn record_scoped_child_natural_empty(
        &self,
        receipt: &ScopedChildNaturalEmptyReceipt,
        observation: &serde_json::Value,
    ) -> Result<String> {
        receipt.recheck_scope_empty()?;
        // Keep the shared CAS mutation guard through the SQLite pointer CAS:
        // a concurrent sweep cannot remove the not-yet-rooted object between
        // publication and the journal cut.
        let authority = self.pinned_state_authority()?;
        let _guard = authority.acquire_shared_guard()?;
        let observation_object_hash = authority.cas_store()?.store_object(observation)?;
        let _permit = self.acquire_write_permit()?;
        let guard = self.lock()?;
        let record = guard
            .runtime_db
            .get_scoped_child_attempt(receipt.attempt_id())?
            .ok_or_else(|| anyhow::anyhow!("scoped child attempt is absent before observation"))?;
        self.ensure_scoped_child_root_running_locked(&guard, &record.initial.owner.thread_id)?;
        guard
            .runtime_db
            .record_scoped_child_natural_empty(receipt, &observation_object_hash)?;
        Ok(observation_object_hash)
    }

    pub fn scoped_child_attempt(
        &self,
        attempt_id: &str,
    ) -> Result<Option<ScopedChildAttemptRecord>> {
        self.lock()?.runtime_db.get_scoped_child_attempt(attempt_id)
    }

    pub fn assert_scoped_child_input_closed(
        &self,
        attempt_id: &str,
        owner: &crate::runtime_db::LaunchOwner,
    ) -> Result<()> {
        self.lock()?
            .runtime_db
            .assert_scoped_child_input_closed(attempt_id, owner)
    }

    pub fn scoped_child_attempt_for_owner(
        &self,
        owner: &crate::runtime_db::LaunchOwner,
    ) -> Result<Option<ScopedChildAttemptRecord>> {
        self.lock()?
            .runtime_db
            .get_scoped_child_attempt_for_owner(owner)
    }

    pub fn unsettled_scoped_child_attempt_ids(&self) -> Result<Vec<String>> {
        self.lock()?.runtime_db.unsettled_scoped_child_attempt_ids()
    }

    pub fn has_unsettled_scoped_child_for_thread(&self, thread_id: &str) -> Result<bool> {
        self.lock()?
            .runtime_db
            .has_unsettled_scoped_child_for_thread(thread_id)
    }

    pub fn claim_bound_scoped_child_retirement(
        &self,
        attempt_id: &str,
        recovery: &lillux::ProcessScopeRecovery,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .claim_bound_scoped_child_retirement(attempt_id, recovery)
    }

    pub fn claim_released_scoped_child_abort(
        &self,
        attempt_id: &str,
        recovery: &lillux::ProcessScopeRecovery,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .claim_released_scoped_child_abort(attempt_id, recovery)
    }

    pub fn complete_bound_scoped_child_retirement(&self, attempt_id: &str) -> Result<()> {
        let record = self
            .scoped_child_attempt(attempt_id)?
            .ok_or_else(|| anyhow::anyhow!("unknown scoped child attempt"))?;
        if record.phase
            != crate::runtime_db::scoped_child_attempt::ScopedChildPhase::BoundDeathProven
            || record.recovery_death_evidence_digest.is_none()
        {
            anyhow::bail!("bound scoped child lacks durable death proof");
        }
        let recovery = record
            .scope_recovery
            .ok_or_else(|| anyhow::anyhow!("bound retirement has no exact scope"))?;
        recovery
            .retire_after_settlement()
            .map_err(anyhow::Error::msg)?;
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .complete_bound_scoped_child_retirement(attempt_id, &recovery)
    }

    /// Startup-only cleanup proof. The potentially blocking kernel wait runs
    /// outside the StateStore mutex; the subsequent CAS records the exact
    /// proof before any scope removal can occur.
    pub fn prove_bound_scoped_child_death(
        &self,
        attempt_id: &str,
        recovery: &lillux::ProcessScopeRecovery,
    ) -> Result<()> {
        recovery
            .terminate_and_wait(recovery.control_timeout())
            .map_err(anyhow::Error::msg)?;
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .record_bound_scoped_child_death(attempt_id, recovery)
    }

    pub fn claim_unbound_scoped_child_discard(&self, attempt_id: &str) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .claim_unbound_scoped_child_discard(attempt_id)
    }

    pub fn complete_unbound_scoped_child_discard(&self, attempt_id: &str) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .complete_unbound_scoped_child_discard(attempt_id)
    }
}
