//! Shared RAII guard for materialised temp directories.
//!
//! A single `TempDirGuard` type used across the engine cache, executor,
//! and API layers. Wrap in `Arc<TempDirGuard>` for shared ownership:
//!
//! | Owner | What it holds |
//! |---|---|
//! | Engine cache entry | `Arc<TempDirGuard>` for user overlay |
//! | Request runner | `Arc<TempDirGuard>` for project checkout |
//! | Callback token lifeline | `Arc<TempDirGuard>` (callback workstream) |
//!
//! The directory is removed recursively when the **last** `Arc` holder
//! drops. The internal `Mutex<Option<PathBuf>>` allows `disarm()` to
//! transfer ownership to a long-running detached owner without dropping
//! the dir. Disarm is rare; the common path is just Drop.

use std::path::PathBuf;
use std::sync::Mutex;

/// RAII guard for a materialised temp directory. Removes the directory
/// recursively when the LAST `Arc<TempDirGuard>` drops.
pub struct TempDirGuard {
    inner: Mutex<Option<PathBuf>>,
    leases: Mutex<Vec<std::fs::File>>,
    explicit_cleanup: bool,
    remove_on_drop: bool,
    owns_removal: bool,
}

impl TempDirGuard {
    pub fn new(path: PathBuf) -> Self {
        Self {
            inner: Mutex::new(Some(path)),
            leases: Mutex::new(Vec::new()),
            explicit_cleanup: false,
            remove_on_drop: true,
            owns_removal: true,
        }
    }

    /// A backend-owned workspace must be destroyed and descriptor-removed by
    /// its owner-fenced lifecycle before the journal can close.
    pub fn new_workspace(path: PathBuf) -> Self {
        Self {
            inner: Mutex::new(Some(path)),
            leases: Mutex::new(Vec::new()),
            explicit_cleanup: true,
            remove_on_drop: false,
            owns_removal: true,
        }
    }

    /// Hold a lease and stable path to a shared derived cache generation.
    /// Dropping the guard releases the lease but never removes the shared
    /// generation; cache eviction owns deletion after all leases are gone.
    pub fn new_borrowed_cache(path: PathBuf) -> Self {
        Self {
            inner: Mutex::new(Some(path)),
            leases: Mutex::new(Vec::new()),
            explicit_cleanup: false,
            remove_on_drop: false,
            owns_removal: false,
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
        let name = path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("guarded directory has no final component"))?;
        let parent_path = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("guarded directory has no parent"))?;
        let parent = lillux::PinnedDirectory::open(parent_path)?
            .ok_or_else(|| anyhow::anyhow!("guarded directory parent disappeared"))?;
        let root = parent
            .open_child_directory(name)?
            .ok_or_else(|| anyhow::anyhow!("guarded directory disappeared"))?;
        root.remove_contents_recursive()?;
        if !parent.remove_empty_child_if_same(name, &root)? {
            anyhow::bail!("guarded directory remained non-empty: {}", path.display());
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
                if let Err(error) = std::fs::remove_dir_all(&p) {
                    tracing::warn!(path = %p.display(), %error, "temporary directory cleanup failed");
                }
            }
        }
    }
}

impl std::fmt::Debug for TempDirGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TempDirGuard")
            .field("path", &self.path())
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
