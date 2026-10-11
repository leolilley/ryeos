//! Stopped-node native host owner. A maintenance command that holds the
//! exclusive node StateLock and the runtime namespace lock issues one
//! process-scoped [`OfflineNativeHost`] funded by the signed maintenance
//! policy's `offline_native` quota for its operation class.
//!
//! Funding is process-local and records no durable debit: the arena is
//! private memory the kernel reclaims when the process ends, and the locks
//! exclude the live daemon and every other maintainer for the whole session.
//! A consumer whose work creates an obligation that outlives its process
//! must use a durable claim (as execution-history reset does), not this host.

use anyhow::{Context, Result, ensure};
use lillux_sqlite::offline_host::{OfflineNativeAuthority, OfflineNativeCensus, OfflineNativeHost};
use lillux_sqlite::{
    OriginalNativeBudget, OriginalNativeConnectionTerminal, OriginalNativeDestinationAuthority,
    OriginalNativeFactory,
};
use ryeos_node_runtime::config::Config;
use ryeos_node_runtime::identity::NodeIdentity;
use ryeos_node_runtime::node_policy::{NodePolicySnapshot, NodePolicyTable};
use ryeos_node_runtime::state_lock::{StateLock, StateLockLease};
use ryeos_scheduler_contract::{NodeMaintenancePolicy, OfflineNativeQuota};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Operation class of explicit replay-index activation.
pub const REPLAY_INDEXES_CLASS: &str = "replay-indexes";

/// Load the complete node-signed policy generation, trusting only this
/// node's operator signer, never installed publishers or applications.
pub(crate) fn load_signed_maintenance_snapshot(
    config: &Config,
    identity: &NodeIdentity,
) -> Result<NodePolicySnapshot> {
    let trust = ryeos_engine::trust::TrustStore::from_signers(vec![
        ryeos_engine::trust::TrustedSigner {
            fingerprint: identity.fingerprint().to_owned(),
            verifying_key: *identity.verifying_key(),
            label: Some("offline maintenance node operator".to_owned()),
        },
    ]);
    let snapshot =
        ryeos_node_runtime::node_policy::load_snapshot(&config.app_root, &trust, &NodePolicyTable::new())
            .context("load complete node-signed maintenance policy generation")?;
    let digest = snapshot.generation_digest();
    ensure!(
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "offline maintenance policy lacks its exact canonical generation digest"
    );
    Ok(snapshot)
}

/// Revalidated on every native boundary of the session's host.
struct SessionAuthority {
    process: u32,
    app_root: PathBuf,
    state_lock: StateLockLease,
    runtime: Arc<lillux::PinnedDirectory>,
    namespace: lillux::PinnedDirectoryLock,
    operation: lillux::time::MonotonicDeadline,
    cleanup: lillux::time::MonotonicDeadline,
    class: String,
}

impl OfflineNativeAuthority for SessionAuthority {
    fn checkpoint(&self, cleanup: bool) -> Result<()> {
        ensure!(self.process == std::process::id(), "inherited offline native session refused");
        self.state_lock.ensure_protects_app_root(&self.app_root)?;
        self.runtime.ensure_path_binding()?;
        self.namespace.ensure_protects(&self.runtime)?;
        let (deadline, window) = if cleanup {
            (&self.cleanup, "cleanup")
        } else {
            (&self.operation, "operation")
        };
        ensure!(
            !deadline.has_elapsed(),
            "offline native `{}` {window} window elapsed",
            self.class
        );
        Ok(())
    }
}

/// One stopped-node native session for one operation class.
pub struct OfflineNativeSession {
    authority: Arc<SessionAuthority>,
    host: OfflineNativeHost,
}

impl OfflineNativeSession {
    /// Issue the class's signed envelope under the caller's exclusive locks.
    pub fn open(
        config: &Config,
        identity: &NodeIdentity,
        state_lock: &StateLock,
        runtime: Arc<lillux::PinnedDirectory>,
        namespace: &lillux::PinnedDirectoryLock,
        class: &str,
    ) -> Result<Self> {
        state_lock.ensure_protects_app_root(&config.app_root)?;
        namespace.ensure_protects(&runtime)?;
        runtime.ensure_path_binding()?;
        let runtime_dir = config.runtime_state_dir();
        let configured = lillux::PinnedDirectory::open(&runtime_dir)?
            .with_context(|| format!("runtime state is absent: {}", runtime_dir.display()))?;
        ensure!(
            runtime.path() == runtime_dir.as_path() && runtime.is_same_directory(&configured)?,
            "offline native session runtime differs from this node's runtime state directory {}",
            runtime_dir.display()
        );
        ryeos_scheduler_contract::validate_offline_native_class(class)?;
        let snapshot = load_signed_maintenance_snapshot(config, identity)?;
        let quota: OfflineNativeQuota = snapshot
            .require::<NodeMaintenancePolicy>()?
            .offline_native
            .get(class)
            .with_context(|| {
                format!("node maintenance policy grants no offline native quota for `{class}`")
            })?
            .clone();
        ryeos_node_runtime::node_policy::sections::maintenance::validate_offline_native_quota(&quota)
            .with_context(|| format!("offline native `{class}` quota"))?;
        let operation = Duration::from_millis(quota.max_operation_millis);
        let cleanup = operation
            .checked_add(Duration::from_millis(quota.max_cleanup_millis))
            .context("offline native window overflow")?;
        let authority = Arc::new(SessionAuthority {
            process: std::process::id(),
            app_root: config.app_root.clone(),
            state_lock: state_lock.retain(),
            runtime,
            namespace: namespace.clone(),
            operation: lillux::time::MonotonicDeadline::after(operation),
            cleanup: lillux::time::MonotonicDeadline::after(cleanup),
            class: class.to_owned(),
        });
        authority.checkpoint(false)?;
        // SAFETY: this envelope comes from the node-signed maintenance policy
        // for `class`, reserved once for this process while the caller holds
        // the exclusive node StateLock and runtime namespace lock; the session
        // authority revalidates both before every native effect.
        let raw = unsafe {
            OriginalNativeBudget::prepaid_by_original_issuer(
                quota.max_native_storage_bytes,
                quota.max_native_allocator_steps,
                quota.max_native_connections,
                quota.max_cleanup_steps,
            )
        }
        .map_err(|refusal| anyhow::anyhow!("offline native funding refused: {refusal}"))?;
        let host_authority: Arc<dyn OfflineNativeAuthority> = authority.clone();
        // SAFETY: `issue`'s contract holds: the caller's StateLock and this
        // frame's namespace lock exclude every other user of the node's
        // databases for the session, `raw` was minted once above for this
        // host, and `SessionAuthority` revalidates both locks and the class
        // windows on every checkpoint.
        match unsafe { OfflineNativeHost::issue(raw, host_authority) } {
            Ok(host) => Ok(Self { authority, host }),
            Err(failure) => match failure.release() {
                Ok(error) => Err(error.context(format!("issue offline native host for `{class}`"))),
                Err(retained) => Err(anyhow::Error::new(retained)),
            },
        }
    }

    /// Revalidate the stopped-node locks and the operation window.
    pub fn verify(&self) -> Result<()> {
        self.authority.checkpoint(false)
    }

    pub fn factory(&self) -> Result<&Arc<OriginalNativeFactory>> {
        self.host.factory()
    }

    pub fn destination(&self) -> Result<Arc<dyn OriginalNativeDestinationAuthority>> {
        self.host.destination()
    }

    /// A refusal hands the terminal back with the reason.
    pub fn retain_terminal(
        &self,
        terminal: OriginalNativeConnectionTerminal,
    ) -> std::result::Result<(), (OriginalNativeConnectionTerminal, anyhow::Error)> {
        self.host.retain_terminal(terminal)
    }

    /// Finish the host. A refusal keeps the session and its host for retry.
    pub fn finish(mut self) -> std::result::Result<OfflineNativeCensus, (Self, anyhow::Error)> {
        match self.host.finish() {
            Ok(census) => Ok(census),
            Err(error) => Err((self, error)),
        }
    }
}

/// Completed replay-index activation and the released host's census.
pub struct ReplayIndexActivation {
    pub database: PathBuf,
    pub report: ryeos_state::operational::ReplayIndexResetReport,
    pub census: OfflineNativeCensus,
}

/// Explicit stopped-node replay-index activation. The caller holds the
/// node StateLock; this pins and locks the runtime namespace, issues the
/// `replay-indexes` native session, activates and closes the Operational
/// database through it, and releases the host.
pub fn reset_replay_indexes(config: &Config, state_lock: &StateLock) -> Result<ReplayIndexActivation> {
    let runtime_dir = config.runtime_state_dir();
    let runtime = Arc::new(
        lillux::PinnedDirectory::open(&runtime_dir)?
            .with_context(|| format!("runtime state is absent: {}", runtime_dir.display()))?,
    );
    let namespace = runtime
        .lock_exclusive()
        .context("lock runtime state namespace for replay-index activation")?;
    let identity = NodeIdentity::load(&config.node_signing_key_path)
        .context("load node identity for replay-index activation")?;
    let session = OfflineNativeSession::open(
        config,
        &identity,
        state_lock,
        runtime.clone(),
        &namespace,
        REPLAY_INDEXES_CLASS,
    )?;
    let activated = (|| -> Result<ryeos_state::operational::ReplayIndexResetReport> {
        let verify = || session.verify();
        let retain = |terminal| session.retain_terminal(terminal);
        // SAFETY: the caller holds the node StateLock and this frame holds the
        // runtime namespace lock for the whole call; the session host was
        // funded under both and `verify` revalidates them with its window.
        unsafe {
            ryeos_state::operational::activate_replay_indexes_offline(
                runtime.clone(),
                namespace.clone(),
                session.factory()?,
                session.destination()?,
                &verify,
                &retain,
            )
        }
    })();
    match (activated, session.finish()) {
        (Ok(report), Ok(census)) => Ok(ReplayIndexActivation {
            database: runtime_dir.join(ryeos_state::operational::OPERATIONAL_DB_FILENAME),
            report,
            census,
        }),
        (Err(error), Ok(_)) => Err(error),
        (Ok(_), Err((session, error))) => Err(retained_session(session, error)
            .context("replay indexes activated; offline native host release retained")),
        (Err(original), Err((session, error))) => Err(retained_session(session, error).context(original)),
    }
}

/// Carry a session whose host could not be released inside the error, so
/// the owner survives until the process ends rather than being dropped.
fn retained_session(session: OfflineNativeSession, error: anyhow::Error) -> anyhow::Error {
    anyhow::Error::new(RetainedOfflineNativeSession { error, _session: session })
}

struct RetainedOfflineNativeSession {
    error: anyhow::Error,
    _session: OfflineNativeSession,
}
impl std::fmt::Debug for RetainedOfflineNativeSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetainedOfflineNativeSession").field("error", &self.error).finish_non_exhaustive()
    }
}
impl std::fmt::Display for RetainedOfflineNativeSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "offline native session retained: {:#}", self.error)
    }
}
impl std::error::Error for RetainedOfflineNativeSession {}
