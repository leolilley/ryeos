//! Process-local custody for one Q backend-created workspace.
//!
//! The durable Q start row identifies cleanup work after restart, but cannot
//! recreate an original backend view descriptor or a pinned state-root owner.
//! Register the original guards before backend Create. A surviving entry is
//! custody only; it does not authorize a Worker launch, view borrow, physical
//! close, or positive qualification claim. There is deliberately no remove API
//! until exact Q close, Destroy, and directory-removal proofs are implemented.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, ensure};

use crate::runtime_db::external_execution::{
    QualificationLocalStartPhase, QualificationLocalStartRecord,
};
use crate::temp_dir_guard::TempDirGuard;

/// Immutable Q identity retained across the start journal's phase changes.
/// The diagnostic path fields identify original guards; they cannot recreate
/// either guard after the daemon loses them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QualificationWorkspaceCustodyCoordinate {
    placement_thread_id: String,
    accepted_root_id: String,
    accepted_capsule_hash: String,
    qualification_operation_id: String,
    qualification_occurrence_id: String,
    channel_binding_digest: String,
    daemon_generation_id: String,
    base_snapshot_hash: String,
    workspace_id: String,
    workspace_root: String,
    state_root: String,
    launch_owner: String,
}

impl QualificationWorkspaceCustodyCoordinate {
    /// Construct only from a checked Q start row. Later phases preserve these
    /// coordinates, so this also names custody during fenced cleanup.
    pub fn from_start(record: &QualificationLocalStartRecord) -> Result<Self> {
        let digest = ryeos_state::objects::canonical_value_digest(&serde_json::json!({
            "placement": &record.placement_thread_id,
            "root": &record.accepted_root_id,
            "capsule": &record.accepted_capsule_hash,
            "operation": &record.qualification_operation_id,
            "occurrence": &record.qualification_occurrence_id,
            "channel": &record.channel_binding_digest,
            "daemon": &record.daemon_generation_id,
        }))?;
        let expected_workspace = format!("QW-{digest}");
        let expected_owner = format!("Q-local-{digest}");
        ensure!(
            record
                .placement_thread_id
                .strip_prefix("Q-")
                .is_some_and(lillux::valid_hash)
                && record.daemon_generation_id == crate::runtime_db::daemon_generation_id()
                && record.workspace_id == expected_workspace
                && record.launch_owner == expected_owner
                && [
                    &record.accepted_capsule_hash,
                    &record.qualification_operation_id,
                    &record.channel_binding_digest,
                    &record.base_snapshot_hash,
                ]
                .into_iter()
                .all(|hash| lillux::valid_hash(hash)),
            "Q workspace custody has no current exact start coordinate"
        );
        let workspace_root = Path::new(&record.workspace_root);
        let cache_root = workspace_root
            .parent()
            .and_then(Path::parent)
            .context("Q workspace custody has no cache parent")?;
        let expected_workspace_parent = cache_root.join("executions");
        let expected_state_root = cache_root
            .join("qualification-local")
            .join(&record.workspace_id);
        ensure!(
            workspace_root.is_absolute()
                && workspace_root.parent() == Some(expected_workspace_parent.as_path())
                && workspace_root.file_name().and_then(|name| name.to_str())
                    == Some(record.workspace_id.as_str())
                && Path::new(&record.state_root) == expected_state_root.as_path(),
            "Q workspace custody changed its original directory coordinates"
        );
        Ok(Self {
            placement_thread_id: record.placement_thread_id.clone(),
            accepted_root_id: record.accepted_root_id.clone(),
            accepted_capsule_hash: record.accepted_capsule_hash.clone(),
            qualification_operation_id: record.qualification_operation_id.clone(),
            qualification_occurrence_id: record.qualification_occurrence_id.clone(),
            channel_binding_digest: record.channel_binding_digest.clone(),
            daemon_generation_id: record.daemon_generation_id.clone(),
            base_snapshot_hash: record.base_snapshot_hash.clone(),
            workspace_id: record.workspace_id.clone(),
            workspace_root: record.workspace_root.clone(),
            state_root: record.state_root.clone(),
            launch_owner: record.launch_owner.clone(),
        })
    }

    pub fn placement(&self) -> &str {
        &self.placement_thread_id
    }
}

/// Original process-local guards, retained even if Q preparation or pool bind
/// returns an error. Borrowing this owner does not clone the backend view.
pub struct QualificationWorkspaceCustody {
    coordinate: QualificationWorkspaceCustodyCoordinate,
    workspace: Arc<TempDirGuard>,
    state_root: lillux::PinnedDirectory,
    /// A Create view that could not be installed remains held in this daemon.
    /// It cannot qualify or be closed by a path-based replacement owner.
    uninstalled_views: Mutex<
        Vec<(
            ryeos_engine::isolation::WorkspaceLifecycleEvidence,
            ryeos_engine::isolation::CreatedWorkspaceView,
        )>,
    >,
}

impl QualificationWorkspaceCustody {
    pub fn coordinate(&self) -> &QualificationWorkspaceCustodyCoordinate {
        &self.coordinate
    }

    pub fn workspace(&self) -> &Arc<TempDirGuard> {
        &self.workspace
    }

    pub fn state_root(&self) -> &lillux::PinnedDirectory {
        &self.state_root
    }

    pub fn retain_uninstalled_view(
        &self,
        evidence: ryeos_engine::isolation::WorkspaceLifecycleEvidence,
        authority: ryeos_engine::isolation::CreatedWorkspaceView,
    ) {
        self.uninstalled_views
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((evidence, authority));
    }

    /// Ordinary receipt-based closure cannot account for a Create descriptor
    /// which failed installation into the original guard.
    pub fn ensure_no_uninstalled_view(&self) -> Result<()> {
        let views = self
            .uninstalled_views
            .lock()
            .map_err(|_| anyhow::anyhow!("Q uninstalled view custody is poisoned"))?;
        ensure!(
            views.is_empty(),
            "Q original workspace has an uninstalled Create view requiring recovery"
        );
        Ok(())
    }
}

/// One entry per Q placement, empty after restart. A cold process may read the
/// durable Q row for reconciliation but cannot adopt an old descriptor from a
/// path or claim that the vanished original view was physically closed.
#[derive(Default)]
pub struct QualificationWorkspaceCustodyRegistry {
    entries: Mutex<BTreeMap<String, Arc<QualificationWorkspaceCustody>>>,
}

impl QualificationWorkspaceCustodyRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register the exact guards returned by original Q workspace and state
    /// root creation while the durable start is still Reserved, before backend
    /// Create. A second insertion is refused even if its coordinates match.
    pub fn insert_exact(
        &self,
        record: &QualificationLocalStartRecord,
        workspace: Arc<TempDirGuard>,
        state_root: &lillux::PinnedDirectory,
    ) -> Result<Arc<QualificationWorkspaceCustody>> {
        ensure!(
            record.phase == QualificationLocalStartPhase::Reserved,
            "Q workspace custody must precede backend Create"
        );
        let coordinate = QualificationWorkspaceCustodyCoordinate::from_start(record)?;
        let project = Path::new(&record.workspace_root).join("project");
        ensure!(
            workspace.path().as_deref() == Some(Path::new(&record.workspace_root))
                && workspace.owns_effective_path(&project)
                && workspace.owns_workspace_project_path(&project)
                && state_root.path() == Path::new(&record.state_root),
            "Q workspace custody did not receive its original guards"
        );
        state_root.ensure_path_binding()?;
        let pinned_state_root = state_root.try_clone()?;
        pinned_state_root.ensure_path_binding()?;
        ensure!(
            pinned_state_root.is_same_directory(state_root)?,
            "Q state-root custody changed its pinned directory"
        );
        let entry = Arc::new(QualificationWorkspaceCustody {
            coordinate,
            workspace,
            state_root: pinned_state_root,
            uninstalled_views: Mutex::new(Vec::new()),
        });
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| anyhow::anyhow!("Q workspace custody registry is poisoned"))?;
        ensure!(
            !entries.contains_key(entry.coordinate.placement()),
            "Q workspace already has original process-local custody"
        );
        entries.insert(entry.coordinate.placement().to_owned(), Arc::clone(&entry));
        Ok(entry)
    }

    /// Retrieve the same registered owner by every immutable Q coordinate.
    /// Missing after restart is a fail-closed cleanup state, not path recovery.
    pub fn borrow_exact(
        &self,
        coordinate: &QualificationWorkspaceCustodyCoordinate,
    ) -> Result<Arc<QualificationWorkspaceCustody>> {
        let entry = self
            .entries
            .lock()
            .map_err(|_| anyhow::anyhow!("Q workspace custody registry is poisoned"))?
            .get(coordinate.placement())
            .cloned()
            .context("Q original workspace custody is unavailable in this daemon")?;
        ensure!(
            entry.coordinate == *coordinate,
            "Q workspace custody changed its original immutable coordinate"
        );
        Ok(entry)
    }
}
