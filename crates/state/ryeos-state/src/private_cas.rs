//! A private scratch content-addressed store with no SQL.
//!
//! The codex runtime verifier captures a guest's candidate tree into a
//! verifier-private scratch CAS: it needs content-addressed objects and blobs
//! under a pinned owner-private directory, and nothing that a node State
//! database provides. This module establishes the State root layout in such a
//! directory and returns a [`PinnedStateAuthority`] over it without opening
//! any database.
//!
//! The returned authority carries an empty trust store, so it cannot validate
//! signed heads. It retains an exclusive lock on the scratch directory for as
//! long as it, or any clone of it, is alive.

use std::ffi::OsStr;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::recovery::CasMutationGuard;
use crate::refs::TrustStore;
use crate::state_db::{PinnedStateAuthority, PreparedOfflineProjection};

/// Content roots of a State runtime directory that a pinned authority
/// requires. The recovery root, with its CAS mutation lock, is established by
/// [`CasMutationGuard::initialize_fresh_recovery_in_pinned_runtime`].
const LAYOUT_ROOTS: [&str; 3] = ["objects", "refs", "locators"];

/// Establish a private scratch CAS in `directory` and return the authority
/// over it.
///
/// `directory` must be an owner-private (mode 0700) directory owned by this
/// process's user, and it must be empty: the scratch store is created fresh
/// for one capture and is never adopted from earlier contents. The layout is
/// created descriptor-relative and without following links. The directory is
/// locked exclusively before anything is created inside it, and an existing
/// holder of that lock is refused rather than waited for.
pub fn open_private_scratch_cas(directory: &lillux::PinnedDirectory) -> Result<PinnedStateAuthority> {
    directory
        .require_owner_private_directory()
        .context("private scratch CAS directory is not owner-private")?;
    let lock = directory
        .try_lock_exclusive()
        .context("lock private scratch CAS directory")?
        .ok_or_else(|| anyhow::anyhow!("private scratch CAS directory is already held"))?;
    lock.ensure_protects(directory)
        .context("private scratch CAS lock does not protect its directory")?;
    anyhow::ensure!(
        directory
            .entries_no_follow_bounded(0)
            .context("private scratch CAS directory is not empty")?
            .is_empty(),
        "private scratch CAS directory is not empty"
    );
    for name in LAYOUT_ROOTS {
        directory
            .create_child(OsStr::new(name), 0o700)
            .with_context(|| format!("create private scratch CAS root {name}"))?;
    }
    CasMutationGuard::initialize_fresh_recovery_in_pinned_runtime(directory)
        .context("establish private scratch CAS recovery and mutation lock")?;
    PreparedOfflineProjection::capture(directory.try_clone()?, Arc::new(TrustStore::new()), &lock)
        .context("capture private scratch CAS layout")?
        .authority()
        .context("pin private scratch CAS authority")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn private_directory() -> (tempfile::TempDir, lillux::PinnedDirectory) {
        let root = tempfile::tempdir().unwrap();
        let directory = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        directory.set_mode(0o700).unwrap();
        (root, directory)
    }

    #[test]
    fn fresh_private_directory_yields_a_usable_cas_without_sql() {
        let (root, directory) = private_directory();
        let authority = open_private_scratch_cas(&directory).unwrap();
        let guard = authority.acquire_shared_guard().unwrap();
        let cas = authority.cas_store().unwrap();
        let hash = cas.store_blob(b"scratch").unwrap();
        assert_eq!(cas.get_blob(&hash).unwrap().as_deref(), Some(b"scratch".as_slice()));
        drop(guard);
        for name in LAYOUT_ROOTS.into_iter().chain(["recovery"]) {
            assert!(root.path().join(name).is_dir(), "{name}");
        }
        let databases = std::fs::read_dir(root.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "sqlite3"))
            .count();
        assert_eq!(databases, 0);
    }

    #[test]
    fn held_or_non_empty_directories_are_refused() {
        let (_root, directory) = private_directory();
        let authority = open_private_scratch_cas(&directory).unwrap();
        // The authority still holds the lock, and the layout now exists.
        assert!(open_private_scratch_cas(&directory).is_err());
        drop(authority);
        // Released, but no longer empty: scratch contents are never adopted.
        assert!(open_private_scratch_cas(&directory).is_err());
    }

    #[test]
    fn shared_directories_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let directory = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        directory.set_mode(0o755).unwrap();
        assert!(open_private_scratch_cas(&directory).is_err());
    }
}
