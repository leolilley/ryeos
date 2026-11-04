//! Dedicated session candidate publication, validation and evaluation.
use super::*;

impl StateStore {
    pub fn terminalize_dedicated_session(
        &self,
        placement_thread_id: &str,
        worker_instance_id: &str,
        boot_epoch: u64,
        reason: &str,
    ) -> Result<()> {
        let g = self.lock()?;
        let changed_profile = g.runtime_db.terminalize_dedicated_session(
            placement_thread_id,
            worker_instance_id,
            boot_epoch,
            reason,
        )?;
        if let Some(profile_id) = changed_profile {
            persist_credential_profile_authority_locked(&g, &profile_id)?;
        }
        drop(g);
        self.note_closed_worker_scope_retirement_for_session(placement_thread_id);
        Ok(())
    }

    pub fn bind_dedicated_session_candidate(
        &self,
        placement_thread_id: &str,
        snapshot_hash: &str,
    ) -> Result<bool> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        g.runtime_db
            .bind_dedicated_session_candidate(placement_thread_id, snapshot_hash)
    }

    pub fn settle_dedicated_candidate_retained_for_review(
        &self,
        placement_thread_id: &str,
        snapshot_hash: &str,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db
            .settle_dedicated_candidate_retained_for_review(placement_thread_id, snapshot_hash)?;
        drop(g);
        // Candidate disposition can be the last of the two independent
        // settlement facts (closed workspace and reaped worker). Re-drive the
        // existing exact retirement owner here so cleanup does not depend on
        // which fact happened last or on a later daemon restart.
        self.note_closed_worker_scope_retirement_for_session(placement_thread_id);
        Ok(())
    }

    pub fn reserve_dedicated_candidate_publication(
        &self,
        placement_thread_id: &str,
        candidate_snapshot_hash: &str,
        publication_root_id: &str,
        publication_operation_id: &str,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.reserve_dedicated_candidate_publication(
            placement_thread_id,
            candidate_snapshot_hash,
            publication_root_id,
            publication_operation_id,
        )
    }

    pub fn settle_dedicated_candidate_publication(
        &self,
        placement_thread_id: &str,
        candidate_snapshot_hash: &str,
        publication_root_id: &str,
        publication_operation_id: &str,
        publication_result: &str,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.settle_dedicated_candidate_publication(
            placement_thread_id,
            candidate_snapshot_hash,
            publication_root_id,
            publication_operation_id,
            publication_result,
        )?;
        drop(g);
        self.note_closed_worker_scope_retirement_for_session(placement_thread_id);
        Ok(())
    }

    pub fn reserve_dedicated_candidate_validation(
        &self,
        placement_thread_id: &str,
        candidate_snapshot_hash: &str,
        candidate_validation_hash: &str,
    ) -> Result<bool> {
        let g = self.lock()?;
        g.runtime_db.reserve_dedicated_candidate_validation(
            placement_thread_id,
            candidate_snapshot_hash,
            candidate_validation_hash,
        )
    }

    pub fn settle_dedicated_candidate_validation(
        &self,
        placement_thread_id: &str,
        candidate_snapshot_hash: &str,
        candidate_validation_hash: &str,
        evidence: &serde_json::Value,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.settle_dedicated_candidate_validation(
            placement_thread_id,
            candidate_snapshot_hash,
            candidate_validation_hash,
            evidence,
        )
    }

    pub fn reserve_dedicated_candidate_evaluation(
        &self,
        placement_thread_id: &str,
        candidate_snapshot_hash: &str,
        candidate_validation_hash: &str,
        expected_previous_evaluation_hash: Option<&str>,
        candidate_evaluation_hash: &str,
        candidate_evaluation: &serde_json::Value,
        qualification_root_id: &str,
        qualification_operation_id: &str,
        reserved_integration_launch: Option<(&str, &str, &str)>,
    ) -> Result<()> {
        if reserved_integration_launch
            .is_some_and(|(launch_id, _, _)| !is_canonical_launch_id(launch_id))
        {
            bail!("candidate integration launch id is not canonical");
        }
        let g = self.lock()?;
        g.runtime_db.reserve_dedicated_candidate_evaluation(
            placement_thread_id,
            candidate_snapshot_hash,
            candidate_validation_hash,
            expected_previous_evaluation_hash,
            candidate_evaluation_hash,
            candidate_evaluation,
            qualification_root_id,
            qualification_operation_id,
            reserved_integration_launch,
        )
    }

    pub fn settle_dedicated_candidate_evaluation(
        &self,
        placement_thread_id: &str,
        candidate_snapshot_hash: &str,
        candidate_validation_hash: &str,
        expected_previous_evaluation_hash: Option<&str>,
        candidate_evaluation_hash: &str,
        candidate_evaluation: &serde_json::Value,
        qualification_root_id: &str,
        qualification_operation_id: &str,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.settle_dedicated_candidate_evaluation(
            placement_thread_id,
            candidate_snapshot_hash,
            candidate_validation_hash,
            expected_previous_evaluation_hash,
            candidate_evaluation_hash,
            candidate_evaluation,
            qualification_root_id,
            qualification_operation_id,
        )
    }

    pub fn rollback_dedicated_candidate_evaluation(
        &self,
        placement_thread_id: &str,
        qualification_root_id: &str,
        qualification_operation_id: &str,
        reserved_integration_launch: Option<(&str, &str, &str)>,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.rollback_dedicated_candidate_evaluation(
            placement_thread_id,
            qualification_root_id,
            qualification_operation_id,
            reserved_integration_launch,
        )
    }

    pub fn reserve_dedicated_candidate_discard(
        &self,
        placement_thread_id: &str,
        candidate_snapshot_hash: &str,
        disposition_root_id: &str,
        disposition_operation_id: &str,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.reserve_dedicated_candidate_discard(
            placement_thread_id,
            candidate_snapshot_hash,
            disposition_root_id,
            disposition_operation_id,
        )
    }

    pub fn rollback_dedicated_candidate_discard(
        &self,
        placement_thread_id: &str,
        candidate_snapshot_hash: &str,
        disposition_root_id: &str,
        disposition_operation_id: &str,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.rollback_dedicated_candidate_discard(
            placement_thread_id,
            candidate_snapshot_hash,
            disposition_root_id,
            disposition_operation_id,
        )
    }

    pub fn settle_dedicated_candidate_discard(
        &self,
        placement_thread_id: &str,
        candidate_snapshot_hash: &str,
        disposition_root_id: &str,
        disposition_operation_id: &str,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.settle_dedicated_candidate_discard(
            placement_thread_id,
            candidate_snapshot_hash,
            disposition_root_id,
            disposition_operation_id,
        )?;
        drop(g);
        self.note_closed_worker_scope_retirement_for_session(placement_thread_id);
        Ok(())
    }

    /// Run a state publication while the exact execution launch owner remains
    /// current. This closes the stale-waiter window for side effects such as a
    /// signed project HEAD compare-and-swap.
    pub fn with_state_db_owned<F, T>(&self, thread_id: &str, launch_owner: &str, f: F) -> Result<T>
    where
        F: FnOnce(&StateDb) -> Result<T>,
    {
        let g = self.lock()?;
        let claim = g
            .runtime_db
            .get_launch_claim(thread_id)?
            .ok_or_else(|| anyhow!("thread {thread_id} has no current launch owner"))?;
        if claim.claimed_by != launch_owner {
            bail!("stale launch owner cannot publish state for thread {thread_id}");
        }
        f(&g.state_db)
    }
}
