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
}

impl ThreadLaunchClaim {
    pub(crate) fn acquire(
        state: &AppState,
        thread_id: &str,
    ) -> anyhow::Result<ThreadLaunchClaimOutcome> {
        let claim_id = ryeos_app::thread_lifecycle::new_thread_id();
        static OWNER: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        let claimed_by = OWNER.get_or_init(|| {
            format!(
                "daemon:{}:{}",
                std::process::id(),
                ryeos_app::thread_lifecycle::new_thread_id()
            )
        });
        match state
            .state_store
            .claim_thread_launch(thread_id, &claim_id, claimed_by)?
        {
            ryeos_app::runtime_db::LaunchClaimOutcome::Claimed => {
                Ok(ThreadLaunchClaimOutcome::Claimed(Box::new(Self {
                    state: state.clone(),
                    thread_id: thread_id.to_string(),
                    claim_id,
                })))
            }
            ryeos_app::runtime_db::LaunchClaimOutcome::AlreadyClaimed => {
                Ok(ThreadLaunchClaimOutcome::AlreadyClaimed)
            }
        }
    }

    /// Reserve a pre-minted thread ID for a fresh launch before publishing it.
    ///
    /// This closes the live-recovery race completely: the thread row cannot be
    /// observed before its launch owner. A collision for a newly allocated ID
    /// is an invariant failure rather than a benign recovery skip.
    pub(crate) fn acquire_fresh(state: &AppState, thread_id: &str) -> anyhow::Result<Self> {
        let claim_id = ryeos_app::thread_lifecycle::new_thread_id();
        static OWNER: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        let claimed_by = OWNER.get_or_init(|| {
            format!(
                "daemon:{}:{}",
                std::process::id(),
                ryeos_app::thread_lifecycle::new_thread_id()
            )
        });
        match state
            .state_store
            .reserve_fresh_thread_launch(thread_id, &claim_id, claimed_by)?
        {
            ryeos_app::runtime_db::LaunchClaimOutcome::Claimed => Ok(Self {
                state: state.clone(),
                thread_id: thread_id.to_string(),
                claim_id,
            }),
            ryeos_app::runtime_db::LaunchClaimOutcome::AlreadyClaimed => anyhow::bail!(
                "fresh thread ID {thread_id} was already reserved by another launch owner"
            ),
        }
    }
}

impl Drop for ThreadLaunchClaim {
    fn drop(&mut self) {
        if let Err(error) = self
            .state
            .state_store
            .release_thread_launch_claim(&self.thread_id, &self.claim_id)
        {
            tracing::warn!(
                thread_id = %self.thread_id,
                claim_id = %self.claim_id,
                error = %error,
                "failed to release owned thread launch claim"
            );
        }
    }
}
