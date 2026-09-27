//! Stopped-node composition for exact bundle-set activation.
//!
//! The admission callback is deliberately supplied by the consumer service
//! that resolved and verified the immutable publication objects. Everything
//! after that boundary is owned here: stopped-node exclusion, registry
//! serialization, active-selection CAS, and pinned-public completion checking.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use sha2::{Digest as _, Sha256};

use ryeos_app::bundle_set_transaction::{
    ActiveBundleSelectionStore, BundleSetJournal, StoppedBundleSetApplyAuthority,
    StoppedBundleSetApplyRequest,
};

pub struct StoppedNodeBundleSetAuthority<F> {
    app_root: PathBuf,
    admit: F,
    selections: ActiveBundleSelectionStore,
}

impl<F> StoppedNodeBundleSetAuthority<F> {
    pub fn new(app_root: &Path, admit: F) -> Self {
        Self {
            app_root: app_root.to_owned(),
            admit,
            selections: ActiveBundleSelectionStore::new(app_root),
        }
    }
}

impl<F> StoppedBundleSetApplyAuthority for StoppedNodeBundleSetAuthority<F>
where
    F: Fn(&BundleSetJournal) -> anyhow::Result<()>,
{
    fn admit_complete_set(&self, journal: &BundleSetJournal) -> anyhow::Result<()> {
        (self.admit)(journal)
    }

    fn stage_active_selection(&self, journal: &BundleSetJournal) -> anyhow::Result<()> {
        self.selections.stage(journal)
    }

    fn verify_new_completion(
        &self,
        app_root: &Path,
        journal: &BundleSetJournal,
    ) -> anyhow::Result<()> {
        if app_root != self.app_root {
            bail!("bundle-set authority was composed for a different app root");
        }
        let bytes = lillux::read_regular_file_bounded_no_follow(
            &ryeos_app::bundle_set_transaction::init_completion_path(app_root),
            256 * 1024,
        )?;
        let digest = format!("{:x}", Sha256::digest(&bytes));
        if digest != journal.new_completion_bytes_hash || digest != journal.new_completion_hash {
            bail!("published init completion differs from the prospective signed document");
        }
        crate::verify_init_completion(app_root)?
            .context("prospective init completion was not present after publication")?;
        Ok(())
    }

    fn commit_active_selection(&self, journal: &BundleSetJournal) -> anyhow::Result<()> {
        self.selections.commit_new(journal)
    }
}

/// Execute the complete local mutation under both stopped-node and registry
/// exclusion. The caller's admission closure must verify the exact prospective
/// roots recorded by the journal; it cannot authorize any later mutation.
pub fn apply_stopped_bundle_set<F>(
    app_root: &Path,
    request: StoppedBundleSetApplyRequest,
    admit: F,
) -> anyhow::Result<()>
where
    F: Fn(&BundleSetJournal) -> anyhow::Result<()>,
{
    let _state_lock = ryeos_app::state_lock::StateLock::acquire(
        &ryeos_app::state_lock::default_lock_path(app_root),
    )
    .context("bundle-set apply requires the daemon to be stopped")?;
    let registry_lock =
        ryeos_app::bundle_transaction::BundleRegistryMutationLock::acquire(app_root)?;
    let authority = StoppedNodeBundleSetAuthority::new(app_root, admit);
    ryeos_app::bundle_set_transaction::apply_stopped_bundle_set(
        app_root,
        &registry_lock,
        request,
        &authority,
    )
}
