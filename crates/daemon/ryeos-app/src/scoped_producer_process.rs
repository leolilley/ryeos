//! Process-local ownership of an exact scoped producer child.
//!
//! Callback identities and durable rows cannot recreate this ownership after
//! daemon restart. A child is inserted once and transferred once to its
//! observer, together with the launch materials that must outlive it.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result, ensure};

use crate::runtime_db::LaunchOwner;
use crate::runtime_db::scoped_child_attempt::{ScopedChildAttemptRecord, ScopedChildPhase};
use crate::scoped_producer_authority::{ScopedProducerAuthorityKey, ScopedProducerLiveAuthority};
use crate::scoped_producer_io::ScopedProducerInteractiveIo;
use ryeos_state::external_content::products::producer_recipe::ProductProducerRecipe;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedProducerProcessKey {
    pub attempt_id: String,
    pub launch_owner: LaunchOwner,
}

impl ScopedProducerProcessKey {
    pub fn new(attempt_id: String, launch_owner: LaunchOwner) -> Result<Self> {
        ScopedProducerAuthorityKey::new(launch_owner.thread_id.clone(), launch_owner.clone())?;
        ensure!(
            attempt_id.len() == "scoped-".len() + 64
                && attempt_id.starts_with("scoped-")
                && attempt_id["scoped-".len()..]
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
            "invalid scoped producer attempt identity"
        );
        Ok(Self {
            attempt_id,
            launch_owner,
        })
    }
}

/// Linear ownership transferred to the one process-death observer. The
/// lifeline must remain beside the process until that observer has settled it.
pub struct ScopedProducerRunningChild {
    pub process: lillux::exec::RunningProcess,
    pub authority: Arc<ScopedProducerLiveAuthority>,
    /// The parent-side channel and stdout reader travel with this exact
    /// process owner. A journal row alone cannot recreate them after restart.
    pub interactive_io: Option<Arc<ScopedProducerInteractiveIo>>,
    /// Exact verifier-root handoff channel retained after relay readiness.
    /// Its loss must abort this child before a clean qualification can be
    /// accepted; the journal cannot recreate it after daemon restart.
    pub ingress_handoff: Option<lillux::InheritedDuplexChannel>,
    /// Canonical handoff whose exact readiness was received before release.
    /// Retained with the live process so observation can join it to the
    /// applied target and natural-empty receipt.
    pub relay_handoff: Option<ryeos_runtime::scoped_relay_handoff::ScopedRelayHandoff>,
    /// Exact signed recipe selected before this process was released. I/O
    /// limits come from here, never from a later callback or moved Config.
    pub producer_recipe: ProductProducerRecipe,
    /// Full signed source selected before release and retained beside the
    /// exact live process, not reconstructed from the later journal row.
    pub producer_source:
        ryeos_state::external_content::products::qualification::ProductProducerRecipeSourceIdentity,
    /// The signed wall clock starts at release, not when an observer first
    /// arrives. A delayed callback cannot renew the process budget.
    pub natural_wait_deadline: lillux::time::MonotonicDeadline,
    pub maximum_stdout_bytes: u64,
    pub maximum_stderr_bytes: u64,
    /// Concrete, redacted compiled-plan identity retained with the exact
    /// process for later independent observation. This is not qualification
    /// testimony by itself.
    pub isolation_provenance: ryeos_engine::isolation::IsolationLaunchProvenance,
    /// Independently compiled before adapter contact; never copied from the
    /// child's receipt or its later observation envelope.
    pub expected_applied_launch: lillux::LinuxSandboxAppliedLaunchCommitments,
}

/// Registration failure retains ownership so the caller can settle or abort
/// the still-running child instead of losing its only process handle.
pub struct ScopedProducerInsertError {
    pub error: anyhow::Error,
    pub child: ScopedProducerRunningChild,
}

enum SlotState {
    Starting,
    Live(ScopedProducerRunningChild),
    Observing(lillux::ProcessScopeRecovery),
    Settling,
    Settled,
    Uncertain,
}

struct AttemptSlot {
    key: ScopedProducerProcessKey,
    state: SlotState,
    cancel_requested: bool,
}

#[derive(Default)]
struct RegistryState {
    slots: Vec<AttemptSlot>,
}

/// An exact process owner is transferred only to the stop coordinator. An
/// observer keeps its own linear handle; stop can signal its retained scope
/// and then await the observer's checked reap and durable settlement.
pub enum ScopedProducerStopClaim {
    Missing,
    Starting,
    Live(ScopedProducerRunningChild),
    Observing(lillux::ProcessScopeRecovery),
    Settling,
    Settled,
    Uncertain,
}

/// Empty on daemon restart. Neither a callback nor a durable attempt record
/// can populate this registry; only a caller holding `RunningProcess` can.
/// Dropping the registry is not settlement: Lillux's process Drop attempts
/// cleanup, while the unsettled journal row remains for startup death proof.
#[derive(Default)]
pub struct ScopedProducerProcessRegistry {
    state: Mutex<RegistryState>,
    changed: Condvar,
}

impl ScopedProducerProcessRegistry {
    /// Borrow only the live channel attached to this exact released process.
    /// The caller must recheck the live slot after any blocking I/O before
    /// acknowledging bytes: stop or observation may win meanwhile.
    pub fn interactive_io_exact(
        &self,
        record: &ScopedChildAttemptRecord,
    ) -> Result<Option<(
        Arc<ScopedProducerInteractiveIo>,
        ProductProducerRecipe,
        lillux::time::MonotonicDeadline,
    )>> {
        let key = ScopedProducerProcessKey::new(
            record.initial.attempt_id.clone(),
            record.initial.owner.clone(),
        )?;
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("scoped producer process registry poisoned"))?;
        let Some(slot) = state.slots.iter().find(|slot| slot.key == key) else {
            return Ok(None);
        };
        let SlotState::Live(child) = &slot.state else {
            return Ok(None);
        };
        let Some(identity) = record.process_identity.as_ref() else {
            return Ok(None);
        };
        if slot.cancel_requested
            || record.phase != ScopedChildPhase::ReleasePermitted
            || record.scope_recovery.as_ref() != child.process.scope_recovery()
            || i64::from(child.process.pid) != identity.target_pid
            || child.process.pgid != identity.group_leader_pid
            || child.authority.workspace().ensure_path_binding().is_err()
        {
            return Ok(None);
        }
        Ok(child
            .interactive_io
            .as_ref()
            .map(|io| (
                Arc::clone(io),
                child.producer_recipe.clone(),
                child.natural_wait_deadline,
            )))
    }

    /// Read-only lost-ACK lookup. A retained journal row by itself is not a
    /// live child: after restart the registry is empty and this refuses.
    pub fn contains_exact(&self, record: &ScopedChildAttemptRecord) -> Result<bool> {
        Ok(self.prelaunch_evidence_exact(record)?.is_some())
    }

    /// Return the engine-compiled target and concrete isolation plan for the
    /// exact live, owner-bound attempt. Neither the journal nor a later child
    /// observation can manufacture these after process ownership is gone.
    pub fn prelaunch_evidence_exact(
        &self,
        record: &ScopedChildAttemptRecord,
    ) -> Result<Option<(lillux::LinuxSandboxAppliedLaunchCommitments, String)>> {
        let key = ScopedProducerProcessKey::new(
            record.initial.attempt_id.clone(),
            record.initial.owner.clone(),
        )?;
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("scoped producer process registry poisoned"))?;
        let Some(slot) = state.slots.iter().find(|slot| slot.key == key) else {
            return Ok(None);
        };
        let SlotState::Live(child) = &slot.state else {
            return Ok(None);
        };
        let Some(identity) = record.process_identity.as_ref() else {
            return Ok(None);
        };
        let exact = !slot.cancel_requested
            && record.phase == ScopedChildPhase::ReleasePermitted
            && record.scope_recovery.as_ref() == child.process.scope_recovery()
            && child.isolation_provenance.plan_digest.is_some()
            && i64::from(child.process.pid) == identity.target_pid
            && child.process.pgid == identity.group_leader_pid
            && identity.process_scope.as_ref() == record.scope_recovery.as_ref()
            && child.authority.workspace().ensure_path_binding().is_ok();
        if !exact {
            return Ok(None);
        }
        let plan_digest = child
            .isolation_provenance
            .plan_digest
            .clone()
            .context("exact scoped producer has no concrete isolation plan")?;
        Ok(Some((child.expected_applied_launch.clone(), plan_digest)))
    }

    /// Register the in-flight attempt before allocating a scope. A stop that
    /// wins after reservation can now leave a cancellation mark even before
    /// the child becomes a `RunningProcess`.
    pub fn register_starting(&self, key: ScopedProducerProcessKey) -> Result<()> {
        ScopedProducerProcessKey::new(key.attempt_id.clone(), key.launch_owner.clone())?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("scoped producer process registry poisoned"))?;
        ensure!(
            !state.slots.iter().any(|slot| slot.key == key),
            "scoped producer attempt already registered"
        );
        state.slots.push(AttemptSlot {
            key,
            state: SlotState::Starting,
            cancel_requested: false,
        });
        self.changed.notify_all();
        Ok(())
    }

    /// Return a cancelled child to the caller, which still owns checked abort
    /// and durable cleanup. Registration never silently drops a process.
    pub fn finish_start_failure(
        &self,
        key: &ScopedProducerProcessKey,
        settled: bool,
    ) -> Result<()> {
        self.finish_owned_attempt(key, settled)
    }

    pub fn finish_observation(&self, key: &ScopedProducerProcessKey, settled: bool) -> Result<()> {
        self.finish_owned_attempt(key, settled)
    }

    pub fn finish_stop(&self, key: &ScopedProducerProcessKey, settled: bool) -> Result<()> {
        self.finish_owned_attempt(key, settled)
    }

    fn finish_owned_attempt(&self, key: &ScopedProducerProcessKey, settled: bool) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("scoped producer process registry poisoned"))?;
        let slot = state
            .slots
            .iter_mut()
            .find(|slot| &slot.key == key)
            .ok_or_else(|| anyhow::anyhow!("scoped producer attempt has no process owner slot"))?;
        ensure!(
            !matches!(&slot.state, SlotState::Live(_) | SlotState::Settled),
            "scoped producer attempt still has a live or settled owner"
        );
        slot.state = if settled {
            SlotState::Settled
        } else {
            SlotState::Uncertain
        };
        self.changed.notify_all();
        Ok(())
    }

    /// No Lillux or StateStore operation occurs beneath this mutex. The
    /// returned owner must be reaped outside it; the slot remains `Settling`
    /// until that exact actor reports durable scope retirement.
    pub fn request_stop(&self, key: &ScopedProducerProcessKey) -> Result<ScopedProducerStopClaim> {
        ScopedProducerProcessKey::new(key.attempt_id.clone(), key.launch_owner.clone())?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("scoped producer process registry poisoned"))?;
        let Some(slot) = state.slots.iter_mut().find(|slot| &slot.key == key) else {
            return Ok(ScopedProducerStopClaim::Missing);
        };
        slot.cancel_requested = true;
        let claim = match std::mem::replace(&mut slot.state, SlotState::Settling) {
            SlotState::Starting => {
                slot.state = SlotState::Starting;
                ScopedProducerStopClaim::Starting
            }
            SlotState::Live(child) => ScopedProducerStopClaim::Live(child),
            SlotState::Observing(recovery) => {
                slot.state = SlotState::Observing(recovery.clone());
                ScopedProducerStopClaim::Observing(recovery)
            }
            SlotState::Settling => ScopedProducerStopClaim::Settling,
            SlotState::Settled => {
                slot.state = SlotState::Settled;
                ScopedProducerStopClaim::Settled
            }
            SlotState::Uncertain => {
                slot.state = SlotState::Uncertain;
                ScopedProducerStopClaim::Uncertain
            }
        };
        self.changed.notify_all();
        Ok(claim)
    }

    /// Wait only on this registry's short state transition. The caller must
    /// re-read the durable journal after wake; this is not death testimony.
    pub fn wait_for_change(&self, timeout: Duration) -> Result<()> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("scoped producer process registry poisoned"))?;
        let _ = self
            .changed
            .wait_timeout(state, timeout)
            .map_err(|_| anyhow::anyhow!("scoped producer process registry poisoned"))?;
        Ok(())
    }

    pub fn insert(
        &self,
        record: &ScopedChildAttemptRecord,
        child: ScopedProducerRunningChild,
    ) -> std::result::Result<(), ScopedProducerInsertError> {
        let key = match ScopedProducerProcessKey::new(
            record.initial.attempt_id.clone(),
            record.initial.owner.clone(),
        ) {
            Ok(key) => key,
            Err(error) => return Err(ScopedProducerInsertError { error, child }),
        };
        let exact_process = record.process_identity.as_ref();
        if record.phase != ScopedChildPhase::ReleasePermitted
            || record.scope_recovery.as_ref() != child.process.scope_recovery()
            || child.ingress_handoff.is_some() != child.producer_recipe.loopback_ingress.is_some()
            || child.relay_handoff.is_some() != child.ingress_handoff.is_some()
            || child.producer_recipe.digest().ok().as_deref()
                != Some(record.initial.recipe_digest.as_str())
            || child.producer_source.recipe_digest != record.initial.recipe_digest
            || child.isolation_provenance.plan_digest.is_none()
            || exact_process.is_none_or(|identity| {
                i64::from(child.process.pid) != identity.target_pid
                    || child.process.pgid != identity.group_leader_pid
                    || identity.process_scope.as_ref() != record.scope_recovery.as_ref()
            })
        {
            return Err(ScopedProducerInsertError {
                error: anyhow::anyhow!(
                    "scoped producer process differs from exact released journal identity"
                ),
                child,
            });
        }
        if let Err(error) = child.authority.workspace().ensure_path_binding() {
            return Err(ScopedProducerInsertError {
                error: error.into(),
                child,
            });
        }
        if let Some(channel) = child.ingress_handoff.as_ref()
            && let Err(error) = channel.ensure_peer_live_and_quiet()
        {
            return Err(ScopedProducerInsertError {
                error: error.into(),
                child,
            });
        }
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(_) => {
                return Err(ScopedProducerInsertError {
                    error: anyhow::anyhow!("scoped producer process registry poisoned"),
                    child,
                });
            }
        };
        let Some(slot) = state.slots.iter_mut().find(|slot| slot.key == key) else {
            return Err(ScopedProducerInsertError {
                error: anyhow::anyhow!("scoped producer start has no registered attempt slot"),
                child,
            });
        };
        if !matches!(&slot.state, SlotState::Starting) || slot.cancel_requested {
            return Err(ScopedProducerInsertError {
                error: anyhow::anyhow!("scoped producer start was cancelled or already consumed"),
                child,
            });
        }
        slot.state = SlotState::Live(child);
        self.changed.notify_all();
        Ok(())
    }

    /// Atomically transfers the exact child and its lifeline to one observer.
    /// A repeated observation fails closed, including after the child exits.
    pub fn take_for_observation(
        &self,
        key: &ScopedProducerProcessKey,
    ) -> Result<ScopedProducerRunningChild> {
        ScopedProducerProcessKey::new(key.attempt_id.clone(), key.launch_owner.clone())?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("scoped producer process registry poisoned"))?;
        let slot = state
            .slots
            .iter_mut()
            .find(|slot| &slot.key == key)
            .ok_or_else(|| {
                anyhow::anyhow!("scoped producer process is absent or already observed")
            })?;
        ensure!(
            !slot.cancel_requested,
            "scoped producer observation was cancelled"
        );
        let recovery = match &slot.state {
            SlotState::Live(child) => child
                .process
                .scope_recovery()
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("scoped producer has no exact recovery scope"))?,
            _ => {
                return Err(anyhow::anyhow!(
                    "scoped producer process is absent or already observed"
                ));
            }
        };
        let SlotState::Live(child) = std::mem::replace(&mut slot.state, SlotState::Settling) else {
            return Err(anyhow::anyhow!(
                "scoped producer process is absent or already observed"
            ));
        };
        slot.state = SlotState::Observing(recovery);
        self.changed.notify_all();
        Ok(child)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_rejects_callback_like_or_stale_identity() {
        let owner = LaunchOwner {
            thread_id: "T-scoped-process-test".into(),
            monotonic_launch_epoch: 1,
            unpredictable_nonce: "nonce".into(),
            daemon_generation_id: crate::runtime_db::daemon_generation_id().into(),
        };
        assert!(ScopedProducerProcessKey::new("scoped-arbitrary".into(), owner.clone()).is_err());
        assert!(
            ScopedProducerProcessKey::new(format!("scoped-{}", "A".repeat(64)), owner.clone())
                .is_err()
        );
        let key =
            ScopedProducerProcessKey::new(format!("scoped-{}", "a".repeat(64)), owner.clone())
                .unwrap();
        assert!(
            ScopedProducerProcessRegistry::default()
                .take_for_observation(&key)
                .is_err()
        );
        let mut stale = owner;
        stale.daemon_generation_id = "old-generation".into();
        assert!(ScopedProducerProcessKey::new(key.attempt_id, stale).is_err());
    }
}
