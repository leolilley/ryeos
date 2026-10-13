//! Shared lock protocol for the installed bundle registry.
//!
//! Registry mutations and composition readers coordinate on the exact same
//! installed-bundles anchor. Transaction journals and publication stay in App.

use anyhow::Result;
use std::path::{Path, PathBuf};

/// Node-wide serialization for mutations of the installed bundle registry.
///
/// Every operation that can reconcile, re-plan, activate, register, or remove
/// an installed bundle must acquire this lock before acquiring a bundle's
/// per-name transaction lock. Keeping that order global -> per-name prevents
/// deadlocks while making each prospective plan exact until its mutation is
/// committed.
pub struct BundleRegistryMutationLock {
    app_root: PathBuf,
    _lock: lillux::ExclusiveFileLock,
}

/// A stable read view of the installed bundle registry.
///
/// Read-side engine operations share this lock with one another. Bundle
/// install/replace/remove paths take [`BundleRegistryMutationLock`], so a
/// verified generation cannot change while any reader is using it.
pub struct BundleRegistryReadLock {
    _lock: lillux::SharedFileLock,
}

impl BundleRegistryMutationLock {
    pub fn acquire(app_root: &Path) -> Result<Self> {
        let target = installed_bundles_directory(app_root);
        let lock = lillux::ExclusiveFileLock::acquire(&target)?;
        Ok(Self {
            app_root: app_root.to_path_buf(),
            _lock: lock,
        })
    }

    pub fn acquire_for_startup(app_root: &Path) -> Result<Self> {
        let target = installed_bundles_directory(app_root);
        let lock = lillux::ExclusiveFileLock::acquire_with_timeout(
            &target,
            std::time::Duration::from_secs(3),
        )?;
        Ok(Self {
            app_root: app_root.to_path_buf(),
            _lock: lock,
        })
    }

    pub fn ensure_protects_app_root(&self, app_root: &Path) -> Result<()> {
        if self.app_root != app_root {
            anyhow::bail!("bundle registry lock belongs to a different app root");
        }
        Ok(())
    }

    /// The root protected for this owner's lifetime. Domain transactions borrow
    /// this owner while taking their per-name locks; the path is not authority.
    pub fn app_root(&self) -> &Path {
        &self.app_root
    }
}

impl BundleRegistryReadLock {
    pub fn acquire(app_root: &Path) -> Result<Self> {
        let target = installed_bundles_directory(app_root);
        // The first read of an otherwise empty installed registry must establish
        // the same coordination anchor used by future writers. Requiring a
        // pre-existing anchor makes valid offline/disabled-isolation nodes fail
        // before they can observe the empty generation, while proceeding
        // unlocked would race the first bundle installation.
        let lock = lillux::SharedFileLock::acquire(&target)?;
        Ok(Self { _lock: lock })
    }

    pub fn acquire_for_composition(app_root: &Path) -> Result<Self> {
        let target = installed_bundles_directory(app_root);
        let lock = lillux::SharedFileLock::acquire_with_timeout(
            &target,
            std::time::Duration::from_secs(3),
        )?;
        Ok(Self { _lock: lock })
    }
}

fn installed_bundles_directory(app_root: &Path) -> PathBuf {
    app_root.join(ryeos_engine::AI_DIR).join("bundles")
}
