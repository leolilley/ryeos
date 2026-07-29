//! Shared RAII guard for materialised temp directories.
//!
//! A single `TempDirGuard` type used across the engine cache, executor,
//! and API layers. Wrap in `Arc<TempDirGuard>` for shared ownership:
//!
//! | Owner | What it holds |
//! |---|---|
//! | Engine user-overlay cache | `Arc<TempDirGuard>` for its shared overlay |
//! | Admitted request binding | `Arc<TempDirGuard>` for its active checkout |
//! | Request runner | `Arc<TempDirGuard>` for project checkout |
//! | Callback token lifeline | `Arc<TempDirGuard>` (callback workstream) |
//!
//! The resolution cache is deliberately different: it retains no project
//! materialization guard, and rebinds hits to the current admitted checkout.
//!
//! The directory is removed recursively when the **last** `Arc` holder
//! drops. The internal `Mutex<Option<PathBuf>>` allows `disarm()` to
//! transfer ownership to a long-running detached owner without dropping
//! the dir. Disarm is rare; the common path is just Drop.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

struct PinnedRemoval {
    parent: lillux::PinnedDirectory,
    name: std::ffi::OsString,
    root: lillux::PinnedDirectory,
}

/// RAII guard for a materialised temp directory. Removes the directory
/// recursively when the LAST `Arc<TempDirGuard>` drops.
pub struct TempDirGuard {
    inner: Mutex<Option<PathBuf>>,
    effective_path: PathBuf,
    leases: Mutex<Vec<std::fs::File>>,
    explicit_cleanup: bool,
    remove_on_drop: bool,
    owns_removal: bool,
    pinned_removal: Option<PinnedRemoval>,
}

impl TempDirGuard {
    pub fn new(path: PathBuf) -> Self {
        Self {
            inner: Mutex::new(Some(path.clone())),
            effective_path: path,
            leases: Mutex::new(Vec::new()),
            explicit_cleanup: false,
            remove_on_drop: true,
            owns_removal: true,
            pinned_removal: None,
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
            explicit_cleanup: true,
            remove_on_drop: false,
            owns_removal: true,
            pinned_removal: None,
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
            explicit_cleanup: false,
            remove_on_drop: false,
            owns_removal: false,
            pinned_removal: None,
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
            explicit_cleanup: false,
            remove_on_drop: true,
            owns_removal: true,
            pinned_removal: Some(PinnedRemoval { parent, name, root }),
        }
    }

    /// Retain an exact-generation cache lease for the lifetime of this guard.
    pub fn retain_lease(&self, lease: std::fs::File) {
        self.leases.lock().unwrap().push(lease);
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
            if self.explicit_cleanup {
                tracing::error!(
                    path = %p.display(),
                    "backend workspace guard dropped while still armed; preserving for journal reconciliation"
                );
            } else if self.remove_on_drop {
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
    fn workspace_guard_carries_exact_effective_path_authority() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("workspace");
        let effective = root.join("lower");
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
}
