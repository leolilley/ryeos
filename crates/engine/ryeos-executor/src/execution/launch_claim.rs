//! Durable ownership for an executor launch boundary.
//!
//! A recovery dispatcher must not report work as queued until it has won the
//! SQLite launch claim that authorizes the spawn.  Keeping that claim in an
//! owned guard lets the caller move ownership into a detached task; every
//! normal, error, cancellation, and panic drop then releases only its own row.

use ryeos_app::state::AppState;

pub(crate) enum ThreadLaunchClaimOutcome {
    Claimed(Box<ThreadLaunchClaim>),
    AlreadyClaimed,
}

/// Owned authorization to launch one existing thread row.
///
/// The guard deliberately owns an `AppState`: recovery can acquire it on the
/// startup task and move it, together with the work, into `tokio::spawn` without
/// leaving a lifetime tied to startup.  Release is claim-id-qualified in the
/// runtime DB, so an expired/reclaimed claim can never be deleted by an older
/// owner dropping late.
pub(crate) struct ThreadLaunchClaim {
    state: AppState,
    thread_id: String,
    claim_id: String,
    owner: ryeos_app::runtime_db::LaunchOwner,
}

impl ThreadLaunchClaim {
    pub(crate) fn acquire(
        state: &AppState,
        thread_id: &str,
    ) -> anyhow::Result<ThreadLaunchClaimOutcome> {
        let claim_id = ryeos_app::thread_lifecycle::new_thread_id();
        match state.state_store.claim_thread_launch_active(
            thread_id,
            &claim_id,
            ryeos_app::runtime_db::daemon_generation_id(),
        )? {
            Some(claim) => {
                let owner = claim.owner;
                Ok(ThreadLaunchClaimOutcome::Claimed(Box::new(Self {
                    state: state.clone(),
                    thread_id: thread_id.to_string(),
                    claim_id,
                    owner,
                })))
            }
            None => Ok(ThreadLaunchClaimOutcome::AlreadyClaimed),
        }
    }

    /// Reserve a pre-minted thread ID for a fresh launch before publishing it.
    ///
    /// This closes the live-recovery race completely: the thread row cannot be
    /// observed before its launch owner. A collision for a newly allocated ID
    /// is an invariant failure rather than a benign recovery skip.
    pub(crate) fn acquire_fresh(state: &AppState, thread_id: &str) -> anyhow::Result<Self> {
        let claim_id = ryeos_app::thread_lifecycle::new_thread_id();
        match state.state_store.reserve_fresh_thread_launch_active(
            thread_id,
            &claim_id,
            ryeos_app::runtime_db::daemon_generation_id(),
        )? {
            Some(claim) => {
                let owner = claim.owner;
                Ok(Self {
                    state: state.clone(),
                    thread_id: thread_id.to_string(),
                    claim_id,
                    owner,
                })
            }
            None => anyhow::bail!(
                "fresh thread ID {thread_id} was already reserved by another launch owner"
            ),
        }
    }

    pub(crate) fn owner(&self) -> &ryeos_app::runtime_db::LaunchOwner {
        &self.owner
    }

    pub(crate) fn canonical_owner(&self) -> anyhow::Result<String> {
        Ok(lillux::canonical_json(&serde_json::to_value(&self.owner)?)?)
    }
}

impl Drop for ThreadLaunchClaim {
    fn drop(&mut self) {
        let release = self.canonical_owner().and_then(|owner| {
            self.state.state_store.release_active_thread_launch_claim(
                &self.thread_id,
                &self.claim_id,
                &owner,
            )
        });
        if let Err(error) = release {
            tracing::warn!(
                thread_id = %self.thread_id,
                claim_id = %self.claim_id,
                error = %error,
                "failed to release owned thread launch claim"
            );
        }
    }
}
