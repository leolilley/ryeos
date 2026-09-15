//! Shared descriptor-rooted project namespace authority.
//!
//! Project mutation protocols must not reimplement pathname traversal or raw
//! OS descriptor operations in handlers.  They pin the project root once and
//! traverse normalized relative coordinates through Lillux.

use std::ffi::OsString;
use std::path::{Component, Path};

use anyhow::{Context, Result};

const PROJECT_MUTATION_LOCK_TIMEOUT: lillux::time::Duration = lillux::time::Duration::from_secs(5);

pub(crate) fn relative_parent(
    root: &lillux::PinnedDirectory,
    relative: &str,
    create: bool,
) -> Result<Option<(lillux::PinnedDirectory, OsString)>> {
    relative_parent_with_mode(root, relative, create, 0o700)
}

pub(crate) fn relative_parent_with_mode(
    root: &lillux::PinnedDirectory,
    relative: &str,
    create: bool,
    directory_mode: u32,
) -> Result<Option<(lillux::PinnedDirectory, OsString)>> {
    ryeos_state::project_sync::validate_safe_relative_path(relative)?;
    let mut parent = root.try_clone()?;
    let mut components = Path::new(relative).components().peekable();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            anyhow::bail!("project path is not normalized: {relative}");
        };
        if components.peek().is_none() {
            return Ok(Some((parent, name.to_os_string())));
        }
        parent = if create {
            parent.open_or_create_child(name, directory_mode)?
        } else {
            let Some(child) = parent.open_child_directory(name)? else {
                return Ok(None);
            };
            child
        };
    }
    Ok(None)
}

pub(crate) async fn acquire_project_mutation_lock(
    project_root: &lillux::PinnedDirectory,
) -> Result<lillux::PinnedDirectoryLock> {
    let project_root = project_root.try_clone()?;
    tokio::task::spawn_blocking(move || {
        project_root.ensure_path_binding()?;
        let lock = project_root
            .lock_exclusive_with_timeout(PROJECT_MUTATION_LOCK_TIMEOUT)
            .context("could not acquire the project mutation lock")?;
        project_root.ensure_path_binding()?;
        Ok(lock)
    })
    .await
    .context("join project mutation-lock acquisition")?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traversal_rejects_parent_escape_before_creating_any_entry() {
        let root = tempfile::TempDir::new().unwrap();
        let pinned = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        assert!(relative_parent(&pinned, "../outside", true).is_err());
        assert!(!root.path().parent().unwrap().join("outside").exists());
    }

    #[test]
    fn traversal_rejects_symlinked_directory_component() {
        #[cfg(not(unix))]
        return;
        #[cfg(unix)]
        {
            let root = tempfile::TempDir::new().unwrap();
            let outside = tempfile::TempDir::new().unwrap();
            std::os::unix::fs::symlink(outside.path(), root.path().join("linked")).unwrap();
            let pinned = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
            assert!(relative_parent(&pinned, "linked/value", true).is_err());
            assert!(!outside.path().join("value").exists());
        }
    }
}
