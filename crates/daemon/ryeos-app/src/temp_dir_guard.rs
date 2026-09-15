//! Shared RAII guard for materialised temp directories.
//!
//! A single `TempDirGuard` type used across the engine cache, executor,
//! and API layers. Wrap in `Arc<TempDirGuard>` for shared ownership:
//!
//! | Owner | What it holds |
//! |---|---|
//! | Engine derived-project cache | `Arc<TempDirGuard>` for its shared generation |
//! | Admitted request binding | `Arc<TempDirGuard>` for its active checkout |
//! | Request runner | `Arc<TempDirGuard>` for project checkout |
//! | Callback token lifeline | `Arc<TempDirGuard>` (callback workstream) |
//!
//! The resolution cache is deliberately different: it retains no project
//! materialization guard, and rebinds hits to the current admitted checkout.
//!
//! Ordinary temporary directories are removed when the last holder drops.
//! Shared cache generations instead release their cache leases; eviction owns
//! their removal. Journal-owned workspaces are deliberately preserved on Drop
//! and require the explicit owner-fenced lifecycle to remove them. An Arc's
//! lifetime is not proof that every independently launched workspace borrower
//! has stopped. Keep that proof in the existing launch/workspace authorities,
//! not in a reference-count check or a guard reconstructed from a path.
//!
//! The internal path slot allows `disarm()` to transfer ordinary cleanup
//! ownership without removing the directory.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

struct PinnedRemoval {
    parent: lillux::PinnedDirectory,
    name: std::ffi::OsString,
    root: lillux::PinnedDirectory,
}

struct OwnedWorkspaceView {
    workspace_id: String,
    view_identity: String,
    authority: ryeos_engine::isolation::CreatedWorkspaceView,
}

/// This is the original materialization's local descriptor slot, not a process
/// tracker. The runtime journal separately fences every borrower. In particular,
/// `Uncreated` after a failed transfer or restart is NOT physical-close proof.
enum WorkspaceViewSlot {
    Uncreated,
    Available(OwnedWorkspaceView),
    Draining(OwnedWorkspaceView),
    Closed {
        workspace_id: String,
        view_identity: String,
    },
}

/// Materialization lifeline with explicit temporary, cache, and journal-owned
/// workspace cleanup modes. Only ordinary temporary guards remove on Drop.
pub struct TempDirGuard {
    inner: Mutex<Option<PathBuf>>,
    effective_path: PathBuf,
    leases: Mutex<Vec<std::fs::File>>,
    explicit_cleanup: AtomicBool,
    remove_on_drop: AtomicBool,
    owns_removal: bool,
    pinned_removal: Option<PinnedRemoval>,
    workspace_view: Mutex<Option<WorkspaceViewSlot>>,
    /// A projectless controller may create one separately confined workspace.
    /// Retain its ORIGINAL guard here; neither path lookup nor a pool registry
    /// may recreate this local physical-close authority.
    owned_workspace_lifeline: Mutex<Option<Arc<TempDirGuard>>>,
}

impl TempDirGuard {
    pub fn new(path: PathBuf) -> Self {
        Self {
            inner: Mutex::new(Some(path.clone())),
            effective_path: path,
            leases: Mutex::new(Vec::new()),
            explicit_cleanup: AtomicBool::new(false),
            remove_on_drop: AtomicBool::new(true),
            owns_removal: true,
            pinned_removal: None,
            workspace_view: Mutex::new(None),
            owned_workspace_lifeline: Mutex::new(None),
        }
    }

    /// A backend-owned workspace must be destroyed and descriptor-removed by
    /// its owner-fenced lifecycle before the journal can close.
    pub fn new_workspace(path: PathBuf, effective_path: PathBuf) -> anyhow::Result<Self> {
        if effective_path.parent() != Some(path.as_path()) {
            anyhow::bail!(
                "workspace effective path {} is not a direct child of its owned root {}",
                effective_path.display(),
                path.display()
            );
        }
        Ok(Self {
            inner: Mutex::new(Some(path)),
            effective_path,
            leases: Mutex::new(Vec::new()),
            explicit_cleanup: AtomicBool::new(true),
            remove_on_drop: AtomicBool::new(false),
            owns_removal: true,
            pinned_removal: None,
            workspace_view: Mutex::new(Some(WorkspaceViewSlot::Uncreated)),
            owned_workspace_lifeline: Mutex::new(None),
        })
    }

    /// Hold a lease and stable path to a shared derived cache generation.
    /// Dropping the guard releases the lease but never removes the shared
    /// generation; cache eviction owns deletion after all leases are gone.
    pub fn new_borrowed_cache(path: PathBuf) -> Self {
        Self {
            inner: Mutex::new(Some(path.clone())),
            effective_path: path,
            leases: Mutex::new(Vec::new()),
            explicit_cleanup: AtomicBool::new(false),
            remove_on_drop: AtomicBool::new(false),
            owns_removal: false,
            pinned_removal: None,
            workspace_view: Mutex::new(None),
            owned_workspace_lifeline: Mutex::new(None),
        }
    }

    pub(crate) fn new_pinned(
        parent: lillux::PinnedDirectory,
        name: std::ffi::OsString,
        root: lillux::PinnedDirectory,
    ) -> Self {
        let path = root.path().to_path_buf();
        Self {
            inner: Mutex::new(Some(path.clone())),
            effective_path: path,
            leases: Mutex::new(Vec::new()),
            explicit_cleanup: AtomicBool::new(false),
            remove_on_drop: AtomicBool::new(true),
            owns_removal: true,
            pinned_removal: Some(PinnedRemoval { parent, name, root }),
            workspace_view: Mutex::new(None),
            owned_workspace_lifeline: Mutex::new(None),
        }
    }

    /// Borrow the original descriptor of an owned scratch root. This never
    /// reopens its diagnostic path or grants access to borrowed generations.
    pub fn owned_scratch_root(&self) -> anyhow::Result<&lillux::PinnedDirectory> {
        self.pinned_removal
            .as_ref()
            .map(|owned| &owned.root)
            .ok_or_else(|| anyhow::anyhow!("temporary guard has no owned scratch descriptor"))
    }

    /// Install the accepted Create result before publishing Ready. A later
    /// journal-bind failure must keep this very slot for explicit closure.
    pub fn install_workspace_view(
        &self,
        evidence: &ryeos_engine::isolation::WorkspaceLifecycleEvidence,
        authority: ryeos_engine::isolation::CreatedWorkspaceView,
    ) -> anyhow::Result<()> {
        if evidence.operation != ryeos_isolation_protocol::WorkspaceLifecycleOperation::Create {
            anyhow::bail!("only Create may install a workspace view");
        }
        let path = self
            .path()
            .ok_or_else(|| anyhow::anyhow!("workspace is disarmed"))?;
        if path.file_name().and_then(|name| name.to_str()) != Some(&evidence.workspace_id) {
            anyhow::bail!("created view belongs to another workspace");
        }
        let view_identity = evidence
            .mount_identity
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("created workspace has no view identity"))?;
        let mut slot = self.workspace_view.lock().unwrap();
        if !matches!(*slot, Some(WorkspaceViewSlot::Uncreated)) {
            anyhow::bail!("workspace view slot is not awaiting its original Create result");
        }
        *slot = Some(WorkspaceViewSlot::Available(OwnedWorkspaceView {
            workspace_id: evidence.workspace_id.clone(),
            view_identity: view_identity.clone(),
            authority,
        }));
        Ok(())
    }

    /// Return the exact operational coordinate for durable borrower admission.
    /// Ordinary materializations have no workspace membership. Construction,
    /// draining and closed slots cannot issue new borrow authority.
    pub fn workspace_view_identity(&self) -> anyhow::Result<Option<(String, String)>> {
        let slot = self.workspace_view.lock().unwrap();
        match slot.as_ref() {
            None => Ok(None),
            Some(WorkspaceViewSlot::Available(view)) => Ok(Some((
                view.workspace_id.clone(),
                view.view_identity.clone(),
            ))),
            Some(_) => anyhow::bail!("workspace view is not accepting borrowers"),
        }
    }

    /// Call only after exact durable borrower admission. The journal must
    /// continue counting that reservation until all launch/held-process aliases
    /// settle; taking this clone is not an independent admission path.
    pub fn borrow_workspace_view(
        &self,
        workspace_id: &str,
        view_identity: &str,
    ) -> anyhow::Result<Option<lillux::InheritedDescriptorAuthority>> {
        let slot = self.workspace_view.lock().unwrap();
        let Some(WorkspaceViewSlot::Available(view)) = slot.as_ref() else {
            anyhow::bail!("workspace view is not accepting borrowers");
        };
        if view.workspace_id != workspace_id || view.view_identity != view_identity {
            anyhow::bail!("workspace borrow coordinate changed");
        }
        Ok(match &view.authority {
            ryeos_engine::isolation::CreatedWorkspaceView::Disabled => None,
            ryeos_engine::isolation::CreatedWorkspaceView::Descriptor(authority) => {
                Some(authority.clone())
            }
        })
    }

    /// After the caller fences admission and proves all exact borrowers dead,
    /// physically close the original registered descriptor. Alias/timeout
    /// refusal leaves the owner in a non-borrowable draining slot for retry.
    /// An Arc count, a fresh guard or an empty slot cannot substitute for this.
    pub fn close_workspace_view(
        &self,
        workspace_id: &str,
        view_identity: &str,
        deadline: lillux::time::MonotonicDeadline,
    ) -> anyhow::Result<()> {
        let mut slot = self.workspace_view.lock().unwrap();
        match slot.as_ref() {
            Some(WorkspaceViewSlot::Closed {
                workspace_id: id,
                view_identity: view,
            }) if id == workspace_id && view == view_identity => return Ok(()),
            Some(WorkspaceViewSlot::Available(view) | WorkspaceViewSlot::Draining(view))
                if view.workspace_id == workspace_id && view.view_identity == view_identity => {}
            _ => anyhow::bail!("original workspace view closure authority is unavailable"),
        }
        let Some(WorkspaceViewSlot::Available(mut view) | WorkspaceViewSlot::Draining(mut view)) =
            slot.take()
        else {
            unreachable!("matching retained workspace view checked above")
        };
        if let ryeos_engine::isolation::CreatedWorkspaceView::Descriptor(authority) = view.authority
        {
            if let Err((authority, error)) = authority.try_close_last_owner(deadline) {
                view.authority =
                    ryeos_engine::isolation::CreatedWorkspaceView::Descriptor(authority);
                *slot = Some(WorkspaceViewSlot::Draining(view));
                return Err(error.into());
            }
        }
        *slot = Some(WorkspaceViewSlot::Closed {
            workspace_id: view.workspace_id,
            view_identity: view.view_identity,
        });
        Ok(())
    }

    /// Retain an exact-generation cache lease for the lifetime of this guard.
    pub fn retain_lease(&self, lease: std::fs::File) {
        self.leases.lock().unwrap().push(lease);
    }

    /// Association only: isolation still pins and verifies this named child
    /// against the node's runtime workspace authority.
    pub fn owns_workspace_project_path(&self, candidate: &std::path::Path) -> bool {
        self.path().is_some_and(|root| {
            candidate.parent() == Some(root.as_path())
                && candidate.file_name().and_then(|name| name.to_str())
                    == Some(ryeos_engine::execution_workspace::PROJECT_DIR)
                && self.workspace_view.lock().unwrap().is_some()
        })
    }

    pub fn retain_owned_workspace_lifeline(
        &self,
        workspace: Arc<TempDirGuard>,
    ) -> anyhow::Result<()> {
        if self.pinned_removal.is_none()
            || self.workspace_view.lock().unwrap().is_some()
            || std::ptr::eq(self, workspace.as_ref())
            || workspace.workspace_view.lock().unwrap().is_none()
            || workspace.owned_workspace_lifeline.lock().unwrap().is_some()
        {
            anyhow::bail!("only a pinned scratch owner may retain one original workspace lifeline");
        }
        let root = self
            .path()
            .ok_or_else(|| anyhow::anyhow!("scratch owner is disarmed"))?;
        let child_root = workspace
            .path()
            .ok_or_else(|| anyhow::anyhow!("workspace is disarmed"))?;
        if root == child_root || child_root.parent() != root.parent() {
            anyhow::bail!(
                "workspace must be a separate sibling, outside the controller's writable root"
            );
        }
        let mut slot = self.owned_workspace_lifeline.lock().unwrap();
        if let Some(current) = slot.as_ref() {
            if !Arc::ptr_eq(current, &workspace) {
                anyhow::bail!("controller already retains another original workspace");
            }
        } else {
            *slot = Some(workspace);
        }
        Ok(())
    }

    /// Exact existing owner, never a new guard synthesized from a journal path.
    pub fn owned_workspace_lifeline(self: &Arc<Self>) -> anyhow::Result<Option<Arc<Self>>> {
        if self.workspace_view.lock().unwrap().is_some() {
            return Ok(Some(self.clone()));
        }
        Ok(self.owned_workspace_lifeline.lock().unwrap().clone())
    }

    /// A durable journal now owns recovery. From this point, Drop preserves
    /// the directory and only the explicit owner-fenced lifecycle may remove
    /// it. Before this transition the guard remains an automatic rollback for
    /// filesystem creation that never reached durable reservation.
    pub fn preserve_for_explicit_cleanup(&self) {
        self.explicit_cleanup.store(true, Ordering::Release);
        self.remove_on_drop.store(false, Ordering::Release);
    }

    /// The guarded path, if not yet disarmed.
    pub fn path(&self) -> Option<PathBuf> {
        self.inner.lock().unwrap().clone()
    }

    /// Whether this still-armed lease owns the exact filesystem view supplied
    /// to item resolution. Workspace layout names stay in the workspace
    /// implementation; consumers compare the authority carried by the guard
    /// instead of reconstructing ownership from path strings.
    pub fn owns_effective_path(&self, candidate: &std::path::Path) -> bool {
        self.inner.lock().unwrap().is_some() && self.effective_path == candidate
    }

    /// Transfer ownership without removing the directory. Returns the
    /// path; subsequent drops are no-ops. Used by callers that hand
    /// off lifecycle to a long-running detached owner.
    pub fn disarm(&self) -> Option<PathBuf> {
        self.inner.lock().unwrap().take()
    }

    /// Remove the exact pinned directory tree now. Failure leaves the guard
    /// armed so recovery retains both the journal evidence and the path.
    pub fn remove_now(&self) -> anyhow::Result<()> {
        if !self.owns_removal {
            anyhow::bail!("borrowed cache/workspace guard does not own directory removal");
        }
        if self
            .workspace_view
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|slot| !matches!(slot, WorkspaceViewSlot::Closed { .. }))
        {
            anyhow::bail!("workspace removal requires original view physical-close proof");
        }
        let mut path_slot = self.inner.lock().unwrap();
        let Some(path) = path_slot.as_ref() else {
            return Ok(());
        };
        if let Some(pinned) = &self.pinned_removal {
            pinned.root.remove_contents_recursive()?;
            if !pinned
                .parent
                .remove_empty_child_if_same(&pinned.name, &pinned.root)?
            {
                anyhow::bail!("guarded directory remained non-empty: {}", path.display());
            }
        } else {
            let path_name = path
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("guarded directory has no final component"))?;
            let parent_path = path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("guarded directory has no parent"))?;
            let opened_parent = lillux::PinnedDirectory::open(parent_path)?
                .ok_or_else(|| anyhow::anyhow!("guarded directory parent disappeared"))?;
            let opened_root = opened_parent
                .open_child_directory(path_name)?
                .ok_or_else(|| anyhow::anyhow!("guarded directory disappeared"))?;
            opened_root.remove_contents_recursive()?;
            if !opened_parent.remove_empty_child_if_same(path_name, &opened_root)? {
                anyhow::bail!("guarded directory remained non-empty: {}", path.display());
            }
        }
        *path_slot = None;
        self.leases.lock().unwrap().clear();
        Ok(())
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        if let Some(p) = self.inner.lock().unwrap().take() {
            if self.explicit_cleanup.load(Ordering::Acquire) {
                tracing::error!(
                    path = %p.display(),
                    "backend workspace guard dropped while still armed; preserving for journal reconciliation"
                );
            } else if self.remove_on_drop.load(Ordering::Acquire) {
                let removal = if let Some(pinned) = &self.pinned_removal {
                    pinned.root.remove_contents_recursive().and_then(|()| {
                        pinned
                            .parent
                            .remove_empty_child_if_same(&pinned.name, &pinned.root)
                            .and_then(|removed| {
                                if removed {
                                    Ok(())
                                } else {
                                    anyhow::bail!("pinned temporary directory identity changed")
                                }
                            })
                    })
                } else {
                    lillux::remove_dir_all_durable(&p)
                };
                if let Err(error) = removal {
                    tracing::warn!(path = %p.display(), %error, "temporary directory cleanup failed");
                }
            }
        }
    }
}

/// Create one projectless execution workspace through descriptor-rooted Lillux
/// authority. The returned guard retains the exact parent/root inodes and
/// removes only that identity; pathname re-resolution is never cleanup
/// authority.
pub fn create_projectless_workspace(
    runtime_cache_root: &std::path::Path,
    workspace_name: &str,
) -> anyhow::Result<(PathBuf, Arc<TempDirGuard>)> {
    let execution_root =
        lillux::PinnedDirectory::open_or_create(&runtime_cache_root.join("executions"))?;
    execution_root.set_mode(0o700)?;
    let name = std::ffi::OsString::from(workspace_name);
    let workspace = execution_root.create_child(&name, 0o700)?;
    workspace.create_child(std::ffi::OsStr::new(ryeos_engine::AI_DIR), 0o700)?;
    let path = workspace.path().to_path_buf();
    let guard = Arc::new(TempDirGuard::new_pinned(execution_root, name, workspace));
    Ok((path, guard))
}

const ADMITTED_INPUT_WORKSPACE_PREFIX: &str = "admitted-input-";
const MAX_ADMITTED_INPUT_WORKSPACE_ROOTS: usize = 65_536;

/// Create the per-process root used to deliver an already-admitted sparse
/// source/external view. The root is mechanical execution state: its name and
/// materialization strategy never enter program or effect identity.
pub fn create_admitted_input_workspace(
    runtime_cache_root: &std::path::Path,
    thread_id: &str,
) -> anyhow::Result<(PathBuf, Arc<TempDirGuard>)> {
    ryeos_runtime::validate_runtime_thread_id(thread_id)
        .map_err(|error| anyhow::anyhow!("invalid admitted-input thread id: {error}"))?;
    create_projectless_workspace(
        runtime_cache_root,
        &format!("{ADMITTED_INPUT_WORKSPACE_PREFIX}{thread_id}"),
    )
}

/// Remove an abandoned admitted-input root for one exclusively claimed
/// thread. Callers must first prove that no prior process owner remains alive.
/// Persistent-session and pinned-COW roots use different namespaces and are
/// structurally outside this cleanup.
pub fn remove_abandoned_admitted_input_workspace(
    runtime_cache_root: &std::path::Path,
    thread_id: &str,
) -> anyhow::Result<bool> {
    ryeos_runtime::validate_runtime_thread_id(thread_id)
        .map_err(|error| anyhow::anyhow!("invalid admitted-input thread id: {error}"))?;
    let Some(execution_root) =
        lillux::PinnedDirectory::open(&runtime_cache_root.join("executions"))?
    else {
        return Ok(false);
    };
    let name = std::ffi::OsString::from(format!("{ADMITTED_INPUT_WORKSPACE_PREFIX}{thread_id}"));
    let Some(workspace) = execution_root.open_child_directory(&name)? else {
        return Ok(false);
    };
    workspace.remove_contents_recursive()?;
    if !execution_root.remove_empty_child_if_same(&name, &workspace)? {
        anyhow::bail!("abandoned admitted-input workspace remained non-empty");
    }
    Ok(true)
}

/// Inventory the exact transient-input namespace for reconciliation. Only the
/// reserved directory form is accepted; malformed entries fail startup rather
/// than being ignored or treated as deletion authority.
pub fn admitted_input_workspace_thread_ids(
    runtime_cache_root: &std::path::Path,
) -> anyhow::Result<Vec<String>> {
    let Some(execution_root) =
        lillux::PinnedDirectory::open(&runtime_cache_root.join("executions"))?
    else {
        return Ok(Vec::new());
    };
    let mut thread_ids = Vec::new();
    for entry in execution_root.entries_no_follow_bounded(MAX_ADMITTED_INPUT_WORKSPACE_ROOTS)? {
        let Some(name) = entry.name.to_str() else {
            continue;
        };
        let Some(thread_id) = name.strip_prefix(ADMITTED_INPUT_WORKSPACE_PREFIX) else {
            continue;
        };
        if entry.entry_type != lillux::PinnedEntryType::Directory {
            anyhow::bail!("admitted-input namespace entry {name} is not a directory");
        }
        ryeos_runtime::validate_runtime_thread_id(thread_id)
            .map_err(|error| anyhow::anyhow!("invalid admitted-input workspace name: {error}"))?;
        thread_ids.push(thread_id.to_owned());
    }
    thread_ids.sort();
    Ok(thread_ids)
}

/// Create one durable runtime-workspace root through the same pinned
/// `.ai/state/cache/executions` authority consumed by the isolation runtime.
/// The caller retains this original view slot through its execution owner and
/// pool borrowers. Before durable reservation it rolls back; after reservation
/// preserve it for explicit cleanup. Never disarm it merely because Create
/// returned successfully.
pub fn create_runtime_workspace(
    runtime_cache_root: &std::path::Path,
    workspace_name: &str,
) -> anyhow::Result<(PathBuf, Arc<TempDirGuard>)> {
    let execution_root =
        lillux::PinnedDirectory::open_or_create(&runtime_cache_root.join("executions"))?;
    execution_root.set_mode(0o700)?;
    let name = std::ffi::OsString::from(workspace_name);
    let workspace = execution_root.create_child(&name, 0o700)?;
    workspace.create_child(
        std::ffi::OsStr::new(ryeos_engine::execution_workspace::PROJECT_DIR),
        0o700,
    )?;
    workspace.sync()?;
    let project = workspace
        .path()
        .join(ryeos_engine::execution_workspace::PROJECT_DIR);
    let mut guard = TempDirGuard::new_pinned(execution_root, name, workspace);
    guard.effective_path = project.clone();
    guard.workspace_view = Mutex::new(Some(WorkspaceViewSlot::Uncreated));
    let guard = Arc::new(guard);
    Ok((project, guard))
}

impl std::fmt::Debug for TempDirGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TempDirGuard")
            .field("path", &self.path())
            .field("effective_path", &self.effective_path)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn removes_dir_on_last_arc_drop() {
        let tmp = tempfile::tempdir().unwrap();
        // tempfile::tempdir creates a real dir; steal its path so we
        // can manage lifecycle ourselves.
        let path = tmp.keep();
        assert!(path.exists(), "dir must exist before guard");

        let g1 = Arc::new(TempDirGuard::new(path.clone()));
        let g2 = Arc::clone(&g1);

        // Drop first Arc — dir must survive.
        drop(g1);
        assert!(path.exists(), "dir survives while one Arc alive");

        // Drop second Arc — dir removed.
        drop(g2);
        assert!(!path.exists(), "dir removed on last Arc drop");
    }

    #[test]
    fn runtime_workspace_uses_the_canonical_execution_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let (project, guard) = create_runtime_workspace(tmp.path(), "runtime-one").unwrap();
        let root = project.parent().unwrap();
        assert_eq!(root.parent().unwrap(), tmp.path().join("executions"));
        assert!(root.join("project").is_dir());
        assert!(!root.join("backend-state").exists());
        assert!(!root.join("upper").exists());
        assert!(!root.join("work").exists());
        drop(guard);
        assert!(!root.exists());
    }

    #[test]
    fn durable_reservation_transfers_drop_to_explicit_reconciliation() {
        let tmp = tempfile::tempdir().unwrap();
        let (project, guard) = create_runtime_workspace(tmp.path(), "runtime-durable").unwrap();
        let root = project.parent().unwrap().to_path_buf();
        guard.preserve_for_explicit_cleanup();
        drop(guard);
        assert!(root.is_dir());

        // A newly opened path guard is not the original retained view owner.
        // The test's disposable fixture cleans this refused history on drop.
        let reopened = TempDirGuard::new_workspace(root.clone(), project).unwrap();
        assert!(reopened.remove_now().is_err());
        assert!(root.exists());
    }

    fn created_view_evidence(
        workspace_id: &str,
    ) -> ryeos_engine::isolation::WorkspaceLifecycleEvidence {
        ryeos_engine::isolation::WorkspaceLifecycleEvidence {
            operation: ryeos_isolation_protocol::WorkspaceLifecycleOperation::Create,
            workspace_id: workspace_id.to_owned(),
            launch_owner: "{\"attempt\":1}".to_owned(),
            backend_id: "test-backend".to_owned(),
            backend_version: "1".to_owned(),
            pinned_root_identities: std::collections::BTreeMap::from([
                ("project".to_owned(), "test-project".to_owned()),
                ("backend_state".to_owned(), "test-state".to_owned()),
            ]),
            mount_identity: Some("a".repeat(64)),
            mutations: Vec::new(),
            destroyed: false,
        }
    }

    // This tests opaque descriptor ownership, not an Overlayfs claim. Actual
    // backend/template validation remains in Lillux's isolated kernel probes.
    fn test_view_authority(path: &std::path::Path) -> lillux::InheritedDescriptorAuthority {
        lillux::PinnedDirectory::open(path)
            .unwrap()
            .unwrap()
            .inherited_descriptor_authority()
            .unwrap()
    }

    fn close_deadline() -> lillux::time::MonotonicDeadline {
        lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(1))
    }

    #[test]
    fn workspace_install_and_borrow_require_the_original_exact_coordinate() {
        let cache = tempfile::tempdir().unwrap();
        let (project, guard) = create_runtime_workspace(cache.path(), "view-one").unwrap();
        let evidence = created_view_evidence("view-one");
        let wrong = created_view_evidence("other-workspace");
        assert!(
            guard
                .install_workspace_view(
                    &wrong,
                    ryeos_engine::isolation::CreatedWorkspaceView::Descriptor(test_view_authority(
                        &project
                    ))
                )
                .is_err()
        );
        assert!(guard.workspace_view_identity().is_err());
        guard
            .install_workspace_view(
                &evidence,
                ryeos_engine::isolation::CreatedWorkspaceView::Descriptor(test_view_authority(
                    &project,
                )),
            )
            .unwrap();
        assert_eq!(
            guard.workspace_view_identity().unwrap(),
            Some(("view-one".into(), "a".repeat(64)))
        );
        assert!(
            guard
                .borrow_workspace_view("other-workspace", &"a".repeat(64))
                .is_err()
        );
        assert!(
            guard
                .borrow_workspace_view("view-one", &"b".repeat(64))
                .is_err()
        );
        assert!(
            guard
                .install_workspace_view(
                    &evidence,
                    ryeos_engine::isolation::CreatedWorkspaceView::Disabled
                )
                .is_err()
        );
        assert!(
            guard
                .close_workspace_view("view-one", &"b".repeat(64), close_deadline())
                .is_err()
        );
        // A wrong coordinate does not drain or replace the legitimate slot.
        assert!(guard.workspace_view_identity().is_ok());
        guard
            .close_workspace_view("view-one", &"a".repeat(64), close_deadline())
            .unwrap();
        guard.remove_now().unwrap();
    }

    #[test]
    fn workspace_alias_refusal_drains_admission_and_last_owner_retry_closes() {
        let cache = tempfile::tempdir().unwrap();
        let (project, guard) = create_runtime_workspace(cache.path(), "view-alias").unwrap();
        guard
            .install_workspace_view(
                &created_view_evidence("view-alias"),
                ryeos_engine::isolation::CreatedWorkspaceView::Descriptor(test_view_authority(
                    &project,
                )),
            )
            .unwrap();
        let borrower = guard
            .borrow_workspace_view("view-alias", &"a".repeat(64))
            .unwrap()
            .unwrap();
        assert!(
            guard
                .close_workspace_view("view-alias", &"a".repeat(64), close_deadline())
                .is_err()
        );
        assert!(guard.workspace_view_identity().is_err());
        assert!(
            guard
                .borrow_workspace_view("view-alias", &"a".repeat(64))
                .is_err()
        );
        assert!(guard.remove_now().is_err());
        assert!(project.is_dir());
        drop(borrower);
        guard
            .close_workspace_view("view-alias", &"a".repeat(64), close_deadline())
            .unwrap();
        // Exact close replay is idempotent; closed slots never reopen borrowing.
        guard
            .close_workspace_view("view-alias", &"a".repeat(64), close_deadline())
            .unwrap();
        assert!(
            guard
                .borrow_workspace_view("view-alias", &"a".repeat(64))
                .is_err()
        );
        guard.remove_now().unwrap();
        assert!(!project.exists());
    }

    #[test]
    fn uncreated_or_reopened_workspace_cannot_supply_close_or_removal_proof() {
        let cache = tempfile::tempdir().unwrap();
        let (project, original) = create_runtime_workspace(cache.path(), "view-uncreated").unwrap();
        original.preserve_for_explicit_cleanup();
        assert!(
            original
                .close_workspace_view("view-uncreated", &"a".repeat(64), close_deadline())
                .is_err()
        );
        assert!(original.remove_now().is_err());
        let root = original.path().unwrap();
        let reopened = TempDirGuard::new_workspace(root, project.clone()).unwrap();
        assert!(
            reopened
                .close_workspace_view("view-uncreated", &"a".repeat(64), close_deadline())
                .is_err()
        );
        assert!(
            reopened
                .borrow_workspace_view("view-uncreated", &"a".repeat(64))
                .is_err()
        );
        assert!(reopened.remove_now().is_err());
        assert!(project.is_dir());
    }

    #[test]
    fn projectless_controller_retains_the_original_separate_workspace_owner() {
        let cache = tempfile::tempdir().unwrap();
        let (controller_path, controller) =
            create_projectless_workspace(cache.path(), "controller").unwrap();
        let (project, workspace) = create_runtime_workspace(cache.path(), "worker-view").unwrap();
        assert!(controller.owned_workspace_lifeline().unwrap().is_none());
        assert!(Arc::ptr_eq(
            &workspace,
            &workspace.owned_workspace_lifeline().unwrap().unwrap()
        ));
        assert!(!project.starts_with(&controller_path));
        controller
            .retain_owned_workspace_lifeline(workspace.clone())
            .unwrap();
        controller
            .retain_owned_workspace_lifeline(workspace.clone())
            .unwrap();
        let weak = Arc::downgrade(&workspace);
        drop(workspace);
        let retained = controller.owned_workspace_lifeline().unwrap().unwrap();
        assert!(Arc::ptr_eq(&retained, &weak.upgrade().unwrap()));
        assert!(retained.owns_effective_path(&project));
        drop(retained);
        drop(controller);
        assert!(weak.upgrade().is_none());
        assert!(!controller_path.exists());
        assert!(!project.exists());
    }

    #[test]
    fn dependent_workspace_refuses_self_nested_and_second_owners() {
        let cache = tempfile::tempdir().unwrap();
        let (controller_path, controller) =
            create_projectless_workspace(cache.path(), "controller").unwrap();
        assert!(
            controller
                .retain_owned_workspace_lifeline(controller.clone())
                .is_err()
        );
        let (_, nested) = create_runtime_workspace(&controller_path, "nested").unwrap();
        assert!(
            controller
                .retain_owned_workspace_lifeline(nested.clone())
                .is_err()
        );
        assert!(controller.owned_workspace_lifeline().unwrap().is_none());
        let (_, first) = create_runtime_workspace(cache.path(), "first").unwrap();
        let (_, second) = create_runtime_workspace(cache.path(), "second").unwrap();
        controller
            .retain_owned_workspace_lifeline(first.clone())
            .unwrap();
        assert!(
            controller
                .retain_owned_workspace_lifeline(second.clone())
                .is_err()
        );
        assert!(first.retain_owned_workspace_lifeline(second).is_err());
        assert!(Arc::ptr_eq(
            &first,
            &controller.owned_workspace_lifeline().unwrap().unwrap()
        ));
        drop(nested);
    }

    #[test]
    fn workspace_guard_carries_exact_effective_path_authority() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("workspace");
        let effective = root.join("project");
        std::fs::create_dir_all(&effective).unwrap();
        let guard = TempDirGuard::new_workspace(root.clone(), effective.clone()).unwrap();

        assert!(guard.owns_effective_path(&effective));
        assert!(!guard.owns_effective_path(&root));
        assert!(!guard.owns_effective_path(&root.join("other")));
        guard.disarm();
    }

    #[test]
    fn workspace_guard_rejects_effective_path_outside_owned_root() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("workspace");
        let foreign = parent.path().join("foreign");
        assert!(TempDirGuard::new_workspace(root, foreign).is_err());
    }

    #[test]
    fn disarm_prevents_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.keep();
        assert!(path.exists());

        let g = Arc::new(TempDirGuard::new(path.clone()));
        let stolen = g.disarm();
        assert_eq!(stolen, Some(path.clone()));

        // Drop the guard — dir must survive because it was disarmed.
        drop(g);
        assert!(path.exists(), "disarmed guard does not remove dir");

        // Clean up manually.
        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn path_returns_none_after_disarm() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.keep();
        let g = TempDirGuard::new(path);
        assert!(g.path().is_some());
        g.disarm();
        assert!(g.path().is_none(), "path returns None after disarm");
        // Prevent TempDirGuard from trying to remove the disarmed dir
        // (it was disarmed, so drop is a no-op, but let's be explicit).
        drop(g);
    }

    #[test]
    fn admitted_input_workspace_is_per_thread_and_removed_by_its_guard() {
        let cache = tempfile::tempdir().unwrap();
        let thread_id = "T-00000000-0000-0000-0000-000000000001";
        let (path, guard) = create_admitted_input_workspace(cache.path(), thread_id).unwrap();
        assert!(path.ends_with(format!("admitted-input-{thread_id}")));
        assert!(path.join(ryeos_engine::AI_DIR).is_dir());
        std::fs::write(path.join("scratch"), b"private").unwrap();
        drop(guard);
        assert!(!path.exists());
    }

    #[test]
    fn abandoned_input_cleanup_cannot_cross_its_reserved_namespace() {
        let cache = tempfile::tempdir().unwrap();
        let thread_id = "T-00000000-0000-0000-0000-000000000002";
        let (path, guard) = create_admitted_input_workspace(cache.path(), thread_id).unwrap();
        std::fs::write(path.join("scratch"), b"abandoned").unwrap();
        guard.disarm();
        drop(guard);

        assert!(remove_abandoned_admitted_input_workspace(cache.path(), thread_id).unwrap());
        assert!(!path.exists());
        assert!(!remove_abandoned_admitted_input_workspace(cache.path(), thread_id).unwrap());
        assert!(remove_abandoned_admitted_input_workspace(cache.path(), "not-a-thread").is_err());
    }

    #[test]
    fn admitted_input_inventory_finds_unattached_terminal_residue() {
        let cache = tempfile::tempdir().unwrap();
        let thread_id = "T-00000000-0000-0000-0000-000000000003";
        let (path, guard) = create_admitted_input_workspace(cache.path(), thread_id).unwrap();
        std::fs::write(path.join("terminal-output"), b"settled").unwrap();
        guard.disarm();
        drop(guard);

        assert_eq!(
            admitted_input_workspace_thread_ids(cache.path()).unwrap(),
            vec![thread_id.to_owned()]
        );
        assert!(remove_abandoned_admitted_input_workspace(cache.path(), thread_id).unwrap());
        assert!(
            admitted_input_workspace_thread_ids(cache.path())
                .unwrap()
                .is_empty()
        );
    }
}
