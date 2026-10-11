//! Authentic policy input for offline reset issuance. This owner deliberately
//! grants no copy, SQL, mutation, or financial allocation by itself.

use anyhow::{Context, Result, ensure};
use ryeos_scheduler_contract::{NodeMaintenancePolicy, OfflineResetQuota};

use ryeos_node_runtime::config::Config;
use ryeos_node_runtime::identity::NodeIdentity;
use ryeos_node_runtime::node_policy::NodePolicySnapshot;
use ryeos_node_runtime::state_lock::StateLock;

/// Retain the original compiled signed generation under the actual stopped
/// node and runtime namespace owners. No path or decoded quota recreates this
/// authority, and no caller can construct it from an unsigned profile.
pub(super) struct OfflineResetPolicyOwner<'a> {
    process: u32,
    state_lock: &'a StateLock,
    namespace_lock: &'a lillux::PinnedDirectoryLock,
    runtime: &'a lillux::PinnedDirectory,
    app_root: lillux::PinnedDirectory,
    snapshot: NodePolicySnapshot,
    signer_fingerprint: String,
    db_target: std::path::PathBuf,
}

impl<'a> OfflineResetPolicyOwner<'a> {
    pub(super) fn load(
        config: &Config,
        identity: &NodeIdentity,
        state_lock: &'a StateLock,
        runtime: &'a lillux::PinnedDirectory,
        namespace_lock: &'a lillux::PinnedDirectoryLock,
    ) -> Result<Self> {
        state_lock.ensure_protects_app_root(&config.app_root)?;
        namespace_lock.ensure_protects(runtime)?;
        runtime.ensure_path_binding()?;
        let app_root = lillux::PinnedDirectory::open(&config.app_root)?
            .context("offline reset policy app root is absent")?;
        let snapshot = crate::offline_native::load_signed_maintenance_snapshot(config, identity)
            .context("load complete node-signed offline reset policy generation")?;
        let owner = Self {
            process: std::process::id(),
            state_lock,
            namespace_lock,
            runtime,
            app_root,
            snapshot,
            signer_fingerprint: identity.fingerprint().to_owned(),
            db_target: config.db_path.clone(),
        };
        owner.require_held()?;
        owner.quota()?;
        Ok(owner)
    }

    pub(super) fn require_held(&self) -> Result<()> {
        ensure!(self.process == std::process::id(), "foreign original offline policy owner");
        self.app_root.ensure_path_binding()?;
        self.runtime.ensure_path_binding()?;
        self.state_lock
            .ensure_protects_app_root(self.app_root.path())?;
        self.namespace_lock.ensure_protects(self.runtime)?;
        Ok(())
    }

    /// Move every owned field while ending only the original borrowed lock
    /// references. This token grants no mutation or disposal authority.
    pub(super) fn into_original_outside(self) -> OriginalOfflinePolicyOutside {
        OriginalOfflinePolicyOutside {
            process: self.process,
            app_root: std::mem::ManuallyDrop::new(Some(self.app_root)),
            shared_root: std::mem::ManuallyDrop::new(None),
            registration: None,
            _snapshot: std::mem::ManuallyDrop::new(self.snapshot),
            _signer_fingerprint: std::mem::ManuallyDrop::new(self.signer_fingerprint),
            _db_target: std::mem::ManuallyDrop::new(self.db_target),
        }
    }

    /// Lifelines for copy work already issued under this authentic owner.
    /// The retained StateLockLease cannot issue stopped-node mutation authority.
    pub(super) fn retain_copy_inputs(
        &self,
    ) -> Result<(
        ryeos_node_runtime::state_lock::StateLockLease,
        lillux::PinnedDirectoryLock,
        NodePolicySnapshot,
    )> {
        self.require_held()?;
        Ok((
            self.state_lock.retain(),
            self.namespace_lock.clone(),
            self.snapshot.clone(),
        ))
    }

    pub(super) fn db_target(&self) -> Result<&std::path::Path> {
        self.require_held()?;
        Ok(&self.db_target)
    }

    pub(super) fn directories(
        &self,
    ) -> Result<(&lillux::PinnedDirectory, &lillux::PinnedDirectory)> {
        self.require_held()?;
        Ok((&self.app_root, self.runtime))
    }

    /// Authenticated inputs for the eventual durable issuer, NOT an allocation.
    pub(super) fn quota(&self) -> Result<&OfflineResetQuota> {
        self.require_held()?;
        self.snapshot
            .require::<NodeMaintenancePolicy>()?
            .offline_reset
            .as_ref()
            .context("node maintenance policy disables offline execution-history reset")
    }

    pub(super) fn generation_digest(&self) -> Result<&str> {
        self.require_held()?;
        Ok(self.snapshot.generation_digest())
    }

    pub(super) fn signer_fingerprint(&self) -> Result<&str> {
        self.require_held()?;
        Ok(&self.signer_fingerprint)
    }
}

pub(super) struct OriginalOfflinePolicyOutside {
    pub(super) process: u32,
    pub(super) app_root: std::mem::ManuallyDrop<Option<lillux::PinnedDirectory>>,
    pub(super) shared_root: std::mem::ManuallyDrop<Option<std::sync::Arc<lillux::PinnedDirectory>>>,
    pub(super) registration: Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration>,
    _snapshot: std::mem::ManuallyDrop<NodePolicySnapshot>,
    _signer_fingerprint: std::mem::ManuallyDrop<String>,
    _db_target: std::mem::ManuallyDrop<std::path::PathBuf>,
}
