//! Retirement of fenced-kind chains under the retention exclusion.
//!
//! The lifecycle owner finds due chains of the kinds whose profiles declare a
//! retirement fence, asks the composition whether each one is qualified and
//! what evidence it keeps, and drives State's retirement for the qualified
//! ones. This module never names a kind or interprets the evidence.

use anyhow::{Result, anyhow};

use crate::retention_exclusion::RetentionExclusion;
use crate::state_store::{FencedChainRetirementPlan, TerminalChainRetirement};

/// Roots examined per pass. Within the State slice bound.
const FENCED_RETIREMENT_SLICE: usize = 16;

impl super::ThreadLifecycleService {
    /// One bounded pass over due fenced-kind chains, continuing from the
    /// cursor the previous pass left and wrapping when the keyset is
    /// exhausted. `qualify` is the composition's decision for one chain root:
    /// `None` leaves the chain for a later pass, a plan retires it. A root
    /// whose retirement fails keeps its fence and durable state and is
    /// retried by a later pass; the failure is logged, not hidden.
    ///
    /// The caller holds the exclusive retention exclusion; its pin count is
    /// the one every chain is checked against.
    pub fn retire_due_fenced_chains<Q>(
        &self,
        exclusion: &RetentionExclusion<'_>,
        now: &str,
        dry_run: bool,
        qualify: Q,
    ) -> Result<TerminalChainRetirement>
    where
        Q: Fn(&str) -> Result<Option<FencedChainRetirementPlan>>,
    {
        exclusion.validate_owner(self.retention())?;
        exclusion.require_exclusive()?;
        let mut cursor = self
            .fenced_retirement_after
            .lock()
            .map_err(|_| anyhow!("fenced retirement cursor lock poisoned"))?;
        let roots = self.state_store.due_fenced_chain_retirement_slice(
            now,
            cursor.as_deref(),
            FENCED_RETIREMENT_SLICE,
        )?;
        *cursor = if roots.len() < FENCED_RETIREMENT_SLICE {
            None
        } else {
            roots.last().cloned()
        };
        drop(cursor);
        let mut result = TerminalChainRetirement::default();
        for root in roots {
            result.candidate_chains += 1;
            let plan = match qualify(&root) {
                Ok(Some(plan)) => plan,
                Ok(None) => continue,
                Err(error) => {
                    tracing::warn!(%root, %error, "fenced chain qualification retained for retry");
                    continue;
                }
            };
            if dry_run {
                result.retired_chains += 1;
                continue;
            }
            match self
                .state_store
                .retire_fenced_chain(&root, now, &plan, |threads| {
                    exclusion.count_thread_pins(threads)
                }) {
                Ok(retired) => {
                    result.retired_chains += retired.retired_chains;
                    result.deleted_runtime_rows += retired.deleted_runtime_rows;
                    result.deleted_runtime_files += retired.deleted_runtime_files;
                    result.pending_retirements_recovered += retired.pending_retirements_recovered;
                }
                Err(error) => {
                    tracing::warn!(%root, %error, "fenced chain retirement retained for retry");
                }
            }
        }
        Ok(result)
    }

    /// Finish the head half of every interrupted fenced retirement before
    /// readiness: a fence whose chain still has a head gets its head unlinked.
    /// The pending Remove it leaves is completed by the ordinary pending
    /// Remove recovery, which owns runtime cleanup and acknowledgement.
    pub fn replay_fenced_chain_retirements(
        &self,
        exclusion: &RetentionExclusion<'_>,
    ) -> Result<ryeos_state::state_db::FencedRetirementReplay> {
        exclusion.validate_owner(self.retention())?;
        exclusion.require_exclusive()?;
        self.state_store.replay_fenced_chain_retirements()
    }
}
