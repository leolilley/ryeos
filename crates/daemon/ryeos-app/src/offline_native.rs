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
use lillux_sqlite::offline_host::{
    OfflineNativeAuthority, OfflineNativeFacts, OfflineNativeHost, OfflineNativeReleased,
    OfflineNativeRetained,
};
use lillux_sqlite::{
    OriginalNativeBudget, OriginalNativeConnectionTerminal, OriginalNativeDestinationAuthority,
    OriginalNativeFactory,
};
use ryeos_node_runtime::config::Config;
use ryeos_node_runtime::identity::NodeIdentity;
use ryeos_node_runtime::node_policy::{NodePolicySnapshot, NodePolicyTable};
use ryeos_node_runtime::state_lock::{StateLock, StateLockLease};
use ryeos_scheduler_contract::{NodeMaintenancePolicy, OfflineNativeQuota};
use ryeos_state::operational::CarriedOwners;
use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Operation class of explicit replay-index activation.
pub const REPLAY_INDEXES_CLASS: &str = "replay-indexes";

/// Operation class of the explicit external-content binding cutover.
pub const EXTERNAL_CONTENT_BINDINGS_CLASS: &str = "external-content-bindings";

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

/// Refuse a node that has never completed its first start. Init creates the
/// runtime state directory, but only the first start creates the Operational
/// initialization marker and the CAS mutation lock the offline resets open;
/// without them the resets would fail deep inside native admission.
pub(crate) fn require_started_once(runtime: &lillux::PinnedDirectory) -> Result<()> {
    let mut absent = Vec::new();
    let operational = ryeos_state::operational::OPERATIONAL_INITIALIZED_FILENAME;
    if runtime.open_regular(OsStr::new(operational), false)?.is_none() {
        absent.push(operational.to_owned());
    }
    let recovery = ryeos_state::recovery::RECOVERY_DIRECTORY_NAME;
    let lock = ryeos_state::recovery::CAS_MUTATION_LOCK_FILENAME;
    let lock_present = match runtime.open_child_directory(OsStr::new(recovery))? {
        Some(directory) => directory.open_regular(OsStr::new(lock), false)?.is_some(),
        None => false,
    };
    if !lock_present {
        absent.push(format!("{recovery}/{lock}"));
    }
    ensure!(
        absent.is_empty(),
        "this node has never started; start it once, then rerun (first-start artefacts absent in {}: {})",
        runtime.path().display(),
        absent.join(", ")
    );
    Ok(())
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
        require_started_once(&runtime)?;
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

    /// Finish the host and return the facts its release captured. A refusal
    /// keeps the session and its host for retry; a release that captured no
    /// facts has already ended the host, so only the refusal remains.
    pub fn finish(self) -> std::result::Result<OfflineNativeFacts, (Option<Self>, anyhow::Error)> {
        let Self { authority, host } = self;
        match host.finish() {
            // The region release fact is plain data the session does not report.
            Ok(OfflineNativeReleased { facts, region: _ }) => {
                facts.ok_or_else(|| {
                    (None, anyhow::anyhow!("offline native session host released without backend facts"))
                })
            }
            Err(OfflineNativeRetained { host, error }) => Err((Some(Self { authority, host }), error)),
        }
    }
}

/// Completed replay-index activation and the released host's facts.
pub struct ReplayIndexActivation {
    pub database: PathBuf,
    pub report: ryeos_state::operational::ReplayIndexResetReport,
    pub facts: OfflineNativeFacts,
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
        (Ok(report), Ok(facts)) => Ok(ReplayIndexActivation {
            database: runtime_dir.join(ryeos_state::operational::OPERATIONAL_DB_FILENAME),
            report,
            facts,
        }),
        (Err(error), Ok(_)) => Err(error),
        (Ok(_), Err((session, error))) => Err(retained_session(session, error)
            .context("replay indexes activated; offline native host release retained")),
        (Err(original), Err((session, error))) => {
            let error = blocked_by_carried(&original, error, "activation");
            Err(retained_session(session, error).context(original))
        }
    }
}

/// Completed external-content binding cutover (or its preview) and the
/// released host's facts.
pub struct ExternalContentBindingReset {
    pub bindings: usize,
    pub facts: OfflineNativeFacts,
}

/// Explicit stopped-node retirement of every external-content binding head.
/// The caller holds the node StateLock; this pins and locks the runtime
/// namespace, issues the `external-content-bindings` native session, opens
/// the State authority roots with a transient projection on that session,
/// retires (or, for `dry_run`, counts) the heads, closes State and releases
/// the host.
///
/// The only SQLite this opens is the transient in-memory projection. The
/// cutover itself unlinks signed head files and publishes the epoch file
/// through pinned directories, each step idempotent on rerun, so nothing the
/// native session funds outlives this process.
pub fn reset_external_content_bindings(
    config: &Config,
    state_lock: &StateLock,
    dry_run: bool,
) -> Result<ExternalContentBindingReset> {
    let runtime_dir = config.runtime_state_dir();
    let runtime = Arc::new(
        lillux::PinnedDirectory::open(&runtime_dir)?
            .with_context(|| format!("runtime state is absent: {}", runtime_dir.display()))?,
    );
    let namespace = runtime
        .lock_exclusive()
        .context("lock runtime state namespace for external-content binding reset")?;
    let identity = NodeIdentity::load(&config.node_signing_key_path)
        .context("load node identity for external-content binding reset")?;
    let mut trust = ryeos_state::refs::TrustStore::new();
    trust.insert(identity.fingerprint().to_owned(), *identity.verifying_key());
    let trust = Arc::new(trust);
    let session = OfflineNativeSession::open(
        config,
        &identity,
        state_lock,
        runtime.clone(),
        &namespace,
        EXTERNAL_CONTENT_BINDINGS_CLASS,
    )?;
    let discarded = (|| -> Result<usize> {
        session.verify()?;
        // SAFETY: the caller holds the node StateLock and this frame holds the
        // runtime namespace lock for the whole call; the session host was
        // funded under both and its destination revalidates them with the
        // class windows on every native boundary.
        let state = unsafe {
            ryeos_state::StateDb::open_offline_native(
                runtime.try_clone()?,
                &namespace,
                trust,
                session.factory()?,
                session.destination()?,
            )
        }
        .context("open pinned State authority for external-content binding reset")?;
        let discarded = (|| -> Result<usize> {
            let authority = state.pinned_authority()?;
            let guard = authority.acquire_exclusive_guard(!dry_run)?;
            state.discard_external_content_binding_heads(&guard, dry_run)
        })();
        let terminal = match state.close_offline_native(session.factory()?) {
            Ok(terminal) => terminal,
            Err((state, error)) => {
                let error = match discarded {
                    Ok(bindings) => error.context(format!(
                        "external-content binding reset {} {bindings} head(s); State close refused",
                        if dry_run { "counted" } else { "retired" }
                    )),
                    Err(original) => error.context(original),
                };
                return Err(match state {
                    Some(state) => CarriedOwners::carry(error, state),
                    None => error,
                });
            }
        };
        match session.retain_terminal(terminal) {
            Ok(()) => discarded,
            Err((terminal, refusal)) => {
                let error = match discarded {
                    Ok(bindings) => refusal.context(format!(
                        "external-content binding reset {} {bindings} head(s); projection terminal not retained",
                        if dry_run { "counted" } else { "retired" }
                    )),
                    Err(original) => refusal.context(original),
                };
                Err(CarriedOwners::carry(error, terminal))
            }
        }
    })();
    match (discarded, session.finish()) {
        (Ok(bindings), Ok(facts)) => Ok(ExternalContentBindingReset { bindings, facts }),
        (Err(error), Ok(_)) => Err(error),
        (Ok(_), Err((session, error))) => Err(retained_session(session, error)
            .context("external-content binding reset done; offline native host release retained")),
        (Err(original), Err((session, error))) => {
            let error = blocked_by_carried(&original, error, "reset");
            Err(retained_session(session, error).context(original))
        }
    }
}

/// Name the owners an operation refusal still carries, which keep the host's
/// factory or terminals alive and so block its release.
fn blocked_by_carried(original: &anyhow::Error, error: anyhow::Error, operation: &str) -> anyhow::Error {
    match original.downcast_ref::<CarriedOwners>() {
        Some(carried) => {
            let names = carried.owner_names();
            error.context(format!(
                "offline native host release is blocked by {} carried owner(s) still alive in the {operation} refusal: {}",
                names.len(),
                names.join(", ")
            ))
        }
        None => error,
    }
}

/// Carry a session whose host could not be released inside the error, so
/// the owner survives until the process ends rather than being dropped.
fn retained_session(session: Option<OfflineNativeSession>, error: anyhow::Error) -> anyhow::Error {
    match session {
        Some(session) => anyhow::Error::new(RetainedOfflineNativeSession { error, _session: session }),
        None => error,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime(dir: &std::path::Path) -> lillux::PinnedDirectory {
        lillux::PinnedDirectory::open(dir).unwrap().unwrap()
    }

    #[test]
    fn never_started_root_is_refused_up_front_naming_the_absent_artefacts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("recovery/thread-projection")).unwrap();
        let error = require_started_once(&runtime(dir.path())).expect_err("an init-only root is refused");
        let message = format!("{error:#}");
        assert!(message.contains("this node has never started; start it once, then rerun"), "{message}");
        assert!(message.contains("operational.initialized") && message.contains("recovery/cas-mutation.lock"), "{message}");

        std::fs::write(
            dir.path().join(ryeos_state::operational::OPERATIONAL_INITIALIZED_FILENAME),
            ryeos_state::operational::OPERATIONAL_INITIALIZED_CONTENT,
        )
        .unwrap();
        let error = require_started_once(&runtime(dir.path())).expect_err("a missing CAS lock is still refused");
        let message = format!("{error:#}");
        assert!(message.contains("never started") && !message.contains("operational.initialized"), "{message}");

        std::fs::write(dir.path().join("recovery/cas-mutation.lock"), b"").unwrap();
        require_started_once(&runtime(dir.path())).expect("a started root is admitted");
    }
}
